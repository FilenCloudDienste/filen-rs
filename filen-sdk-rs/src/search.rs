use std::{
	borrow::Cow,
	cmp::min,
	collections::{HashSet, VecDeque},
};

use filen_types::api::v3::search::{
	add::{SearchAddItem, SearchAddItemType},
	find::SearchFindItem,
};

use crate::{
	api,
	auth::Client,
	crypto::shared::MetaCrypter,
	fs::{HasMeta, HasUUID, NonRootFSObject, dir::DirectoryMeta, file::RemoteFile},
};

struct SplitName<'a> {
	normalized_input: Cow<'a, str>,
	results: VecDeque<&'a str>,
}

impl<'a> Iterator for SplitName<'a> {
	type Item = &'a str;
	fn next(&mut self) -> Option<Self::Item> {
		if self.results.is_empty() {
			return None;
		}
		self.results.pop_front()
	}
}

pub fn split_name(input: &str, min_len: usize, max_len: usize) -> Vec<Cow<'_, str>> {
	if input.is_empty() {
		return Vec::new();
	}

	// optimization so we only allocate if the input has uppercase letters
	let normalized_input = if input.chars().any(|c| c.is_uppercase()) {
		Cow::Owned(input.trim().to_lowercase())
	} else {
		Cow::Borrowed(input.trim())
	};

	let normalized_input_len = normalized_input.len();
	let mut results = HashSet::new();
	// let mut results = vec![normalized_input.into()];
	let max_len = min(max_len, normalized_input.len());

	for start_index in 0..normalized_input_len {
		for length in min_len..=max_len {
			if start_index + length <= normalized_input_len {
				let substring = match &normalized_input {
					Cow::Borrowed(s) => Cow::Borrowed(&s[start_index..start_index + length]),
					Cow::Owned(s) => Cow::Owned(s[start_index..start_index + length].to_string()),
				};
				results.insert(substring);
			}
		}
	}

	results.insert(normalized_input);

	let mut results: Vec<Cow<'_, str>> = results.into_iter().collect();

	results.sort_unstable();
	results.truncate(4096);
	results
}

pub fn generate_search_hashes_for_name(
	name: &str,
	client: &Client,
) -> impl IntoIterator<Item = String> {
	split_name(name, 2, 16)
		.into_iter()
		.map(|s| client.hmac_key.hash_to_string(s.as_bytes()))
}

pub fn generate_search_items_for_item<'a>(
	item: &'a NonRootFSObject<'a>,
	client: &'a Client,
) -> impl IntoIterator<Item = filen_types::api::v3::search::add::SearchAddItem> {
	let item_type = match item {
		NonRootFSObject::File(_) => SearchAddItemType::File,
		NonRootFSObject::Dir(_) => SearchAddItemType::Directory,
	};

	let uuid = item.uuid();

	generate_search_hashes_for_name(item.name(), client)
		.into_iter()
		.map(move |hash| SearchAddItem {
			hash,
			uuid,
			r#type: item_type,
		})
}

pub async fn update_search_hashes_for_item<'a>(
	client: &'a Client,
	item: impl Into<NonRootFSObject<'a>>,
) -> Result<api::v3::search::add::Response, filen_types::error::ResponseError> {
	let items = generate_search_items_for_item(&item.into(), client)
		.into_iter()
		.collect::<Vec<_>>();
	api::v3::search::add::post(client.client(), &api::v3::search::add::Request { items }).await
}

pub async fn find_item_matches_for_name(
	client: &Client,
	name: impl AsRef<str>,
) -> Result<Vec<(NonRootFSObject<'static>, String)>, crate::error::Error> {
	let name = name.as_ref().trim().to_lowercase();
	let response = api::v3::search::find::post(
		client.client(),
		&api::v3::search::find::Request {
			hashes: vec![client.hmac_key.hash(name.as_bytes())],
		},
	)
	.await?;
	response
		.items
		.into_iter()
		.map(|item| {
			let (item, metadata_path) = match item {
				SearchFindItem::Dir(found_dir) => (
					NonRootFSObject::Dir(Cow::Owned(crate::fs::dir::Directory::from_encrypted(
						filen_types::api::v3::dir::content::Directory {
							uuid: found_dir.uuid,
							meta: found_dir.metadata,
							parent: found_dir.parent,
							color: found_dir.color,
							timestamp: found_dir.timestamp,
							favorited: found_dir.favorited,
							is_sync: false,
							is_default: false,
						},
						client.crypter(),
					)?)),
					found_dir.metadata_path,
				),
				SearchFindItem::File(found_file) => (
					NonRootFSObject::File(Cow::Owned(RemoteFile::from_encrypted(
						filen_types::api::v3::dir::content::File {
							uuid: found_file.uuid,
							parent: found_file.parent,
							size: found_file.size,
							timestamp: found_file.timestamp,
							metadata: found_file.metadata,
							chunks: found_file.chunks,
							bucket: found_file.bucket,
							region: found_file.region,
							version: found_file.version,
							favorited: found_file.favorited,
							rm: "".to_string(),
						},
						client.crypter(),
					)?)),
					found_file.metadata_path,
				),
			};

			let mut path = metadata_path
				.into_iter()
				.filter_map(|meta| match meta.0.as_ref() {
					"default" => None,
					_ => {
						let decrypted = match client.crypter().decrypt_meta(&meta) {
							Ok(decrypted) => decrypted,
							Err(e) => {
								return Some(Err(e));
							}
						};
						Some(match serde_json::from_str::<DirectoryMeta>(&decrypted) {
							Ok(meta) => Ok(meta),
							Err(e) => Err(e.into()),
						})
					}
				})
				.try_fold("/".to_string(), |mut acc, meta| match meta {
					Ok(meta) => {
						acc.push_str(meta.name());
						acc.push('/');
						Ok(acc)
					}
					Err(e) => Err(e),
				})?;
			if path.len() > 1 {
				path.pop(); // remove final /
			}

			Ok((item, path))
		})
		.collect()
}

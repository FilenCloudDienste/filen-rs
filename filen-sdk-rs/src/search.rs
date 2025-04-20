use std::{borrow::Cow, cmp::min, collections::HashSet};

use filen_types::api::v3::search::{
	add::{SearchAddItem, SearchAddItemType},
	find::SearchFindItem,
};

use crate::{
	api,
	auth::Client,
	crypto::shared::MetaCrypter,
	fs::{NonRootFSObject, dir::DirectoryMeta, file::RemoteFile},
};

// wish we could return Vec<&str> instead of Vec<String>
// but we need to to_lowercase the input which requires an allocation
pub fn split_name(input: &str, min_len: usize, max_len: usize) -> Vec<String> {
	if input.is_empty() {
		return Vec::new();
	}

	let normalized_input = input.trim().to_lowercase();
	let normalized_input_len = normalized_input.len();
	let mut results = HashSet::new();
	// let mut results = vec![normalized_input.into()];
	let max_len = min(max_len, normalized_input.len());

	for start_index in 0..normalized_input_len {
		for length in min_len..=max_len {
			if start_index + length <= normalized_input_len {
				results.insert(normalized_input[start_index..start_index + length].to_string());
			}
		}
	}

	results.insert(normalized_input);

	let mut results: Vec<String> = results.into_iter().collect();

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
	item: impl Into<NonRootFSObject<'a>>,
	client: &Client,
) -> impl IntoIterator<Item = filen_types::api::v3::search::add::SearchAddItem> {
	let item = item.into();
	let (name, uuid, item_type) = match &item {
		NonRootFSObject::File(file) => (file.name(), file.uuid(), SearchAddItemType::File),
		NonRootFSObject::Dir(dir) => (dir.name(), dir.uuid(), SearchAddItemType::Directory),
	};

	split_name(name, 2, 16)
		.into_iter()
		.map(move |s| SearchAddItem {
			uuid,
			hash: client.hmac_key.hash_to_string(s.as_bytes()),
			r#type: item_type,
		})
}

pub async fn update_search_hashes_for_items<'a>(
	client: &'a Client,
	items: impl IntoIterator<Item = impl Into<NonRootFSObject<'a>>>,
) -> Result<api::v3::search::add::Response, filen_types::error::ResponseError> {
	let items = items
		.into_iter()
		.flat_map(|item| generate_search_items_for_item(item, client))
		.collect::<Vec<_>>();

	api::v3::search::add::post(client.client(), &api::v3::search::add::Request { items }).await
}

pub async fn update_search_hashes_for_item<'a>(
	client: &'a Client,
	item: impl Into<NonRootFSObject<'a>>,
) -> Result<api::v3::search::add::Response, filen_types::error::ResponseError> {
	update_search_hashes_for_items(client, std::iter::once(item)).await
}

pub async fn find_item_matches_for_name(
	client: &Client,
	name: impl AsRef<str>,
) -> Result<Vec<(NonRootFSObject<'static>, String)>, crate::error::Error> {
	let name = name.as_ref().trim().to_lowercase();
	let response = api::v3::search::find::post(
		client.client(),
		&api::v3::search::find::Request {
			hashes: vec![client.hmac_key.hash_to_string(name.as_bytes())],
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

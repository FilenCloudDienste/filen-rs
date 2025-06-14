use std::{
	borrow::Cow,
	cmp::min,
	collections::{HashSet, VecDeque},
};

use filen_types::api::v3::search::{add::SearchAddItem, find::SearchFindItem};

use crate::{
	api,
	auth::Client,
	crypto::shared::MetaCrypter,
	error::Error,
	fs::{
		HasName, HasType, HasUUID, NonRootFSObject,
		dir::{DirectoryMeta, RemoteDirectory},
		file::{RemoteFile, meta::FileMeta},
	},
};

pub struct SplitName<'a> {
	normalized_input: Cow<'a, str>,
	min_len: usize,
	max_len: usize,
}

fn trim_string_in_place(s: &mut String) {
	let left_trim = s.chars().take_while(|&c| c.is_ascii_whitespace()).count();
	let right_trim = s
		.chars()
		.rev()
		.take_while(|&c| c.is_ascii_whitespace())
		.count();

	s.drain(..left_trim);
	s.truncate(s.len() - right_trim - left_trim);
}

impl<'a> SplitName<'a> {
	fn new(input: impl Into<Cow<'a, str>>, min_len: usize, max_len: usize) -> Self {
		let normalized_input = match input.into() {
			Cow::Borrowed(s) => {
				if s.chars().any(|c| c.is_uppercase()) {
					Cow::Owned(s.trim().to_lowercase())
				} else {
					Cow::Borrowed(s.trim())
				}
			}
			Cow::Owned(mut s) => {
				trim_string_in_place(&mut s);
				Cow::Owned(s)
			}
		};

		Self {
			normalized_input,
			min_len,
			max_len,
		}
	}

	pub fn iter(&self) -> SplitNameIter<'_> {
		// Collect character boundaries (byte indices) for the string
		let char_boundaries: Vec<usize> = self
			.normalized_input
			.char_indices()
			.map(|(i, _)| i)
			.chain(std::iter::once(self.normalized_input.len())) // Add the end boundary
			.collect();

		let char_count = char_boundaries.len() - 1; // Number of characters

		let max_len = min(self.max_len, char_count);
		let min_len = min(self.min_len, max_len);

		let substring_count =
			(max_len - min_len + 1) * ((char_count + 1) - (max_len + min_len) / 2);

		let mut slices = HashSet::with_capacity(substring_count + 1);

		// Iterate over character positions, not byte positions
		for start_char in 0..char_count {
			for end_char in start_char + self.min_len..self.max_len {
				if end_char > char_count {
					break;
				}
				let start_byte = char_boundaries[start_char];
				let end_byte = char_boundaries[end_char];

				let substring = &self.normalized_input[start_byte..end_byte];
				slices.insert(substring);
			}
		}

		slices.insert(&self.normalized_input);
		let mut slices: VecDeque<&str> = slices.into_iter().collect();
		slices.make_contiguous().sort_unstable();
		slices.truncate(4096);

		SplitNameIter { slices }
	}
}

pub struct SplitNameIter<'a> {
	slices: VecDeque<&'a str>,
}

impl<'a> Iterator for SplitNameIter<'a> {
	type Item = &'a str;
	fn next(&mut self) -> Option<Self::Item> {
		self.slices.pop_front()
	}
}

pub fn split_name(input: &str, min_len: usize, max_len: usize) -> SplitName {
	SplitName::new(input, min_len, max_len)
}

impl Client {
	pub fn generate_search_items_for_item<I>(&self, item: &I) -> Vec<SearchAddItem>
	where
		I: HasName + HasUUID + HasType,
	{
		split_name(item.name(), 2, 16)
			.iter()
			.map(move |s| SearchAddItem {
				hash: self.hmac_key.hash(s.as_bytes()),
				uuid: item.uuid(),
				r#type: item.object_type().into(),
			})
			.collect()
	}

	pub async fn update_search_hashes_for_item<I>(
		&self,
		item: &I,
	) -> Result<api::v3::search::add::Response, Error>
	where
		I: HasName + HasUUID + HasType,
	{
		let items = self.generate_search_items_for_item(item);
		api::v3::search::add::post(self.client(), &api::v3::search::add::Request { items }).await
	}

	pub async fn find_item_matches_for_name(
		&self,
		name: impl AsRef<str>,
	) -> Result<Vec<(NonRootFSObject<'static>, String)>, crate::error::Error> {
		let name = name.as_ref().trim().to_lowercase();
		let response = api::v3::search::find::post(
			self.client(),
			&api::v3::search::find::Request {
				hashes: vec![self.hmac_key.hash(name.as_bytes())],
			},
		)
		.await?;
		response
			.items
			.into_iter()
			.map(|item| {
				let (item, metadata_path) = match item {
					SearchFindItem::Dir(found_dir) => (
						NonRootFSObject::Dir(Cow::Owned(RemoteDirectory::from_encrypted(
							found_dir.uuid,
							found_dir.parent,
							found_dir.color.map(|s| s.into_owned()),
							found_dir.favorited,
							&found_dir.metadata,
							self.crypter(),
						)?)),
						found_dir.metadata_path,
					),
					SearchFindItem::File(found_file) => {
						let meta = FileMeta::from_encrypted(&found_file.metadata, self.crypter())?;
						(
							NonRootFSObject::File(Cow::Owned(RemoteFile::from_meta(
								found_file.uuid,
								found_file.parent,
								found_file.size,
								found_file.chunks,
								found_file.region,
								found_file.bucket,
								found_file.favorited,
								meta,
							))),
							found_file.metadata_path,
						)
					}
				};

				let mut path = metadata_path
					.into_iter()
					.filter_map(|meta| match meta.0.as_ref() {
						"default" => None,
						_ => {
							let decrypted = match self.crypter().decrypt_meta(&meta) {
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
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn split_name_iter() {
		let minimal = "a";
		let splitter = SplitName::new(minimal, 2, 16);
		assert_eq!(vec!["a"], splitter.iter().collect::<Vec<_>>());

		let normal = "abc";
		let splitter = SplitName::new(normal, 2, 16);
		assert_eq!(vec!["ab", "abc", "bc"], splitter.iter().collect::<Vec<_>>());

		let normal = "файл.txt";
		let splitter = SplitName::new(normal, 2, 16);
		assert_eq!(
			vec![
				".t",
				".tx",
				".txt",
				"tx",
				"txt",
				"xt",
				"ай",
				"айл",
				"айл.",
				"айл.t",
				"айл.tx",
				"айл.txt",
				"йл",
				"йл.",
				"йл.t",
				"йл.tx",
				"йл.txt",
				"л.",
				"л.t",
				"л.tx",
				"л.txt",
				"фа",
				"фай",
				"файл",
				"файл.",
				"файл.t",
				"файл.tx",
				"файл.txt"
			],
			splitter.iter().collect::<Vec<_>>()
		);
	}
}

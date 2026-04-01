use std::borrow::Cow;

use filen_types::fs::{ObjectType, ParentUuid};

use crate::{
	ErrorKind, api,
	auth::Client,
	error::Error,
	fs::{
		HasName, HasParent, HasUUID, SetRemoteInfo,
		categories::{
			DirType, NonRootFileType, NonRootItemType, Normal,
			fs::{
				CategoryFSExt, GetItemsResponseError, GetItemsResponseSuccess,
				ObjectOrRemainingPath,
			},
		},
		dir::RemoteDirectory,
		file::RemoteFile,
	},
};

impl Client {
	pub async fn find_item_at_path<'a>(
		&'a self,
		path: &str,
	) -> Result<Option<NonRootFileType<'a, Normal>>, Error> {
		let (_, item): (_, ObjectOrRemainingPath<'a, '_, Normal>) =
			self.get_items_in_path(path).await.map_err(|(e, _, _)| e)?;
		match item {
			ObjectOrRemainingPath::Object(fs_object) => Ok(Some(fs_object)),
			ObjectOrRemainingPath::RemainingPath(_) => Ok(None),
		}
	}

	pub async fn get_items_in_path<'a, 'b>(
		&'a self,
		path: &'b str,
	) -> Result<
		GetItemsResponseSuccess<'a, 'b, Normal>,
		(Error, GetItemsResponseError<'a, Normal>, &'b str),
	> {
		<Normal as CategoryFSExt>::get_items_in_path_starting_at(
			self,
			path,
			DirType::<Normal>::Root(Cow::Borrowed(self.root())),
			(),
		)
		.await
	}

	/// Gets the path of an item by traversing up the directory tree until it reaches the root.
	/// Returns the full path (directories end with `/`, files do not)
	/// and a vector of the ancestors (excluding the item itself).
	pub async fn get_item_path(
		&self,
		item: &NonRootItemType<'_, Normal>,
	) -> Result<(String, Vec<RemoteDirectory>), Error> {
		let _lock = self.lock_drive().await?;
		let mut current_item = Cow::Borrowed(item);
		let mut ancestors: Vec<RemoteDirectory> = Vec::new();
		loop {
			let parent_uuid = match current_item.parent() {
				ParentUuid::Uuid(uuid) => *uuid,
				_ => {
					// If the parent UUID is not a real UUID,
					// we try to refetch the item to see if we can get a valid parent UUID.
					// If not, we return an error.
					let parent_uuid = match current_item.as_ref() {
						NonRootItemType::Dir(dir) => {
							let dir = self.get_dir(*dir.uuid()).await?;
							let parent_uuid = *dir.parent();
							if let Some(last) = ancestors.last_mut()
								&& last.uuid() == dir.uuid()
							{
								*last = dir;
							}
							parent_uuid
						}
						NonRootItemType::File(file) => {
							let file = self.get_file(*file.uuid()).await?;
							*file.parent()
						}
					};
					match parent_uuid {
						ParentUuid::Uuid(uuid) => uuid,
						_ => {
							return Err(Error::custom(
								ErrorKind::MetadataWasNotDecrypted,
								format!(
									"Item {} does not have a valid parent: {}",
									item.uuid(),
									parent_uuid.as_ref()
								),
							));
						}
					}
				}
			};

			if &parent_uuid == self.root().uuid() {
				break;
			}

			let parent = self.get_dir(parent_uuid).await?;
			ancestors.push(parent);
			current_item = Cow::Owned(NonRootItemType::Dir(Cow::Borrowed(
				ancestors.last().unwrap(),
			)));
		}

		ancestors.reverse();

		let mut path = ancestors
			.iter()
			.try_fold(String::new(), |mut acc, ancestor| {
				match ancestor.name() {
					Some(name) => {
						acc.push_str(name);
						acc.push('/');
					}
					None => {
						return Err(Error::custom(
							ErrorKind::MetadataWasNotDecrypted,
							format!("Name for item {} could not be decrypted", item.uuid()),
						));
					}
				}
				Ok(acc)
			})?;

		match item.name() {
			Some(name) => {
				path.push_str(name);
				if matches!(item, NonRootItemType::Dir(_)) {
					path.push('/');
				}
			}
			None => {
				return Err(Error::custom(
					ErrorKind::MetadataWasNotDecrypted,
					format!("Name for item {} could not be decrypted", item.uuid()),
				));
			}
		}

		Ok((path, ancestors))
	}

	pub async fn empty_trash(&self) -> Result<(), Error> {
		api::v3::trash::empty::post(self.client()).await?;
		Ok(())
	}

	pub async fn set_dir_favorite(
		&self,
		dir: &mut RemoteDirectory,
		value: bool,
	) -> Result<(), Error> {
		let resp = api::v3::item::favorite::post(
			self.client(),
			&api::v3::item::favorite::Request {
				uuid: *dir.uuid(),
				r#type: ObjectType::Dir,
				value,
			},
		)
		.await?;
		dir.set_favorited(resp.value);
		Ok(())
	}

	pub async fn set_file_favorite(&self, file: &mut RemoteFile, value: bool) -> Result<(), Error> {
		let resp = api::v3::item::favorite::post(
			self.client(),
			&api::v3::item::favorite::Request {
				uuid: *file.uuid(),
				r#type: ObjectType::File,
				value,
			},
		)
		.await?;
		file.set_favorited(resp.value);
		Ok(())
	}

	pub async fn set_favorite(
		&self,
		object: &mut NonRootItemType<'static, Normal>,
		value: bool,
	) -> Result<(), Error> {
		match object {
			NonRootItemType::Dir(dir) => self.set_dir_favorite(dir.to_mut(), value).await,
			NonRootItemType::File(file) => self.set_file_favorite(file.to_mut(), value).await,
		}
	}
}

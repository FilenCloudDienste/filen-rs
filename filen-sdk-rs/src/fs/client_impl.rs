use std::borrow::Cow;

use filen_types::fs::ObjectType;

use crate::{
	api,
	auth::Client,
	error::Error,
	fs::{
		HasUUID, SetRemoteInfo,
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

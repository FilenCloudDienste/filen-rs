use std::borrow::Cow;

use filen_types::fs::ObjectType;

use crate::{
	api,
	auth::Client,
	error::{Error, InvalidTypeError},
	fs::{
		HasType, HasUUID, NonRootFSObject, SetRemoteInfo, UnsharedFSObject,
		dir::UnsharedDirectoryType,
	},
	util::PathIteratorExt,
};

use super::enums::FSObject;

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectOrRemainingPath<'a, 'b> {
	Object(UnsharedFSObject<'a>),
	RemainingPath(&'b str),
}
pub type GetItemsResponseSuccess<'a, 'b> = (
	Vec<UnsharedDirectoryType<'a>>,
	ObjectOrRemainingPath<'a, 'b>,
);
pub type GetItemsResponseError<'a> = (Vec<UnsharedDirectoryType<'a>>, UnsharedFSObject<'a>);

impl Client {
	pub async fn find_item_at_path<'a>(
		&'a self,
		path: &str,
	) -> Result<Option<FSObject<'a>>, Error> {
		let (_, item): (_, ObjectOrRemainingPath<'a, '_>) =
			self.get_items_in_path(path).await.map_err(|(e, _, _)| e)?;
		match item {
			ObjectOrRemainingPath::Object(fs_object) => {
				let fs_object: FSObject = fs_object.into();
				Ok(Some(fs_object))
			}
			ObjectOrRemainingPath::RemainingPath(_) => Ok(None),
		}
	}

	pub async fn get_items_in_path_starting_at<'a, 'b>(
		&'a self,
		path: &'b str,
		mut curr_dir: UnsharedDirectoryType<'a>,
	) -> Result<GetItemsResponseSuccess<'a, 'b>, (Error, GetItemsResponseError<'a>, &'b str)> {
		let mut dirs: Vec<UnsharedDirectoryType> =
			Vec::with_capacity(path.chars().filter(|c| *c == '/').count() + 1);

		let mut path_iter = path.path_iter().peekable();
		let mut last_rest_of_path = path;
		let _lock = match self.lock_drive().await {
			Ok(lock) => lock,
			Err(e) => return Err((e, (dirs, curr_dir.into()), path)),
		};
		while let Some((component, rest_of_path)) = path_iter.next() {
			match self.find_item_in_dir(&curr_dir, component).await {
				Ok(Some(NonRootFSObject::Dir(dir))) => {
					let old_dir = std::mem::replace(&mut curr_dir, UnsharedDirectoryType::Dir(dir));
					dirs.push(old_dir);
					if path_iter.peek().is_none() {
						return Ok((dirs, ObjectOrRemainingPath::Object(curr_dir.into())));
					}
					last_rest_of_path = rest_of_path;
					continue;
				}
				Ok(Some(NonRootFSObject::File(file))) => {
					let file = UnsharedFSObject::File(file);
					dirs.push(curr_dir);
					if path_iter.peek().is_some() {
						return Err((
							InvalidTypeError {
								actual: ObjectType::File,
								expected: ObjectType::Dir,
							}
							.into(),
							(dirs, file),
							rest_of_path,
						));
					}
					return Ok((dirs, ObjectOrRemainingPath::Object(file)));
				}
				Ok(None) => {
					dirs.push(curr_dir);
					return Ok((
						dirs,
						ObjectOrRemainingPath::RemainingPath(last_rest_of_path),
					));
				}
				Err(e) => return Err((e, (dirs, curr_dir.into()), rest_of_path)),
			}
		}
		Ok((dirs, ObjectOrRemainingPath::Object(curr_dir.into())))
	}

	pub async fn get_items_in_path<'a, 'b>(
		&'a self,
		path: &'b str,
	) -> Result<GetItemsResponseSuccess<'a, 'b>, (Error, GetItemsResponseError<'a>, &'b str)> {
		self.get_items_in_path_starting_at(
			path,
			UnsharedDirectoryType::Root(Cow::Borrowed(self.root())),
		)
		.await
	}

	pub async fn empty_trash(&self) -> Result<(), Error> {
		api::v3::trash::empty::post(self.client()).await?;
		Ok(())
	}

	pub async fn set_favorite<T>(&self, object: &mut T, value: bool) -> Result<(), Error>
	where
		T: SetRemoteInfo + HasUUID + HasType,
	{
		let resp = api::v3::item::favorite::post(
			self.client(),
			&api::v3::item::favorite::Request {
				uuid: *object.uuid(),
				r#type: object.object_type(),
				value,
			},
		)
		.await?;
		object.set_favorited(resp.value);
		Ok(())
	}
}

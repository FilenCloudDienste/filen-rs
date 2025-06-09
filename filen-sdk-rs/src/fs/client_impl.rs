use std::borrow::Cow;

use filen_types::fs::ObjectType;

use crate::{
	api,
	auth::Client,
	error::Error,
	fs::{UnsharedFSObject, dir::UnsharedDirectoryType},
	util::PathIteratorExt,
};

use super::enums::FSObject;

pub type GetItemsResponse<'a> = (Vec<UnsharedDirectoryType<'a>>, Option<UnsharedFSObject<'a>>);

impl Client {
	pub async fn find_item_at_path(
		&self,
		path: impl AsRef<str>,
	) -> Result<Option<FSObject>, Error> {
		let items = self
			.get_items_in_path(path.as_ref())
			.await
			.map_err(|(e, _, _)| e)?;

		Ok(items.1.map(Into::into))
	}

	pub async fn get_items_in_path_starting_at<'a, 'b>(
		&'a self,
		path: &'b str,
		mut curr_dir: UnsharedDirectoryType<'a>,
	) -> Result<GetItemsResponse<'a>, (Error, GetItemsResponse<'a>, &'b str)> {
		let mut dirs: Vec<UnsharedDirectoryType> =
			Vec::with_capacity(path.chars().filter(|c| *c == '/').count());

		let mut path_iter = path.path_iter().peekable();
		while let Some((component, rest_of_path)) = path_iter.next() {
			match self.find_item_in_dir(&curr_dir, component).await {
				Ok(Some(FSObject::Dir(dir))) => {
					let old_dir = std::mem::replace(&mut curr_dir, UnsharedDirectoryType::Dir(dir));
					dirs.push(old_dir);
					if path_iter.peek().is_none() {
						return Ok((dirs, Some(curr_dir.into())));
					}
					continue;
				}
				Ok(Some(FSObject::File(file))) => {
					let file = UnsharedFSObject::File(file);
					dirs.push(curr_dir);
					if path_iter.peek().is_some() {
						return Err((
							Error::InvalidType(ObjectType::File, ObjectType::Dir),
							(dirs, Some(file)),
							rest_of_path,
						));
					}
					return Ok((dirs, Some(file)));
				}
				Ok(None) => return Ok((dirs, None)),
				Err(e) => return Err((e, (dirs, Some(curr_dir.into())), rest_of_path)),
				Ok(Some(o)) => unreachable!("Unexpected fs_object {:?} in path search", o),
			}
		}
		dirs.push(curr_dir);
		Ok((dirs, None))
	}

	pub async fn get_items_in_path<'a>(
		&self,
		path: &'a str,
	) -> Result<GetItemsResponse, (Error, GetItemsResponse, &'a str)> {
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
}

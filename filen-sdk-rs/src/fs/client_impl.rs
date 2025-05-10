use std::borrow::Cow;

use crate::{api, auth::Client, error::Error};

use super::{dir::DirectoryType, enums::FSObject};

impl Client {
	pub async fn find_item_at_path(
		&self,
		path: impl AsRef<str>,
	) -> Result<Option<FSObject>, Error> {
		let mut curr_dir = DirectoryType::Root(Cow::Borrowed(self.root()));
		let mut curr_path = String::with_capacity(path.as_ref().len());
		let mut path_iter = path.as_ref().split('/');
		while let Some(component) = path_iter.next() {
			if component.is_empty() {
				continue;
			}
			match self.find_item_in_dir(&curr_dir, component).await {
				Ok(Some(FSObject::Dir(dir))) => {
					curr_dir = DirectoryType::Dir(Cow::Owned(dir.into_owned()));
					curr_path.push_str(component);
					curr_path.push('/');
					continue;
				}
				Ok(Some(FSObject::File(file))) => {
					if let Some(next) = path_iter.next() {
						return Err(Error::Custom(format!(
							"Path {} is a file, but tried to access {}/{}",
							curr_path, curr_path, next
						)));
					}
					return Ok(Some(FSObject::File(Cow::Owned(file.into_owned()))));
				}
				Err(e) => return Err(e),
				_ => {}
			}
			return Ok(None);
		}
		match curr_dir {
			DirectoryType::Root(_) => Ok(Some(FSObject::Root(Cow::Borrowed(self.root())))),
			DirectoryType::Dir(dir) => Ok(Some(FSObject::Dir(dir))),
			DirectoryType::RootWithMeta(_) => {
				unreachable!("RootWithMeta should not be returned from find_item_at_path")
			}
		}
	}

	pub async fn empty_trash(&self) -> Result<(), Error> {
		api::v3::trash::empty::post(self.client()).await?;
		Ok(())
	}
}

//! Binding conversions for the sources every drive job shares (copies, compressed archives).

use crate::{
	Error, ErrorKind,
	fs::{categories::DirType, file::enums::RemoteFileType},
	js::{AnyItemWithContext, DirByCategoryWithContext},
};

use super::listing::{ItemSource, ItemSourceDir};

/// The job source a binding's `item` names. The drive's root is none: `done_to_it` says what the
/// job would have done to it ("copied", "compressed") in the error refusing it.
pub(crate) fn item_source(item: AnyItemWithContext, done_to_it: &str) -> Result<ItemSource, Error> {
	Ok(match item {
		AnyItemWithContext::File(file) => ItemSource::File(RemoteFileType::try_from(file)?),
		AnyItemWithContext::Dir(dir) => {
			ItemSource::Dir(match DirByCategoryWithContext::from(dir) {
				DirByCategoryWithContext::Normal(DirType::Dir(dir)) => {
					ItemSourceDir::Normal(dir.into_owned())
				}
				DirByCategoryWithContext::Normal(DirType::Root(_)) => {
					return Err(Error::custom(
						ErrorKind::InvalidState,
						format!("the root directory cannot be {done_to_it}"),
					));
				}
				DirByCategoryWithContext::Shared(dir, role) => ItemSourceDir::Shared(dir, role),
				DirByCategoryWithContext::Linked(dir, link) => {
					ItemSourceDir::Linked(dir, link.try_into()?)
				}
			})
		}
	})
}

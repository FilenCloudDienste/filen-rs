//! The items commands are given: a path, read from the working path when it is relative, or
//! `uuid:<UUID>`, which names an item wherever it is, even one whose name cannot be decrypted.

use std::{borrow::Cow, fmt};

use anyhow::{Context as _, Result};
use filen_sdk_rs::{
	auth::Client,
	fs::{
		HasParent as _, HasUUID as _,
		categories::{NonRootFileType, NonRootItemType, Normal},
	},
};
use filen_types::fs::{ParentUuid, Uuid};

use crate::{ui::UI, util::RemotePath};

/// What an operand starts with to name an item by its UUID.
const UUID_PREFIX: &str = "uuid:";

/// An item a command was given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ItemOperand {
	/// The item at this path.
	Path(RemotePath),
	/// `uuid:<UUID>`: the item with this UUID.
	Uuid(Uuid),
}

impl ItemOperand {
	/// The item `operand` names: `uuid:<UUID>`, or a path relative to `working_path` unless it
	/// starts with '/'.
	pub(crate) fn parse(working_path: &RemotePath, operand: &str) -> Result<Self> {
		match operand.strip_prefix(UUID_PREFIX) {
			Some(uuid) => Uuid::parse_str(uuid)
				.map(Self::Uuid)
				.map_err(|_| UI::failure(&format!("Not a UUID: {}", uuid))),
			None => Ok(Self::Path(working_path.navigate(operand))),
		}
	}

	/// The failure of a command that found no `what` (such as "file or directory") here.
	pub(crate) fn not_found(&self, what: &str) -> anyhow::Error {
		match self {
			Self::Path(path) => UI::failure(&format!("No such {}: {}", what, path)),
			Self::Uuid(uuid) => UI::failure(&format!("No item with UUID {}", uuid)),
		}
	}
}

impl fmt::Display for ItemOperand {
	/// The operand as it was given, but for a path, which is shown normalized and absolute.
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Path(path) => write!(f, "{}", path),
			Self::Uuid(uuid) => write!(f, "{}{}", UUID_PREFIX, uuid),
		}
	}
}

/// The item `operand` names, if there is one in the drive. An item named by its UUID that is in
/// the trash (as the server reports every item in a trashed directory to be), or is an earlier
/// version of a file, is not in the drive: that fails, saying where it is instead.
pub(crate) async fn find_item<'a>(
	client: &'a Client,
	operand: &ItemOperand,
) -> Result<Option<NonRootFileType<'a, Normal>>> {
	match operand {
		ItemOperand::Path(path) => client
			.find_item_at_path(&path.0)
			.await
			.context("Failed to find file or directory"),
		ItemOperand::Uuid(uuid) => find_item_by_uuid(client, *uuid).await,
	}
}

/// The item `operand` names, or the failure of finding no `what` (such as "file or directory").
pub(crate) async fn resolve_item<'a>(
	client: &'a Client,
	operand: &ItemOperand,
	what: &str,
) -> Result<NonRootFileType<'a, Normal>> {
	find_item(client, operand)
		.await?
		.ok_or_else(|| operand.not_found(what))
}

/// `found`, or `None` if the server found no such file or directory.
fn optional<T>(found: Result<T, filen_sdk_rs::Error>) -> Result<Option<T>, filen_sdk_rs::Error> {
	// called by its path: in scope, its `context` would clash with anyhow's
	filen_sdk_rs::error::ResultExt::optional(found)
}

async fn find_item_by_uuid(
	client: &Client,
	uuid: Uuid,
) -> Result<Option<NonRootFileType<'_, Normal>>> {
	if uuid == client.root().uuid() {
		return Ok(Some(NonRootFileType::Root(Cow::Borrowed(client.root()))));
	}
	let in_trash = || {
		UI::failure(&format!(
			"{UUID_PREFIX}{uuid} is in the trash: restore it with trash-restore {UUID_PREFIX}{uuid}"
		))
	};
	// the server answers either lookup of the other kind's UUID as not found
	if let Some(found) =
		optional(client.get_file_with_info(uuid).await).context("Failed to find file")?
	{
		if found.trash {
			return Err(in_trash());
		}
		if found.versioned {
			return Err(UI::failure(&format!(
				"{UUID_PREFIX}{uuid} is an earlier version of a file, which `versions` lists"
			)));
		}
		return Ok(Some(NonRootFileType::File(Cow::Owned(found.file))));
	}
	match optional(client.get_dir(uuid).await).context("Failed to find directory")? {
		Some(dir) if matches!(dir.parent(), ParentUuid::Trash(_)) => Err(in_trash()),
		Some(dir) => Ok(Some(NonRootFileType::Dir(Cow::Owned(dir)))),
		None => Ok(None),
	}
}

/// The path of `item`, which `operand` names: the operand's own path, or, for a UUID, the path
/// the item is at, which only an item whose name and whose directories' names can be decrypted
/// has.
pub(crate) async fn path_of(
	client: &Client,
	operand: &ItemOperand,
	item: &NonRootFileType<'_, Normal>,
) -> Result<RemotePath> {
	let item = match (operand, item) {
		(ItemOperand::Path(path), _) => return Ok(path.clone()),
		(ItemOperand::Uuid(_), NonRootFileType::Root(_)) => return Ok(RemotePath::new("/")),
		(ItemOperand::Uuid(_), NonRootFileType::Dir(dir)) => {
			NonRootItemType::Dir(Cow::Borrowed(dir.as_ref()))
		}
		(ItemOperand::Uuid(_), NonRootFileType::File(file)) => {
			NonRootItemType::File(Cow::Borrowed(file.as_ref()))
		}
	};
	let (path, _) = client
		.get_item_path(&item)
		.await
		.with_context(|| format!("Failed to find the path of {}", operand))?;
	Ok(RemotePath::new(&path))
}

/// The path `operand` names: its own, or that of the item with its UUID, which must exist.
pub(crate) async fn operand_path(
	client: &Client,
	operand: &ItemOperand,
	what: &str,
) -> Result<RemotePath> {
	match operand {
		ItemOperand::Path(path) => Ok(path.clone()),
		ItemOperand::Uuid(_) => {
			let item = resolve_item(client, operand, what).await?;
			path_of(client, operand, &item).await
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn uuid() -> Uuid {
		Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef)
	}

	#[test]
	fn a_path_operand_is_read_from_the_working_path() {
		let working_path = RemotePath::new("/a/b");

		assert_eq!(
			ItemOperand::parse(&working_path, "c/../d").unwrap(),
			ItemOperand::Path(RemotePath::new("/a/b/d"))
		);
		assert_eq!(
			ItemOperand::parse(&working_path, "/c").unwrap(),
			ItemOperand::Path(RemotePath::new("/c"))
		);
	}

	#[test]
	fn a_uuid_operand_names_the_item_with_that_uuid_wherever_the_working_path_is() {
		let operand = ItemOperand::parse(
			&RemotePath::new("/a"),
			"uuid:01234567-89AB-CDEF-0123-456789ABCDEF",
		)
		.unwrap();

		assert_eq!(operand, ItemOperand::Uuid(uuid()));
		assert_eq!(
			operand.to_string(),
			"uuid:01234567-89ab-cdef-0123-456789abcdef"
		);
	}

	#[test]
	fn a_uuid_operand_that_is_no_uuid_fails() {
		let error = ItemOperand::parse(&RemotePath::new("/"), "uuid:abc").unwrap_err();

		assert_eq!(error.to_string(), "Not a UUID: abc");
	}

	#[test]
	fn a_path_naming_a_uuid_without_the_prefix_is_a_path() {
		let operand = ItemOperand::parse(
			&RemotePath::new("/"),
			"01234567-89ab-cdef-0123-456789abcdef",
		)
		.unwrap();

		assert_eq!(
			operand,
			ItemOperand::Path(RemotePath::new("/01234567-89ab-cdef-0123-456789abcdef"))
		);
	}

	#[test]
	fn a_missing_item_is_named_by_its_path_or_its_uuid() {
		assert_eq!(
			ItemOperand::Path(RemotePath::new("/a"))
				.not_found("file")
				.to_string(),
			"No such file: /a"
		);
		assert_eq!(
			ItemOperand::Uuid(uuid()).not_found("file").to_string(),
			"No item with UUID 01234567-89ab-cdef-0123-456789abcdef"
		);
	}
}

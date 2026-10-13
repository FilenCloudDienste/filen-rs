//! Small helpers shared across MCP endpoint implementations, to avoid re-deriving the same
//! "look up a path", "narrow to a directory/non-root item" and "public link subscription error"
//! boilerplate in every endpoint file.

use anyhow::{Context, Result};
use filen_sdk_rs::{
	ErrorKind,
	auth::Client,
	fs::categories::{DirType, NonRootFileType, NonRootItemType, Normal},
};

use crate::ui::UI;

/// Looks up an item by path, turning "not found" into a friendly `"No such {what}: {path}"` error.
pub(crate) async fn find_item_or_err<'a>(
	client: &'a Client,
	path: &str,
	what: &str,
) -> Result<NonRootFileType<'a, Normal>> {
	client
		.find_item_at_path(path)
		.await
		.context("Failed to look up path")?
		.ok_or_else(|| UI::failure(&format!("No such {}: {}", what, path)))
}

/// Narrows an item to a directory (the root directory counts as one), erroring on a file.
pub(crate) fn require_dir<'a>(
	item: NonRootFileType<'a, Normal>,
	path: &str,
) -> Result<DirType<'a, Normal>> {
	match item {
		NonRootFileType::Dir(dir) => Ok(DirType::Dir(dir)),
		NonRootFileType::Root(root) => Ok(DirType::Root(root)),
		NonRootFileType::File(_) => Err(UI::failure(&format!("Not a directory: {}", path))),
	}
}

/// Narrows an item to a file or directory, erroring on the root directory with a caller-supplied
/// message (the message varies by endpoint - "cannot move", "cannot have a public link", etc).
pub(crate) fn reject_root<'a>(
	item: NonRootFileType<'a, Normal>,
	root_err: &str,
) -> Result<NonRootItemType<'a, Normal>> {
	match item {
		NonRootFileType::File(file) => Ok(NonRootItemType::File(file)),
		NonRootFileType::Dir(dir) => Ok(NonRootItemType::Dir(dir)),
		NonRootFileType::Root(_) => Err(UI::failure(root_err)),
	}
}

/// Maps an SDK error from a public-link status lookup, special-casing the server's
/// "no public-link support on this account" response into a friendlier message.
pub(crate) fn map_link_status_err(e: filen_sdk_rs::Error) -> anyhow::Error {
	if e.kind() == ErrorKind::Server && e.server_code().as_deref() == Some("subscription_needed") {
		UI::failure("Public links are not available for this account (subscription required)")
	} else {
		anyhow::Error::new(e).context("Failed to get public link status")
	}
}

/// Resolves an item back to its normalized absolute path (e.g. `/a/b`, no trailing slash).
pub(crate) async fn resolve_full_path(
	client: &Client,
	item: &NonRootItemType<'_, Normal>,
) -> Result<String> {
	let (path, _) = client
		.get_item_path(item)
		.await
		.context("Failed to resolve path")?;
	Ok(format!("/{}", path.trim_end_matches('/')))
}

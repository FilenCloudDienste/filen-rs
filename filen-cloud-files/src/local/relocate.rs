//! Moving a file to another directory on the drive without ever moving it over another file. The
//! server lets a file move onto a name another file in the destination holds, and that file then
//! stops being current, living on only as an older version of the moved one (measured); it refuses
//! a file's rename onto such a name (measured: `file_with_name_already_exists_at_destination`). So a
//! file that changes directory first takes a name only it holds ([`transit_name`]), moves under it,
//! and only then takes its name in the destination, in one request the server refuses while
//! another file holds the name: a file created there meanwhile is never superseded either. A
//! refusal moves the file back where it was.
//!
//! A stop in the middle leaves the file under its transit name, which gets no placeholder: the
//! caller's rename record puts it back, or the job that moved it finds it there on its next look.

use filen_mobile_native_cache::{CacheError, ffi::FfiId};
use filen_types::fs::Uuid;
use tracing::warn;

use super::jobs::{find_by_name, object_uuid, same_uuid};
use crate::{drive::Drive, identity::stable_id, log_safe, names::transit_name};

/// Where a file is on the drive.
#[derive(Debug, Clone, Copy)]
pub(super) struct Place<'a> {
	/// Its directory's cache id.
	pub(super) dir: &'a FfiId,
	/// Its name there; `None` when the drive's name is unknown (its metadata does not decrypt).
	pub(super) name: Option<&'a str>,
}

/// How a relocation ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Relocated {
	/// The file is at its new place.
	Done,
	/// Another file holds the name: the file is back where it was.
	Taken,
}

/// Gives the file `stable`, now at `from`, the name `name` in the directory `to`, or in its own
/// directory when `to` is `None`, never over another file. An error leaves the file where the
/// failed request found it: at `from`, or under its transit name in either directory.
pub(super) async fn relocate_file<D: Drive>(
	drive: &D,
	stable: Uuid,
	from: Place<'_>,
	to: Option<&FfiId>,
	name: &str,
) -> Result<Relocated, CacheError> {
	let Some(to) = to else {
		return rename_unless_taken(drive, stable, from.dir, name).await;
	};
	let item = stable_id(stable);
	drive.rename(item.clone(), transit_name(stable)).await?;
	drive.move_to(item, to.clone()).await?;
	let relocated = rename_unless_taken(drive, stable, to, name).await?;
	if relocated == Relocated::Taken {
		back_from_transit(drive, stable, from).await;
	}
	Ok(relocated)
}

/// [`relocate_file`] for the live tests, which run it against the server: moves the file
/// `stable`, now in the directory `from_dir` as `from_name`, into the directory `to` as `name`.
/// `true` once it is there; `false` when another file held the name, which leaves it back where it
/// was.
#[cfg(feature = "test-seams")]
pub async fn relocate_for_test<D: Drive>(
	drive: &D,
	stable: Uuid,
	from_dir: &FfiId,
	from_name: &str,
	to: &FfiId,
	name: &str,
) -> Result<bool, CacheError> {
	let from = Place {
		dir: from_dir,
		name: Some(from_name),
	};
	Ok(relocate_file(drive, stable, from, Some(to), name).await? == Relocated::Done)
}

/// Renames the file `stable` in the directory `dir` to `name`: [`Relocated::Taken`] when the server
/// refused it because another item there holds the name, from a listing taken after the refusal.
async fn rename_unless_taken<D: Drive>(
	drive: &D,
	stable: Uuid,
	dir: &FfiId,
	name: &str,
) -> Result<Relocated, CacheError> {
	let refused = match drive.rename(stable_id(stable), name.to_owned()).await {
		Ok(_) => return Ok(Relocated::Done),
		Err(error) => error,
	};
	match drive.list_children(dir.clone()).await {
		Ok(Some(listing))
			if find_by_name(&listing, name)
				.is_some_and(|taken| !same_uuid(object_uuid(taken), &stable.to_string())) =>
		{
			Ok(Relocated::Taken)
		}
		Ok(_) => Err(refused),
		Err(error) => {
			warn!(%stable, error = %log_safe::error(&error), "listing a directory after a refused rename failed");
			Err(refused)
		}
	}
}

/// Moves the file `stable`, under its transit name, back to `from` after its new name was taken.
/// One that cannot be moved back, or given its old name, stays under its transit name, where the
/// caller's record or the next look finds it; nothing is lost.
async fn back_from_transit<D: Drive>(drive: &D, stable: Uuid, from: Place<'_>) {
	let item = stable_id(stable);
	if let Err(error) = drive.move_to(item.clone(), from.dir.clone()).await {
		warn!(%stable, error = %log_safe::error(&error), "moving a file back after its new name was taken failed");
		return;
	}
	let Some(name) = from.name else {
		warn!(%stable, "a file moved back has no name to go back to; it keeps its transit name");
		return;
	};
	if let Err(error) = drive.rename(item, name.to_owned()).await {
		warn!(%stable, error = %log_safe::error(&error), "renaming a file back after its new name was taken failed");
	}
}

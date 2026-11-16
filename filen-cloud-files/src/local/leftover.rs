//! The directories this run created on the drive for a new local directory whose take-in a rename
//! overtook ([`jobs::take_in_dir`](super::jobs)): converted, the local directory stood for one for
//! a moment, so a job on an item in it may have read it so and be sending an upload into it, which
//! no listing shows until it lands. It stays on the drive until nothing can be: no job running or
//! waiting at or under where the local directory was, where it went, or where a placeholder of it
//! stands, and no rename into those settling. Then, only while the server lists it empty, still
//! where and as it was created and not trashed, and no placeholder of it holds anything (an empty
//! one the provider placed goes first, through the clean removal), it goes to the trash, never
//! deleted for good. Otherwise it is looked at again on the engine's backoff, a bounded number of
//! times, and then left for good.
//!
//! Kept in memory only: after a restart the directory stays, an empty directory nobody made.

use std::{collections::HashMap, sync::Arc, time::Duration};

use filen_mobile_native_cache::{CacheError, ffi::FfiObject};
use filen_sdk_rs::{fs::name::collision_key, sync_engine::watch_backoff};
use filen_types::fs::Uuid;
use tokio::time::sleep;
use tracing::{debug, warn};

use super::{
	Inner,
	jobs::{is_trashed, same_uuid},
	lock,
};
use crate::{
	drive::Drive,
	identity::{PlaceholderIdentity, stable_id},
	log_safe::{self, path_tag},
	os::{LocalEntry, LocalError, LocalFs, LocalKind},
	root_path::RootPath,
};

/// The most directories remembered at once: past it, one more simply stays on the drive.
const LEFTOVERS_MAX: usize = 64;
/// How many times a directory is looked at before it is left for good.
const LOOKS: u32 = 8;
/// The longest wait between two looks, should the backoff give none.
const LOOK_AGAIN_MAX: Duration = Duration::from_secs(300);

/// A directory this run created for a local directory a rename took away from its take-in.
#[derive(Debug, Clone)]
pub(super) struct Leftover {
	/// Where the local directory was.
	pub(super) from: RootPath,
	/// Where it went, read through the job's handle; `None` when that could not be read or it left
	/// the root, and then no job anywhere may run.
	pub(super) went: Option<RootPath>,
	/// The uuid text of the directory it was created in.
	pub(super) parent: String,
	/// The name it was created under.
	pub(super) name: String,
}

/// The directories waiting to go, by uuid.
#[derive(Debug, Default)]
pub(super) struct Leftovers(HashMap<Uuid, Leftover>);

/// What the drive holds of a directory this run created, as the server answers now.
#[derive(Debug)]
pub(super) enum AsCreated {
	/// Where and as it was created, and listed empty.
	Empty,
	/// Gone, or in the trash.
	Gone,
	/// Moved or renamed since, or no longer a directory.
	Moved,
	/// Listed holding items.
	Holds,
	/// The cache does not hold its listing.
	Unlisted,
	Unread(CacheError),
}

/// Reads the directory `uuid` from the server: whether it is still in the directory `parent` (uuid
/// text) under `name`, not trashed, and listed empty.
pub(super) async fn as_created<D: Drive, L: LocalFs>(
	inner: &Inner<D, L>,
	uuid: Uuid,
	parent: &str,
	name: &str,
) -> AsCreated {
	let id = stable_id(uuid);
	match inner.drive.refresh_item(id.clone()).await {
		Ok(Some(head @ FfiObject::Dir(_))) if is_trashed(&head) => return AsCreated::Gone,
		Ok(Some(FfiObject::Dir(dir)))
			if same_uuid(&dir.parent, parent)
				&& dir.meta.as_ref().map(|meta| collision_key(&meta.name))
					== Some(collision_key(name)) => {}
		Ok(Some(_)) => return AsCreated::Moved,
		Ok(None) => return AsCreated::Gone,
		Err(error) => return AsCreated::Unread(error),
	}
	match inner.drive.list_children(id).await {
		Ok(Some(listing)) if listing.objects.is_empty() => AsCreated::Empty,
		Ok(Some(_)) => AsCreated::Holds,
		Ok(None) => AsCreated::Unlisted,
		Err(error) => AsCreated::Unread(error),
	}
}

/// Remembers `leftover`, the directory `uuid`, and looks at it later; past [`LEFTOVERS_MAX`] it
/// simply stays.
pub(super) fn remember<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	uuid: Uuid,
	leftover: Leftover,
) {
	{
		let mut state = lock(&inner.state);
		let leftovers = &mut state.leftovers.0;
		if leftovers.len() >= LEFTOVERS_MAX && !leftovers.contains_key(&uuid) {
			warn!(item = %path_tag(&leftover.from), %uuid, "too many directories left by overtaken take-ins; this one stays");
			return;
		}
		debug!(path = %leftover.from, %uuid, "a directory created for one a rename overtook stays for now");
		leftovers.insert(uuid, leftover);
	}
	inner.spawn(look_later(Arc::clone(inner), uuid));
}

/// The directory `uuid` was taken in as a local directory of the user's: it stays.
pub(super) fn claimed<D: Drive, L: LocalFs>(inner: &Inner<D, L>, uuid: Uuid) {
	if let Some(leftover) = lock(&inner.state).leftovers.0.remove(&uuid) {
		debug!(path = %leftover.from, %uuid, "a directory left by an overtaken take-in was taken in; it stays");
	}
}

/// What a look at a directory came to.
#[derive(Debug)]
enum Verdict {
	/// In the trash, by this look or otherwise.
	Gone,
	/// Something stands for it or is in it: it stays for good.
	Stays(&'static str),
	/// Something may still be sending into it, or a read failed: looked at again.
	Later(&'static str),
}

/// Looks at the directory `uuid` on the backoff until it is gone or stays, at most [`LOOKS`] times.
async fn look_later<D: Drive, L: LocalFs>(inner: Arc<Inner<D, L>>, uuid: Uuid) {
	for attempt in 1..=LOOKS {
		sleep(watch_backoff(attempt).unwrap_or(LOOK_AGAIN_MAX)).await;
		let Some(leftover) = lock(&inner.state).leftovers.0.get(&uuid).cloned() else {
			return;
		};
		let path = &leftover.from;
		match look(&inner, uuid, &leftover).await {
			Verdict::Gone => {
				debug!(%path, %uuid, "a directory left by an overtaken take-in is in the trash");
			}
			Verdict::Stays(why) => {
				debug!(%path, %uuid, why, "a directory left by an overtaken take-in stays");
			}
			Verdict::Later(why) => {
				debug!(%path, %uuid, why, attempt, "a directory left by an overtaken take-in is looked at again");
				continue;
			}
		}
		lock(&inner.state).leftovers.0.remove(&uuid);
		return;
	}
	if let Some(leftover) = lock(&inner.state).leftovers.0.remove(&uuid) {
		warn!(item = %path_tag(&leftover.from), %uuid, "a directory left by an overtaken take-in could not go at any look; it stays");
	}
}

/// One look at the directory `uuid`: see the module.
async fn look<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	uuid: Uuid,
	leftover: &Leftover,
) -> Verdict {
	if busy(inner, uuid, leftover, true) {
		return Verdict::Later("a job may be sending into it");
	}
	if let Some(verdict) = remote_verdict(inner, uuid, leftover).await {
		return verdict;
	}
	if let Some(verdict) = unclaim(inner, uuid, leftover).await {
		return verdict;
	}
	// The removal raises events of its own, and a job they bring finds nothing there: only a job
	// already running may hold the directory as its parent.
	if busy(inner, uuid, leftover, false) {
		return Verdict::Later("a job may be sending into it");
	}
	if let Some(verdict) = remote_verdict(inner, uuid, leftover).await {
		return verdict;
	}
	match inner.drive.trash(stable_id(uuid)).await {
		Ok(_) => Verdict::Gone,
		Err(error) => {
			warn!(item = %path_tag(&leftover.from), %uuid, error = %log_safe::error(&error), "trashing a directory left by an overtaken take-in failed");
			Verdict::Later("the trash failed")
		}
	}
}

/// What the server's answer on the directory `uuid` decides, if anything: `None` while it is
/// where and as it was created, and empty.
async fn remote_verdict<D: Drive, L: LocalFs>(
	inner: &Inner<D, L>,
	uuid: Uuid,
	leftover: &Leftover,
) -> Option<Verdict> {
	match as_created(inner, uuid, &leftover.parent, &leftover.name).await {
		AsCreated::Empty => None,
		AsCreated::Gone => Some(Verdict::Gone),
		AsCreated::Moved => Some(Verdict::Stays("it moved on the drive")),
		AsCreated::Holds => Some(Verdict::Stays("it holds items on the drive")),
		AsCreated::Unlisted => Some(Verdict::Later("the cache does not hold its listing")),
		AsCreated::Unread(error) => {
			debug!(path = %leftover.from, %uuid, %error, "reading a directory left by an overtaken take-in failed");
			Some(Verdict::Later("reading it failed"))
		}
	}
}

/// Whether a job may hold the directory `uuid` as the parent it sends into: one running, or with
/// `waiting` one waiting too, at or under where the local directory was, where it went (anywhere
/// when that is not known), or where a placeholder of it stands; or a rename to or from there, or
/// of it, not settled.
fn busy<D: Drive, L: LocalFs>(
	inner: &Inner<D, L>,
	uuid: Uuid,
	leftover: &Leftover,
	waiting: bool,
) -> bool {
	let state = lock(&inner.state);
	let mut paths = vec![
		leftover.from.clone(),
		leftover.went.clone().unwrap_or_else(RootPath::root),
	];
	paths.extend(state.index.path_of(uuid).cloned());
	paths.iter().any(|path| {
		state.queue.active_under(path, waiting)
			|| state.uploading.values().any(|at| at.starts_with(path))
	}) || state.rename_records.iter().any(|record| {
		record.concerns(uuid)
			|| paths
				.iter()
				.any(|path| record.from.starts_with(path) || record.to.starts_with(path))
	})
}

/// Removes every placeholder of the directory `uuid` while it is an empty directory in sync, as
/// the provider placed it: `None` once none is left, or what keeps the directory otherwise.
async fn unclaim<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	uuid: Uuid,
	leftover: &Leftover,
) -> Option<Verdict> {
	let mut paths = vec![leftover.from.clone()];
	if let Some(indexed) = lock(&inner.state).index.path_of(uuid)
		&& indexed.case_key() != leftover.from.case_key()
	{
		paths.push(indexed.clone());
	}
	for path in paths {
		if inner.renaming_onto(&path) {
			return Some(Verdict::Later(
				"a rename onto its placeholder is not settled",
			));
		}
		let identity = match inner.inspect(&path).await {
			Ok(LocalEntry::Placeholder {
				kind: LocalKind::Dir { .. },
				identity: Ok(identity @ PlaceholderIdentity::Dir { uuid: held }),
				..
			}) if held == uuid => identity,
			Ok(_) => continue,
			Err(_) => return Some(Verdict::Later("reading its placeholder failed")),
		};
		let at = path.clone();
		match inner
			.fs(move |local| local.remove_clean(&at, identity))
			.await
		{
			Ok(()) | Err(LocalError::NotFound) => {
				debug!(%path, %uuid, "removed the empty placeholder of a directory left by an overtaken take-in");
				lock(&inner.state).index.remove_under(&path);
			}
			Err(LocalError::Exists) => return Some(Verdict::Stays("its placeholder holds items")),
			Err(error) => {
				debug!(%path, %uuid, %error, "removing the placeholder of a directory left by an overtaken take-in failed");
				return Some(Verdict::Later("its placeholder is not clean or is held"));
			}
		}
	}
	None
}

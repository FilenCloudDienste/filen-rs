//! The remote job on one path: bring a clean placeholder to what the cache now says of its item.
//!
//! | The cache says | The clean placeholder |
//! |---|---|
//! | nothing (confirmed by the server), or in the trash | removed: the drive holds its bytes |
//! | in a directory with no placeholder in a populated one | removed |
//! | a name Windows cannot hold, or one losing a case collision | removed, and reported skipped |
//! | another name or directory | renamed or moved there |
//! | a newer version | updated to it, dehydrated |
//!
//! A placeholder that is not clean (written, not in sync, an upload pending), a detached one or an
//! ordinary item is the local side's: the job hands the path over, and the local side uploads what
//! it holds, keeping both versions when the drive moved on. A directory that went with items that
//! are not clean inside is kept for them and created on the drive anew.

use std::{sync::Arc, time::Duration};

use filen_mobile_native_cache::ffi::{FfiNonRootObject, FfiObject};
use filen_types::fs::Uuid;
use tokio::time::Instant;
use tracing::{debug, warn};

use filen_mobile_native_cache::ffi::QueryChildrenResponse;

use crate::{
	drive::Drive,
	failure::Failure,
	identity::{PlaceholderIdentity, stable_id},
	local::{
		Inner,
		jobs::{
			choose_aside, claimed_by_save, failed, file_ids, is_trashed, local_failed, parent_of,
			same_uuid, set_aside,
		},
		lock,
		queue::{Admit, Delay, Kind, Outcome},
		remote::{FileVersion, populated_uuid},
		report::{ConflictKind, LocalReport, RemoteApplied},
	},
	log_safe::path_tag,
	names::is_upload_name,
	os::{LocalEntry, LocalError, LocalFileId, LocalFs, LocalKind, PlaceholderState},
	placeholder::PlaceholderSpec,
	populate::{SkipReason, SkippedItem, listed_id_and_name, plan_entries, spec_for},
	root_path::RootPath,
};

/// How soon a job whose path or target another job holds looks again.
const BUSY_AGAIN: Duration = Duration::from_millis(250);
/// How soon a job looks again that waits for a read of its file to end or for the index.
const WAIT_AGAIN: Duration = Duration::from_secs(1);

/// How a remote job ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::local) enum Step {
	/// As the outcome says.
	Ended(Outcome),
	/// The item holds what the drive may not have: the local side decides, on the same path.
	Local,
}

/// A clean placeholder the job may change.
#[derive(Debug, Clone, Copy)]
struct Clean {
	identity: PlaceholderIdentity,
	uuid: Uuid,
	is_dir: bool,
}

/// Whether a placeholder in `state` holds nothing the drive lacks: in sync, and for a file, with no
/// byte written since (an append through an append-only handle leaves it in sync, not clean).
fn is_clean(state: PlaceholderState, is_dir: bool) -> bool {
	state.in_sync() && (is_dir || state.modified_bytes() == 0)
}

/// Whether the file placeholder in `state`, of `size` bytes, may hold bytes the drive lacks: a byte
/// written since it was marked in sync, or, not in sync, all of it on disk. One not all on disk with
/// no byte written only had its name or attributes changed, as a write hydrates the whole file
/// first.
pub(in crate::local) fn may_hold_edit(state: PlaceholderState, size: u64) -> bool {
	state.modified_bytes() > 0 || (!state.in_sync() && state.on_disk() >= size)
}

/// The remote item `head` as a listed child.
fn as_child(head: FfiObject) -> Option<FfiNonRootObject> {
	match head {
		FfiObject::File(file) => Some(FfiNonRootObject::File(file)),
		FfiObject::Dir(dir) => Some(FfiNonRootObject::Dir(dir)),
		FfiObject::Root(_) => None,
	}
}

/// Whether a file's cache row holds an edit not yet on the server.
pub(in crate::local) fn upload_pending(head: &FfiObject) -> bool {
	matches!(head, FfiObject::File(file) if file.pending_upload_at.is_some())
}

/// The job on `path`.
pub(in crate::local) async fn apply<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
) -> Step {
	if path.is_root() {
		return Step::Ended(Outcome::Done);
	}
	if lock(&inner.state).rename_records.iter().any(|record| {
		!record.confirmed
			&& (record.from.case_key() == path.case_key()
				|| record.to.case_key() == path.case_key())
	}) {
		// A rename from here is not settled: the drive may hold it renamed while the user was
		// refused, until it is put back. Nor one to here, whose job the drive's change queued at the
		// old path and the rename moved here: the path may hold a file made only to be replaced,
		// as `ReplaceFileW` reserves its backup name, and reading it opens a handle the rename
		// over it fails on (see `children`).
		return Step::Ended(Outcome::Wait(WAIT_AGAIN));
	}
	let entry = match inner.inspect(path).await {
		Ok(entry) => entry,
		Err(error) => return Step::Ended(local_failed(error)),
	};
	let (identity, kind, state, id) = match entry {
		LocalEntry::Missing => {
			let mut state = lock(&inner.state);
			if state.index.uuid_at(path).is_some() {
				// The index placed an item here that is gone unseen: it may be elsewhere now.
				state.index.remove_under(path);
				state.index.mark_incomplete();
				drop(state);
				Inner::request_walk(inner);
			}
			return Step::Ended(Outcome::Done);
		}
		LocalEntry::Placeholder {
			identity: Ok(PlaceholderIdentity::Root { .. }),
			..
		} => return Step::Ended(Outcome::Done),
		LocalEntry::Placeholder {
			identity:
				Ok(identity @ (PlaceholderIdentity::File { .. } | PlaceholderIdentity::Dir { .. })),
			kind,
			state,
			id,
		} => (identity, kind, state, id),
		LocalEntry::Placeholder { .. } | LocalEntry::Plain { .. } | LocalEntry::Unsupported(_) => {
			return Step::Local;
		}
	};
	let is_dir = matches!(kind, LocalKind::Dir { .. });
	if is_dir != matches!(identity, PlaceholderIdentity::Dir { .. }) {
		return Step::Local;
	}
	let Some(uuid) = populated_uuid(identity) else {
		return Step::Local;
	};
	let head = match head_of(inner, identity).await {
		Ok(head) => head,
		Err(outcome) => return Step::Ended(outcome),
	};
	let gone = head.as_ref().is_none_or(is_trashed);
	// A directory holds no data of its own: one whose remote directory went is removed whether in
	// sync or not, its items each as clean as they are.
	let dirty = if is_dir {
		!gone && !state.in_sync()
	} else {
		!is_clean(state, false) || head.as_ref().is_some_and(upload_pending)
	};
	let clean = Clean {
		identity,
		uuid,
		is_dir,
	};
	if dirty {
		if let LocalKind::File(stamp) = kind
			&& may_hold_edit(state, stamp.size)
			&& let Some(head) = head.filter(|_| !gone)
			&& let Some(step) = keep_extra_edit(inner, path, id, clean, head).await
		{
			return step;
		}
		// The local side keeps what it holds, reporting it kept when the remote item went.
		return Step::Local;
	}
	let Some(head) = head.filter(|_| !gone) else {
		return remove(inner, path, clean, Reach::Gone).await;
	};
	if matches!(head, FfiObject::Dir(_)) != is_dir {
		// The id names an item of the other kind: nothing this build places.
		warn!(item = %path_tag(path), "a placeholder's remote item is of the other kind");
		return Step::Ended(Outcome::Park(Failure::Internal));
	}
	let (target, spec) = match drive_place(inner, head).await {
		Ok(Some(place)) => place,
		Ok(None) => return remove(inner, path, clean, Reach::Elsewhere).await,
		Err(step) => return step,
	};
	let at = if target == *path {
		path.clone()
	} else {
		if let Some(step) = before_moving(inner, path, &target, clean).await {
			return step;
		}
		match rename(inner, path, &target, clean).await {
			Ok(()) => target,
			Err(step) => return step,
		}
	};
	if !is_dir && spec.identity() != identity {
		return update(inner, &at, clean, &spec).await;
	}
	Step::Ended(Outcome::Done)
}

/// Where the remote item `head` gets its placeholder, with the placeholder it gets: `Ok(None)` when
/// it gets none in the populated directories, which only a complete index can say.
async fn drive_place<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	head: FfiObject,
) -> Result<Option<(RootPath, PlaceholderSpec)>, Step> {
	let Some(parent) = local_parent(inner, &head)? else {
		// Out of the populated directories: the drive keeps it.
		return Ok(None);
	};
	let Some(spec) = target_spec(inner, &parent, head).await? else {
		return Ok(None);
	};
	Ok(Some((parent.join(spec.name().clone()), spec)))
}

/// What stops the clean placeholder at `path` from moving to `target`, the drive's place of its
/// item: `None` when nothing does. While the item is in a local change not settled yet (a delete
/// of another placeholder of it waiting for its trash, a rename of it waiting to be settled, a
/// save taking it over), the drive's place may be one the user just left, as a save's backup name
/// is: the job waits for the change to settle. A placeholder of the same item at `target` makes
/// this one an extra placeholder of it, which goes ([`remove_extra`]). And a rename's target is
/// never read before the rename happened ([`Inner::renaming_onto`]).
async fn before_moving<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	target: &RootPath,
	clean: Clean,
) -> Option<Step> {
	let unsettled = inner.renaming_onto(target) || {
		let state = lock(&inner.state);
		state
			.deletes
			.values()
			.any(|pending| populated_uuid(pending.identity) == Some(clean.uuid))
			|| state
				.rename_records
				.iter()
				.any(|record| !record.confirmed && record.concerns(clean.uuid))
	};
	if unsettled {
		return Some(Step::Ended(Outcome::Wait(WAIT_AGAIN)));
	}
	if let PlaceholderIdentity::File { stable, .. } = clean.identity
		&& claimed_by_save(inner, stable, path).await
	{
		return Some(Step::Ended(Outcome::Wait(WAIT_AGAIN)));
	}
	if target.case_key() == path.case_key() {
		// A rename in case only: `target` opens this very placeholder, no other one of the item.
		return None;
	}
	match inner.inspect(target).await {
		Ok(LocalEntry::Placeholder {
			identity: Ok(held), ..
		}) if populated_uuid(held) == Some(clean.uuid) => {
			Some(remove_extra(inner, path, target, clean).await)
		}
		Ok(_) => None,
		Err(error) => Some(Step::Ended(local_failed(error))),
	}
}

/// Removes the clean placeholder at `path`, an extra placeholder of the item whose placeholder at
/// `target` is where the drive has it: one item never has two. Its bytes are a version the drive
/// holds; a directory goes with every clean item in it. Left so, the two would each move onto the item's name in turn, moving the other aside.
async fn remove_extra<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	target: &RootPath,
	clean: Clean,
) -> Step {
	if clean.is_dir {
		// With what it holds: each clean item in it goes, and one holding an edit stays, the
		// directory with it, until it is clean too.
		let step = remove_tree(inner, path, clean.uuid, Reach::Elsewhere).await;
		if step == Step::Ended(Outcome::Done) {
			warn!(item = %path_tag(path), target = %path_tag(target), "removed an extra placeholder of a directory, or what in it was clean");
			lock(&inner.state)
				.index
				.insert_if_populated(target, clean.uuid);
			// An item of the directory whose only placeholder was in the extra one is placed
			// again where the drive has it.
			inner.push_kind(target, Delay::Now, Kind::Children);
		}
		return step;
	}
	let (at, expect) = (path.clone(), clean.identity);
	match inner.fs(move |local| local.remove_clean(&at, expect)).await {
		Ok(()) | Err(LocalError::NotFound) => {
			warn!(item = %path_tag(path), target = %path_tag(target), "removed an extra placeholder of an item");
			{
				let mut state = lock(&inner.state);
				state.index.remove_under(path);
				state.index.insert_if_populated(target, clean.uuid);
				state.lineage.forget(path);
			}
			inner.report(LocalReport::Remote(RemoteApplied::Removed {
				path: path.clone(),
			}));
			Step::Ended(Outcome::Done)
		}
		Err(LocalError::Modified) => Step::Local,
		Err(error) => Step::Ended(local_failed(error)),
	}
}

/// Keeps the file placeholder `id` at `path`, which holds an edit, as a file of its own when it is
/// an extra placeholder of its item: another placeholder of the item stands where the drive has
/// it. Uploaded as a version of the item, the edit would land over that one's; detached, the local
/// side uploads it as a new file, and the conflict is reported. `None` when it is the item's only
/// placeholder, or where the drive has it: the local side uploads its edit as usual.
async fn keep_extra_edit<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	id: LocalFileId,
	clean: Clean,
	head: FfiObject,
) -> Option<Step> {
	let (target, _) = match drive_place(inner, head).await {
		Ok(Some(place)) => place,
		Ok(None) => return None,
		Err(step) => return Some(step),
	};
	// A rename in case only leaves the item one placeholder: `target` opens this one.
	if target.case_key() == path.case_key() || inner.renaming_onto(&target) {
		return None;
	}
	match inner.inspect(&target).await {
		Ok(LocalEntry::Placeholder {
			identity: Ok(held), ..
		}) if populated_uuid(held) == Some(clean.uuid) => {}
		Ok(_) => return None,
		Err(error) => return Some(Step::Ended(local_failed(error))),
	}
	let (at, detached) = (
		path.clone(),
		PlaceholderIdentity::Detached {
			nonce: Uuid::new_v4(),
		},
	);
	if let Err(error) = inner
		.fs(move |local| local.retag_unsynced(&at, id, detached))
		.await
	{
		return Some(Step::Ended(local_failed(error)));
	}
	warn!(item = %path_tag(path), target = %path_tag(&target), "kept an extra placeholder of an item holding an edit as a file of its own");
	{
		let mut state = lock(&inner.state);
		state.index.remove_under(path);
		state.index.insert_if_populated(&target, clean.uuid);
		state.lineage.forget(path);
	}
	inner.report(LocalReport::Conflict {
		path: target,
		kept_as: path.clone(),
		kind: ConflictKind::EditVsEdit,
	});
	Some(Step::Local)
}

/// [`keep_extra_edit`] for the local side, which looks at the edited file placeholder `identity`
/// at `path` first when a walk queued it: whether it was an extra placeholder of its item, now
/// detached, to be uploaded as a file of its own. Asked only of a file the lineage remembers at
/// another path too; any failure to tell leaves it to the local side as it is.
pub(in crate::local) async fn detached_as_extra<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	id: LocalFileId,
	identity: PlaceholderIdentity,
) -> bool {
	let (Some(uuid), PlaceholderIdentity::File { stable, .. }) =
		(populated_uuid(identity), identity)
	else {
		return false;
	};
	// Only a file whose placeholder was seen at another path too may have two: no drive call for
	// any other edit.
	if lock(&inner.state).lineage.others(stable, path).is_empty() {
		return false;
	}
	let head = match head_of(inner, identity).await {
		Ok(Some(head)) => head,
		Ok(None) => return false,
		// Left to the local side as it is, which uploads the edit checking the version it was
		// made from.
		Err(outcome) => {
			warn!(item = %path_tag(path), ?outcome, "reading the item of an edited placeholder another path held failed");
			return false;
		}
	};
	if is_trashed(&head) || matches!(head, FfiObject::Dir(_)) {
		return false;
	}
	let clean = Clean {
		identity,
		uuid,
		is_dir: false,
	};
	matches!(
		keep_extra_edit(inner, path, id, clean, head).await,
		Some(Step::Local)
	)
}

/// The item `identity` names as the cache holds it, confirmed by the server when the cache holds
/// none: a cache miss is never taken for the item's absence.
async fn head_of<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	identity: PlaceholderIdentity,
) -> Result<Option<FfiObject>, Outcome> {
	let Some(id) = identity.cache_id() else {
		return Ok(None);
	};
	match inner.drive.query_item(&id).await {
		Ok(Some(head)) => Ok(Some(head)),
		Ok(None) => inner
			.drive
			.refresh_item(id)
			.await
			.map_err(|error| failed(&error)),
		Err(error) => Err(failed(&error)),
	}
}

/// Where the directory `head` is in has its placeholder: `Ok(None)` when it has none, which only
/// a complete index can say; with an incomplete one the job waits for the walk.
fn local_parent<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	head: &FfiObject,
) -> Result<Option<RootPath>, Step> {
	let parent = parent_of(head).and_then(|parent| Uuid::parse_str(parent).ok());
	let state = lock(&inner.state);
	match parent.and_then(|parent| state.index.path_of(parent)) {
		Some(path) => Ok(Some(path.clone())),
		None if state.index.is_complete() => Ok(None),
		None => {
			drop(state);
			Inner::request_walk(inner);
			Err(Step::Ended(Outcome::Wait(WAIT_AGAIN)))
		}
	}
}

/// The placeholder `head` gets in the directory `parent`, as its population would place it: the
/// cache's listing of the directory decides case collisions, lowest uuid first. `Ok(None)` when it
/// gets none, reported skipped.
async fn target_spec<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	parent: &RootPath,
	head: FfiObject,
) -> Result<Option<PlaceholderSpec>, Step> {
	let Some(child) = as_child(head) else {
		return Err(Step::Ended(Outcome::Done));
	};
	if listed_id_and_name(&child)
		.1
		.as_deref()
		.is_some_and(is_upload_name)
	{
		// On its way to its name, as a moved file or an upload is: its placeholder stays where it
		// is, never renamed to a name nothing places, and the change that gives it its name brings
		// the job back.
		return Err(Step::Ended(Outcome::Done));
	}
	let uuid = match PlaceholderIdentity::from_child(&child).map(populated_uuid) {
		Ok(Some(uuid)) => uuid,
		_ => return Err(Step::Ended(Outcome::Park(Failure::Internal))),
	};
	let parent_identity = parent_identity(inner, parent);
	let listing = cached_listing(inner, parent_identity).await?;
	let planned = listing.as_ref().map(plan_entries);
	if let Some((specs, skipped)) = &planned {
		if let Some(spec) = specs
			.iter()
			.find(|spec| populated_uuid(spec.identity()) == Some(uuid))
		{
			return Ok(Some(spec.clone()));
		}
		if let Some(skip) = skipped.iter().find(|skip| skip.id() == uuid.to_string()) {
			return skip_target(inner, parent_identity, skip.clone());
		}
	}
	// Not in the cache's listing of the directory yet: placed from its own row.
	match spec_for(&child) {
		Ok(spec) => Ok(Some(spec)),
		Err(reason) => {
			let skip = SkippedItem::new(uuid.to_string(), None, reason);
			skip_target(inner, parent_identity, skip)
		}
	}
}

/// A remote item that gets no placeholder, reported: `Ok(None)`, so a clean one is removed; an
/// undecryptable one is left as it is, its name unknown.
fn skip_target<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	parent: Option<PlaceholderIdentity>,
	skip: SkippedItem,
) -> Result<Option<PlaceholderSpec>, Step> {
	let undecryptable = *skip.reason() == SkipReason::Undecryptable;
	if let Some(parent) = parent {
		inner.report_listed_skips(parent, vec![skip]);
	}
	if undecryptable {
		debug!("a remote item's name does not decrypt; its placeholder stays");
		return Err(Step::Ended(Outcome::Done));
	}
	Ok(None)
}

/// The identity of the populated directory at `parent`, from the index.
fn parent_identity<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	parent: &RootPath,
) -> Option<PlaceholderIdentity> {
	let uuid = lock(&inner.state).index.uuid_at(parent)?;
	Some(if parent.is_root() {
		PlaceholderIdentity::Root { root: uuid }
	} else {
		PlaceholderIdentity::Dir { uuid }
	})
}

/// The cache's listing of the populated directory `parent`, with no server round trip.
async fn cached_listing<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	parent: Option<PlaceholderIdentity>,
) -> Result<Option<QueryChildrenResponse>, Step> {
	match parent.and_then(PlaceholderIdentity::cache_id) {
		Some(id) => inner
			.drive
			.cached_children(&id)
			.await
			.map_err(|error| Step::Ended(failed(&error))),
		None => Ok(None),
	}
}

/// Renames the clean placeholder at `path` to `target`, the index, the queue and the lineage
/// following it. A name taken by an ordinary item moves that item aside first, as a conflict that
/// keeps both; one taken by another placeholder of the drive's is cleared or moved aside for its
/// own job; one taken by an item holding local data waits for the local side.
async fn rename<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	target: &RootPath,
	clean: Clean,
) -> Result<(), Step> {
	{
		let state = lock(&inner.state);
		// Two remote jobs renaming onto each other's paths (a swap) would wait for each other
		// forever: the one on the path first in case-key order goes ahead.
		let target_busy = state.queue.busy_at_or_under(target, path)
			&& !(path.case_key() < target.case_key()
				&& state.queue.runs_only(target, Kind::Remote));
		if target_busy || (clean.is_dir && state.queue.busy_at_or_under(path, path)) {
			return Err(Step::Ended(Outcome::Wait(BUSY_AGAIN)));
		}
	}
	match rename_clean(inner, path, target, clean.identity).await {
		Ok(()) => {}
		Err(LocalError::Exists) => {
			clear_target(inner, target, clean).await?;
			rename_clean(inner, path, target, clean.identity)
				.await
				.map_err(rename_failed)?;
		}
		Err(error) => return Err(rename_failed(error)),
	}
	let now = Instant::now();
	let admit = {
		let mut state = lock(&inner.state);
		state.index.rebase(path, target);
		if clean.is_dir {
			state.lineage.rebase(path, target);
		} else {
			state.lineage.relocated(path, target, now);
		}
		state.queue.rebase(path, target, now)
	};
	if admit == Admit::Overflow {
		inner.overflowed();
	}
	inner.report(LocalReport::Remote(RemoteApplied::Moved {
		from: path.clone(),
		to: target.clone(),
	}));
	Ok(())
}

/// How a remote job ends after a clean rename failed.
fn rename_failed(error: LocalError) -> Step {
	match error {
		LocalError::Modified => Step::Local,
		error => Step::Ended(local_failed(error)),
	}
}

async fn rename_clean<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	from: &RootPath,
	to: &RootPath,
	expect: PlaceholderIdentity,
) -> Result<(), LocalError> {
	let (from, to) = (from.clone(), to.clone());
	inner
		.fs(move |local| local.rename_clean(&from, &to, expect))
		.await
}

/// Frees `target` for a renamed placeholder: an ordinary item moves aside to a free `.old` name
/// and is taken in from there; another clean placeholder, dehydrated, is removed (its directory's
/// reconcile places it again where it belongs), and otherwise moves aside for its own remote job;
/// anything else waits for the local side.
async fn clear_target<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	target: &RootPath,
	clean: Clean,
) -> Result<(), Step> {
	let occupant = inner
		.inspect(target)
		.await
		.map_err(|error| Step::Ended(local_failed(error)))?;
	let Some(parent) = target.parent() else {
		return Err(Step::Ended(Outcome::Done));
	};
	match occupant {
		// Another placeholder of the same item took the name meanwhile: the next look finds this
		// one the extra one.
		LocalEntry::Placeholder {
			identity: Ok(held), ..
		} if populated_uuid(held) == Some(clean.uuid) => Err(Step::Ended(Outcome::Wait(BUSY_AGAIN))),
		LocalEntry::Missing => Ok(()),
		LocalEntry::Plain { .. } => {
			let aside = aside_for(inner, &parent, target).await?;
			set_aside(inner, target, aside, ConflictKind::CreateVsExisting)
				.await
				.map_err(|error| Step::Ended(local_failed(error)))
		}
		LocalEntry::Placeholder {
			identity:
				Ok(identity @ (PlaceholderIdentity::File { .. } | PlaceholderIdentity::Dir { .. })),
			kind,
			state,
			..
		} if is_clean(state, matches!(kind, LocalKind::Dir { .. })) => {
			let is_file = matches!(kind, LocalKind::File(_));
			if is_file && state.on_disk() == 0 {
				let at = target.clone();
				inner
					.fs(move |local| local.remove_clean(&at, identity))
					.await
					.map_err(|error| Step::Ended(local_failed(error)))?;
				lock(&inner.state).index.remove_under(target);
				replace_later(inner, identity).await;
				return Ok(());
			}
			let aside = aside_for(inner, &parent, target).await?;
			rename_clean(inner, target, &aside, identity)
				.await
				.map_err(|error| Step::Ended(local_failed(error)))?;
			lock(&inner.state).index.rebase(target, &aside);
			inner.push_kind(&aside, Delay::Now, Kind::Remote);
			Ok(())
		}
		LocalEntry::Placeholder { .. } | LocalEntry::Unsupported(_) => {
			inner.push(target, Delay::Now);
			Err(Step::Ended(Outcome::Wait(WAIT_AGAIN)))
		}
	}
}

/// Queues a reconcile of the populated directory the item `identity` is in now, whose
/// placeholder was removed to free a name: it places the item again where it belongs.
async fn replace_later<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	identity: PlaceholderIdentity,
) {
	let Some(id) = identity.cache_id() else {
		return;
	};
	let head = match inner.drive.query_item(&id).await {
		Ok(head) => head,
		Err(error) => {
			debug!(%error, "looking up a removed name holder failed");
			None
		}
	};
	let parent = head
		.as_ref()
		.and_then(parent_of)
		.and_then(|parent| Uuid::parse_str(parent).ok());
	let path = parent.and_then(|parent| lock(&inner.state).index.path_of(parent).cloned());
	match path {
		Some(path) => inner.push_kind(&path, Delay::Now, Kind::Children),
		None => Inner::request_walk(inner),
	}
}

/// A free `.old` name beside `target`, free in the cache's listing of `parent` too.
async fn aside_for<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	parent: &RootPath,
	target: &RootPath,
) -> Result<RootPath, Step> {
	let listing = cached_listing(inner, parent_identity(inner, parent)).await?;
	let listed = listing.map(|listing| listing.objects).unwrap_or_default();
	match choose_aside(inner, target, &listed).await {
		Some(aside) => Ok(aside),
		None => {
			warn!(target = %path_tag(target), "no free name to move a placeholder's name holder aside");
			Err(Step::Ended(Outcome::Park(Failure::Internal)))
		}
	}
}

/// Updates the clean file placeholder at `path` to the newer version `spec`, dehydrated, unless a
/// read of the file is in flight (a dehydrate must never race a transfer); a pinned one stays
/// pinned and is hydrated again.
async fn update<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	clean: Clean,
	spec: &PlaceholderSpec,
) -> Step {
	if inner.hints.reads_in_flight(clean.uuid) > 0 {
		return Step::Ended(Outcome::Wait(WAIT_AGAIN));
	}
	if let Some(step) = unconfirmed_update(inner, path, clean, spec).await {
		return step;
	}
	let (at, to, expect) = (path.clone(), spec.clone(), clean.identity);
	match inner
		.fs(move |local| local.update_from_remote(&at, expect, &to))
		.await
	{
		Ok(updated) => {
			if let PlaceholderIdentity::File { stable, version } = spec.identity() {
				lock(&inner.state)
					.lineage
					.remember(path, stable, version, Instant::now());
			}
			inner.report(LocalReport::Remote(RemoteApplied::Updated {
				path: path.clone(),
				identity: spec.identity(),
			}));
			if updated.was_pinned {
				// Kept on this device: the new version is hydrated and checked.
				inner.push_kind(path, Delay::Now, Kind::Hydration);
			}
			Step::Ended(Outcome::Done)
		}
		Err(LocalError::Modified) => Step::Local,
		Err(error) => Step::Ended(local_failed(error)),
	}
}

/// What stops the update of the clean file placeholder `clean` at `path` to `spec` when the file
/// holds the version this run uploaded: `None` once the server confirms `spec` as the head. The
/// cache's row can show a head older than the upload after it (a listing fetched before the upload,
/// or the echo of an earlier upload, written over the upload's row: neither is ordered against
/// it), and updating to it would put the user's newest bytes out of sight until the cache caught
/// up. A version restored elsewhere looks the same in the cache, as a restore makes the old
/// version's uuid the head again, so the server tells them apart. When it holds the uploaded
/// version still, the job ends with nothing done (the refresh corrected the cache's row); when
/// another version, the job looks again.
async fn unconfirmed_update<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	clean: Clean,
	spec: &PlaceholderSpec,
) -> Option<Step> {
	let ours = FileVersion::of(clean.identity)?;
	if !inner.hints.uploaded_here(ours) {
		return None;
	}
	let head = match inner.drive.refresh_item(stable_id(ours.stable)).await {
		Ok(head) => head,
		Err(error) => return Some(Step::Ended(failed(&error))),
	};
	let version = match head.as_ref().filter(|head| !is_trashed(head)) {
		Some(FfiObject::File(file)) => file_ids(file).map(|(_, version)| version),
		Some(FfiObject::Dir(_) | FfiObject::Root(_)) | None => None,
	};
	let to = FileVersion::of(spec.identity()).map(|to| to.version);
	match version {
		Some(version) if Some(version) == to => None,
		Some(version) if version == ours.version => {
			debug!(%path, "the cache showed a head older than this run's upload; the file stays");
			Some(Step::Ended(Outcome::Done))
		}
		_ => Some(Step::Ended(Outcome::Wait(WAIT_AGAIN))),
	}
}

/// Where the remote item of a placeholder being removed is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reach {
	/// Nowhere, or in the trash.
	Gone,
	/// Alive, out of the populated directories or under a name the sync root does not hold.
	Elsewhere,
}

/// Removes the clean placeholder at `path`, whose remote item is gone or out of reach: a file at
/// once, a directory with everything clean in it.
async fn remove<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	clean: Clean,
	reach: Reach,
) -> Step {
	if clean.is_dir {
		return remove_tree(inner, path, clean.uuid, reach).await;
	}
	let (at, expect) = (path.clone(), clean.identity);
	match inner.fs(move |local| local.remove_clean(&at, expect)).await {
		Ok(()) => {
			forget(inner, path);
			inner.report(LocalReport::Remote(RemoteApplied::Removed {
				path: path.clone(),
			}));
			Step::Ended(Outcome::Done)
		}
		Err(LocalError::Modified) => Step::Local,
		Err(error) => Step::Ended(local_failed(error)),
	}
}

fn forget<D: Drive, L: LocalFs>(inner: &Arc<Inner<D, L>>, path: &RootPath) {
	let mut state = lock(&inner.state);
	state.index.remove_under(path);
	state.lineage.forget(path);
}

/// What became of one item of a directory removed with its remote directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fate {
	Removed,
	/// Holds what the drive may not have: kept.
	Kept,
	/// Another handle refused its removal: tried again later.
	Busy,
}

/// Removes the directory placeholder `uuid` at `path`, whose remote directory the server confirmed
/// gone or holds in its trash, with every clean item in it: as the remote job does, for the local
/// side, which found it so.
pub(in crate::local) async fn remove_gone_dir<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	uuid: Uuid,
) -> Outcome {
	match remove_tree(inner, path, uuid, Reach::Gone).await {
		Step::Ended(outcome) => outcome,
		Step::Local => Outcome::Done,
	}
}

/// Removes the directory placeholder `uuid` at `path` with every clean item in it, deepest first,
/// once no job runs under it (one may be uploading into it). An item holding what the drive may not
/// have is kept, and with it every directory above it. When the remote directory is gone, each is
/// detached from it: the local side creates them on the drive anew and takes the kept items in
/// under them. When it lives elsewhere, they stay as they are, the kept items uploaded to their
/// remote items there, and a later reconcile removes them once clean. A directory in it that the
/// drive holds alive in another directory, moved out before its remote directory went, is left
/// to its own remote job, and the removal waits until that moved it.
async fn remove_tree<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	uuid: Uuid,
	reach: Reach,
) -> Step {
	if lock(&inner.state).queue.busy_at_or_under(path, path) {
		return Step::Ended(Outcome::Wait(BUSY_AGAIN));
	}
	let mut kept = 0usize;
	let mut busy = false;
	let mut moved_out = false;
	// Every directory under `path` with its uuid, parents before children, then removed children
	// first.
	let mut dirs = vec![(path.clone(), uuid)];
	let mut at = 0;
	while let Some((dir, dir_uuid)) = dirs.get(at).cloned() {
		at += 1;
		let listed = {
			let dir = dir.clone();
			inner.fs(move |local| local.list_dir(&dir)).await
		};
		let children = match listed {
			Ok(children) => children,
			Err(error) => return Step::Ended(local_failed(error)),
		};
		for child in children {
			let Ok(child) = child else {
				kept += 1;
				continue;
			};
			if inner.renaming_onto(&child) {
				// Never read before the rename happened: looked at again once it settled.
				busy = true;
				continue;
			}
			match inner.inspect(&child).await {
				Ok(LocalEntry::Placeholder {
					kind: LocalKind::Dir { .. },
					identity: Ok(PlaceholderIdentity::Dir { uuid: child_uuid }),
					..
				}) => {
					if reach == Reach::Gone {
						match moved_away(inner, child_uuid, dir_uuid).await {
							Ok(false) => {}
							Ok(true) => {
								// Moved out on the drive before its directory went: its own remote
								// job moves it after its remote directory, with what it holds, and
								// this one looks again once it did.
								inner.push_kind(&child, Delay::Now, Kind::Remote);
								moved_out = true;
								continue;
							}
							Err(step) => return step,
						}
					}
					dirs.push((child, child_uuid));
				}
				Ok(LocalEntry::Placeholder {
					kind: LocalKind::File(_),
					identity: Ok(identity @ PlaceholderIdentity::File { .. }),
					state,
					..
				}) if is_clean(state, false) => match remove_item(inner, &child, identity).await {
					Fate::Removed => {}
					Fate::Kept => kept += 1,
					Fate::Busy => busy = true,
				},
				Ok(LocalEntry::Placeholder {
					kind: LocalKind::File(stamp),
					identity: Ok(identity @ PlaceholderIdentity::File { stable, .. }),
					state,
					id,
				}) => {
					let unwritten = if may_hold_edit(state, stamp.size) {
						Err(LocalError::Modified)
					} else {
						remove_unwritten(inner, &child, id, identity).await
					};
					match unwritten {
						Ok(Fate::Removed) => {}
						Ok(Fate::Kept) | Err(LocalError::NotFound) => kept += 1,
						Ok(Fate::Busy) | Err(LocalError::Busy) => busy = true,
						Err(LocalError::Modified) => {
							// Holds an edit: with its directory gone, detached, so the local side
							// uploads it as a new file where the directory is created anew, never as
							// a version of the file trashed with it.
							if reach == Reach::Gone
								&& let Err(step) = detach_file(inner, &child, id, stable).await
							{
								return step;
							}
							kept += 1;
						}
						Err(
							error @ (LocalError::Exists | LocalError::Pinned | LocalError::Os(_)),
						) => {
							return Step::Ended(local_failed(error));
						}
					}
				}
				Ok(LocalEntry::Missing) => {}
				Ok(_) | Err(_) => kept += 1,
			}
		}
	}
	if moved_out {
		return Step::Ended(Outcome::Wait(WAIT_AGAIN));
	}
	let mut detached = Vec::new();
	for (dir, _) in dirs.iter().rev() {
		let entry = inner.inspect(dir).await;
		let Ok(LocalEntry::Placeholder {
			identity: Ok(identity @ PlaceholderIdentity::Dir { .. }),
			id,
			state,
			..
		}) = entry
		else {
			continue;
		};
		// A directory holds no data of its own: one not in sync goes with its remote directory too.
		let removable = state.in_sync()
			|| reach == Reach::Gone && {
				let at = dir.clone();
				inner
					.fs(move |local| local.set_dir_in_sync(&at, id))
					.await
					.is_ok()
			};
		let fate = if removable {
			remove_item(inner, dir, identity).await
		} else {
			Fate::Kept
		};
		match fate {
			Fate::Removed => {}
			Fate::Busy => busy = true,
			Fate::Kept => detached.push((dir.clone(), id)),
		}
	}
	// A clean item another handle held stays: the whole tree is looked at again once it is let go,
	// before anything kept is detached, or the clean item would stay behind under a directory the
	// local side creates anew.
	if busy {
		return Step::Ended(Outcome::Busy);
	}
	if detached.is_empty() {
		forget(inner, path);
		inner.report(LocalReport::Remote(RemoteApplied::Removed {
			path: path.clone(),
		}));
		return Step::Ended(Outcome::Done);
	}
	if reach == Reach::Elsewhere {
		debug!(%path, kept, %uuid, "kept items holding local data where their remote directory left");
		return Step::Ended(Outcome::Done);
	}
	for (dir, id) in &detached {
		let (at, expect) = (dir.clone(), *id);
		let identity = PlaceholderIdentity::Detached {
			nonce: Uuid::new_v4(),
		};
		if let Err(error) = inner
			.fs(move |local| local.retag_unsynced(&at, expect, identity))
			.await
		{
			debug!(%dir, %error, "detaching a kept directory failed");
			return Step::Ended(local_failed(error));
		}
	}
	forget(inner, path);
	debug!(%path, kept, %uuid, "kept a directory whose remote directory went");
	inner.report(LocalReport::Remote(RemoteApplied::DirKept {
		path: path.clone(),
		kept,
	}));
	inner.push(path, Delay::Now);
	Step::Ended(Outcome::Done)
}

/// Whether the cache holds the directory `uuid`, placed in the local directory of `parent`, alive
/// in another directory: moved away from it on the drive.
async fn moved_away<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	uuid: Uuid,
	parent: Uuid,
) -> Result<bool, Step> {
	match inner.drive.query_item(&stable_id(uuid)).await {
		Ok(Some(item @ FfiObject::Dir(_))) => Ok(!is_trashed(&item)
			&& parent_of(&item).is_some_and(|now| !same_uuid(now, &parent.to_string()))),
		Ok(_) => Ok(false),
		Err(error) => Err(Step::Ended(failed(&error))),
	}
}

/// Detaches the file placeholder `id` at `path`, the file `stable` holding an edit, after giving up
/// the cache's copy of the edit, which would otherwise be uploaded into the trashed file.
async fn detach_file<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	id: LocalFileId,
	stable: Uuid,
) -> Result<(), Step> {
	inner
		.drive
		.drop_pending_upload(stable_id(stable))
		.await
		.map_err(|error| Step::Ended(failed(&error)))?;
	let (at, identity) = (
		path.clone(),
		PlaceholderIdentity::Detached {
			nonce: Uuid::new_v4(),
		},
	);
	inner
		.fs(move |local| local.retag_unsynced(&at, id, identity))
		.await
		.map_err(|error| Step::Ended(local_failed(error)))?;
	lock(&inner.state).lineage.forget(path);
	Ok(())
}

/// Removes the file placeholder `id` at `path`, holding `identity`, not in sync but not all on disk
/// with no byte written: only its name or attributes changed, so it holds nothing the drive lacks.
/// Marked in sync first, which neither reads nor fetches its data, then removed as a clean item.
/// Detaching it instead would leave the local side a placeholder to upload whose missing bytes
/// nothing can fetch any more. [`LocalError::Modified`] when it came to hold an edit meanwhile.
async fn remove_unwritten<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	id: LocalFileId,
	identity: PlaceholderIdentity,
) -> Result<Fate, LocalError> {
	let at = path.clone();
	inner
		.fs(move |local| local.mark_partial_in_sync(&at, id, identity))
		.await?;
	Ok(remove_item(inner, path, identity).await)
}

/// Removes one clean item of a removed directory.
async fn remove_item<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	identity: PlaceholderIdentity,
) -> Fate {
	let (at, expect) = (path.clone(), identity);
	match inner.fs(move |local| local.remove_clean(&at, expect)).await {
		Ok(()) | Err(LocalError::NotFound) => {
			forget(inner, path);
			Fate::Removed
		}
		Err(LocalError::Busy) => Fate::Busy,
		Err(LocalError::Exists | LocalError::Modified | LocalError::Pinned | LocalError::Os(_)) => {
			Fate::Kept
		}
	}
}

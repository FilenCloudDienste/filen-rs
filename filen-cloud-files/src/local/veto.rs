//! Deletes and renames of placeholders: the platform asks before each, and the core allows it or
//! vetoes it within the platform's time limit.
//!
//! A rename or move inside the root is applied to the drive first and allowed once it is, so one
//! the drive cannot take (offline, a name taken) is vetoed and the item keeps its name: the default
//! for the open decision on offline renames. Where the item was on the drive is recorded before the
//! first change, and the changes run in a task of their own that the answer's time limit does not
//! cancel: a rename vetoed after the drive moved the item (the limit ran out, or the provider
//! stopped) is put back on the drive, as is one that fails locally after it was allowed.
//!
//! A rename onto the name of another file's clean placeholder replaces that placeholder locally,
//! as an editor renaming the old file onto its backup name over the last backup does: the other
//! file is moved out of the way on the drive under its transit name, recorded with the rename,
//! trashed once the rename happened locally, and given its name back if the rename is put back.
//! Onto anything else holding the name, the rename is vetoed.
//!
//! A rename of a save's temporary file into the place of the file it saves is that file's save,
//! whenever the editor's process gets to it: the temporary file may have been uploaded as a file
//! of its own by then. The file renamed is one this run uploaded moments before from an ordinary
//! file, still at that version ([`FreshUploads`](super::lineage::FreshUploads)); it stays in its
//! directory; and the path it goes to holds another file's placeholder with nothing the drive
//! lacks, or claims a file that was there moments ago ([`super::lineage`]). Then the drive is not
//! changed for the rename: once it happened, the temporary file is made the ordinary file it was
//! before its upload, which the path's job uploads as the saved file's new version as for any
//! atomic save, and its upload goes to the trash ([`RenameRecord::save_over`]). Any other file
//! renamed over another, one moved in from another directory, or a file older than that, is the
//! user moving one file over another, and replaces it as above.
//!
//! A delete, or a move out of the root, is checked against the drive (offline vetoes it) and
//! allowed, and the remote item is trashed only once the item is gone locally, after a grace
//! period: neither the delete notification nor its completion proves a delete (the recycle bin
//! announces one, takes it back, then moves the item out), and an editor's save deletes the old
//! file and puts a new one in its place, which is a new version of the item rather than a delete.
//! The trash is where the item goes, never a permanent delete.
//!
//! Allowed deletes waiting for their grace period are kept in memory only: a provider that stops
//! within it leaves the remote item in place, and the remote side puts it back locally. Nothing is
//! lost, but the delete is.
//!
//! A rename's record ([`RenameRecord`]) is saved before the drive is changed for it, and dropped
//! only once the rename is settled: put back, or found done locally. A provider killed before it
//! put a vetoed rename back puts it back on its next start, from the record, before the remote side
//! may follow the drive; one whose put-back fails (offline) tries again with backoff, and the
//! remote side leaves the placeholder alone meanwhile.

use std::{
	sync::{Arc, Mutex},
	time::Duration,
};

use filen_mobile_native_cache::ffi::{FfiId, FfiNonRootObject, FfiObject};
use filen_sdk_rs::{
	fs::name::{ValidatedName, aside_names, collision_key},
	sync_engine::watch_backoff,
};
use filen_types::fs::Uuid;
use tokio::{
	sync::oneshot,
	time::{Instant, sleep, timeout},
};
use tracing::{debug, warn};

use super::{
	Inner, State,
	jobs::{file_ids, find_by_name, is_trashed, object_uuid, same_uuid, still_claims, take_in_dir},
	lock,
	queue::{Admit, Delay, Outcome, PARK_AFTER},
	records::{Displaced, RenameRecord, SaveOver},
	relocate::{Place, Relocated, relocate_file},
	remote::may_hold_edit,
	report::{LocalReport, UndoneOp, VetoedOp},
};
use crate::{
	drive::Drive,
	failure::{Answer, Failure, cache_answer},
	identity::{PlaceholderIdentity, stable_id},
	log_safe::{self, path_tag},
	names::transit_name,
	os::{
		DeleteRequest, DeleteResponder, LocalEntry, LocalError, LocalFs, LocalKind, RenameRequest,
		RenameResponder, RenameTarget,
	},
	root_path::RootPath,
};

/// How many free names beside its own a file moved out of a rename's way tries, once another file
/// took its own, before it tries again later: each try is a drive request.
const DISPLACED_ASIDE_ATTEMPTS: usize = 16;
/// How long the core takes to answer a delete or rename: under the platform's 60 s limit, after
/// which it fails the operation by itself. Running out vetoes it as unreachable.
const VETO_BUDGET: Duration = Duration::from_secs(45);
/// How long after an allowed delete the core checks the item is gone before trashing it: long
/// enough for an editor's save to put its new file in the old one's place.
pub(super) const DELETE_GRACE: Duration = Duration::from_secs(3);
/// How long after an allowed rename the core checks it happened, unless its completion comes
/// first.
const RENAME_GRACE: Duration = Duration::from_secs(10);
/// The longest a put-back that failed waits before it is tried again.
const PUT_BACK_AGAIN_MAX: Duration = Duration::from_secs(5 * 60);
/// How long a save's rename over a placeholder holding an edit waits for the edit to reach the drive
/// as a version of its own, which the save then follows: within the answer's budget.
const EDIT_FIRST_WAIT: Duration = Duration::from_secs(20);
/// How often that wait looks at the placeholder again.
const EDIT_FIRST_POLL: Duration = Duration::from_millis(250);

/// An allowed delete, waiting to be confirmed locally before the remote item is trashed.
#[derive(Debug, Clone)]
pub(super) struct PendingDelete {
	pub(super) path: RootPath,
	pub(super) identity: PlaceholderIdentity,
	pub(super) due: Instant,
	attempts: u32,
	pub(super) stage: DeleteStage,
}

/// Where a pending delete is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DeleteStage {
	/// Waiting for its due time.
	Waiting,
	/// Its confirmation runs; `then` is a later delete at the same path, of another item, to
	/// confirm after it. `covered` once a directory above it was trashed meanwhile, which took the
	/// item with it: the confirmation ends with nothing more to do.
	Running {
		then: Option<PlaceholderIdentity>,
		covered: bool,
	},
}

impl PendingDelete {
	/// Marks its confirmation running, and returns what the confirmation takes.
	pub(super) fn start(&mut self) -> Self {
		self.stage = DeleteStage::Running {
			then: None,
			covered: false,
		};
		self.clone()
	}
}

/// A rename the drive was changed for, waiting to be confirmed locally, or put back.
#[derive(Debug, Clone)]
pub(super) struct PendingRename {
	/// What it changes, and where the item was on the drive before.
	pub(super) record: RenameRecord,
	pub(super) due: Instant,
	// Reason for the Arc: the answer and the task changing the drive share it.
	decision: Arc<RenameDecision>,
	/// Read back from its record at the start: a previous run was stopped before it settled.
	restored: bool,
	/// How many times putting it back failed.
	attempts: u32,
}

impl PendingRename {
	/// Whether its confirmation may run: the drive is no longer being changed for it.
	pub(super) fn settled(&self) -> bool {
		matches!(
			self.decision.get(),
			RenameStage::Allowed | RenameStage::Refused
		)
	}

	/// Whether this run vetoed it: the platform never makes it.
	pub(super) fn vetoed(&self) -> bool {
		!self.restored
			&& matches!(
				self.decision.get(),
				RenameStage::Refused | RenameStage::Vetoed
			)
	}

	/// Whether the drive was being changed for it when the provider stopped.
	pub(super) fn interrupted(&self) -> bool {
		matches!(
			self.decision.get(),
			RenameStage::Deciding | RenameStage::Vetoed
		)
	}
}

/// Where a rename's answer is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RenameStage {
	/// The drive is being changed, and nothing was answered.
	Deciding,
	/// Vetoed while the drive was still being changed: put the drive back once that ends.
	Vetoed,
	/// Allowed: confirm it happened locally.
	Allowed,
	/// Vetoed after the drive was changed: put the drive back.
	Refused,
}

/// A rename's stage, shared by its answer and the task changing the drive for it.
#[derive(Debug)]
pub(super) struct RenameDecision(Mutex<RenameStage>);

impl RenameDecision {
	fn new() -> Self {
		Self(Mutex::new(RenameStage::Deciding))
	}

	fn refused() -> Self {
		Self(Mutex::new(RenameStage::Refused))
	}

	fn get(&self) -> RenameStage {
		*lock(&self.0)
	}

	/// Moves the stage on by `step`, returning the new stage.
	fn step(&self, step: impl FnOnce(RenameStage) -> RenameStage) -> RenameStage {
		let mut stage = lock(&self.0);
		*stage = step(*stage);
		*stage
	}
}

/// Whether two identities name the same remote item, whatever version a file holds.
fn same_item(a: PlaceholderIdentity, b: PlaceholderIdentity) -> bool {
	match (a, b) {
		(
			PlaceholderIdentity::File { stable: a, .. },
			PlaceholderIdentity::File { stable: b, .. },
		) => a == b,
		(a, b) => a == b,
	}
}

/// The failure a veto reports for a cache error, and `None` when the error says the item is gone.
fn veto_failure(error: &filen_mobile_native_cache::CacheError) -> Option<Failure> {
	match cache_answer(error) {
		Answer::Fail(Failure::NotFound) => None,
		Answer::Fail(failure) => Some(failure),
		Answer::Cancelled | Answer::Damaged => Some(Failure::Transient),
	}
}

/// Answers a delete request: allowed, or vetoed with why.
pub(super) async fn answer_delete<D: Drive, L: LocalFs, R: DeleteResponder>(
	inner: &Arc<Inner<D, L>>,
	request: DeleteRequest,
	responder: R,
) {
	let decision = timeout(VETO_BUDGET, decide_delete(inner, &request))
		.await
		.unwrap_or(Err(Failure::Unreachable));
	match decision {
		Ok(()) => responder.allow(),
		Err(failure) => {
			inner.report(LocalReport::Vetoed {
				path: request.path().clone(),
				op: VetoedOp::Delete,
				failure,
			});
			responder.veto(failure);
		}
	}
}

async fn decide_delete<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	request: &DeleteRequest,
) -> Result<(), Failure> {
	let path = request.path();
	let Ok(identity) = PlaceholderIdentity::decode(request.identity()) else {
		// No remote item this build can name: deleting it touches nothing remote.
		debug!(%path, "allowed the delete of a placeholder with an unknown identity");
		return Ok(());
	};
	if request.undelete() {
		// The platform took the delete back: there is nothing to trash.
		lock(&inner.state).deletes.remove(&path.case_key());
		return Ok(());
	}
	match identity {
		PlaceholderIdentity::Root { .. } => Err(Failure::InvalidRequest),
		PlaceholderIdentity::Detached { .. } => {
			lock(&inner.state).queue.remove_under(path);
			Ok(())
		}
		PlaceholderIdentity::File { .. } | PlaceholderIdentity::Dir { .. } => {
			approve_removal(inner, path, identity).await
		}
	}
}

/// Checks the item `identity` at `path` can be trashed (the drive answers, and holds no newer
/// version than the placeholder) and records its delete to be confirmed.
async fn approve_removal<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	identity: PlaceholderIdentity,
) -> Result<(), Failure> {
	let Some(id) = identity.cache_id() else {
		return Ok(());
	};
	if let PlaceholderIdentity::File { stable, version } = identity {
		// What the placeholder held: a new file put in its place is a save of it.
		lock(&inner.state)
			.lineage
			.remember(path, stable, version, Instant::now());
	}
	let item = match inner.drive.refresh_item(id).await {
		Ok(item) => item,
		Err(error) => return veto_failure(&error).map_or(Ok(()), Err),
	};
	match (identity, item) {
		(_, None) => {}
		(_, Some(item)) if is_trashed(&item) => {}
		(PlaceholderIdentity::File { version, .. }, Some(FfiObject::File(head)))
			if Uuid::parse_str(&head.uuid).ok() != Some(version) =>
		{
			// The remote got a version this placeholder never held: the edit outlives the delete.
			inner.report(LocalReport::RemoteOutlivedDelete { path: path.clone() });
		}
		_ => trash_later(inner, path, identity),
	}
	Ok(())
}

/// A delete of `identity` at `path`, due after the grace period.
fn pending_delete(path: &RootPath, identity: PlaceholderIdentity) -> PendingDelete {
	PendingDelete {
		path: path.clone(),
		identity,
		due: Instant::now() + DELETE_GRACE,
		attempts: 0,
		stage: DeleteStage::Waiting,
	}
}

/// Trashes the remote item `identity` once the item at `path` is confirmed gone locally. While a
/// confirmation of another item at the path runs, this one waits to follow it.
pub(super) fn trash_later<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	identity: PlaceholderIdentity,
) {
	let mut state = lock(&inner.state);
	let key = path.case_key();
	match state.deletes.get_mut(&key) {
		Some(PendingDelete {
			identity: running,
			stage: DeleteStage::Running { then, .. },
			..
		}) => {
			if !same_item(*running, identity) {
				*then = Some(identity);
			}
		}
		_ => {
			state.deletes.insert(key, pending_delete(path, identity));
		}
	}
	drop(state);
	inner.wake.notify_one();
}

/// Drops every delete waiting to trash the file `stable`: a saved file took it over, as its new
/// version.
pub(super) fn forget_deletes_of<D: Drive, L: LocalFs>(inner: &Inner<D, L>, stable: Uuid) {
	lock(&inner.state).deletes.retain(|_, pending| {
		matches!(pending.stage, DeleteStage::Running { .. })
			|| !matches!(pending.identity, PlaceholderIdentity::File { stable: held, .. } if held == stable)
	});
}

/// Has every delete waiting to trash the file `stable` trash it at `version` instead: a save that
/// claims it uploaded that version, and is not taken over yet. Deleted before it is, the save goes
/// with the file, rather than reading as a version another device made.
pub(super) fn retarget_deletes_of<D: Drive, L: LocalFs>(
	inner: &Inner<D, L>,
	stable: Uuid,
	version: Uuid,
) {
	for pending in lock(&inner.state).deletes.values_mut() {
		if let PlaceholderIdentity::File { stable: held, .. } = pending.identity
			&& held == stable
		{
			pending.identity = PlaceholderIdentity::File { stable, version };
		}
	}
}

/// What confirming a delete came to.
enum Confirmed {
	/// Nothing more to do.
	Done,
	/// Look again after the wait given.
	Later(Duration),
	/// The drive failed; retry with backoff.
	Retry(Failure),
}

/// Confirms an allowed delete and trashes its remote item.
pub(super) async fn confirm_delete<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	pending: PendingDelete,
) {
	let confirmed = confirm(inner, &pending).await;
	let now = Instant::now();
	let mut reports = Vec::new();
	{
		let mut state = lock(&inner.state);
		let key = pending.path.case_key();
		// A directory above it was trashed while it ran: whatever it came to, nothing is left.
		let confirmed = if covered(&state, &pending.path) {
			Confirmed::Done
		} else {
			confirmed
		};
		match confirmed {
			Confirmed::Done => {
				let then = state
					.deletes
					.remove(&key)
					.and_then(|entry| match entry.stage {
						DeleteStage::Running { then, .. } => then,
						DeleteStage::Waiting => None,
					});
				if let Some(identity) = then {
					state
						.deletes
						.insert(key, pending_delete(&pending.path, identity));
				}
			}
			Confirmed::Later(wait) => {
				if let Some(entry) = state.deletes.get_mut(&key) {
					entry.stage = DeleteStage::Waiting;
					entry.due = now + wait;
				}
			}
			Confirmed::Retry(failure) => {
				if let Some(entry) = state.deletes.get_mut(&key) {
					entry.stage = DeleteStage::Waiting;
					entry.attempts += 1;
					entry.due = now
						+ watch_backoff(entry.attempts)
							.expect("a failed confirmation counts at least one failure");
					if entry.attempts == PARK_AFTER {
						reports.push(LocalReport::Parked {
							path: entry.path.clone(),
							failure,
							attempts: entry.attempts,
						});
					}
				}
			}
		}
	}
	for report in reports {
		inner.report(report);
	}
	inner.wake.notify_one();
}

/// Whether a directory above the item at `path` was trashed while its confirmation ran.
fn covered(state: &State, path: &RootPath) -> bool {
	matches!(
		state.deletes.get(&path.case_key()),
		Some(PendingDelete {
			stage: DeleteStage::Running { covered: true, .. },
			..
		})
	)
}

async fn confirm<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	pending: &PendingDelete,
) -> Confirmed {
	let path = &pending.path;
	{
		let state = lock(&inner.state);
		if covered(&state, path) {
			return Confirmed::Done;
		}
		// A directory above it waits to be deleted too: its trash covers this item, or its
		// failure says whether this one went.
		if state
			.deletes
			.values()
			.any(|other| other.path.depth() < path.depth() && path.starts_with(&other.path))
		{
			return Confirmed::Later(DELETE_GRACE);
		}
	}
	match inner.inspect(path).await {
		Ok(LocalEntry::Placeholder {
			identity: Ok(identity),
			..
		}) if same_item(identity, pending.identity) => {
			// The delete never happened.
			return Confirmed::Done;
		}
		Ok(LocalEntry::Plain { .. }) => {
			let PlaceholderIdentity::File { stable, .. } = pending.identity else {
				inner.push(path, Delay::Now);
				return Confirmed::Done;
			};
			if lock(&inner.state)
				.lineage
				.get(path)
				.is_some_and(|seen| seen.stable == stable)
			{
				// A save put a new file in its place, which its job makes the new version: the delete
				// waits until the job took the file over, which drops it, or gave up on that, or
				// the new file is gone too.
				return Confirmed::Later(DELETE_GRACE);
			}
			// A new file that is no save of the deleted one: taken in on its own.
			inner.push(path, Delay::Now);
		}
		Ok(_) => {}
		Err(_) => return Confirmed::Retry(Failure::Transient),
	}
	if let PlaceholderIdentity::File { stable, .. } = pending.identity {
		if saving(inner, stable) {
			// A save's temporary file is being put in the file's place: the file outlives its old
			// copy's delete, as its new version.
			return Confirmed::Later(DELETE_GRACE);
		}
		let others = lock(&inner.state).lineage.others(stable, path);
		for other in others {
			if inner.renaming_onto(&other) {
				continue;
			}
			if matches!(inner.inspect(&other).await, Ok(LocalEntry::Plain { .. })) {
				// A save elsewhere claims the file (an editor's backup was deleted).
				inner.push(&other, Delay::Now);
				return Confirmed::Done;
			}
		}
	}
	let Some(id) = pending.identity.cache_id() else {
		return Confirmed::Done;
	};
	let refreshed = inner.drive.refresh_item(id.clone()).await;
	// A save's upload may have retargeted the delete while this ran ([`retarget_deletes_of`]): the
	// version it started with and the one it has now both count as the deleted file's.
	let retargeted = lock(&inner.state)
		.deletes
		.get(&path.case_key())
		.map(|entry| entry.identity);
	let deleted_version = |head: &str| {
		[Some(pending.identity), retargeted]
			.into_iter()
			.flatten()
			.any(|identity| {
				matches!(identity, PlaceholderIdentity::File { version, .. }
				if Uuid::parse_str(head).ok() == Some(version))
			})
	};
	match refreshed {
		Ok(None) => return Confirmed::Done,
		Ok(Some(item)) if is_trashed(&item) => return Confirmed::Done,
		Ok(Some(FfiObject::File(head)))
			if matches!(pending.identity, PlaceholderIdentity::File { .. })
				&& !deleted_version(&head.uuid) =>
		{
			inner.report(LocalReport::RemoteOutlivedDelete { path: path.clone() });
			return Confirmed::Done;
		}
		Ok(Some(_)) => {}
		Err(error) => {
			return veto_failure(&error).map_or(Confirmed::Done, Confirmed::Retry);
		}
	}
	// A directory above it may have been trashed while this looked.
	if covered(&lock(&inner.state), path) {
		return Confirmed::Done;
	}
	match inner.drive.trash(id).await {
		Ok(_) => {
			{
				let mut state = lock(&inner.state);
				state.lineage.forget(path);
				// The deletes under it are done: a waiting one is dropped, and a running one,
				// which may be past its look for a pending directory above it, is told so.
				state.deletes.retain(|_, other| {
					if other.path.depth() <= path.depth() || !other.path.starts_with(path) {
						return true;
					}
					match &mut other.stage {
						DeleteStage::Running { covered, .. } => {
							*covered = true;
							true
						}
						DeleteStage::Waiting => false,
					}
				});
				state.queue.remove_under(path);
			}
			inner.report(LocalReport::Trashed { path: path.clone() });
			Confirmed::Done
		}
		Err(error) => veto_failure(&error).map_or(Confirmed::Done, Confirmed::Retry),
	}
}

/// Whether a rename that saves the file `stable` is not settled yet.
fn saving<D: Drive, L: LocalFs>(inner: &Inner<D, L>, stable: Uuid) -> bool {
	lock(&inner.state).rename_records.iter().any(|record| {
		!record.confirmed && record.save_over.is_some_and(|save| save.stable == stable)
	})
}

/// Trashes the file `stable`, a save's temporary file uploaded as a file of its own, whose bytes
/// the saved file takes as its new version: in a task of its own, tried again with backoff while
/// the drive fails, for as long as the engine runs. The trash is where it goes, never a permanent
/// delete, and the local file holds its bytes until that version is up.
pub(super) fn trash_transient<D: Drive, L: LocalFs>(inner: &Arc<Inner<D, L>>, stable: Uuid) {
	let task_inner = Arc::clone(inner);
	inner.spawn(async move {
		let mut attempts = 0_u32;
		loop {
			match task_inner.drive.trash(stable_id(stable)).await {
				Ok(_) => {
					debug!(%stable, "trashed a save's temporary file");
					return;
				}
				Err(error) if veto_failure(&error).is_none() => return,
				Err(error) => {
					attempts = attempts.saturating_add(1);
					warn!(%stable, error = %log_safe::error(&error), "trashing a save's temporary file failed; trying again");
					sleep(watch_backoff(attempts).unwrap_or(PUT_BACK_AGAIN_MAX)).await;
				}
			}
		}
	});
}

/// Answers a rename request: applied to the drive and allowed, or vetoed with why. The drive is
/// changed in a task of its own, so running out of time vetoes the rename without cancelling a
/// change half made: that task puts the drive back once it ends.
pub(super) async fn answer_rename<D: Drive, L: LocalFs, R: RenameResponder>(
	inner: &Arc<Inner<D, L>>,
	request: RenameRequest,
	responder: R,
) {
	let from = request.from().clone();
	let decision = Arc::new(RenameDecision::new());
	let (sender, decided) = oneshot::channel();
	{
		let task_inner = Arc::clone(inner);
		let decision = Arc::clone(&decision);
		inner.spawn(async move {
			let _ = sender.send(decide_rename(&task_inner, &request, &decision).await);
		});
	}
	let answer = match timeout(VETO_BUDGET, decided).await {
		Ok(Ok(answer)) => answer,
		// Not spawned, or ended unanswered: the provider is stopping.
		Ok(Err(_)) => Err(Failure::Transient),
		Err(_) => {
			let stage = decision.step(|stage| match stage {
				RenameStage::Deciding => RenameStage::Vetoed,
				other => other,
			});
			if stage == RenameStage::Allowed {
				Ok(())
			} else {
				Err(Failure::Unreachable)
			}
		}
	};
	match answer {
		Ok(()) => responder.allow(),
		Err(failure) => {
			inner.report(LocalReport::Vetoed {
				path: from,
				op: VetoedOp::Rename,
				failure,
			});
			responder.veto(failure);
		}
	}
}

async fn decide_rename<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	request: &RenameRequest,
	decision: &Arc<RenameDecision>,
) -> Result<(), Failure> {
	let from = request.from();
	let Ok(identity) = PlaceholderIdentity::decode(request.identity()) else {
		debug!(%from, "allowed the rename of a placeholder with an unknown identity");
		return Ok(());
	};
	match (identity, request.target()) {
		(PlaceholderIdentity::Root { .. }, _) => Err(Failure::InvalidRequest),
		(PlaceholderIdentity::Detached { .. }, RenameTarget::InRoot(to)) => {
			follow_locally(inner, from, to, identity);
			inner.push(to, Delay::Debounce);
			Ok(())
		}
		(PlaceholderIdentity::Detached { .. }, RenameTarget::OutOfRoot) => {
			lock(&inner.state).queue.remove_under(from);
			Ok(())
		}
		(_, RenameTarget::OutOfRoot) => approve_removal(inner, from, identity).await,
		(_, RenameTarget::InRoot(to)) => apply_rename(inner, from, to, identity, decision).await,
	}
}

/// Moves the core's own bookkeeping for `from` and what is under it to `to`.
fn follow_locally<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	from: &RootPath,
	to: &RootPath,
	identity: PlaceholderIdentity,
) {
	let now = Instant::now();
	let mut state = lock(&inner.state);
	let admit = state.queue.rebase(from, to, now);
	match identity {
		PlaceholderIdentity::File { stable, version } => {
			// What the placeholder held, whether or not a job looked at it: an editor saving
			// through a backup renames it away and puts the new file in its place.
			state.lineage.remember(from, stable, version, now);
			state.lineage.moved(from, to, now);
		}
		_ => state.lineage.rebase(from, to),
	}
	for at in state.uploading.values_mut() {
		if let Some(moved) = at.rebase(from, to) {
			*at = moved;
		}
	}
	drop(state);
	if admit == Admit::Overflow {
		inner.overflowed();
	}
}

/// The drive's id of the directory `parent` (a placeholder, or the root), creating an ordinary
/// directory on the drive first so an item can move into it. A directory so created stays even when
/// the rename is vetoed: it is the local directory's own, now in sync.
async fn target_parent<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	parent: &RootPath,
) -> Result<(FfiId, String), Failure> {
	if parent.is_root() {
		let root = inner
			.drive
			.root_uuid()
			.await
			.map_err(|error| veto_failure(&error).unwrap_or(Failure::NotFound))?;
		return Ok((FfiId(root.clone()), root));
	}
	for _ in 0..2 {
		match inner.inspect(parent).await {
			Ok(LocalEntry::Placeholder {
				kind: LocalKind::Dir { .. },
				identity: Ok(PlaceholderIdentity::Dir { uuid }),
				..
			}) => return Ok((stable_id(uuid), uuid.to_string())),
			Ok(LocalEntry::Plain {
				kind: LocalKind::Dir { created },
				id,
			}) => {
				// A new directory something is moved into: taken in first.
				match take_in_dir(inner, parent, id, created).await {
					Outcome::Done => continue,
					Outcome::Failed(failure) | Outcome::Park(failure) => return Err(failure),
					_ => return Err(Failure::Transient),
				}
			}
			_ => return Err(Failure::Transient),
		}
	}
	Err(Failure::Transient)
}

/// The drive's id of the parent whose uuid text is `uuid`, or why it cannot be named.
pub(super) async fn parent_id<D: Drive, L: LocalFs>(
	inner: &Inner<D, L>,
	uuid: &str,
) -> Result<FfiId, Failure> {
	let root = inner.drive.root_uuid().await.map_err(|error| {
		warn!(error = %log_safe::error(&error), "reading the root to name an item's parent failed");
		veto_failure(&error).unwrap_or(Failure::NotFound)
	})?;
	if same_uuid(&root, uuid) {
		return Ok(FfiId(root));
	}
	Uuid::parse_str(uuid).map(stable_id).map_err(|error| {
		warn!(uuid, %error, "an item's parent on the drive is not a uuid");
		Failure::Transient
	})
}

/// An item's parent uuid text and name as the drive holds it.
fn place_of(object: &FfiObject) -> Option<(String, Option<String>, String)> {
	match object {
		FfiObject::File(file) => Some((
			file.parent.clone(),
			file.meta.as_ref().map(|meta| meta.name.clone()),
			file.stable_uuid.clone(),
		)),
		FfiObject::Dir(dir) => Some((
			dir.parent.clone(),
			dir.meta.as_ref().map(|meta| meta.name.clone()),
			dir.uuid.clone(),
		)),
		FfiObject::Root(_) => None,
	}
}

/// Applies the rename of the item `identity` from `from` to `to` on the drive. Where the item was
/// is recorded before the first change, with `decision`, so a rename vetoed after any change is
/// put back.
async fn apply_rename<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	from: &RootPath,
	to: &RootPath,
	identity: PlaceholderIdentity,
	decision: &Arc<RenameDecision>,
) -> Result<(), Failure> {
	let (Some(name), Some(parent)) = (to.name(), to.parent()) else {
		return Err(Failure::InvalidRequest);
	};
	let name = ValidatedName::try_from(name.as_str()).map_err(|_| Failure::InvalidRequest)?;
	let Some(item) = identity.cache_id() else {
		return Ok(());
	};
	let (new_parent, new_parent_uuid) = target_parent(inner, &parent).await?;
	let current = match inner.drive.refresh_item(item.clone()).await {
		Ok(Some(current)) if !is_trashed(&current) => current,
		// Nothing on the drive to rename.
		Ok(_) => return Ok(()),
		Err(error) => return veto_failure(&error).map_or(Ok(()), Err),
	};
	let Some((current_parent, current_name, item_uuid)) = place_of(&current) else {
		return Err(Failure::InvalidRequest);
	};
	let moves = !same_uuid(&current_parent, &new_parent_uuid);
	let renames = current_name.as_deref() != Some(name.as_ref());
	let save_over = match identity {
		PlaceholderIdentity::File { stable, version } if !moves => {
			saved_by_rename(inner, from, to, stable, version).await
		}
		_ => None,
	};
	let mut displaced = None;
	if save_over.is_none()
		&& (moves
			|| current_name.as_deref().map(collision_key) != Some(collision_key(name.as_ref())))
	{
		let listing = match inner.drive.list_children(new_parent.clone()).await {
			Ok(Some(listing)) => listing,
			Ok(None) => return Err(Failure::Transient),
			Err(error) => return Err(veto_failure(&error).unwrap_or(Failure::Transient)),
		};
		if let Some(taken) = find_by_name(&listing, name.as_ref())
			.filter(|taken| !same_uuid(object_uuid(taken), &item_uuid))
		{
			// Never over another item, unless the rename replaces its clean placeholder: the user
			// picks another name.
			displaced = Some(
				replaced_by_rename(inner, to, taken, &new_parent)
					.await
					.ok_or(Failure::Transient)?,
			);
		}
	}
	if !moves && !renames {
		// The drive has it there already.
		follow_locally(inner, from, to, identity);
		inner.report(LocalReport::Moved {
			from: from.clone(),
			to: to.clone(),
		});
		inner.push(to, Delay::Debounce);
		return Ok(());
	}
	if decision.get() == RenameStage::Vetoed {
		// Out of time before anything changed: nothing to put back.
		return Err(Failure::Unreachable);
	}
	// Where to put the item back: known, and saved, before anything changes, or nothing changes.
	let old_parent = parent_id(inner, &current_parent).await?;
	let old_parent_uuid = Uuid::parse_str(&current_parent).map_err(|error| {
		warn!(from = %path_tag(from), %error, "an item's parent on the drive is not a uuid");
		Failure::Transient
	})?;
	let from_place = old_parent.clone();
	let record = inner
		.add_rename_record(RenameRecord {
			id: 0,
			identity,
			from: from.clone(),
			to: to.clone(),
			item: item.clone(),
			old_parent,
			old_parent_uuid,
			old_name: current_name.clone(),
			confirmed: false,
			displaced: displaced.clone(),
			save_over,
		})
		.await
		.map_err(|error| {
			warn!(from = %path_tag(from), %error, "saving a rename's record failed; vetoed");
			Failure::Transient
		})?;
	lock(&inner.state).renames.push(PendingRename {
		record,
		due: Instant::now() + RENAME_GRACE,
		decision: Arc::clone(decision),
		restored: false,
		attempts: 0,
	});
	if save_over.is_some() {
		// Nothing changes on the drive until the rename happened.
		let stage = decision.step(|stage| match stage {
			RenameStage::Deciding => RenameStage::Allowed,
			_ => RenameStage::Refused,
		});
		if stage == RenameStage::Refused {
			set_due(inner, decision, Instant::now());
			return Err(Failure::Unreachable);
		}
		follow_save_over(inner, from, to);
		set_due(inner, decision, Instant::now() + RENAME_GRACE);
		debug!(%from, %to, "a save's temporary file renamed into the place of the file it saves");
		return Ok(());
	}
	let place = Place {
		dir: &from_place,
		name: current_name.as_deref(),
	};
	let changed = match &displaced {
		Some(displaced) => displace(inner, displaced).await,
		None => Ok(()),
	};
	let changed = match changed {
		Ok(()) => {
			change_drive(
				inner,
				&item,
				identity,
				place,
				moves.then_some(new_parent),
				renames,
				&name,
			)
			.await
		}
		Err(failure) => Err(failure),
	};
	let stage = decision.step(|stage| match (stage, &changed) {
		(RenameStage::Deciding, Ok(())) => RenameStage::Allowed,
		_ => RenameStage::Refused,
	});
	if let Err(failure) = changed {
		// Nothing was answered yet: the item keeps its name, and its confirmation puts the drive
		// back at once.
		set_due(inner, decision, Instant::now());
		return Err(failure);
	}
	if stage == RenameStage::Refused {
		// Vetoed meanwhile: the confirmation puts the drive back, the item never having moved.
		set_due(inner, decision, Instant::now());
		return Err(Failure::Unreachable);
	}
	follow_locally(inner, from, to, identity);
	set_due(inner, decision, Instant::now() + RENAME_GRACE);
	inner.report(LocalReport::Moved {
		from: from.clone(),
		to: to.clone(),
	});
	// A rename clears the placeholder's in-sync state; its job marks it again.
	inner.push(to, Delay::Debounce);
	Ok(())
}

/// The file `taken` holds the rename's target name `to` in the directory `dir` on the drive: when
/// the local item at `to` is its clean placeholder, which the rename replaces (an editor renaming
/// the old file onto its backup name over the last backup, `MoveFileEx` with `REPLACE_EXISTING`),
/// the file to move out of the way; `None` otherwise, and the rename is vetoed. A placeholder in
/// sync with nothing written since, of the version the drive lists, holds nothing the drive lacks,
/// and the file, trashed once the rename happened, keeps its bytes in the trash. A newer version
/// the placeholder has not received, or an edit the cache holds for the file, is one the user never
/// saw replaced: the rename is vetoed.
async fn replaced_by_rename<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	to: &RootPath,
	taken: &FfiNonRootObject,
	dir: &FfiId,
) -> Option<Displaced> {
	let FfiNonRootObject::File(file) = taken else {
		return None;
	};
	if file.pending_upload_at.is_some() {
		return None;
	}
	let (stable, version) = file_ids(file)?;
	let name = file.meta.as_ref()?.name.clone();
	match inner.inspect(to).await {
		Ok(LocalEntry::Placeholder {
			kind: LocalKind::File(_),
			identity:
				Ok(PlaceholderIdentity::File {
					stable: held,
					version: held_version,
				}),
			state,
			..
		}) if held == stable
			&& held_version == version
			&& state.in_sync()
			&& state.modified_bytes() == 0 =>
		{
			Some(Displaced {
				stable,
				dir: dir.clone(),
				name,
			})
		}
		_ => None,
	}
}

/// The file the rename of the file `stable`, at the version `version`, from `from` to `to` saves,
/// when the rename puts a save's temporary file in that file's place (see the module's docs): the
/// file `stable` is one this run uploaded moments before from an ordinary file and still at that
/// version, all on disk, renamed within its directory, and `to` holds another file's placeholder
/// holding nothing the drive lacks, or claims a file that was there moments ago. A placeholder
/// there holding an edit has it uploaded first, as a version of its own, waited for a while;
/// one still holding it is no save, and the rename is decided as any other.
async fn saved_by_rename<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	from: &RootPath,
	to: &RootPath,
	stable: Uuid,
	version: Uuid,
) -> Option<SaveOver> {
	if from.parent() != to.parent()
		|| !lock(&inner.state)
			.fresh
			.is_fresh(stable, version, Instant::now())
	{
		return None;
	}
	match inner.inspect(from).await {
		Ok(LocalEntry::Placeholder {
			kind: LocalKind::File(stamp),
			identity: Ok(PlaceholderIdentity::File { stable: held, .. }),
			state,
			..
		}) if held == stable && state.on_disk() >= stamp.size => {}
		_ => return None,
	}
	let deadline = Instant::now() + EDIT_FIRST_WAIT;
	let mut asked = false;
	loop {
		match inner.inspect(to).await {
			Ok(LocalEntry::Placeholder {
				kind: LocalKind::File(stamp),
				identity:
					Ok(PlaceholderIdentity::File {
						stable: saved,
						version: saved_version,
					}),
				state,
				..
			}) if saved != stable => {
				if !may_hold_edit(state, stamp.size) {
					return Some(SaveOver {
						stable: saved,
						version: saved_version,
					});
				}
				if Instant::now() >= deadline {
					debug!(%to, "the edit a save renames over is not on the drive yet");
					return None;
				}
				if !asked {
					// Its job uploads it.
					inner.push(to, Delay::Now);
					asked = true;
				}
				sleep(EDIT_FIRST_POLL).await;
			}
			Ok(
				LocalEntry::Missing
				| LocalEntry::Plain {
					kind: LocalKind::File(_),
					..
				},
			) => break,
			_ => return None,
		}
	}
	let seen = lock(&inner.state)
		.lineage
		.get(to)
		.filter(|seen| !seen.unconverted && seen.stable != stable)
		.cloned()?;
	still_claims(inner, &seen).await.then_some(SaveOver {
		stable: seen.stable,
		version: seen.version,
	})
}

/// Moves the core's own bookkeeping for a save's temporary file renamed from `from` into the place
/// of the file it saves at `to`: what is queued and indexed at `from` goes to `to`, and `to` keeps
/// claiming the saved file.
fn follow_save_over<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	from: &RootPath,
	to: &RootPath,
) {
	let mut state = lock(&inner.state);
	let admit = state.queue.rebase(from, to, Instant::now());
	state.lineage.forget(from);
	state.index.rebase(from, to);
	drop(state);
	if admit == Admit::Overflow {
		inner.overflowed();
	}
}

/// What finishing a save's rename came to.
enum SaveTaken {
	/// The temporary file is an ordinary file claiming the saved file, and its upload is trashed.
	Done,
	/// Not yet: look again after a backoff.
	Again,
	/// The temporary file is not at the rename's target.
	NotThere,
}

/// What the temporary file at a save's target was found as.
enum Found {
	/// Its placeholder, now made an ordinary file.
	Reverted,
	/// An ordinary file already: made one before.
	Plain,
	/// Its placeholder, not all on disk: hydrated, to be made an ordinary file next.
	Hydrated,
	/// Something else.
	Other,
}

/// Finishes the rename `record` of a save's temporary file over or into the place of the file
/// `save`, once it happened: the temporary file's placeholder at the target becomes the ordinary
/// file it was before its upload, all its bytes on disk, the target claims `save`, and the
/// temporary upload is trashed. The job at the target then uploads the file as `save`'s new
/// version, as for any atomic save.
async fn take_save_over<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	record: &RenameRecord,
	save: SaveOver,
) -> SaveTaken {
	let PlaceholderIdentity::File {
		stable: temporary, ..
	} = record.identity
	else {
		return SaveTaken::NotThere;
	};
	let to = record.to.clone();
	let found = inner
		.fs(move |local| match local.inspect(&to)? {
			LocalEntry::Placeholder {
				kind: LocalKind::File(stamp),
				identity: Ok(PlaceholderIdentity::File { stable, .. }),
				id,
				state,
			} if stable == temporary => {
				if state.on_disk() < stamp.size {
					// An ordinary file holds only what is on disk.
					local.hydrate(&to, id)?;
					return Ok(Found::Hydrated);
				}
				local.revert_to_plain(&to, id)?;
				Ok(Found::Reverted)
			}
			LocalEntry::Plain {
				kind: LocalKind::File(_),
				..
			} => Ok(Found::Plain),
			_ => Ok(Found::Other),
		})
		.await;
	match found {
		Ok(Found::Reverted) => {}
		Ok(Found::Plain) => {
			// Made one before this look, unless the rename never happened and the temporary file's
			// placeholder is still where it was.
			if matches!(inner.inspect(&record.from).await, Ok(LocalEntry::Placeholder {
				identity: Ok(PlaceholderIdentity::File { stable, .. }),
				..
			}) if stable == temporary)
			{
				return SaveTaken::NotThere;
			}
		}
		Ok(Found::Hydrated) => return SaveTaken::Again,
		Ok(Found::Other) | Err(LocalError::NotFound) => return SaveTaken::NotThere,
		Err(error) => {
			debug!(to = %record.to, %error, "making a save's temporary file ordinary failed");
			return SaveTaken::Again;
		}
	}
	{
		let mut state = lock(&inner.state);
		if state
			.lineage
			.get(&record.to)
			.is_none_or(|seen| seen.stable != save.stable)
		{
			state
				.lineage
				.remember(&record.to, save.stable, save.version, Instant::now());
		}
		state.index.remove_id(temporary);
	}
	match inner.drive.trash(stable_id(temporary)).await {
		Ok(_) => debug!(to = %record.to, "trashed a save's temporary file"),
		Err(error) if veto_failure(&error).is_none() => {}
		Err(error) => {
			warn!(to = %path_tag(&record.to), error = %log_safe::error(&error), "trashing a save's temporary file failed; trying again");
			return SaveTaken::Again;
		}
	}
	SaveTaken::Done
}

/// Confirms the rename `pending` of a save's temporary file into the place of the file `save`
/// ([`take_save_over`]), tried again with backoff until it is done. One that never happened, as
/// the drive was not changed for it, only goes.
async fn confirm_save_over<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	pending: PendingRename,
	save: SaveOver,
) {
	let record = pending.record.clone();
	let allowed = pending.decision.get() != RenameStage::Refused;
	if allowed || pending.restored {
		match take_save_over(inner, &record, save).await {
			SaveTaken::Done => {
				inner.confirm_rename_record(record.id);
				settled_at(inner, &record);
				return;
			}
			SaveTaken::Again => {
				again(inner, pending);
				return;
			}
			SaveTaken::NotThere => {}
		}
	}
	inner.drop_rename_record(record.id).await;
	settled_at(inner, &record);
	let PlaceholderIdentity::File { stable, version } = record.identity else {
		return;
	};
	let at_old_path = matches!(inner.inspect(&record.from).await, Ok(LocalEntry::Placeholder {
		identity: Ok(PlaceholderIdentity::File { stable: held, .. }),
		..
	}) if held == stable);
	if allowed && at_old_path {
		{
			let now = Instant::now();
			let mut state = lock(&inner.state);
			let admit = state.queue.rebase(&record.to, &record.from, now);
			state.index.rebase(&record.to, &record.from);
			state.lineage.remember(&record.from, stable, version, now);
			drop(state);
			if admit == Admit::Overflow {
				inner.overflowed();
			}
		}
		inner.report(LocalReport::Undone {
			path: record.from.clone(),
			op: UndoneOp::Rename,
		});
	}
}

/// Moves the file `displaced` out of a rename's way, to its transit name.
async fn displace<D: Drive, L: LocalFs>(
	inner: &Inner<D, L>,
	displaced: &Displaced,
) -> Result<(), Failure> {
	inner
		.drive
		.rename(stable_id(displaced.stable), transit_name(displaced.stable))
		.await
		.map(|_| ())
		.map_err(|error| veto_failure(&error).unwrap_or(Failure::NotFound))
}

/// Trashes the file a rename that happened locally replaced: whether it is done, or gone already.
async fn trash_displaced<D: Drive, L: LocalFs>(inner: &Inner<D, L>, displaced: &Displaced) -> bool {
	match inner.drive.trash(stable_id(displaced.stable)).await {
		Ok(_) => {
			debug!(name = %displaced.name, "trashed the file a rename replaced");
			true
		}
		Err(error) => {
			let gone = veto_failure(&error).is_none();
			if !gone {
				warn!(error = %log_safe::error(&error), stable = %displaced.stable, "trashing the file a rename replaced failed");
			}
			gone
		}
	}
}

/// Gives the file a put-back rename had moved out of its way its name back: whether it is done,
/// or cannot be (gone). When another file holds the name now, it gets a free `.old` name beside it
/// instead: under its transit name it would get no placeholder, and the user's file would look
/// gone. `false` leaves it under its transit name for now, to be tried again.
async fn restore_displaced<D: Drive, L: LocalFs>(
	inner: &Inner<D, L>,
	displaced: &Displaced,
) -> bool {
	let gone = |error: &filen_mobile_native_cache::CacheError| {
		let gone = veto_failure(error).is_none();
		if !gone {
			warn!(error = %log_safe::error(&error), stable = %displaced.stable, "giving a file moved out of a rename's way its name back failed");
		}
		gone
	};
	let transit = transit_name(displaced.stable);
	let from = Place {
		dir: &displaced.dir,
		name: Some(&transit),
	};
	let names = std::iter::once(displaced.name.clone())
		.chain(aside_names(&displaced.name).take(DISPLACED_ASIDE_ATTEMPTS));
	for name in names {
		match relocate_file(inner.drive.as_ref(), displaced.stable, from, None, &name).await {
			Ok(Relocated::Done) => {
				if name != displaced.name {
					debug!(stable = %displaced.stable, name = %displaced.name, aside = %name, "the names of a file moved out of a rename's way");
					warn!(stable = %displaced.stable, "another file took the name of a file moved out of a rename's way; it has a name beside it");
				}
				return true;
			}
			Ok(Relocated::Taken) => {}
			Err(error) => return gone(&error),
		}
	}
	warn!(stable = %displaced.stable, "no free name for a file moved out of a rename's way; trying again later");
	false
}

/// Moves the item `item` (`identity`), now at `from`, into `new_parent`, when given, and renames it
/// to `name`, when `renames`, or says why the rename is vetoed. A file never lands over another
/// file ([`relocate_file`]): one whose new name is taken is back where it was, and vetoed.
async fn change_drive<D: Drive, L: LocalFs>(
	inner: &Inner<D, L>,
	item: &FfiId,
	identity: PlaceholderIdentity,
	from: Place<'_>,
	new_parent: Option<FfiId>,
	renames: bool,
	name: &ValidatedName,
) -> Result<(), Failure> {
	let refused = |error: &filen_mobile_native_cache::CacheError| {
		veto_failure(error).unwrap_or(Failure::NotFound)
	};
	if let PlaceholderIdentity::File { stable, .. } = identity {
		return match relocate_file(
			inner.drive.as_ref(),
			stable,
			from,
			new_parent.as_ref(),
			name.as_ref(),
		)
		.await
		{
			Ok(Relocated::Done) => Ok(()),
			// Never over another item: the user picks another name.
			Ok(Relocated::Taken) => Err(Failure::Transient),
			Err(error) => Err(refused(&error)),
		};
	}
	if let Some(new_parent) = new_parent {
		inner
			.drive
			.move_to(item.clone(), new_parent)
			.await
			.map_err(|error| refused(&error))?;
	}
	if renames {
		inner
			.drive
			.rename(item.clone(), name.as_ref().to_owned())
			.await
			.map_err(|error| refused(&error))?;
	}
	Ok(())
}

/// Makes the rename `decision` stands for due at `due`, and wakes the scheduler.
fn set_due<D: Drive, L: LocalFs>(
	inner: &Inner<D, L>,
	decision: &Arc<RenameDecision>,
	due: Instant,
) {
	for pending in &mut lock(&inner.state).renames {
		if Arc::ptr_eq(&pending.decision, decision) {
			pending.due = due;
		}
	}
	inner.wake.notify_one();
}

/// How a put-back ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PutBack {
	/// The item is where it was, or the drive holds it no more, or in its trash.
	Back,
	/// The item stays where the drive has it, for good: another device moved or renamed it since,
	/// or its old place cannot take it any more (another item holds its old name there, or its old
	/// directory is gone). The remote side brings the placeholder to it.
	Left,
	/// It failed and may pass later: the drive is out of reach, or did not answer.
	Again,
}

/// Puts the item `record` names back where it was on the drive, changing only what differs, and
/// never over another file ([`relocate_file`]).
async fn put_back<D: Drive, L: LocalFs>(inner: &Inner<D, L>, record: &RenameRecord) -> PutBack {
	let item = record.item.clone();
	let current = match inner.drive.refresh_item(item.clone()).await {
		Ok(Some(current)) if !is_trashed(&current) => current,
		Ok(_) => return PutBack::Back,
		Err(error) => {
			warn!(from = %path_tag(&record.from), error = %log_safe::error(&error), "reading an item to put it back failed");
			return PutBack::Again;
		}
	};
	let Some((parent, name, _)) = place_of(&current) else {
		return PutBack::Back;
	};
	if !as_the_rename_left_it(inner, record, &parent, name.as_deref()).await {
		debug!(from = %record.from, "an item changed on the drive since its rename; left there");
		return PutBack::Left;
	}
	let moves = Uuid::parse_str(&parent).ok() != Some(record.old_parent_uuid);
	let old_parent = record.old_parent.clone();
	let changed = match (
		record.identity,
		record.old_name.as_deref().or(name.as_deref()),
	) {
		(PlaceholderIdentity::File { stable, .. }, Some(old_name)) => {
			let Ok(here) = parent_id(inner, &parent).await else {
				return PutBack::Again;
			};
			// Stopped in the middle of its move, it goes back, should its old name be taken, under
			// the name the rename gave it rather than its transit name.
			let transit = transit_name(stable);
			let to_name = record.to.name();
			let left_as = match name.as_deref() {
				Some(name) if name == transit => to_name.as_ref().map(|to| to.as_str()),
				name => name,
			};
			let from = Place {
				dir: &here,
				name: left_as,
			};
			// Boxed: awaited from the engine's stop, inside the helper's own future, whose layout
			// would otherwise outgrow the compiler's query depth limit.
			match Box::pin(relocate_file(
				inner.drive.as_ref(),
				stable,
				from,
				moves.then_some(&old_parent),
				old_name,
			))
			.await
			{
				Ok(Relocated::Done) => Ok(()),
				Ok(Relocated::Taken) => {
					warn!(from = %path_tag(&record.from), "another file holds a renamed file's old name; it stays renamed");
					if !moves
						&& name.as_deref() == Some(transit.as_str())
						&& let Some(left_as) = left_as
						&& let Err(error) = inner.drive.rename(item, left_as.to_owned()).await
					{
						warn!(from = %path_tag(&record.from), error = %log_safe::error(&error), "naming a file left in transit failed");
						return PutBack::Again;
					}
					return PutBack::Left;
				}
				Err(error) => Err(error),
			}
		}
		_ => put_back_in_place(inner, &item, moves.then_some(old_parent), record, name).await,
	};
	match changed {
		Ok(()) => PutBack::Back,
		Err(error) => {
			warn!(from = %path_tag(&record.from), error = %log_safe::error(&error), "putting an item back failed");
			if old_parent_gone(inner, record).await {
				warn!(from = %path_tag(&record.from), "a renamed item's old directory is gone; it stays renamed");
				PutBack::Left
			} else {
				PutBack::Again
			}
		}
	}
}

/// Moves the item `item`, a directory or a file whose names are unknown, into `old_parent`, when
/// given, and renames it to `record`'s old name, when it has another.
async fn put_back_in_place<D: Drive, L: LocalFs>(
	inner: &Inner<D, L>,
	item: &FfiId,
	old_parent: Option<FfiId>,
	record: &RenameRecord,
	name: Option<String>,
) -> Result<(), filen_mobile_native_cache::CacheError> {
	if let Some(old_parent) = old_parent {
		inner.drive.move_to(item.clone(), old_parent).await?;
	}
	if let Some(old_name) = &record.old_name
		&& name.as_ref() != Some(old_name)
	{
		inner.drive.rename(item.clone(), old_name.clone()).await?;
	}
	Ok(())
}

/// Whether the item `record` names is on the drive in `parent` as `name` the way the rename, or a
/// put-back of it under way, would leave it: in its old directory or the rename's, under its old
/// name, the rename's or its transit name. Anything else, another device moved or renamed it
/// since, and putting it back would undo that. A directory that cannot be read counts as one.
async fn as_the_rename_left_it<D: Drive, L: LocalFs>(
	inner: &Inner<D, L>,
	record: &RenameRecord,
	parent: &str,
	name: Option<&str>,
) -> bool {
	let in_old_parent = Uuid::parse_str(parent).ok() == Some(record.old_parent_uuid);
	let in_new_parent = match record.to.parent() {
		Some(dir) => match drive_dir_of(inner, &dir).await {
			Some(uuid) => same_uuid(parent, &uuid),
			None => true,
		},
		None => false,
	};
	let transit = match record.identity {
		PlaceholderIdentity::File { stable, .. } => Some(transit_name(stable)),
		_ => None,
	};
	let named = |candidate: Option<&str>| {
		candidate
			.zip(name)
			.is_some_and(|(candidate, name)| collision_key(candidate) == collision_key(name))
	};
	let name_as_left = name.is_none()
		|| named(record.old_name.as_deref())
		|| named(record.to.name().as_ref().map(|to| to.as_str()))
		|| named(transit.as_deref());
	(in_old_parent || in_new_parent) && name_as_left
}

/// The uuid text of the drive's directory the local directory `dir` stands for: the root's, or its
/// placeholder's; `None` when it cannot be read.
async fn drive_dir_of<D: Drive, L: LocalFs>(inner: &Inner<D, L>, dir: &RootPath) -> Option<String> {
	if dir.is_root() {
		return inner.drive.root_uuid().await.ok();
	}
	match inner.inspect(dir).await {
		Ok(LocalEntry::Placeholder {
			identity: Ok(PlaceholderIdentity::Dir { uuid }),
			..
		}) => Some(uuid.to_string()),
		_ => None,
	}
}

/// Whether the directory a renamed item was in is gone from the drive, or in its trash: then the
/// item can never be put back there.
async fn old_parent_gone<D: Drive, L: LocalFs>(inner: &Inner<D, L>, record: &RenameRecord) -> bool {
	match inner.drive.refresh_item(record.old_parent.clone()).await {
		Ok(Some(parent)) => is_trashed(&parent),
		Ok(None) => true,
		Err(_) => false,
	}
}

/// The rename a previous run saved the record of and never settled, to put back: its request was
/// vetoed when that run stopped, unless the rename was allowed and happened, which its
/// confirmation finds.
pub(super) fn restored_rename(record: RenameRecord) -> PendingRename {
	PendingRename {
		record,
		due: Instant::now(),
		decision: Arc::new(RenameDecision::refused()),
		restored: true,
		attempts: 0,
	}
}

/// Confirms a rename: allowed, when the item still sits at its old path the local rename failed
/// after it was allowed, and the drive is put back; refused, the drive is put back, unless the
/// rename was read back from a previous run and happened locally. A put-back that fails is tried
/// again with backoff; the record stays until it is done.
pub(super) async fn confirm_rename<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	mut pending: PendingRename,
) {
	if let Some(save) = pending.record.save_over {
		confirm_save_over(inner, pending, save).await;
		return;
	}
	let record = pending.record.clone();
	let holds = |entry: &Result<LocalEntry, _>| {
		matches!(entry, Ok(LocalEntry::Placeholder { identity: Ok(identity), .. })
			if same_item(*identity, record.identity))
	};
	let allowed = pending.decision.get() != RenameStage::Refused;
	if allowed || pending.restored {
		let at_target = holds(&inner.inspect(&record.to).await);
		if at_target || allowed && !holds(&inner.inspect(&record.from).await) {
			if let Some(displaced) = &record.displaced
				&& !trash_displaced(inner, displaced).await
			{
				again(inner, pending);
				return;
			}
			// Done as the drive has it: the record goes once the lineage saved after it holds
			// where the item went.
			inner.confirm_rename_record(record.id);
			settled_at(inner, &record);
			return;
		}
	}
	let put = match put_back(inner, &record).await {
		PutBack::Again => PutBack::Again,
		put => match &record.displaced {
			Some(displaced) if !restore_displaced(inner, displaced).await => PutBack::Again,
			_ => put,
		},
	};
	match put {
		PutBack::Back => {}
		PutBack::Left => {
			inner.drop_rename_record(record.id).await;
			settled_at(inner, &record);
			return;
		}
		PutBack::Again => {
			// For as long as it takes: the drive holds a rename the user was refused. Reported once
			// it failed as often as a change that parks, as the drive and the sync root disagree
			// meanwhile.
			pending.attempts = pending.attempts.saturating_add(1);
			pending.due =
				Instant::now() + watch_backoff(pending.attempts).unwrap_or(PUT_BACK_AGAIN_MAX);
			let report = (pending.attempts == PARK_AFTER).then(|| LocalReport::Parked {
				path: record.from.clone(),
				failure: Failure::Transient,
				attempts: pending.attempts,
			});
			lock(&inner.state).renames.push(pending);
			inner.wake.notify_one();
			if let Some(report) = report {
				inner.report(report);
			}
			return;
		}
	}
	inner.drop_rename_record(record.id).await;
	settled_at(inner, &record);
	if allowed {
		follow_locally(inner, &record.to, &record.from, record.identity);
		inner.report(LocalReport::Undone {
			path: record.from.clone(),
			op: UndoneOp::Rename,
		});
	}
}

/// Looks at the target of `record`, a rename now settled: nothing read it while it was not, a walk
/// included, and a rename read back from a previous run queued no job there.
fn settled_at<D: Drive, L: LocalFs>(inner: &Arc<Inner<D, L>>, record: &RenameRecord) {
	inner.push(&record.to, Delay::Now);
}

/// Tries `pending` again after the backoff its failures so far give: a file a rename replaced is
/// trashed for as long as it takes, as the drive holds it out of the way under its transit name
/// meanwhile.
fn again<D: Drive, L: LocalFs>(inner: &Arc<Inner<D, L>>, mut pending: PendingRename) {
	pending.attempts = pending.attempts.saturating_add(1);
	pending.due = Instant::now() + watch_backoff(pending.attempts).unwrap_or(PUT_BACK_AGAIN_MAX);
	lock(&inner.state).renames.push(pending);
	inner.wake.notify_one();
}

/// Puts back every rename the drive was being changed for when the provider stopped: their
/// requests were vetoed as their responders dropped. One not put back is put back on the next
/// start, from its record.
pub(super) async fn put_back_interrupted<D: Drive, L: LocalFs>(inner: &Inner<D, L>) {
	let interrupted: Vec<RenameRecord> = lock(&inner.state)
		.renames
		.iter()
		.filter(|pending| pending.interrupted())
		.map(|pending| pending.record.clone())
		.collect();
	for record in interrupted {
		if record.save_over.is_some() {
			// The drive was not changed for it.
			inner.drop_rename_record(record.id).await;
			continue;
		}
		match put_back(inner, &record).await {
			PutBack::Back | PutBack::Left => {
				let restored = match &record.displaced {
					Some(displaced) => restore_displaced(inner, displaced).await,
					None => true,
				};
				if restored {
					inner.drop_rename_record(record.id).await;
				}
			}
			PutBack::Again => {}
		}
	}
}

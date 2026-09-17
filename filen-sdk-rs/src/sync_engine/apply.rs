//! The apply layer: execute a screened [`SyncAction`] plan against the remote (via `Client`) and
//! the local filesystem, advancing the baseline after each action so an interrupted pass simply
//! re-reconciles to the same plan.
//!
//! Each action is best-effort: a failure is recorded in the [`SyncReport`] and the pass continues,
//! since the actions are independent and the next pass re-plans from truth. The exception is a
//! failure no transfer to that side can get past — a full local disk or a full account — which
//! stops the pass starting any more of those (see [`SyncReport::halted`]). Local deletions move
//! to the pair's quarantine dir (recoverable); remote deletions go to Filen trash. The local mtime
//! written to the baseline after a download is read back from disk (not the value we asked for) so
//! the scanner's fast-path stays stable.

use std::{
	collections::{BTreeMap, BTreeSet, HashMap, HashSet},
	path::{Path, PathBuf},
	sync::{
		Arc, PoisonError,
		atomic::{AtomicBool, Ordering},
	},
};

use chrono::{DateTime, Utc};
use filen_types::{crypto::Blake3Hash, fs::StableUuid};
use futures::{StreamExt, stream::FuturesUnordered};
use uuid::Uuid;

use super::{
	baseline::{BaselineChange, BaselineEntry, BaselineState, BaselineStore, NodeKind, PairId},
	engine::{LockBudget, Observations, PASS_LOCK_MAX_SLEEP, PendingKind, PendingWrites},
	events::SyncEvent,
	guard::GuardReason,
	ignore::IgnoredPath,
	outcome::{
		HaltReason, PlannedAction, PlannedActionKind, PlannedConflict, PlannedNodeKind,
		RefuseReason, UnsyncablePath,
	},
	pause::PassGate,
	plan::{RemoteNode, SyncAction, create_target_paths, is_under},
	scan::{LocalNode, QUARANTINE_DIR, collision_key},
};
use crate::{
	auth::Client,
	cache::SearchResult,
	fs::{
		HasUUID,
		categories::{DirType, Normal},
		dir::{cache::CacheableDir, meta::DirectoryMetaChanges},
		file::{cache::CacheableFile, meta::FileMetaChanges},
	},
	io::{RemoteDirectory, RemoteFile, client_impl::IoSharedClientExt},
	sync::lock::{ATTEMPTS_DEFAULT, ResourceLock},
	util::{MaybeArc, MaybeSendCallback},
};
use tokio::{
	sync::{Mutex, mpsc::UnboundedSender},
	time::Instant,
};

/// Outcome of one apply pass.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SyncReport {
	pub downloaded: usize,
	pub uploaded: usize,
	pub local_dirs_created: usize,
	pub remote_dirs_created: usize,
	pub locally_deleted: usize,
	pub remotely_trashed: usize,
	/// Items re-parented/renamed on the remote in place of a re-upload — a directory counts once,
	/// whatever its subtree holds.
	pub moved_remote: usize,
	/// Items renamed/moved locally in place of a re-download — a directory counts once, whatever its
	/// subtree holds.
	pub moved_local: usize,
	/// Paths surfaced as two-way conflicts (left untouched), with what each side held.
	pub conflicts: Vec<PlannedConflict>,
	/// What the mass-delete guard held back this pass — the deletions, plus the create half of a
	/// held type flip (which is only meaningful together with its delete). Use
	/// [`held_deletions`](Self::held_deletions) for the deletion count alone.
	pub held: Vec<PlannedAction>,
	/// Paths the engine will not act on at all, and why. Reported on every pass the condition
	/// holds, so a caller always sees the current set rather than having to remember past ones.
	pub unsyncable: Vec<UnsyncablePath>,
	/// The top of each subtree the ignore rules hide, on either side, reported on every pass the rule
	/// stands. One only the built-in defaults hide is left out unless this pass stopped tracking it.
	pub ignored: Vec<IgnoredPath>,
	/// Set when the pass refused to run: nothing was applied.
	pub refused: Option<RefuseReason>,
	/// How many paths the pass deliberately left alone rather than acting on: a name the cache is
	/// listing twice (so the view cannot resolve it), or a remote deletion this engine has already
	/// made. Such a pass does nothing there on purpose, which is not the same as having nothing
	/// to do.
	pub deferred_paths: usize,
	/// Set when the guard held deletions: why it did.
	pub guard: Option<GuardReason>,
	/// Set when the guard held deletions: the token identifying THIS held batch, to hand back to
	/// [`SyncEngine::approve_deletions`](super::SyncEngine::approve_deletions). It changes if the
	/// batch changes, so an approval can never leak onto a different set of deletions.
	pub deletion_token: Option<String>,
	/// Per-action failures (the pass continues past them; one that ran out of room also holds back
	/// the transfers behind it, see [`halted`](Self::halted)).
	pub errors: Vec<String>,
	/// The pair is PAUSED (see [`SyncEngine::pause_pair`](super::SyncEngine::pause_pair)): the pass
	/// planned nothing, wrote no baseline row and applied nothing, so every field above is at its
	/// zero value. Not the same as a pass that ran and found nothing to do.
	///
	/// Two shapes report the same way. A pass that never started — the pair was already paused when
	/// it was asked for — read neither side. A pass a cancel dropped while it was still READING the
	/// two sides may have got as far as finishing its local scan and retiring the journal records
	/// that scan settled; it had no plan, so there was nothing to count as
	/// [`interrupted`](Self::interrupted) and nothing to undo.
	pub paused: bool,
	/// How many planned actions this pass did NOT carry out because the pair was paused with
	/// [`PauseMode::Cancel`](super::PauseMode::Cancel) while it ran — the transfer dropped in
	/// flight plus everything queued behind it — or because the pass could not take the drive lock,
	/// in which case it is every planned action and `errors` says why. Those actions recorded
	/// nothing — a row is written only after its action succeeded, and a transfer that succeeded is
	/// never dropped before its row — and the next pass re-plans them. Zero on a pass that ran to
	/// the end, including one that was merely SUSPENDED and resumed.
	pub interrupted: usize,
	/// Set when the pass could not take the drive lock, so it applied nothing. The report is still
	/// returned (conflicts and held deletions were recorded before the lock was asked for), but a
	/// watch loop must count the pass as failed rather than healthy. The error itself is the entry
	/// in `errors` that starts with [`LOCK_FAILURE`].
	pub(super) lock_failed: bool,
	/// Set when a side ran out of room during the pass: the local disk or the account, whichever
	/// was found full first. The action that ran into it — and any transfer already running that
	/// did too — is in [`errors`](Self::errors) but counts against no path's failure streak, since
	/// the path itself is not what is broken. Each side found full has its own line there, so a
	/// pass that fills both says so.
	///
	/// From then on the pass starts no further transfer that WRITES to a full side (see
	/// [`HaltReason::held_transfers`]): no upload once the account is full, no download once the
	/// disk is. Everything else still runs — the other direction's transfers, directory creates,
	/// moves and deletions — so a full account does not stop a two-way pair pulling the other
	/// devices' changes, and deletions that free room are not held behind the transfers that need
	/// it. [`interrupted`](Self::interrupted) does not count the held transfers: the next pass
	/// re-plans them. A watch treats such a pass as a failed one and backs off before the next.
	pub halted: Option<HaltReason>,
	/// The `(rel_path, error)` of every action that failed, for the engine's per-path failure
	/// bookkeeping. `errors` is the human-facing rendering of the same failures plus the pass-level
	/// ones (a refusal, a lock that could not be taken) that belong to no path.
	pub(super) failed_paths: Vec<(String, String)>,
}

impl SyncReport {
	/// How many deletions the mass-delete guard held back this pass.
	pub fn held_deletions(&self) -> usize {
		self.held
			.iter()
			.filter(|action| {
				matches!(
					action.kind,
					PlannedActionKind::TrashRemote | PlannedActionKind::DeleteLocal
				)
			})
			.count()
	}

	/// Just the paths of [`conflicts`](Self::conflicts) — what
	/// [`SyncEngine::resolve_conflict`](super::SyncEngine::resolve_conflict) takes.
	pub fn conflict_paths(&self) -> impl Iterator<Item = &str> {
		self.conflicts.iter().map(|c| c.rel_path.as_str())
	}
}

/// Everything the apply pass needs besides the actions: the resolved remote-side objects and the
/// scan/view maps.
pub(super) struct ApplyContext<'a> {
	pub(super) client: &'a Client,
	pub(super) local_root: &'a Path,
	pub(super) pair: PairId,
	/// The pair's mode. Read only where the direction changes what an action MEANS — a two-way
	/// upload has a divergence to surface where a one-way one has an authoritative side.
	pub(super) mode: super::SyncMode,
	pub(super) store: &'a Mutex<BaselineStore>,
	pub(super) local: &'a HashMap<String, LocalNode>,
	/// The pair's baseline as of the start of the pass — what the local side is expected to hold.
	pub(super) baseline: &'a HashMap<String, BaselineEntry>,
	pub(super) remote: &'a HashMap<String, RemoteNode>,
	/// The sync root resolved to a remote directory (every top-level parent).
	pub(super) root_remote: RemoteDirectory,
	/// Whether this pass's absence evidence is trustworthy (see
	/// [`ScreenState::absence_trusted`](super::guard::ScreenState::absence_trusted)) — gates
	/// dropping a baseline row because both sides look gone.
	pub(super) absence_trusted: bool,
	/// Where each remote uuid this pass writes is recorded, so the NEXT pass does not mistake a
	/// cache that has not caught up for a remote-side deletion.
	pub(super) pending: &'a PendingWrites,
	/// The uuids the cache has announced, stamped — read when a pending write is recorded so the
	/// write only retires on an announcement that came AFTER it.
	pub(super) observed: &'a Observations,
	/// The pair's pause checkpoint: consulted before every action, and raced against every
	/// transfer (see [`PassGate`]).
	pub(super) gate: &'a PassGate,
	/// The bounds of each hold of the drive-write lock (see [`Lease`]).
	pub(super) lock_budget: LockBudget,
}

fn local_path(root: &Path, rel_path: &str) -> PathBuf {
	rel_path
		.split('/')
		.fold(root.to_path_buf(), |p, c| p.join(c))
}

/// Resolve a local WRITE target under `root`, confined to the sync root. Rejects a `rel_path` with
/// a `.`/`..`/empty component (lexical traversal) and — to defeat a symlink in the tree that points
/// outside — canonicalizes the deepest existing ancestor and verifies it stays under the
/// canonicalized root. Without this, downloading `link/x` where `link` is a symlink to `/etc` would
/// write outside the sync folder. Reads/quarantine moves are unaffected; only writes are confined.
fn confined_local_target(root: &Path, rel_path: &str) -> Result<PathBuf, crate::Error> {
	if rel_path
		.split('/')
		.any(|c| c.is_empty() || c == "." || c == "..")
	{
		return Err(internal_owned(format!(
			"refusing an unsafe relative path: {rel_path:?}"
		)));
	}
	let target = local_path(root, rel_path);
	let root_canon = std::fs::canonicalize(root).map_err(io_err)?;
	// The target itself may not exist yet; canonicalize the deepest existing ancestor (which
	// resolves any symlink in the chain) and require it to stay under the root.
	let mut ancestor: &Path = &target;
	let existing = loop {
		if ancestor.exists() {
			break ancestor;
		}
		match ancestor.parent() {
			Some(parent) => ancestor = parent,
			None => break ancestor,
		}
	};
	let existing_canon = std::fs::canonicalize(existing).map_err(io_err)?;
	if !existing_canon.starts_with(&root_canon) {
		return Err(internal_owned(format!(
			"refusing to write outside the sync root via {rel_path:?}"
		)));
	}
	Ok(target)
}

/// Rename the local file at `rel_path` aside to `<stem>.old.<ext>` (`<stem>.old.N.<ext>` when that
/// name is taken), returning the new relative path. This is how a keep-BOTH conflict resolution
/// preserves the losing local copy: under its new name it has no baseline row, so the next pass
/// treats it as an ordinary new file and uploads it. Source and destination are both confined to
/// the sync root.
///
/// `None` if there is nothing at `rel_path` any more — the copy the caller meant to keep was
/// deleted since the conflict was recorded, so there is nothing to move and nothing to fail over.
pub(super) fn rename_aside(root: &Path, rel_path: &str) -> Result<Option<String>, crate::Error> {
	let source = confined_local_target(root, rel_path)?;
	if source.symlink_metadata().is_err() {
		return Ok(None);
	}
	let (rel, dest) = aside_target(root, rel_path)?;
	std::fs::rename(&source, &dest).map_err(io_err)?;
	Ok(Some(rel))
}

/// A free `<stem>.old.<ext>` name beside `rel_path` (`<stem>.old.N.<ext>` when that is taken), with
/// its confined local path. What [`rename_aside`] moves the losing copy to, and where a keep-both
/// resolution of a buried version downloads it.
pub(super) fn aside_target(root: &Path, rel_path: &str) -> Result<(String, PathBuf), crate::Error> {
	let (parent, name) = parent_and_name(rel_path);
	let (stem, ext) = match name.rsplit_once('.') {
		Some((stem, ext)) if !stem.is_empty() => (stem, Some(ext)),
		_ => (name, None),
	};
	for n in 0..100_000u32 {
		let ordinal = if n == 0 {
			String::new()
		} else {
			format!(".{n}")
		};
		let candidate = match ext {
			Some(ext) => format!("{stem}.old{ordinal}.{ext}"),
			None => format!("{stem}.old{ordinal}"),
		};
		let rel = if parent.is_empty() {
			candidate
		} else {
			format!("{parent}/{candidate}")
		};
		let dest = confined_local_target(root, &rel)?;
		if dest.symlink_metadata().is_err() {
			return Ok((rel, dest));
		}
	}
	Err(internal_owned(format!(
		"no free `.old` name is available to keep the local copy of {rel_path:?} aside"
	)))
}

fn parent_and_name(rel_path: &str) -> (&str, &str) {
	match rel_path.rsplit_once('/') {
		Some((parent, name)) => (parent, name),
		None => ("", rel_path),
	}
}

fn millis_to_dt(millis: i64) -> DateTime<Utc> {
	DateTime::from_timestamp_millis(millis).unwrap_or_else(Utc::now)
}

/// Execute `actions` (already ordered + guard-screened) against the remote and local tree.
///
/// The drive-write lock is held per bounded BATCH of remote writes (see [`Lease`]) so the per-op
/// `lock_drive` calls inside upload/create become free clones of the held lock instead of each
/// doing an acquire+release server round-trip — the dominant per-file cost at scale — while no other
/// device of the account waits on it for longer than about [`PASS_LOCK_MAX_HOLD`](super::engine::PASS_LOCK_MAX_HOLD). It is let go
/// only between actions that do not depend on one another (see [`release_points`]), once no remote
/// write is in flight, and taken again before the next remote write; a pure pull never takes it.
///
/// File transfers (uploads/downloads) — the bulk of the work and independent of one another — run
/// CONCURRENTLY, bounded by the client's configured concurrency ([`ClientConfig::with_concurrency`],
/// default 16) and the global request rate limiter. Directory creates/moves (which populate the
/// parent map and have parent-before-child ordering) and deletes (descendant-first for cascade
/// safety) stay serial.
///
/// Each action emits its [`SyncEvent`] to `observer` (and an [`ActionFailed`](SyncEvent::ActionFailed)
/// if it errors): serial actions when they start, concurrent transfers as each completes.
///
/// Pausing the pair reaches into this loop through [`ApplyContext::gate`]: a SUSPENDED pass parks
/// before its next action — and before taking the drive lock — while a transfer already running
/// finishes, since the transfer paths poll no pause signal of their own; a CANCELLED one drops the
/// transfers in flight and skips everything left, counting them in [`SyncReport::interrupted`]. A
/// transfer that got as far as FINISHING is never dropped between its network op and the row that
/// records it (see [`apply_transfer`]), so the two always agree.
pub(super) async fn apply(
	ctx: ApplyContext<'_>,
	actions: Vec<SyncAction>,
	report: &mut SyncReport,
	observer: &mut (dyn FnMut(SyncEvent) + Send),
) {
	// uuid -> the remote file object, for every file this plan acts on: hydrated from the cache
	// by [`resolve_folded_objects`], and from the server for what this engine wrote and the cache
	// has not listed yet.
	let mut files: HashMap<Uuid, RemoteFile> = HashMap::new();
	// path -> remote directory, for resolving the parent of an item. Seeded with the root at "";
	// [`resolve_folded_objects`] adds the ones this plan actually names, and a new dir joins as it
	// is created. Building one for every directory of the tree up front was a `RemoteDirectory`
	// clone per directory — tens of thousands of them on a large pair — to answer a few lookups.
	let mut dir_by_path: HashMap<String, RemoteDirectory> = HashMap::new();
	dir_by_path.insert(String::new(), ctx.root_remote.clone());
	resolve_folded_objects(&ctx, &actions, &mut files, &mut dir_by_path).await;

	// Split the phase-ordered plan: creates+moves first (serial — they fill `dir_by_path` and are
	// parent-before-child), then transfers (concurrent), then deletes (serial — descendant-first).
	// A "replace-delete" (a delete whose path is (re)created this pass — a file<->dir type flip) is
	// routed into `pre` so it runs BEFORE the create at that path (the actions are already ordered
	// so it sorts ahead of the create); otherwise the server rejects the create (old item exists).
	let create_targets = create_target_paths(&actions);
	let mut pre = Vec::new();
	let mut transfers = Vec::new();
	let mut post = Vec::new();
	for action in actions {
		if action.is_delete() && create_targets.contains(action.rel_path()) {
			pre.push(action);
		} else if is_transfer(&action) {
			transfers.push(action);
		} else if action.is_delete() {
			post.push(action);
		} else {
			pre.push(action);
		}
	}
	order_transfers(&pre, &mut transfers);

	// What the pass still owes if it is cancelled part-way: everything it has not carried out.
	let total = pre.len() + transfers.len() + post.len();
	let mut applied = 0usize;

	// Where the drive lock may be let go: before an action no earlier one is still waiting on.
	let releasable = {
		let order: Vec<&SyncAction> = pre.iter().chain(&transfers).chain(&post).collect();
		release_points(&order, &create_targets)
	};
	let (pre_release, rest) = releasable.split_at(pre.len());
	let (transfer_release, post_release) = rest.split_at(transfers.len());
	let mut lease = Lease::Unheld;

	let mut writes = PassWrites::default();
	// The sides found full so far: each holds back the transfers that write to it.
	let full = FullSides::default();
	for (action, &releasable) in pre.iter().zip(pre_release) {
		if !ctx.gate.wait_to_start().await {
			return note_interrupted(report, total - applied, observer);
		}
		if !writes.skips(action, report, observer) {
			match lease.prepare(&ctx, action, releasable).await {
				Some(Ok(())) => {}
				Some(Err(error)) => {
					return note_lock_failure(report, total - applied, &error, observer);
				}
				None => return note_interrupted(report, total - applied, observer),
			}
			if let Some(reason) = apply_serial(
				&ctx,
				action,
				&files,
				&mut dir_by_path,
				&mut writes,
				report,
				observer,
			)
			.await
			{
				full.note(reason);
			}
		}
		applied += 1;
	}

	let pending: Vec<(&SyncAction, bool)> = transfers
		.iter()
		.zip(transfer_release.iter().copied())
		.filter(|(action, _)| !writes.skips(action, report, observer))
		.collect();
	applied += transfers.len() - pending.len();

	if !pending.is_empty() {
		let concurrency = ctx.client.unauthed().state().max_concurrency().max(1);
		// Byte progress comes from inside the SDK's transfer futures, which cannot reach the
		// observer this loop holds: they queue it here and the loop delivers it — between
		// completions, and all of a transfer's progress ahead of that transfer's own event.
		let (progress, mut progress_events) = tokio::sync::mpsc::unbounded_channel();
		let mut pending = pending.into_iter().peekable();
		let mut in_flight = FuturesUnordered::new();
		// Uploads in flight use the lock, so it is let go only once none is; downloads do not.
		let mut remote_in_flight = 0usize;
		loop {
			while in_flight.len() < concurrency
				&& let Some(&(action, releasable)) = pending.peek()
			{
				// Once an action finds a side full, the transfers already running finish, and the ones
				// not started yet that write to that side never start (they record nothing). Held back
				// for want of room: carried out as far as this pass can, owed to no pause.
				if full.holds(action) {
					pending.next();
					applied += 1;
					tracing::debug!("apply: {} held, its side is full", action.describe());
					continue;
				}
				let now = Instant::now();
				if remote_in_flight == 0 {
					lease.release_if_spent(&ctx.lock_budget, releasable, now);
				}
				if !lease.admits(&ctx.lock_budget, action, releasable, now) {
					break;
				}
				lease.note_start(action);
				pending.next();
				remote_in_flight += usize::from(mutates_remote(action));
				in_flight.push(run_transfer(&ctx, action, &files, &dir_by_path, &progress));
			}
			let next = tokio::select! {
				biased;
				Some(event) = progress_events.recv() => {
					observer(event);
					continue;
				}
				next = in_flight.next() => next,
			};
			let Some((action, result)) = next else {
				// Nothing in flight: the transfers are done, or the next one waits on the lock.
				let Some((action, releasable)) = pending.next() else {
					break;
				};
				match lease.prepare(&ctx, action, releasable).await {
					Some(Ok(())) => {}
					Some(Err(error)) => {
						return note_lock_failure(report, total - applied, &error, observer);
					}
					None => return note_interrupted(report, total - applied, observer),
				}
				remote_in_flight += usize::from(mutates_remote(action));
				in_flight.push(run_transfer(&ctx, action, &files, &dir_by_path, &progress));
				continue;
			};
			remote_in_flight -= usize::from(mutates_remote(action));
			// A transfer queues its last progress before it resolves.
			while let Ok(event) = progress_events.try_recv() {
				observer(event);
			}
			match result {
				// A cancel skips a transfer that has not started and DROPS one that is mid-flight;
				// either way it recorded nothing, and the pass owes it to the next one.
				Ok(Transfer::Interrupted) => {
					tracing::debug!("apply: {} interrupted", action.describe());
				}
				Ok(Transfer::Done) => {
					applied += 1;
					writes.note(action);
					tracing::debug!("apply: {} done", action.describe());
					observer(action.to_event());
					match action {
						SyncAction::UploadFile { .. } => report.uploaded += 1,
						SyncAction::DownloadFile { .. } => report.downloaded += 1,
						_ => {}
					}
				}
				// The upload landed and stands — it is still an upload — but it buried a
				// concurrent edit, and that is a conflict of this pass's own making.
				Ok(Transfer::Overwrote(conflict)) => {
					applied += 1;
					writes.note(action);
					report.uploaded += 1;
					observer(action.to_event());
					observer(SyncEvent::Conflict {
						rel_path: conflict.rel_path.clone(),
					});
					report.conflicts.push(*conflict);
				}
				Err(error) => {
					applied += 1;
					tracing::debug!("apply: {} FAILED — {error}", action.describe());
					observer(SyncEvent::ActionFailed {
						rel_path: action.rel_path().to_string(),
						error: error.to_string(),
					});
					if let Some(reason) = note_failure(report, action.rel_path(), &error) {
						full.note(reason);
					}
				}
			}
		}
	}

	for (action, &releasable) in post.iter().zip(post_release) {
		if !ctx.gate.wait_to_start().await {
			return note_interrupted(report, total - applied, observer);
		}
		if !writes.skips(action, report, observer) {
			match lease.prepare(&ctx, action, releasable).await {
				Some(Ok(())) => {}
				Some(Err(error)) => {
					return note_lock_failure(report, total - applied, &error, observer);
				}
				None => return note_interrupted(report, total - applied, observer),
			}
			apply_serial(
				&ctx,
				action,
				&files,
				&mut dir_by_path,
				&mut writes,
				report,
				observer,
			)
			.await;
		}
		applied += 1;
	}
	note_interrupted(report, total - applied, observer);
}

/// One transfer, paired with the action it ran — named so every transfer of a pass is the same
/// future type and they can share one [`FuturesUnordered`].
async fn run_transfer<'a>(
	ctx: &ApplyContext<'_>,
	action: &'a SyncAction,
	files: &HashMap<Uuid, RemoteFile>,
	dir_by_path: &HashMap<String, RemoteDirectory>,
	progress: &UnboundedSender<SyncEvent>,
) -> (&'a SyncAction, Result<Transfer, crate::Error>) {
	(
		action,
		apply_transfer(ctx, action, files, dir_by_path, progress).await,
	)
}

/// The pass's hold on the account-wide drive-write lock, taken per bounded batch of its apply.
///
/// Nothing but this holds the pass's `Arc` of the lock, so moving out of [`Held`](Self::Held) drops
/// it; the lock is really released when the last in-process holder lets go — the SDK ops of the pass
/// clone it only for their own duration, which is why it is let go only with no remote write in
/// flight. Another holder in the process (the cache worker's resync) may legitimately keep it.
enum Lease {
	/// Not taken yet this pass.
	Unheld,
	Held {
		lock: Arc<ResourceLock>,
		/// When this hold began.
		since: Instant,
		/// How many remote writes started under it.
		writes: usize,
	},
	/// Let go at `at`; not asked for again before [`PASS_LOCK_YIELD`](super::engine::PASS_LOCK_YIELD) has passed.
	Released { at: Instant },
}

impl Lease {
	/// Whether the hold has run its course (see [`hold_spent`]).
	fn spent(&self, budget: &LockBudget, now: Instant) -> bool {
		matches!(self, Self::Held { since, writes, .. } if hold_spent(budget, *since, *writes, now))
	}

	/// Let a spent hold go, when the next action sits where a release splits nothing.
	fn release_if_spent(&mut self, budget: &LockBudget, releasable: bool, now: Instant) {
		if releasable && self.spent(budget, now) {
			*self = Self::Released { at: now };
		}
	}

	/// Whether `action` may start now without the lease changing first: no spent hold is waiting to
	/// be let go before it, and a remote write finds a lock whose lease is still good.
	fn admits(
		&self,
		budget: &LockBudget,
		action: &SyncAction,
		releasable: bool,
		now: Instant,
	) -> bool {
		if releasable && self.spent(budget, now) {
			return false;
		}
		!mutates_remote(action) || matches!(self, Self::Held { lock, .. } if lock.is_valid())
	}

	/// Count `action` against the hold when it writes to the remote.
	fn note_start(&mut self, action: &SyncAction) {
		if mutates_remote(action)
			&& let Self::Held { writes, .. } = self
		{
			*writes += 1;
		}
	}

	/// Get the lease ready for `action` and count it: let a spent hold go at a boundary that allows
	/// it, and take the lock before a remote write that finds none (or one whose lease was lost).
	/// `None` means the pass was cancelled meanwhile; an `Err` is a lock the pass could not take.
	///
	/// The lock is polled on a schedule of the engine's own (`PASS_LOCK_MAX_SLEEP`, default attempt
	/// count) rather than the shared default ladder, so a pass that misses another client's release
	/// picks the lock up inside the confirmation window instead of tens of seconds later, where its
	/// round no longer reads as a race against the one it queued behind.
	///
	/// It is taken UNDER the gate, because acquiring it is itself a wait a pause has to be able to
	/// reach: a contended acquisition retries for hours, so a pass told to stop while queueing for
	/// the lock would otherwise take it just to release it — and report what it did not do only
	/// then. A suspended pass parks here instead of holding a lock it is not using. Dropping the
	/// acquisition mid-request can leave the server holding a lock nothing refreshes; that lease
	/// expires on its own within ~30 s.
	async fn prepare(
		&mut self,
		ctx: &ApplyContext<'_>,
		action: &SyncAction,
		releasable: bool,
	) -> Option<Result<(), crate::Error>> {
		let now = Instant::now();
		self.release_if_spent(&ctx.lock_budget, releasable, now);
		if !self.admits(&ctx.lock_budget, action, releasable, now) {
			if let Self::Released { at } = *self {
				ctx.gate
					.guard(tokio::time::sleep_until(at + ctx.lock_budget.rest))
					.await?;
			}
			if !ctx.gate.wait_to_start().await {
				return None;
			}
			let lock = match ctx
				.gate
				.guard(
					ctx.client
						.lock_drive_bounded(PASS_LOCK_MAX_SLEEP, ATTEMPTS_DEFAULT),
				)
				.await?
			{
				Ok(lock) => lock,
				Err(error) => return Some(Err(error)),
			};
			*self = Self::Held {
				lock,
				since: Instant::now(),
				writes: 0,
			};
		}
		self.note_start(action);
		Some(Ok(()))
	}
}

/// Whether a hold of the drive lock that began at `since` and has started `writes` remote writes has
/// run its course: the budget's time or its batch — by default
/// [`PASS_LOCK_MAX_HOLD`](super::engine::PASS_LOCK_MAX_HOLD) or
/// [`PASS_LOCK_MAX_BATCH`](super::engine::PASS_LOCK_MAX_BATCH) — whichever comes first.
fn hold_spent(budget: &LockBudget, since: Instant, writes: usize, now: Instant) -> bool {
	now.saturating_duration_since(since) >= budget.hold || writes >= budget.batch
}

/// Put the transfers — independent of one another, so their order is free — in the order that lets
/// the pass hold the drive lock least: first the ones that finish a compound step of `pre` (the create
/// half of a replace-delete, anything under a moved directory), so that step ends as early as it can
/// (see [`release_points`]); then downloads, which need no lock; then uploads, which do.
fn order_transfers(pre: &[SyncAction], transfers: &mut [SyncAction]) {
	let replaced: HashSet<&str> = pre
		.iter()
		.filter(|action| action.is_delete())
		.map(SyncAction::rel_path)
		.collect();
	let moved_dirs: Vec<&str> = pre.iter().filter_map(moved_dir).collect();
	transfers.sort_by_key(|action| {
		let path = action.rel_path();
		let completes_step = replaced.contains(path)
			|| moved_dirs
				.iter()
				.any(|dir| path == *dir || is_under(path, dir));
		(
			!completes_step,
			matches!(action, SyncAction::UploadFile { .. }),
		)
	});
}

/// Where a directory move is headed, for a directory move.
fn moved_dir(action: &SyncAction) -> Option<&str> {
	match action {
		SyncAction::MoveRemote {
			to_path,
			kind: NodeKind::Dir,
			..
		}
		| SyncAction::MoveLocal {
			to_path,
			kind: NodeKind::Dir,
			..
		} => Some(to_path),
		_ => None,
	}
}

/// For each position of the pass's execution `order`, whether the drive lock may be let go just
/// before it: `false` where an action already run is still waiting on one at or after it.
///
/// Two shapes tie actions together. A replace-delete is half of a type flip: the path stays empty
/// until the create at it runs, and another client let in between would find it free. A directory
/// move carries its subtree: what the plan names under the destination (a create, a move, a
/// transfer) assumes the move has just happened. A deletion under a moved directory names its item
/// by uuid and ties nothing. Every other boundary is between independent actions — each one's write
/// already committed — and so is every boundary between phases that no such tie crosses.
fn release_points(order: &[&SyncAction], create_targets: &HashSet<String>) -> Vec<bool> {
	// The last position at which each path is created, transferred or moved from or to.
	let mut last: BTreeMap<&str, usize> = BTreeMap::new();
	for (position, action) in order.iter().enumerate() {
		if !action.is_delete() {
			let (from, to) = action.endpoints();
			last.insert(from, position);
			last.insert(to, position);
		}
	}
	let mut reach: Option<usize> = None;
	order
		.iter()
		.enumerate()
		.map(|(position, action)| {
			let releasable = reach.is_none_or(|reach| reach < position);
			let tied_until = if action.is_delete() && create_targets.contains(action.rel_path()) {
				last.get(action.rel_path()).copied()
			} else if let Some(dir) = moved_dir(action) {
				let prefix = format!("{dir}/");
				last.range(prefix.as_str()..)
					.take_while(|(path, _)| path.starts_with(&prefix))
					.map(|(_, &position)| position)
					.max()
			} else {
				None
			};
			reach = reach.max(tied_until);
			releasable
		})
		.collect()
}

/// What this pass has written so far, so that no later action of the SAME pass takes it down.
///
/// The server dedups a create against an existing item case-insensitively and hands back that
/// item, and a case-insensitive filesystem resolves two spellings to one entry, so a delete planned
/// for one path can land on what another action of the pass just created, moved or renamed. The
/// planner does not plan that shape (see [`plan::fold_dir_moves`](super::plan)); this
/// is the net under it: a remote trash or local quarantine whose target is, or holds, something the
/// pass already wrote is refused, and the next pass re-plans from what is really there.
#[derive(Debug, Default)]
struct PassWrites {
	/// Remote uuids the pass created, moved or renamed — including a create the dedup answered with
	/// an existing directory's uuid.
	remote_uuids: HashSet<Uuid>,
	/// Collision keys of the remote paths the pass wrote.
	remote_keys: BTreeSet<String>,
	/// Collision keys of the local paths the pass wrote.
	local_keys: BTreeSet<String>,
	/// Where the directory moves that failed were headed: nothing under those paths is attempted.
	failed_dir_moves: Vec<String>,
}

impl PassWrites {
	/// Record a successfully applied action.
	fn note(&mut self, action: &SyncAction) {
		match action {
			SyncAction::CreateLocalDir { rel_path } | SyncAction::DownloadFile { rel_path, .. } => {
				self.local_keys.insert(collision_key(rel_path));
			}
			SyncAction::MoveLocal { to_path, .. } => {
				self.local_keys.insert(collision_key(to_path));
			}
			SyncAction::CreateRemoteDir { rel_path } | SyncAction::UploadFile { rel_path } => {
				self.remote_keys.insert(collision_key(rel_path));
			}
			SyncAction::MoveRemote {
				to_path,
				remote_uuid,
				..
			} => {
				self.remote_keys.insert(collision_key(to_path));
				self.remote_uuids.insert(*remote_uuid);
			}
			SyncAction::DeleteLocal { .. }
			| SyncAction::TrashRemote { .. }
			| SyncAction::Conflict { .. }
			| SyncAction::AdoptBaseline { .. } => {}
		}
	}

	/// Whether trashing the remote item `remote_uuid` at `rel_path` would take down something this
	/// pass wrote.
	fn shields_remote(&self, rel_path: &str, remote_uuid: Uuid) -> bool {
		self.remote_uuids.contains(&remote_uuid)
			|| holds_key_at_or_under(&self.remote_keys, rel_path)
	}

	/// Whether quarantining the local item at `rel_path` would take down something this pass wrote.
	fn shields_local(&self, rel_path: &str) -> bool {
		holds_key_at_or_under(&self.local_keys, rel_path)
	}

	/// Skip `action` when it sits under a directory whose move failed this pass — the plan names that
	/// subtree by a path neither side carries — recording it as a failure of its own.
	fn skips(
		&self,
		action: &SyncAction,
		report: &mut SyncReport,
		observer: &mut (dyn FnMut(SyncEvent) + Send),
	) -> bool {
		let (from, to) = action.endpoints();
		let Some(dir) = self.failed_dir_moves.iter().find(|dir| {
			[from, to]
				.into_iter()
				.any(|path| path == dir.as_str() || is_under(path, dir))
		}) else {
			return false;
		};
		let error = internal_owned(format!(
			"skipped: the directory {dir:?} could not be renamed this pass"
		));
		observer(SyncEvent::ActionFailed {
			rel_path: action.rel_path().to_string(),
			error: error.to_string(),
		});
		note_failure(report, action.rel_path(), &error);
		true
	}
}

/// Whether `keys` holds the collision key of `rel_path` or of something under it.
fn holds_key_at_or_under(keys: &BTreeSet<String>, rel_path: &str) -> bool {
	let key = collision_key(rel_path);
	if keys.contains(&key) {
		return true;
	}
	let prefix = format!("{key}/");
	keys.range(prefix.clone()..)
		.next()
		.is_some_and(|next| next.starts_with(&prefix))
}

/// Journal a remote directory move from `from` to `to` and carry its subtree's rows along, in one
/// transaction.
///
/// The rows are re-keyed where they lie, from what the STORE holds rather than from the pass's
/// baseline — see [`BaselineChange::MoveSubtree`] for why that difference matters to a move with
/// another nested inside it.
async fn commit_dir_move(
	ctx: &ApplyContext<'_>,
	uuid: Uuid,
	from: &str,
	to: &str,
) -> Result<(), crate::Error> {
	let kind = PendingKind::Moved {
		from: from.to_string(),
		to: to.to_string(),
	};
	commit_remote_write(ctx, uuid, kind, &[BaselineChange::MoveSubtree { from, to }]).await
}

/// Record a pass that could not take the drive lock. It ran none of its `remaining` actions, so it
/// is reported as [`interrupted`](SyncReport::interrupted) exactly like a cancel before the first
/// action — which is also what keeps every planned path's failure streak untouched — and its
/// failure is flagged in [`lock_failed`](SyncReport::lock_failed) so a watch loop backs off on it
/// instead of reading the pass as healthy.
pub(super) fn note_lock_failure(
	report: &mut SyncReport,
	remaining: usize,
	error: &crate::Error,
	observer: &mut (dyn FnMut(SyncEvent) + Send),
) {
	report.errors.push(format!("{LOCK_FAILURE}: {error}"));
	report.lock_failed = true;
	note_interrupted(report, remaining, observer);
}

/// How the error of a pass that could not take the drive lock begins in [`SyncReport::errors`].
pub(super) const LOCK_FAILURE: &str = "failed to acquire the drive lock";

/// Record a pass cut short — by a cancel, or by a drive lock it could not take: how many planned
/// actions it did not carry out, both on the report and as an event, so a caller can tell "nothing
/// left to do" from "stopped part-way". A pass that ran to the end owes nothing and reports nothing.
fn note_interrupted(
	report: &mut SyncReport,
	remaining: usize,
	observer: &mut (dyn FnMut(SyncEvent) + Send),
) {
	if remaining == 0 {
		return;
	}
	tracing::debug!("apply: interrupted with {remaining} action(s) left unapplied");
	report.interrupted = remaining;
	observer(SyncEvent::Interrupted { actions: remaining });
}

/// Resolve the remote objects this plan acts on that the cache snapshot cannot supply.
///
/// The remote view carries the writes this engine made that the cache has not listed yet, so an
/// action can name an item no snapshot entry describes. The server is authoritative about those
/// (it minted them), and only the few the plan actually touches are fetched. One that cannot be
/// fetched is left out: its own action then fails and the pass carries on, exactly as it did when
/// the snapshot entry was missing.
///
/// Directories the snapshot DOES describe are built from its rows here, for the same handful of
/// paths, which is why every parent lookup in the apply below is a path this function was asked
/// about: the parent of an upload or a create, the target of a trash, and both ends of a move.
async fn resolve_folded_objects(
	ctx: &ApplyContext<'_>,
	actions: &[SyncAction],
	files: &mut HashMap<Uuid, RemoteFile>,
	dir_by_path: &mut HashMap<String, RemoteDirectory>,
) {
	let mut wanted_files: HashSet<Uuid> = HashSet::new();
	let mut wanted_dirs: HashSet<&str> = HashSet::new();
	for action in actions {
		match action {
			SyncAction::DownloadFile { remote_uuid, .. } => {
				wanted_files.insert(*remote_uuid);
			}
			SyncAction::TrashRemote {
				rel_path,
				kind,
				remote_uuid,
			} => match kind {
				NodeKind::File => {
					wanted_files.insert(*remote_uuid);
				}
				NodeKind::Dir => {
					wanted_dirs.insert(rel_path);
				}
			},
			SyncAction::MoveRemote {
				to_path,
				kind,
				remote_uuid,
				..
			} => {
				match kind {
					NodeKind::File => {
						wanted_files.insert(*remote_uuid);
					}
					// The view already lists the directory at the path it moves to.
					NodeKind::Dir => {
						wanted_dirs.insert(to_path);
					}
				}
				wanted_dirs.insert(parent_and_name(to_path).0);
			}
			SyncAction::UploadFile { rel_path } | SyncAction::CreateRemoteDir { rel_path } => {
				wanted_dirs.insert(parent_and_name(rel_path).0);
			}
			_ => {}
		}
	}

	// What the plan names and the map has not got: the root is in it already, and a directory this
	// very pass creates is filled in as it goes.
	let missing_dirs: Vec<&str> = wanted_dirs
		.into_iter()
		.filter(|path| !dir_by_path.contains_key(*path))
		.collect();
	// One cache read for every object this plan acts on, files and directories together: the
	// pass's snapshot carries only the columns its view reads (see `cache::enumerate`), so the
	// whole payloads are hydrated here, by uuid, and only for the handful the plan names.
	let wanted: Vec<Uuid> = wanted_files
		.iter()
		.copied()
		.chain(missing_dirs.iter().filter_map(|path| {
			ctx.remote
				.get(*path)
				.filter(|node| node.kind == NodeKind::Dir)
				.map(|node| node.remote_uuid)
		}))
		.collect();
	let mut cached_files: HashMap<Uuid, CacheableFile<'static>> = HashMap::new();
	let mut cached_dirs: HashMap<Uuid, CacheableDir<'static>> = HashMap::new();
	match ctx.client.hydrate_cached_items(wanted).await {
		Ok(items) => {
			for item in items {
				match item {
					SearchResult::File(file) => {
						cached_files.insert(file.uuid, file);
					}
					SearchResult::Dir(dir) => {
						cached_dirs.insert(dir.uuid, dir);
					}
				}
			}
		}
		Err(error) => tracing::debug!(
			"apply: could not read the cached objects this plan acts on ({error}); the server is asked for each of them"
		),
	}

	for uuid in wanted_files {
		if let Some(cacheable) = cached_files.remove(&uuid) {
			files.insert(uuid, RemoteFile::from(cacheable));
			continue;
		}
		match ctx.client.get_file(uuid).await {
			Ok(file) => {
				files.insert(uuid, file);
			}
			Err(error) => tracing::debug!(
				"apply: cannot resolve the file {uuid} this engine wrote ({error}); its action fails this pass"
			),
		}
	}
	for path in missing_dirs {
		let Some(node) = ctx
			.remote
			.get(path)
			.filter(|node| node.kind == NodeKind::Dir)
		else {
			continue;
		};
		// The cache held this one, so there is nothing to ask the server for.
		if let Some(cacheable) = cached_dirs.remove(&node.remote_uuid) {
			dir_by_path.insert(path.to_string(), RemoteDirectory::from(cacheable));
			continue;
		}
		match ctx.client.get_dir(node.remote_uuid).await {
			Ok(dir) => {
				dir_by_path.insert(path.to_string(), dir);
			}
			Err(error) => tracing::debug!(
				"apply: cannot resolve the remote dir {path:?} this engine created ({error}); its actions fail this pass"
			),
		}
	}
}

/// Whether an action writes to the remote (so the pass must hold the drive-write lock).
fn mutates_remote(action: &SyncAction) -> bool {
	matches!(
		action,
		SyncAction::UploadFile { .. }
			| SyncAction::CreateRemoteDir { .. }
			| SyncAction::TrashRemote { .. }
			| SyncAction::MoveRemote { .. }
	)
}

/// Whether an action is a file transfer (the concurrently-applied bulk).
fn is_transfer(action: &SyncAction) -> bool {
	matches!(
		action,
		SyncAction::UploadFile { .. } | SyncAction::DownloadFile { .. }
	)
}

/// Apply one action serially: emit its in-progress event, run it, and record success/failure.
/// Returns the side its failure found full, if it did (see [`note_failure`]).
async fn apply_serial(
	ctx: &ApplyContext<'_>,
	action: &SyncAction,
	files: &HashMap<Uuid, RemoteFile>,
	dir_by_path: &mut HashMap<String, RemoteDirectory>,
	writes: &mut PassWrites,
	report: &mut SyncReport,
	observer: &mut (dyn FnMut(SyncEvent) + Send),
) -> Option<HaltReason> {
	observer(action.to_event());
	tracing::debug!("apply: {}", action.describe());
	let error = match apply_one(ctx, action, files, dir_by_path, writes, report).await {
		Ok(()) => {
			writes.note(action);
			return None;
		}
		Err(error) => error,
	};
	tracing::debug!("apply: {} FAILED — {error}", action.describe());
	if let SyncAction::MoveRemote {
		to_path,
		kind: NodeKind::Dir,
		..
	}
	| SyncAction::MoveLocal {
		to_path,
		kind: NodeKind::Dir,
		..
	} = action
	{
		writes.failed_dir_moves.push(to_path.clone());
	}
	observer(SyncEvent::ActionFailed {
		rel_path: action.rel_path().to_string(),
		error: error.to_string(),
	});
	note_failure(report, action.rel_path(), &error)
}

/// Record one action failure: the human-readable line AND the `(path, error)` pair the engine's
/// per-path failure counter consumes, so a path that fails every pass is eventually reported as
/// unsyncable instead of retried forever.
///
/// A failure that says a side has no room left (see [`pass_halt`]) is not the path's fault and
/// counts against no streak: it sets [`halted`](SyncReport::halted) instead (if nothing did yet),
/// with one line per side saying so, and is returned so the caller holds back the transfers that
/// write there.
fn note_failure(
	report: &mut SyncReport,
	rel_path: &str,
	error: &crate::Error,
) -> Option<HaltReason> {
	report.errors.push(format!("{rel_path}: {error}"));
	let Some(reason) = pass_halt(error) else {
		report
			.failed_paths
			.push((rel_path.to_string(), error.to_string()));
		return None;
	};
	let line = reason.held_line();
	if !report.errors.contains(&line) {
		report.errors.push(line);
	}
	report.halted.get_or_insert(reason);
	Some(reason)
}

/// The sides of a pass found full so far, read by the transfers as they start. Atomics, because
/// those run concurrently while the apply loop records their failures.
#[derive(Default)]
struct FullSides {
	local: AtomicBool,
	remote: AtomicBool,
}

impl FullSides {
	fn note(&self, reason: HaltReason) {
		match reason {
			HaltReason::LocalStorageFull => &self.local,
			HaltReason::RemoteStorageFull => &self.remote,
		}
		.store(true, Ordering::Relaxed);
	}

	/// Whether `action` writes to a side found full, and so must not start: an upload once the
	/// account is full, a download once the local disk is.
	fn holds(&self, action: &SyncAction) -> bool {
		match action {
			SyncAction::UploadFile { .. } => self.remote.load(Ordering::Relaxed),
			SyncAction::DownloadFile { .. } => self.local.load(Ordering::Relaxed),
			_ => false,
		}
	}
}

/// Whether `error` means a side of this pass has no room left: the local disk, or the account. Looked for along the whole error chain, including inside an `io::Error` wrapping
/// another error — which is how the upload writer hands on a server refusal.
fn pass_halt(error: &crate::Error) -> Option<HaltReason> {
	let mut next: Option<&(dyn std::error::Error + 'static)> = Some(error);
	while let Some(current) = next {
		if current
			.downcast_ref::<crate::Error>()
			.is_some_and(|sdk| sdk.kind() == crate::ErrorKind::MaxStorageReached)
			|| matches!(
				current.downcast_ref::<filen_types::error::ResponseError>(),
				Some(filen_types::error::ResponseError::ApiError { code: Some(code), .. })
					if code == "max_storage_reached"
			) {
			return Some(HaltReason::RemoteStorageFull);
		}
		next = match current.downcast_ref::<std::io::Error>() {
			Some(io)
				if matches!(
					io.kind(),
					std::io::ErrorKind::StorageFull | std::io::ErrorKind::QuotaExceeded
				) =>
			{
				return Some(HaltReason::LocalStorageFull);
			}
			// `io::Error::source` skips the error it wraps and reports that error's own source, so
			// step into the wrapped one directly.
			Some(io) => io
				.get_ref()
				.map(|inner| inner as &(dyn std::error::Error + 'static)),
			None => current.source(),
		};
	}
	None
}

/// Whether one transfer ran at all — the pass can be cancelled before it starts, or while its
/// network op is in flight (see [`PassGate`]).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Transfer {
	/// It ran to the end and recorded what it did.
	Done,
	/// The upload landed and IS the remote head — but the server's version chain shows it went on
	/// top of a version this pass never saw. Another client edited the file between the snapshot
	/// this pass read and our upload, and our bytes buried it; the conflict says so.
	Overwrote(Box<PlannedConflict>),
	/// The pass was cancelled: it never started, or a cancel dropped it mid-flight. Either way it
	/// recorded nothing, and the next pass re-plans it.
	Interrupted,
}

/// Perform a transfer (download or upload): the network op + baseline write only. It touches no
/// shared mutable state — it READS `dir_by_path` and the baseline store is internally locked — so
/// transfers run concurrently; the caller does the report/event accounting as each completes.
///
/// The pair's pause gates it in two places, and only those two: nothing starts while the pass is
/// suspended, and the NETWORK op alone is raced against a cancel. The bookkeeping after that op is
/// deliberately outside the race — it waits on the store lock every other transfer of the pass
/// contends for, and a cancel landing in that window would leave the remote holding a write no
/// baseline row describes and no report counts.
async fn apply_transfer(
	ctx: &ApplyContext<'_>,
	action: &SyncAction,
	files: &HashMap<Uuid, RemoteFile>,
	dir_by_path: &HashMap<String, RemoteDirectory>,
	progress: &UnboundedSender<SyncEvent>,
) -> Result<Transfer, crate::Error> {
	if !ctx.gate.wait_to_start().await {
		return Ok(Transfer::Interrupted);
	}
	match action {
		SyncAction::DownloadFile {
			rel_path,
			remote_uuid,
		} => {
			let remote_file = files
				.get(remote_uuid)
				.cloned()
				.ok_or_else(|| internal("download target missing from the snapshot"))?;
			let path = confined_local_target(ctx.local_root, rel_path)?;
			if let Some(parent) = path.parent() {
				std::fs::create_dir_all(parent).map_err(io_err)?;
			}
			// Remote-wins: stash a local file the baseline cannot vouch for before the download
			// overwrites it, the same way a propagated deletion is quarantined rather than
			// destroyed. An unmodified copy still matching its baseline row is just refreshed.
			//
			// `ctx.baseline` is the PASS-START snapshot, so it has no row for a path this same pass
			// moved a file onto — a remote move that carried a content edit is a `MoveLocal` paired
			// with this very download, and the move already wrote the moved file's row. Read that
			// row back before deciding; without it the download would quarantine the copy the move
			// just renamed into place. Only when something is actually sitting at the target: an
			// ordinary download onto free space has nothing to lose and skips the read.
			let base = match ctx.baseline.get(rel_path) {
				Some(base) => Some(base.clone()),
				None if path.exists() => ctx
					.store
					.lock()
					.await
					.entry(ctx.pair, rel_path)
					.map_err(db_err)?,
				None => None,
			};
			// The scan's hash describes the pass-start row only; a row read back from the store
			// describes a file this pass moved here since.
			let scanned_hash = ctx
				.baseline
				.get(rel_path)
				.and(ctx.local.get(rel_path))
				.and_then(|node| node.content_hash);
			let stashed =
				stash_local_target(ctx.local_root, rel_path, base.as_ref(), scanned_hash)?;
			let Some(downloaded) = ctx
				.gate
				.guard(ctx.client.download_file_to_path(
					&remote_file,
					&path,
					Some(progress_callback(
						progress,
						rel_path,
						ctx.remote.get(rel_path).map_or(0, |node| node.size),
					)),
				))
				.await
			else {
				// Dropped before it could put anything at the path — so the copy moved out of its
				// way is all that path has, and it goes back. Leaving it in the bin would leave the
				// path empty for the next pass to read as a local deletion.
				restore_stashed(stashed, &path);
				return Ok(Transfer::Interrupted);
			};
			// A download that FAILED leaves the same empty path behind, and the retry the next pass
			// makes stashes the restored copy again if it is still unsynced.
			if let Err(error) = downloaded {
				restore_stashed(stashed, &path);
				return Err(error);
			}
			let remote = ctx.remote.get(rel_path);
			upsert_file_baseline(
				ctx,
				rel_path,
				Some(*remote_uuid),
				remote.and_then(|n| n.stable_uuid),
				remote.and_then(|n| n.content_hash),
				remote.map(|n| n.size).unwrap_or(0),
				local_mtime_of(&path),
				remote.map(|n| n.modified_millis),
				// A pull is the moment both sides demonstrably hold the same bytes: the content just
				// fetched IS the agreed content.
				remote.and_then(|n| n.content_hash),
			)
			.await?;
		}
		SyncAction::UploadFile { rel_path } => {
			let (parent_path, _) = parent_and_name(rel_path);
			let parent = dir_by_path
				.get(parent_path)
				.ok_or_else(|| internal("remote parent dir for upload is missing"))?
				.clone();
			let parent_type = DirType::<Normal>::Dir(std::borrow::Cow::Owned(parent));
			let path = local_path(ctx.local_root, rel_path);
			let Some(upload) = ctx
				.gate
				.guard(ctx.client.upload_file_from_path(
					&parent_type,
					path,
					Some(progress_callback(
						progress,
						rel_path,
						ctx.local.get(rel_path).map_or(0, |node| node.size),
					)),
				))
				.await
			else {
				return Ok(Transfer::Interrupted);
			};
			let (uploaded, _file) = upload?;
			let local = ctx.local.get(rel_path);
			let new_uuid: Uuid = uploaded.uuid();
			// A same-name upload versions whatever the path held: record that uuid, so the next
			// pass can tell a cache that has not caught up (the old uuid still at the path) from
			// someone ELSE having written there since (a third uuid).
			let replaced = ctx.remote.get(rel_path).map(|node| node.remote_uuid);
			let kind = PendingKind::Created {
				path: rel_path.clone(),
				replaced,
			};
			// The push leaves the agreed content behind (below); from here on the cache's
			// announcements are what date it, so that a version of ours that quietly stood as the
			// remote head confirms even without a pass ever catching it there.
			ctx.observed
				.watch_push(new_uuid, Some(uploaded.stable_uuid()), replaced);
			// ... and the version chain says whether we landed on the version we meant to.
			if let Some(buried) = buried_version(
				ctx,
				&uploaded,
				new_uuid,
				replaced,
				local.and_then(|n| n.content_hash),
			)
			.await
			{
				let entry = overwritten_entry(rel_path, local, &buried);
				tracing::debug!(
					"apply: the upload of {rel_path:?} went on top of version {} — a concurrent edit this pass never saw",
					buried.uuid()
				);
				// No pending-write record: the path is HELD from here on, so no pass reconciles
				// against it, and a fold would have to read the row — which describes the buried
				// version, not what the remote holds.
				upsert_baseline(ctx, &entry).await?;
				return Ok(Transfer::Overwrote(Box::new(PlannedConflict {
					rel_path: rel_path.clone(),
					local: local.map(|node| node.kind.into()),
					remote: Some(PlannedNodeKind::File),
				})));
			}
			let entry = file_entry(
				rel_path,
				Some(new_uuid),
				// The upload reports the lineage it landed in: the same one when it versioned an
				// existing file at this name, a brand-new one when it created a file.
				Some(uploaded.stable_uuid()),
				local.and_then(|n| n.content_hash),
				local.map(|n| n.size).unwrap_or(0),
				local.map(|n| n.mtime_millis),
				Some(uploaded.timestamp.timestamp_millis()),
				// A push does NOT make its own content agreed: the server took our bytes, but
				// another client's edit may already be on its way to the same path. The marker stays
				// on the previous agreed content until a snapshot lists this version at this path
				// (`plan::confirm_agreed_content`), and the gap is what surfaces a concurrent edit.
				ctx.baseline.get(rel_path).and_then(|b| b.agreed_hash),
			);
			commit_remote_write(ctx, new_uuid, kind, &[BaselineChange::Upsert(&entry)]).await?;
		}
		_ => return Err(internal("apply_transfer called with a non-transfer action")),
	}
	Ok(Transfer::Done)
}

/// The SDK progress callback for the transfer of `rel_path`. The SDK hands it byte deltas, already
/// throttled to one call per 200 ms plus a final flush; it adds them up and queues a
/// [`SyncEvent::Progress`] for the apply loop to deliver. The running total is updated and sent
/// under one lock, so the queued `bytes` never go down even if two chunk futures report at once.
fn progress_callback<'a>(
	progress: &'a UnboundedSender<SyncEvent>,
	rel_path: &'a str,
	total: u64,
) -> MaybeSendCallback<'a, u64> {
	let bytes = std::sync::Mutex::new(0u64);
	MaybeArc::new(move |delta| {
		let mut bytes = bytes.lock().unwrap_or_else(PoisonError::into_inner);
		*bytes += delta;
		// The receiver lives until every transfer of the pass has resolved, so a send cannot fail
		// while one is still reporting.
		let _ = progress.send(SyncEvent::Progress {
			rel_path: rel_path.to_string(),
			bytes: *bytes,
			total,
		});
	})
}

async fn apply_one(
	ctx: &ApplyContext<'_>,
	action: &SyncAction,
	files: &HashMap<Uuid, RemoteFile>,
	dir_by_path: &mut HashMap<String, RemoteDirectory>,
	writes: &mut PassWrites,
	report: &mut SyncReport,
) -> Result<(), crate::Error> {
	match action {
		SyncAction::CreateLocalDir { rel_path } => {
			let path = confined_local_target(ctx.local_root, rel_path)?;
			std::fs::create_dir_all(&path).map_err(io_err)?;
			let remote_uuid = ctx.remote.get(rel_path).map(|n| n.remote_uuid);
			upsert_dir_baseline(ctx, rel_path, remote_uuid, local_mtime_of(&path)).await?;
			report.local_dirs_created += 1;
		}
		// Transfers normally run via the concurrent path in `apply`; these arms keep `apply_one`
		// total and correct if a transfer is ever applied serially.
		SyncAction::DownloadFile { .. } => {
			// No observer reaches this fallback, so its progress goes to a channel nobody reads.
			if apply_transfer(
				ctx,
				action,
				files,
				dir_by_path,
				&tokio::sync::mpsc::unbounded_channel().0,
			)
			.await? == Transfer::Done
			{
				report.downloaded += 1;
			}
		}
		SyncAction::DeleteLocal { rel_path, .. } => {
			if writes.shields_local(rel_path) {
				return Err(internal_owned(format!(
					"refusing to quarantine {rel_path:?}: this pass already wrote there, under this or another spelling"
				)));
			}
			let _stashed = quarantine_local(ctx.local_root, rel_path)?;
			delete_baseline(ctx, rel_path).await?;
			report.locally_deleted += 1;
		}
		SyncAction::CreateRemoteDir { rel_path } => {
			let (parent_path, name) = parent_and_name(rel_path);
			let parent = dir_by_path
				.get(parent_path)
				.ok_or_else(|| internal("remote parent dir not yet created"))?
				.clone();
			let created = ctx
				.local
				.get(rel_path)
				.map(|n| millis_to_dt(n.mtime_millis))
				.unwrap_or_else(Utc::now);
			let parent_type = DirType::<Normal>::Dir(std::borrow::Cow::Owned(parent));
			let new_dir = ctx
				.client
				.create_dir_with_created(&parent_type, name, created)
				.await?;
			let new_uuid: Uuid = new_dir.uuid();
			// When the dedup answered with an existing directory, this is ITS uuid.
			writes.remote_uuids.insert(new_uuid);
			let kind = PendingKind::Created {
				path: rel_path.clone(),
				replaced: ctx.remote.get(rel_path).map(|node| node.remote_uuid),
			};
			dir_by_path.insert(rel_path.clone(), new_dir);
			let entry = dir_entry(rel_path, Some(new_uuid), None);
			commit_remote_write(ctx, new_uuid, kind, &[BaselineChange::Upsert(&entry)]).await?;
			report.remote_dirs_created += 1;
		}
		SyncAction::UploadFile { .. } => {
			match apply_transfer(
				ctx,
				action,
				files,
				dir_by_path,
				&tokio::sync::mpsc::unbounded_channel().0,
			)
			.await?
			{
				Transfer::Done => report.uploaded += 1,
				Transfer::Overwrote(conflict) => {
					report.uploaded += 1;
					report.conflicts.push(*conflict);
				}
				Transfer::Interrupted => {}
			}
		}
		SyncAction::TrashRemote {
			rel_path,
			kind,
			remote_uuid,
		} => {
			if writes.shields_remote(rel_path, *remote_uuid) {
				return Err(internal_owned(format!(
					"refusing to trash {rel_path:?}: this pass already wrote that item, or under that name"
				)));
			}
			match kind {
				NodeKind::File => {
					let mut remote_file = files
						.get(remote_uuid)
						.cloned()
						.ok_or_else(|| internal("trash target file missing from the snapshot"))?;
					ctx.client.trash_file(&mut remote_file).await?;
				}
				NodeKind::Dir => {
					let mut remote_dir = dir_by_path
						.get(rel_path)
						.cloned()
						.ok_or_else(|| internal("trash target dir missing from the snapshot"))?;
					ctx.client.trash_dir(&mut remote_dir).await?;
				}
			}
			// The row is about to go, so a snapshot that has not applied the trash yet reads this
			// item as an untracked remote file with nothing local — a deletion to make all over
			// again. Record the trash so the next pass suppresses that.
			commit_remote_write(
				ctx,
				*remote_uuid,
				PendingKind::Trashed,
				&[BaselineChange::Delete(rel_path)],
			)
			.await?;
			report.remotely_trashed += 1;
		}
		SyncAction::MoveRemote {
			from_path,
			to_path,
			kind: NodeKind::File,
			remote_uuid,
		} => {
			let mut remote_file = files
				.get(remote_uuid)
				.cloned()
				.ok_or_else(|| internal("move-source file missing from the snapshot"))?;
			let (from_parent, from_name) = parent_and_name(from_path);
			let (to_parent, to_name) = parent_and_name(to_path);
			if to_parent != from_parent {
				let parent = dir_by_path
					.get(to_parent)
					.ok_or_else(|| internal("move-target parent dir is missing"))?
					.clone();
				let parent_type = DirType::<Normal>::Dir(std::borrow::Cow::Owned(parent));
				ctx.client.move_file(&mut remote_file, &parent_type).await?;
			}
			if to_name != from_name {
				let changes = FileMetaChanges::default()
					.name(to_name)
					.map_err(crate::Error::from)?;
				ctx.client
					.update_file_metadata(&mut remote_file, changes)
					.await?;
			}
			let local = ctx.local.get(to_path);
			let kind = PendingKind::Moved {
				from: from_path.clone(),
				to: to_path.clone(),
			};
			let entry = file_entry(
				to_path,
				Some(*remote_uuid),
				Some(remote_file.stable_uuid()),
				local.and_then(|n| n.content_hash),
				local.map(|n| n.size).unwrap_or(0),
				local.map(|n| n.mtime_millis),
				Some(remote_file.timestamp.timestamp_millis()),
				// A move changes no content, so whatever the two sides agreed on at the old path
				// they still agree on at the new one.
				ctx.baseline.get(from_path).and_then(|b| b.agreed_hash),
			);
			commit_remote_write(
				ctx,
				*remote_uuid,
				kind,
				&[
					BaselineChange::Delete(from_path),
					BaselineChange::Upsert(&entry),
				],
			)
			.await?;
			report.moved_remote += 1;
		}
		SyncAction::MoveLocal {
			from_path,
			to_path,
			kind: NodeKind::File,
		} => {
			let from = local_path(ctx.local_root, from_path);
			let to = confined_local_target(ctx.local_root, to_path)?;
			// The reconciler only plans a move onto a destination the SCAN saw free, but a pass
			// waits on the drive lock in between and a file can land there meanwhile — and
			// `rename` would destroy it without a trace. Same window, and same remedy, as the
			// pre-download stash above.
			stash_move_target(ctx.local_root, to_path, ctx.baseline.get(to_path))?;
			if let Some(parent) = to.parent() {
				std::fs::create_dir_all(parent).map_err(io_err)?;
			}
			std::fs::rename(&from, &to).map_err(io_err)?;
			let row = moved_file_row(
				to_path,
				ctx.baseline.get(from_path),
				ctx.remote.get(to_path),
				local_mtime_of(&to),
			);
			// One transaction, delete before upsert: a crash between the two would leave the file
			// with no row at either path, and the next pass would read it as newly created.
			ctx.store
				.lock()
				.await
				.apply_changes(
					ctx.pair,
					&[
						BaselineChange::Delete(from_path.as_str()),
						BaselineChange::Upsert(&row),
					],
				)
				.map_err(db_err)?;
			report.moved_local += 1;
		}
		SyncAction::MoveRemote {
			from_path,
			to_path,
			kind: NodeKind::Dir,
			remote_uuid,
		} => {
			// The view — and so this map — already lists the directory at the path it moves to.
			let mut dir = dir_by_path
				.get(to_path)
				.filter(|dir| dir.uuid() == *remote_uuid)
				.cloned()
				.ok_or_else(|| internal("move-source dir missing from the snapshot"))?;
			let (from_parent, from_name) = parent_and_name(from_path);
			let (to_parent, to_name) = parent_and_name(to_path);
			// Validated before anything moves, so a name the rename would refuse fails the action
			// with the directory still where the baseline has it.
			let rename = (to_name != from_name)
				.then(|| DirectoryMetaChanges::default().name(to_name))
				.transpose()
				.map_err(crate::Error::from)?;
			let reparented = to_parent != from_parent;
			if reparented {
				let parent = dir_by_path
					.get(to_parent)
					.ok_or_else(|| internal("move-target parent dir is missing"))?
					.clone();
				let parent_type = DirType::<Normal>::Dir(std::borrow::Cow::Owned(parent));
				ctx.client.move_dir(&mut dir, &parent_type).await?;
			}
			if let Some(changes) = rename
				&& let Err(error) = ctx.client.update_dir_metadata(&mut dir, changes).await
			{
				if reparented {
					// The re-parent landed and the rename did not: the directory now sits under its
					// new parent with its old name. Record that, or the next pass finds it at neither
					// end of the move — no baseline row where it is, nothing on either side where the
					// rows are — and plans the whole subtree as new on both sides. Recorded, the next
					// pass reads a synced directory and plans the rename alone.
					let via = match to_parent {
						"" => from_name.to_string(),
						parent => format!("{parent}/{from_name}"),
					};
					commit_dir_move(ctx, *remote_uuid, from_path, &via).await?;
				}
				return Err(error);
			}
			dir_by_path.insert(to_path.clone(), dir);
			commit_dir_move(ctx, *remote_uuid, from_path, to_path).await?;
			report.moved_remote += 1;
		}
		SyncAction::MoveLocal {
			from_path,
			to_path,
			kind: NodeKind::Dir,
		} => {
			let from = local_path(ctx.local_root, from_path);
			let to = confined_local_target(ctx.local_root, to_path)?;
			// The destination can have become a directory of its own since the scan, and a rename
			// onto an empty one would silently replace it. A case-only rename cannot ask for the path
			// itself — a case-insensitive filesystem answers with the source — so it asks the parent
			// for the exact name.
			let case_only = collision_key(from_path) == collision_key(to_path);
			if has_own_directory_entry(&to) || (!case_only && to.symlink_metadata().is_ok()) {
				return Err(internal_owned(format!(
					"refusing to rename {from_path:?} onto the existing {to_path:?}"
				)));
			}
			std::fs::rename(&from, &to).map_err(io_err)?;
			if !has_own_directory_entry(&to) {
				// A filesystem that keeps the old spelling on a case-only rename: step through a free
				// name so the new spelling is what the directory is listed under.
				let step = to.with_file_name(format!(
					".{}.filen-rename-{}",
					parent_and_name(to_path).1,
					Uuid::new_v4()
				));
				std::fs::rename(&to, &step).map_err(io_err)?;
				std::fs::rename(&step, &to).map_err(io_err)?;
			}
			ctx.store
				.lock()
				.await
				.apply_changes(
					ctx.pair,
					&[BaselineChange::MoveSubtree {
						from: from_path.as_str(),
						to: to_path.as_str(),
					}],
				)
				.map_err(db_err)?;
			report.moved_local += 1;
		}
		SyncAction::Conflict { rel_path } => {
			// The engine screens conflicts out of the applied plan and records them itself; this
			// arm keeps `apply_one` total and correct if one is ever routed through here.
			record_conflict(
				ctx.store,
				ctx.pair,
				rel_path,
				ctx.local.get(rel_path),
				ctx.remote.get(rel_path),
			)
			.await?;
		}
		SyncAction::AdoptBaseline { rel_path } => {
			// Record the converged state into the baseline with no transfer, so a later one-sided
			// change at this path is classified correctly. Both sides agree here (the reconciler
			// only emits this when they do).
			match adopt_outcome(
				rel_path,
				ctx.local.get(rel_path),
				ctx.remote.get(rel_path),
				ctx.absence_trusted,
			) {
				AdoptOutcome::Record(entry) => upsert_baseline(ctx, &entry).await?,
				AdoptOutcome::DropRow => delete_baseline(ctx, rel_path).await?,
				AdoptOutcome::Keep => {}
			}
		}
	}
	Ok(())
}

/// Stash whatever sits at a local write target that the baseline cannot vouch for, before the
/// write lands on top of it. Every local write that can OVERWRITE — a remote-wins download, and a
/// move whose destination is occupied — goes through here, so a local edit is never destroyed
/// silently; it lands in the quarantine bin the same way a propagated deletion does.
///
/// `scanned_hash` is the content hash this pass's scan read at the target, passed only when `base`
/// is the pass-start row that scan was measured against. A scan that hashed something other than
/// the row records is an edit the `(size, mtime)` test below cannot see — a same-size edit that kept
/// its mtime, which only a [deep](super::scan::ScanDepth::Deep) scan hashes — so it is stashed too.
///
/// Returns where it was stashed (nothing stashed: `None`), for a caller whose write may still not
/// happen — see [`restore_stashed`].
fn stash_local_target(
	local_root: &Path,
	rel_path: &str,
	base: Option<&BaselineEntry>,
	scanned_hash: Option<Blake3Hash>,
) -> Result<Option<PathBuf>, crate::Error> {
	let scan_saw_an_edit = matches!(
		(base.and_then(|b| b.content_hash), scanned_hash),
		(Some(recorded), Some(scanned)) if recorded != scanned
	);
	if scan_saw_an_edit || local_holds_unsynced_content(&local_path(local_root, rel_path), base) {
		return quarantine_local(local_root, rel_path);
	}
	Ok(None)
}

/// Whether the item at a pull's target path holds content the baseline cannot vouch for — a local
/// edit, or a file the baseline never recorded. A remote-wins download would destroy it, so it is
/// quarantined first. A file still matching its baseline row is an unmodified copy: nothing is
/// lost by refreshing it in place.
///
/// Read from DISK rather than from the pass's scan snapshot: a pass waits on the drive lock
/// between the scan and its transfers, and an edit landing in that window is exactly the one that
/// must not be overwritten. The `(size, mtime)` comparison is the scanner's own fast-path test
/// (`scan::fast_path_hash`), so an untouched file is not re-hashed here either.
fn local_holds_unsynced_content(path: &Path, base: Option<&BaselineEntry>) -> bool {
	use crate::io::FilenMetaExt;
	let Ok(meta) = std::fs::metadata(path) else {
		// Nothing readable on disk: there is nothing to lose.
		return false;
	};
	// No baseline row, or one describing something else (a type flip): nothing on record says what
	// is on disk was ever synced.
	let Some(base) = base else {
		return true;
	};
	base.kind != NodeKind::File
		|| !meta.is_file()
		|| base.size != Some(meta.len())
		|| base.local_mtime != Some(FilenMetaExt::modified(&meta).timestamp_millis())
}

/// What an [`SyncAction::AdoptBaseline`] writes for one path. Decided purely (no I/O) so the
/// policy is unit-testable.
#[derive(Debug, PartialEq, Eq)]
enum AdoptOutcome {
	/// Both sides hold the item and agree — record the converged state. Boxed: the row dwarfs the
	/// two unit variants beside it.
	Record(Box<BaselineEntry>),
	/// Both sides agree the item is GONE (a convergent delete) — drop the stale baseline row.
	DropRow,
	/// Both sides LOOK gone, but this pass's absence evidence is untrustworthy — leave the row for
	/// a healthy pass to decide.
	Keep,
}

/// Decide what adopting `rel_path` writes to the baseline.
///
/// The local mtime recorded is the SCAN-time one — the same read the scan-time content hash
/// belongs to. Re-stat'ing here would pair a fresh mtime with the scan's hash, and a same-size
/// local edit landing between the scan and the apply (a pass can wait minutes on the drive lock)
/// would then satisfy the scanner's `(size, mtime)` fast-path forever: the edit never seen, and
/// pull-overwritten by the next remote change.
fn adopt_outcome(
	rel_path: &str,
	local: Option<&LocalNode>,
	remote: Option<&RemoteNode>,
	absence_trusted: bool,
) -> AdoptOutcome {
	match (local, remote) {
		(Some(local), Some(remote)) => AdoptOutcome::Record(Box::new(BaselineEntry {
			rel_path: rel_path.to_string(),
			kind: remote.kind,
			remote_uuid: Some(remote.remote_uuid),
			content_hash: match remote.kind {
				NodeKind::Dir => None,
				NodeKind::File => remote.content_hash,
			},
			size: match remote.kind {
				NodeKind::Dir => None,
				NodeKind::File => Some(remote.size),
			},
			local_mtime: Some(local.mtime_millis),
			remote_modified: match remote.kind {
				NodeKind::Dir => None,
				NodeKind::File => Some(remote.modified_millis),
			},
			state: BaselineState::Synced,
			local_kind: None,
			remote_kind: None,
			remote_hash: None,
			remote_size: None,
			remote_stable_uuid: remote.stable_uuid,
			// Adopting is the observation that both sides already hold this content, which is
			// exactly what the agreed marker records.
			agreed_hash: match remote.kind {
				NodeKind::Dir => None,
				NodeKind::File => remote.content_hash,
			},
		})),
		// A convergent delete drops the row — but only when the pass can trust that both sides are
		// really gone. Under an incomplete scan or an un-converged/empty remote view the row is the
		// only record that the item was ever synced; dropping it makes the next healthy pass read
		// the surviving side as newly Created and resurrect the item instead of propagating the
		// deletion. The guard holds deletions for exactly this reason; this bookkeeping is not
		// screened by it, so it checks the same condition itself.
		_ if absence_trusted => AdoptOutcome::DropRow,
		_ => AdoptOutcome::Keep,
	}
}

/// The version this upload buried, when it buried one: the pusher's side of a concurrent edit.
///
/// A same-name upload is versioned rather than refused, so an edit another client made between
/// this pass reading the remote and our upload landing ends up UNDER our bytes, invisible to
/// everything the pass can see. The server's version chain is the only place it shows, so the
/// upload arm asks — once per upload, and only for an upload that replaced a version this pass
/// actually saw. A brand-new file has no chain to read (nothing was there to interleave with), and
/// a one-way mode has an authoritative side and no divergence to surface.
///
/// A lookup that fails leaves the push looking ordinary: the pass records the row it always did,
/// and the other client's own conflict is what surfaces the divergence.
async fn buried_version(
	ctx: &ApplyContext<'_>,
	uploaded: &RemoteFile,
	ours: Uuid,
	replaced: Option<Uuid>,
	ours_hash: Option<filen_types::crypto::Blake3Hash>,
) -> Option<crate::fs::file::FileVersion> {
	if ctx.mode != super::SyncMode::TwoWay {
		return None;
	}
	let replaced = replaced?;
	let versions = match ctx.client.list_file_versions(uploaded).await {
		Ok(versions) => versions,
		Err(error) => {
			tracing::debug!(
				"apply: could not read the version chain after uploading {ours} — {error}"
			);
			return None;
		}
	};
	// Versions this engine minted itself are not candidates, whatever the chain's order says: the
	// server stamps to the second, so our own previous upload and a stranger's concurrent edit can
	// share a second with the version we replaced and be told apart by nothing else.
	let chain: Vec<(Uuid, chrono::DateTime<Utc>)> = versions
		.iter()
		.filter(|version| version.uuid() == replaced || !ctx.observed.minted(version.uuid()))
		.map(|version| (version.uuid(), version.timestamp()))
		.collect();
	let buried = crate::sync_engine::plan::interleaved_version(&chain, ours, replaced)?;
	let buried = versions
		.into_iter()
		.find(|version| version.uuid() == buried)?;
	// Landing on a version nobody told us about is only a DIVERGENCE if its bytes differ from
	// ours: two clients making the identical edit at once bury each other's copy and lose nothing.
	// A version the server stored no hash for is no evidence of one either — the same rule the
	// two-way reconcile applies to a foreign version sitting at a path.
	match (ours_hash, buried.metadata().hash()) {
		(Some(ours), Some(theirs)) if ours != theirs => Some(buried),
		_ => None,
	}
}

/// The row that holds a buried concurrent edit for the caller to resolve.
///
/// Its local half is our own copy — which is also what the remote head now carries, since our
/// upload won — and its remote half names the BURIED version: the uuid, hash and size
/// [`resolve_conflict`](super::SyncEngine::resolve_conflict) needs to restore or fetch it.
fn overwritten_entry(
	rel_path: &str,
	local: Option<&LocalNode>,
	buried: &crate::fs::file::FileVersion,
) -> BaselineEntry {
	BaselineEntry {
		rel_path: rel_path.to_string(),
		kind: NodeKind::File,
		remote_uuid: Some(buried.uuid()),
		content_hash: local.and_then(|l| l.content_hash),
		size: local.map(|l| l.size),
		local_mtime: local.map(|l| l.mtime_millis),
		remote_modified: Some(buried.timestamp().timestamp_millis()),
		state: BaselineState::Overwritten,
		local_kind: Some(NodeKind::File),
		remote_kind: Some(NodeKind::File),
		remote_hash: buried.metadata().hash(),
		remote_size: Some(buried.size()),
		remote_stable_uuid: Some(buried.stable_uuid()),
		// Our push buried theirs: there is no content the two sides ever agreed on here.
		agreed_hash: None,
	}
}

/// Record a two-way conflict in the baseline: the path is HELD (excluded from planning, along with
/// its subtree) until [`SyncEngine::resolve_conflict`](super::SyncEngine::resolve_conflict) picks a
/// winner. The row keeps both sides' evidence as of this pass — the local kind/hash/size/mtime the
/// scan saw and the remote kind/uuid/hash/size — so the resolution can re-anchor the row to the
/// winner and leave the loser reading as stale on the next pass.
pub(super) async fn record_conflict(
	store: &Mutex<BaselineStore>,
	pair: PairId,
	rel_path: &str,
	local: Option<&LocalNode>,
	remote: Option<&RemoteNode>,
) -> Result<(), crate::Error> {
	let entry = BaselineEntry {
		rel_path: rel_path.to_string(),
		// `kind` is NOT NULL; the per-side kinds below are what resolution reads.
		kind: local
			.map(|l| l.kind)
			.or_else(|| remote.map(|r| r.kind))
			.unwrap_or(NodeKind::File),
		remote_uuid: remote.map(|r| r.remote_uuid),
		content_hash: local.and_then(|l| l.content_hash),
		size: local.map(|l| l.size),
		local_mtime: local.map(|l| l.mtime_millis),
		remote_modified: remote.map(|r| r.modified_millis),
		state: BaselineState::Conflicted,
		local_kind: local.map(|l| l.kind),
		remote_kind: remote.map(|r| r.kind),
		remote_hash: remote.and_then(|r| r.content_hash),
		remote_size: remote.map(|r| r.size),
		remote_stable_uuid: remote.and_then(|r| r.stable_uuid),
		// While the divergence is held there is no agreed content; resolving it records one again.
		agreed_hash: None,
	};
	store
		.lock()
		.await
		.upsert_entry(pair, &entry)
		.map_err(db_err)
}

fn local_mtime_of(path: &Path) -> Option<i64> {
	use crate::io::FilenMetaExt;
	std::fs::metadata(path)
		.ok()
		.map(|m| FilenMetaExt::modified(&m).timestamp_millis())
}

/// The synced baseline row a directory write leaves behind.
fn dir_entry(rel_path: &str, remote_uuid: Option<Uuid>, local_mtime: Option<i64>) -> BaselineEntry {
	BaselineEntry {
		rel_path: rel_path.to_string(),
		kind: NodeKind::Dir,
		remote_uuid,
		content_hash: None,
		size: None,
		local_mtime,
		remote_modified: None,
		state: BaselineState::Synced,
		local_kind: None,
		remote_kind: None,
		remote_hash: None,
		remote_size: None,
		// A directory has no whole-life id of its own: its uuid already survives its renames.
		remote_stable_uuid: None,
		// Nor any content for the two sides to agree on.
		agreed_hash: None,
	}
}

/// The synced baseline row a file write leaves behind.
///
/// `agreed_hash` is the caller's to decide, because only the caller knows what its write proved:
/// a pull records the content it just fetched (both sides demonstrably hold it), a push carries
/// the PREVIOUS agreed content forward (the upload says nothing about what the remote holds by the
/// time we look again), and a move carries the moved row's marker to its new path. See
/// [`BaselineEntry::agreed_hash`].
#[allow(clippy::too_many_arguments)]
fn file_entry(
	rel_path: &str,
	remote_uuid: Option<Uuid>,
	remote_stable_uuid: Option<StableUuid>,
	content_hash: Option<filen_types::crypto::Blake3Hash>,
	size: u64,
	local_mtime: Option<i64>,
	remote_modified: Option<i64>,
	agreed_hash: Option<filen_types::crypto::Blake3Hash>,
) -> BaselineEntry {
	BaselineEntry {
		rel_path: rel_path.to_string(),
		kind: NodeKind::File,
		remote_uuid,
		content_hash,
		size: Some(size),
		local_mtime,
		remote_modified,
		state: BaselineState::Synced,
		local_kind: None,
		remote_kind: None,
		remote_hash: None,
		remote_size: None,
		remote_stable_uuid,
		agreed_hash,
	}
}

/// The baseline row a [`SyncAction::MoveLocal`] leaves at the destination — decided purely (no I/O)
/// so the policy is unit-testable; `local_mtime` is the renamed file's, read by the caller.
///
/// The row follows the LINEAGE — what the file held at its old path, now at its new one — not the
/// destination's current remote state. Anchoring it to the latter would be a lie whenever the move
/// carried a content edit: the renamed copy is still the PRE-edit one, and a row claiming the new
/// version's hash and size is one the scanner's fast-path can never catch up with, so the stale
/// bytes would eventually be pushed back over the remote's edit. The reconciler pairs such a move
/// with a download that corrects the row; this is what keeps the row honest if that download fails.
///
/// The same goes for a row that records NO local content — a `KeepLocal` resolution clears the local
/// half on purpose and leaves it that way until the re-push. The destination's remote state is not a
/// stand-in for it: with the kept copy and the remote's the same length, a row carrying the remote's
/// hash and size would satisfy the scanner's `(size, mtime)` fast path at the new path forever, so
/// the push the row is waiting for would never happen and the next foreign edit would overwrite the
/// kept bytes without even a quarantine. Recording nothing makes the scanner re-hash there, which is
/// what puts the pending push back on the next pass.
pub(super) fn moved_file_row(
	to_path: &str,
	base: Option<&BaselineEntry>,
	remote: Option<&RemoteNode>,
	local_mtime: Option<i64>,
) -> BaselineEntry {
	let (content_hash, size, local_mtime) = match base.and_then(|b| b.content_hash) {
		Some(hash) => (
			Some(hash),
			base.and_then(|b| b.size).unwrap_or(0),
			local_mtime,
		),
		None => (None, 0, None),
	};
	file_entry(
		to_path,
		base.and_then(|b| b.remote_uuid)
			.or_else(|| remote.map(|n| n.remote_uuid)),
		base.and_then(|b| b.remote_stable_uuid)
			.or_else(|| remote.and_then(|n| n.stable_uuid)),
		content_hash,
		size,
		local_mtime,
		remote.map(|n| n.modified_millis),
		// The row follows the moved file, and so does its agreed content. A move that carried an
		// edit is paired with a download, which re-records both.
		base.and_then(|b| b.agreed_hash),
	)
}

async fn upsert_dir_baseline(
	ctx: &ApplyContext<'_>,
	rel_path: &str,
	remote_uuid: Option<Uuid>,
	local_mtime: Option<i64>,
) -> Result<(), crate::Error> {
	upsert_baseline(ctx, &dir_entry(rel_path, remote_uuid, local_mtime)).await
}

#[allow(clippy::too_many_arguments)]
async fn upsert_file_baseline(
	ctx: &ApplyContext<'_>,
	rel_path: &str,
	remote_uuid: Option<Uuid>,
	remote_stable_uuid: Option<StableUuid>,
	content_hash: Option<filen_types::crypto::Blake3Hash>,
	size: u64,
	local_mtime: Option<i64>,
	remote_modified: Option<i64>,
	agreed_hash: Option<filen_types::crypto::Blake3Hash>,
) -> Result<(), crate::Error> {
	upsert_baseline(
		ctx,
		&file_entry(
			rel_path,
			remote_uuid,
			remote_stable_uuid,
			content_hash,
			size,
			local_mtime,
			remote_modified,
			agreed_hash,
		),
	)
	.await
}

/// Journal a remote write together with the baseline rows it produced — ONE transaction, so a
/// crash cannot leave the write recorded without its state or the state without its record — and
/// then publish the write to the in-memory journal the current process folds from.
async fn commit_remote_write(
	ctx: &ApplyContext<'_>,
	uuid: Uuid,
	kind: PendingKind,
	changes: &[BaselineChange<'_>],
) -> Result<(), crate::Error> {
	ctx.store
		.lock()
		.await
		.record_pending(
			ctx.pair,
			uuid,
			&kind,
			Utc::now().timestamp_millis(),
			changes,
		)
		.map_err(db_err)?;
	ctx.pending.record(ctx.observed, ctx.pair, uuid, kind);
	Ok(())
}

async fn upsert_baseline(
	ctx: &ApplyContext<'_>,
	entry: &BaselineEntry,
) -> Result<(), crate::Error> {
	ctx.store
		.lock()
		.await
		.upsert_entry(ctx.pair, entry)
		.map_err(db_err)
}

async fn delete_baseline(ctx: &ApplyContext<'_>, rel_path: &str) -> Result<(), crate::Error> {
	ctx.store
		.lock()
		.await
		.delete_entry(ctx.pair, rel_path)
		.map_err(db_err)
}

/// Move a locally-deleted item into the pair's quarantine dir (recoverable) rather than destroying
/// it. The original tree position is preserved under the quarantine root. The destination is made
/// UNIQUE on collision (` (N)` suffix): two deletions at the same path over time must not overwrite
/// each other (data loss), and renaming a directory onto an existing non-empty quarantine entry
/// would otherwise fail and wedge the pass forever (a re-failing, never-advancing deletion).
///
/// Returns where the item went, so a caller whose write then does NOT happen can put it back (see
/// [`restore_stashed`]); `None` when there was nothing to move.
pub(super) fn quarantine_local(
	root: &Path,
	rel_path: &str,
) -> Result<Option<PathBuf>, crate::Error> {
	let source = local_path(root, rel_path);
	// A missing source is a no-op — e.g. it already moved as part of an ancestor's quarantine.
	if source.symlink_metadata().is_err() {
		return Ok(None);
	}
	let dest = unique_quarantine_dest(local_path(&root.join(QUARANTINE_DIR), rel_path));
	if let Some(parent) = dest.parent() {
		std::fs::create_dir_all(parent).map_err(io_err)?;
	}
	std::fs::rename(&source, &dest).map_err(io_err)?;
	Ok(Some(dest))
}

/// Put a [`stash_local_target`] back where it came from, for a write that never landed after all —
/// a download the pair's pause dropped mid-flight. Without this the path is left EMPTY: the next
/// pass reads a baseline row with nothing on disk as a local deletion and conflicts over a
/// deletion nobody made, for a file whose only copy sits in the quarantine bin.
///
/// Never overwrites: something back at the path is somebody's newer write, and the stashed copy
/// stays recoverable in the bin rather than being put on top of it. Best-effort for the same
/// reason — a restore that fails leaves the copy where it is still safe.
fn restore_stashed(stashed: Option<PathBuf>, path: &Path) {
	let Some(stashed) = stashed else {
		return;
	};
	if path.exists() {
		return;
	}
	if let Err(error) = std::fs::rename(&stashed, path) {
		tracing::warn!(
			"failed to restore {} from the quarantine bin: {error}",
			path.display()
		);
	}
}

/// Stash whatever occupies a local move's destination, unless the destination is the SOURCE seen
/// under another spelling: a case-insensitive filesystem resolves both ends of a case-only rename
/// to one file, and stashing it would carry the file off and leave the rename with nothing to
/// rename.
///
/// Which of the two it is cannot be settled by asking the filesystem for the path — `metadata`
/// case-folds exactly as `rename` does, and so does `canonicalize` — nor by case-folding the two
/// paths, which calls every case-only rename a self-move even where the filesystem keeps the two
/// names apart. It is the parent directory that knows: the destination is a file of its own
/// exactly when the directory lists that name literally. `from_path` comes from the scan, so it
/// carries the source's on-disk spelling and cannot be the name found here.
fn stash_move_target(
	local_root: &Path,
	to_path: &str,
	base: Option<&BaselineEntry>,
) -> Result<(), crate::Error> {
	if has_own_directory_entry(&local_path(local_root, to_path)) {
		// The rename that follows always lands, so nothing here is ever put back. No scanned hash:
		// the scan saw this destination free, so whatever is here now arrived after it.
		let _stashed = stash_local_target(local_root, to_path, base, None)?;
	}
	Ok(())
}

/// Whether `path`'s parent directory lists `path`'s file name byte for byte.
fn has_own_directory_entry(path: &Path) -> bool {
	let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
		return false;
	};
	std::fs::read_dir(parent).is_ok_and(|entries| {
		entries
			.flatten()
			.any(|entry| entry.file_name().as_os_str() == name)
	})
}

/// A quarantine destination that does not already exist: `base`, else `base (1)`, `base (2)`, ...
/// (the suffix is appended to the whole file name, which is fine for a recovery bin).
fn unique_quarantine_dest(base: PathBuf) -> PathBuf {
	if base.symlink_metadata().is_err() {
		return base;
	}
	let parent = base
		.parent()
		.map(Path::to_path_buf)
		.unwrap_or_else(|| PathBuf::from("."));
	let name = base
		.file_name()
		.and_then(|n| n.to_str())
		.unwrap_or("quarantined");
	for n in 1..100_000 {
		let candidate = parent.join(format!("{name} ({n})"));
		if candidate.symlink_metadata().is_err() {
			return candidate;
		}
	}
	base
}

fn io_err(error: std::io::Error) -> crate::Error {
	crate::Error::custom_with_source(crate::ErrorKind::IO, error, None::<String>)
}

fn db_err(error: rusqlite::Error) -> crate::Error {
	crate::Error::custom_with_source(
		crate::ErrorKind::Internal,
		error,
		Some("baseline".to_string()),
	)
}

fn internal(message: &'static str) -> crate::Error {
	crate::Error::custom(crate::ErrorKind::Internal, message)
}

fn internal_owned(message: String) -> crate::Error {
	crate::Error::custom(crate::ErrorKind::Internal, message)
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use filen_types::crypto::Blake3Hash;
	use uuid::Uuid;

	use super::*;

	fn upload(rel_path: &str) -> SyncAction {
		SyncAction::UploadFile {
			rel_path: rel_path.to_string(),
		}
	}

	fn download(rel_path: &str) -> SyncAction {
		SyncAction::DownloadFile {
			rel_path: rel_path.to_string(),
			remote_uuid: Uuid::new_v4(),
		}
	}

	fn create_dir(rel_path: &str) -> SyncAction {
		SyncAction::CreateRemoteDir {
			rel_path: rel_path.to_string(),
		}
	}

	fn trash(rel_path: &str, kind: NodeKind) -> SyncAction {
		SyncAction::TrashRemote {
			rel_path: rel_path.to_string(),
			kind,
			remote_uuid: Uuid::new_v4(),
		}
	}

	fn move_dir(from_path: &str, to_path: &str) -> SyncAction {
		SyncAction::MoveRemote {
			from_path: from_path.to_string(),
			to_path: to_path.to_string(),
			kind: NodeKind::Dir,
			remote_uuid: Uuid::new_v4(),
		}
	}

	fn release_points_of(order: &[SyncAction]) -> Vec<bool> {
		let refs: Vec<&SyncAction> = order.iter().collect();
		release_points(&refs, &create_target_paths(order))
	}

	/// Between independent actions — and across phases no tie crosses — the lock may go anywhere.
	#[test]
	fn independent_actions_may_release_between_any_two() {
		let order = [
			create_dir("a"),
			create_dir("a/b"),
			upload("a/x"),
			download("y"),
			trash("old", NodeKind::File),
		];
		assert_eq!(release_points_of(&order), vec![true; order.len()]);
	}

	/// A type flip empties its path until the create at it runs: no release may land in between,
	/// whatever runs between the two halves.
	#[test]
	fn a_release_never_splits_a_replace_delete_from_its_create() {
		let order = [
			trash("x", NodeKind::File),
			create_dir("a"),
			create_dir("x"),
			create_dir("z"),
		];
		assert_eq!(release_points_of(&order), vec![true, false, false, true]);

		// The directory half of a flip is recreated by a TRANSFER, in the next phase.
		let order = [trash("x", NodeKind::Dir), upload("x"), upload("y")];
		assert_eq!(release_points_of(&order), vec![true, false, true]);
	}

	/// What the plan names under a moved directory — a nested move, a create, a transfer — assumes
	/// the move just happened; a deletion under it names its item by uuid and waits on nothing.
	#[test]
	fn a_release_never_splits_a_directory_move_from_what_it_carries() {
		let order = [
			move_dir("d", "e"),
			move_dir("e/in", "e/in2"),
			create_dir("a"),
			create_dir("e/in2/new"),
			upload("e/in2/f"),
			upload("g"),
			trash("e/old", NodeKind::File),
		];
		assert_eq!(
			release_points_of(&order),
			vec![true, false, false, false, false, true, true]
		);

		// A sibling that only shares the name's prefix is not under the moved directory.
		let order = [move_dir("d", "e"), upload("ex"), upload("e2/f")];
		assert_eq!(release_points_of(&order), vec![true, true, true]);
	}

	/// The transfers that finish a compound step go first, so the step — and the stretch the lock
	/// cannot be let go in — ends as early as it can; then the downloads, which need no lock, then the
	/// uploads.
	#[test]
	fn transfers_that_finish_a_compound_step_run_first_then_downloads_then_uploads() {
		let pre = [trash("flip", NodeKind::Dir), move_dir("d", "e")];
		let mut transfers = [
			upload("a"),
			download("b"),
			upload("e/child"),
			download("c"),
			upload("flip"),
		];
		order_transfers(&pre, &mut transfers);
		let paths: Vec<&str> = transfers.iter().map(SyncAction::rel_path).collect();
		assert_eq!(paths, ["e/child", "flip", "b", "c", "a"]);
	}

	/// A hold runs its course on time or on count, whichever comes first.
	#[test]
	fn a_hold_is_spent_after_its_time_or_its_batch() {
		let budget = LockBudget::default();
		let since = Instant::now();
		assert!(!hold_spent(&budget, since, 0, since));
		assert!(!hold_spent(
			&budget,
			since,
			budget.batch - 1,
			since + budget.hold - Duration::from_millis(1)
		));
		assert!(hold_spent(&budget, since, 0, since + budget.hold));
		assert!(hold_spent(&budget, since, budget.batch, since));
	}

	/// Without a held lock only a remote write has to wait for one; nothing that is merely local or a
	/// download does, released or not.
	#[test]
	fn only_a_remote_write_waits_for_the_lock() {
		let now = Instant::now();
		let budget = LockBudget::default();
		for lease in [Lease::Unheld, Lease::Released { at: now }] {
			assert!(lease.admits(&budget, &download("a"), true, now));
			assert!(!lease.admits(&budget, &trash("b", NodeKind::File), false, now));
			assert!(!lease.admits(&budget, &upload("c"), true, now));
			assert!(lease.admits(
				&budget,
				&SyncAction::CreateLocalDir {
					rel_path: "d".to_string()
				},
				true,
				now
			));
		}
	}

	fn over_quota() -> crate::Error {
		filen_types::error::ResponseError::ApiError {
			message: None,
			code: Some("max_storage_reached".to_string()),
		}
		.into()
	}

	fn io_kind(kind: std::io::ErrorKind) -> std::io::Error {
		std::io::Error::from(kind)
	}

	/// The SDK reports byte deltas; the engine turns them into a running total for the path, with
	/// the planned size alongside.
	#[test]
	fn progress_callback_reports_a_running_total() {
		let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
		let callback = progress_callback(&sender, "dir/f.bin", 12);
		callback(5);
		callback(7);
		drop(callback);
		let progress = |bytes| SyncEvent::Progress {
			rel_path: "dir/f.bin".to_string(),
			bytes,
			total: 12,
		};
		assert_eq!(receiver.try_recv().unwrap(), progress(5));
		assert_eq!(receiver.try_recv().unwrap(), progress(12));
		assert!(receiver.try_recv().is_err());
	}

	/// A full disk or a full account is found wherever the SDK put it: straight from a local write
	/// (`?` on an io result, or the engine's own `io_err`), from a server response, and inside the
	/// `io::Error` the upload writer wraps a server refusal in. Anything else stays a per-path
	/// failure.
	#[test]
	fn storage_full_is_found_anywhere_in_the_error_chain() {
		use std::io::ErrorKind as Io;

		let local = Some(HaltReason::LocalStorageFull);
		assert_eq!(
			pass_halt(&crate::Error::from(io_kind(Io::StorageFull))),
			local
		);
		assert_eq!(pass_halt(&io_err(io_kind(Io::StorageFull))), local);
		assert_eq!(pass_halt(&io_err(io_kind(Io::QuotaExceeded))), local);
		assert_eq!(
			pass_halt(&crate::Error::from(std::io::Error::other(io_err(io_kind(
				Io::StorageFull
			))))),
			local
		);

		let remote = Some(HaltReason::RemoteStorageFull);
		assert_eq!(pass_halt(&over_quota()), remote);
		assert_eq!(pass_halt(&over_quota().with_context("uploading")), remote);
		assert_eq!(
			pass_halt(&crate::Error::from(std::io::Error::other(over_quota()))),
			remote,
			"the upload writer hands a failed chunk on inside an io::Error"
		);
		assert_eq!(
			pass_halt(&crate::Error::custom_with_source(
				crate::ErrorKind::Server,
				filen_types::error::ResponseError::ApiError {
					message: None,
					code: Some("max_storage_reached".to_string()),
				},
				None::<String>,
			)),
			remote,
			"the server's code decides, whatever kind it was wrapped under"
		);

		assert_eq!(
			pass_halt(&crate::Error::from(io_kind(Io::PermissionDenied))),
			None
		);
		assert_eq!(
			pass_halt(&crate::Error::from(std::io::Error::other("disk on fire"))),
			None
		);
		assert_eq!(
			pass_halt(
				&filen_types::error::ResponseError::ApiError {
					message: None,
					code: Some("file_not_found".to_string()),
				}
				.into()
			),
			None
		);
		assert_eq!(pass_halt(&internal("boom")), None);
	}

	/// A storage-full failure halts the pass once, with one line saying why, and counts against no
	/// path; an ordinary failure next to it still counts against its own.
	#[test]
	fn a_storage_full_failure_halts_the_pass_instead_of_counting_against_its_path() {
		let mut report = SyncReport::default();
		note_failure(
			&mut report,
			"locked.txt",
			&crate::Error::from(io_kind(std::io::ErrorKind::PermissionDenied)),
		);
		note_failure(
			&mut report,
			"big.bin",
			&io_err(io_kind(std::io::ErrorKind::StorageFull)),
		);
		note_failure(
			&mut report,
			"again.bin",
			&io_err(io_kind(std::io::ErrorKind::StorageFull)),
		);
		note_failure(&mut report, "other.bin", &over_quota());

		assert_eq!(report.halted, Some(HaltReason::LocalStorageFull));
		assert_eq!(
			report
				.failed_paths
				.iter()
				.map(|(path, _)| path.as_str())
				.collect::<Vec<_>>(),
			["locked.txt"],
			"only the failure that is the path's own counts towards a streak"
		);
		for reason in [HaltReason::LocalStorageFull, HaltReason::RemoteStorageFull] {
			assert_eq!(
				report
					.errors
					.iter()
					.filter(|line| **line == reason.held_line())
					.count(),
				1,
				"one line per full side: {:?}",
				report.errors
			);
		}
		assert_eq!(report.errors.len(), 6, "{:?}", report.errors);
		assert_eq!(report.interrupted, 0);
	}

	/// A full side holds back only the transfers that write to it: a full account stops uploads and
	/// lets downloads (and every non-transfer action) through, a full disk the reverse.
	#[test]
	fn a_full_side_holds_only_the_transfers_that_write_to_it() {
		let upload = SyncAction::UploadFile {
			rel_path: "up.bin".to_string(),
		};
		let download = SyncAction::DownloadFile {
			rel_path: "down.bin".to_string(),
			remote_uuid: Uuid::new_v4(),
		};
		let trash = SyncAction::TrashRemote {
			rel_path: "gone.bin".to_string(),
			kind: NodeKind::File,
			remote_uuid: Uuid::new_v4(),
		};
		let actions = [&upload, &download, &trash];
		let held = |full: &FullSides| actions.map(|action| full.holds(action));

		let full = FullSides::default();
		assert_eq!(held(&full), [false, false, false]);
		full.note(HaltReason::RemoteStorageFull);
		assert_eq!(held(&full), [true, false, false]);
		let full = FullSides::default();
		full.note(HaltReason::LocalStorageFull);
		assert_eq!(held(&full), [false, true, false]);
		full.note(HaltReason::RemoteStorageFull);
		assert_eq!(held(&full), [true, true, false]);
	}

	fn temp_dir() -> PathBuf {
		let dir = std::env::temp_dir().join(format!("filen_confine_test_{}", Uuid::new_v4()));
		std::fs::create_dir_all(&dir).unwrap();
		dir
	}

	/// The shape a case-only directory rename planned per path takes on the remote: the create at
	/// the new spelling is answered by the dedup with the EXISTING directory's uuid, and the trash
	/// at the old spelling then names that same uuid. The trash must be refused.
	#[test]
	fn a_trash_of_a_directory_this_pass_created_is_refused() {
		let existing = Uuid::new_v4();
		let mut writes = PassWrites::default();
		writes.note(&SyncAction::CreateRemoteDir {
			rel_path: "docs".to_string(),
		});
		writes.remote_uuids.insert(existing);
		assert!(writes.shields_remote("Docs", existing), "same uuid");
		assert!(
			writes.shields_remote("Docs", Uuid::new_v4()),
			"same name under another spelling"
		);
		assert!(
			!writes.shields_remote("Other", Uuid::new_v4()),
			"an unrelated trash is not held"
		);
	}

	/// A trash or quarantine of a directory that now holds something this pass wrote — a child
	/// moved or downloaded into it under another spelling — would take that write down with it.
	#[test]
	fn a_delete_above_a_write_of_this_pass_is_refused() {
		let mut writes = PassWrites::default();
		writes.note(&SyncAction::MoveRemote {
			from_path: "Docs/a.txt".to_string(),
			to_path: "docs/a.txt".to_string(),
			kind: NodeKind::File,
			remote_uuid: Uuid::new_v4(),
		});
		writes.note(&SyncAction::DownloadFile {
			rel_path: "docs/b.txt".to_string(),
			remote_uuid: Uuid::new_v4(),
		});
		assert!(writes.shields_remote("Docs", Uuid::new_v4()));
		assert!(writes.shields_local("Docs"));
		assert!(
			!writes.shields_local("Docs/c.txt"),
			"a sibling of the write is not held"
		);
		assert!(
			!writes.shields_local("doc"),
			"a name sharing only a prefix is not held"
		);
	}

	#[test]
	fn a_key_sorting_between_a_dir_and_its_children_does_not_hide_them() {
		// `docs-x` sorts after `docs` but before `docs/`, so a range read has to start at the
		// child prefix rather than at the key itself.
		let keys = BTreeSet::from(["docs-x".to_string(), "docs/a.txt".to_string()]);
		assert!(holds_key_at_or_under(&keys, "Docs"));
		assert!(!holds_key_at_or_under(
			&BTreeSet::from(["docs-x".to_string()]),
			"Docs"
		));
	}

	/// A move nested inside another commits the rows of its own step only: after the outer move the
	/// inner directory's rows sit where the disk has them until the inner move runs, nothing stays
	/// behind under the outer source, and an inner move that never runs leaves the store agreeing
	/// with the disk.
	#[test]
	fn nested_dir_moves_commit_each_step_from_the_store() {
		let store = BaselineStore::open_in_memory().unwrap();
		let (pair, _) = store
			.create_pair("/local", Uuid::new_v4(), super::super::SyncMode::TwoWay)
			.unwrap();
		for path in ["D", "D/in", "D/in/x"] {
			store
				.upsert_entry(pair, &dir_entry(path, Some(Uuid::new_v4()), None))
				.unwrap();
		}
		let paths = |store: &BaselineStore| -> Vec<String> {
			store
				.entries(pair)
				.unwrap()
				.into_iter()
				.map(|entry| entry.rel_path)
				.collect()
		};
		let step = |from, to| {
			store
				.apply_changes(pair, &[BaselineChange::MoveSubtree { from, to }])
				.unwrap();
		};

		step("D", "E");
		assert_eq!(
			paths(&store),
			["E", "E/in", "E/in/x"],
			"the outer move committed more than its own step"
		);
		step("E/in", "E/in2");
		assert_eq!(paths(&store), ["E", "E/in2", "E/in2/x"]);
	}

	#[test]
	fn confine_allows_paths_under_the_root() {
		let root = temp_dir();
		let target = confined_local_target(&root, "sub/file.txt").expect("in-root path allowed");
		assert!(target.starts_with(&root));
		std::fs::remove_dir_all(&root).ok();
	}

	#[test]
	fn confine_rejects_lexical_traversal() {
		let root = temp_dir();
		assert!(
			confined_local_target(&root, "../escape.txt").is_err(),
			".. component must be refused"
		);
		assert!(confined_local_target(&root, "a/../../b").is_err());
		std::fs::remove_dir_all(&root).ok();
	}

	#[test]
	fn confine_rejects_writing_through_a_symlink_escaping_the_root() {
		let root = temp_dir();
		let outside = temp_dir(); // a sibling dir, NOT under root
		// `root/link` -> `outside`. Writing `root/link/x` would land in `outside` without confinement.
		std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();

		let result = confined_local_target(&root, "link/x.txt");
		assert!(
			result.is_err(),
			"writing through a symlink that escapes the root must be refused, got {result:?}"
		);

		std::fs::remove_dir_all(&root).ok();
		std::fs::remove_dir_all(&outside).ok();
	}

	#[test]
	fn confine_allows_a_symlink_that_stays_within_the_root() {
		let root = temp_dir();
		std::fs::create_dir_all(root.join("real")).unwrap();
		std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
		// `root/link` -> `root/real` (inside) — writing through it stays confined.
		assert!(confined_local_target(&root, "link/x.txt").is_ok());
		std::fs::remove_dir_all(&root).ok();
	}

	#[test]
	fn quarantine_makes_a_unique_dest_on_collision_never_overwriting() {
		let root = temp_dir();
		let trash = root.join(QUARANTINE_DIR);

		std::fs::write(root.join("x.txt"), b"first").unwrap();
		assert_eq!(
			quarantine_local(&root, "x.txt").unwrap(),
			Some(trash.join("x.txt"))
		);
		assert_eq!(std::fs::read(trash.join("x.txt")).unwrap(), b"first");

		// A second deletion at the same rel path must NOT clobber the first quarantined copy.
		std::fs::write(root.join("x.txt"), b"second").unwrap();
		assert_eq!(
			quarantine_local(&root, "x.txt").unwrap(),
			Some(trash.join("x.txt (1)"))
		);
		assert_eq!(
			std::fs::read(trash.join("x.txt")).unwrap(),
			b"first",
			"first copy preserved"
		);
		assert_eq!(
			std::fs::read(trash.join("x.txt (1)")).unwrap(),
			b"second",
			"second copy under a unique name"
		);
		std::fs::remove_dir_all(&root).ok();
	}

	fn local_node(rel: &str, mtime: i64, hash: Option<Blake3Hash>) -> LocalNode {
		LocalNode {
			rel_path: rel.to_string(),
			kind: NodeKind::File,
			size: 3,
			mtime_millis: mtime,
			content_hash: hash,
		}
	}

	fn remote_node(rel: &str, hash: Option<Blake3Hash>) -> RemoteNode {
		let uuid = Uuid::new_v4();
		RemoteNode {
			rel_path: rel.to_string(),
			kind: NodeKind::File,
			remote_uuid: uuid,
			stable_uuid: Some(StableUuid::new_for_test(uuid)),
			content_hash: hash,
			size: 3,
			modified_millis: 900,
		}
	}

	fn baseline_file(rel: &str, hash: Option<Blake3Hash>) -> BaselineEntry {
		BaselineEntry {
			rel_path: rel.to_string(),
			kind: NodeKind::File,
			remote_uuid: Some(Uuid::new_v4()),
			content_hash: hash,
			size: Some(3),
			local_mtime: Some(10),
			remote_modified: Some(10),
			state: BaselineState::Synced,
			local_kind: None,
			remote_kind: None,
			remote_hash: None,
			remote_size: None,
			remote_stable_uuid: None,
			agreed_hash: None,
		}
	}

	/// A file on disk plus a baseline row that vouches for exactly it.
	fn synced_file(root: &Path, bytes: &[u8]) -> (PathBuf, BaselineEntry) {
		let path = root.join("a.txt");
		std::fs::write(&path, bytes).unwrap();
		let mut base = baseline_file("a.txt", Some(Blake3Hash::from([1; 32])));
		base.size = Some(bytes.len() as u64);
		base.local_mtime = local_mtime_of(&path);
		(path, base)
	}

	#[test]
	fn a_pull_over_an_unmodified_local_file_does_not_quarantine() {
		let root = temp_dir();
		let (path, base) = synced_file(&root, b"abc");
		assert!(!local_holds_unsynced_content(&path, Some(&base)));
		std::fs::remove_dir_all(&root).ok();
	}

	#[test]
	fn a_pull_over_a_file_edited_after_the_scan_quarantines_first() {
		// The scan snapshot still says this file matches its baseline; the file on disk no longer
		// does. A pass waits on the drive lock between the scan and the download, so the decision
		// has to be read from disk.
		let root = temp_dir();
		let (path, base) = synced_file(&root, b"abc");
		std::fs::write(&path, b"edited after the scan").unwrap();
		assert!(
			local_holds_unsynced_content(&path, Some(&base)),
			"a file that no longer matches its baseline row must be stashed"
		);
		assert!(
			local_holds_unsynced_content(&path, None),
			"a file the baseline never recorded must be stashed"
		);
		std::fs::remove_dir_all(&root).ok();
	}

	#[test]
	fn a_pull_over_a_local_dir_quarantines_first() {
		let root = temp_dir();
		let path = root.join("a.txt");
		std::fs::create_dir(&path).unwrap();
		let base = baseline_file("a.txt", Some(Blake3Hash::from([1; 32])));
		assert!(
			local_holds_unsynced_content(&path, Some(&base)),
			"a directory where a file is being pulled must be stashed, not written over"
		);
		std::fs::remove_dir_all(&root).ok();
	}

	#[test]
	fn a_pull_with_nothing_local_does_not_quarantine() {
		let root = temp_dir();
		assert!(!local_holds_unsynced_content(
			&root.join("missing.txt"),
			None
		));
		std::fs::remove_dir_all(&root).ok();
	}

	#[test]
	fn adopt_records_the_scan_time_mtime_not_a_fresh_stat() {
		// The scan hashed the file at mtime 4242. Pairing that hash with any LATER mtime would
		// make a same-size edit made after the scan invisible to the fast-path forever.
		let hash = Blake3Hash::from([7; 32]);
		let local = local_node("a.txt", 4242, Some(hash));
		let remote = remote_node("a.txt", Some(hash));
		let AdoptOutcome::Record(entry) = adopt_outcome("a.txt", Some(&local), Some(&remote), true)
		else {
			panic!("both sides present must record a row");
		};
		assert_eq!(entry.local_mtime, Some(4242), "scan-time mtime recorded");
		assert_eq!(entry.content_hash, Some(hash));
		assert_eq!(entry.remote_uuid, Some(remote.remote_uuid));
		assert_eq!(entry.remote_modified, Some(900));
		// Adopting a converged path is the observation that BOTH sides hold this content, so it is
		// also what records the agreed content the concurrent-edit rule reads.
		assert_eq!(entry.agreed_hash, entry.content_hash);
		assert!(entry.agreed_hash.is_some());
	}

	/// A directory has no content, so nothing for the two sides to agree on.
	#[test]
	fn adopting_a_directory_records_no_agreed_content() {
		let local = local_node("d", 1, None);
		let mut remote = remote_node("d", None);
		remote.kind = NodeKind::Dir;
		match adopt_outcome("d", Some(&local), Some(&remote), true) {
			AdoptOutcome::Record(entry) => assert_eq!(entry.agreed_hash, None),
			other => panic!("expected a recorded row, got {other:?}"),
		}
	}

	#[test]
	fn adopt_of_a_convergent_delete_drops_the_row() {
		assert_eq!(
			adopt_outcome("gone.txt", None, None, true),
			AdoptOutcome::DropRow
		);
	}

	#[test]
	fn adopt_keeps_the_row_when_the_absence_evidence_is_untrusted() {
		// Incomplete scan / un-converged or empty remote view: both sides only LOOK gone. Dropping
		// the row here makes the next healthy pass see the surviving side as Created and resurrect
		// the item instead of propagating the deletion.
		assert_eq!(
			adopt_outcome("gone.txt", None, None, false),
			AdoptOutcome::Keep
		);
	}

	/// A local move whose destination is occupied by a file the baseline cannot vouch for must
	/// stash that file first: `rename` would overwrite it and lose it for good.
	#[test]
	fn a_local_move_onto_an_unsynced_file_stashes_it_first() {
		let root = temp_dir();
		std::fs::write(root.join("dest.txt"), b"never synced").unwrap();

		let _stashed = stash_local_target(&root, "dest.txt", None, None).unwrap();

		assert!(
			!root.join("dest.txt").exists(),
			"the destination is cleared for the rename"
		);
		assert_eq!(
			std::fs::read(root.join(QUARANTINE_DIR).join("dest.txt")).unwrap(),
			b"never synced",
			"and the file the baseline could not vouch for is recoverable"
		);

		std::fs::remove_dir_all(&root).ok();
	}

	/// A case-only rename resolves both ends of the move to one file on a case-insensitive
	/// filesystem: stashing "the destination" would carry the source away and leave the rename
	/// with nothing to rename.
	#[test]
	fn a_case_only_rename_is_not_treated_as_an_occupied_destination() {
		let root = temp_dir();
		std::fs::write(root.join("report.txt"), b"the source").unwrap();

		stash_move_target(&root, "REPORT.TXT", None).unwrap();

		assert_eq!(
			std::fs::read(root.join("report.txt")).unwrap(),
			b"the source",
			"the file the rename is about to move is left where it is"
		);
		assert!(
			!root.join(QUARANTINE_DIR).exists(),
			"nothing is quarantined"
		);
		std::fs::remove_dir_all(&root).ok();
	}

	/// Where the filesystem DOES keep the two spellings apart, the destination is somebody else's
	/// file and the rename would destroy it: case-folding the two paths would have waved it
	/// through as a self-move.
	#[test]
	fn a_destination_that_only_looks_like_the_source_is_still_stashed() {
		let root = temp_dir();
		std::fs::write(root.join("REPORT.TXT"), b"never synced").unwrap();

		stash_move_target(&root, "REPORT.TXT", None).unwrap();

		assert_eq!(
			std::fs::read(root.join(QUARANTINE_DIR).join("REPORT.TXT")).unwrap(),
			b"never synced"
		);
		std::fs::remove_dir_all(&root).ok();
	}

	/// The other side of it: a destination holding exactly what the baseline records is just an
	/// unmodified copy, and moving over it stashes nothing.
	#[test]
	fn a_local_move_onto_a_synced_copy_stashes_nothing() {
		let root = temp_dir();
		let (path, base) = synced_file(&root, b"abc");

		let stashed = stash_local_target(&root, "a.txt", Some(&base), None).unwrap();
		assert_eq!(stashed, None, "nothing was stashed, so nothing to put back");

		assert!(path.exists(), "an unmodified copy is left where it is");
		assert!(
			!root.join(QUARANTINE_DIR).exists(),
			"nothing is quarantined"
		);

		std::fs::remove_dir_all(&root).ok();
	}

	/// A same-size edit that kept its mtime passes the `(size, mtime)` test, so only the hash a deep
	/// scan read can tell it from the synced copy. A pull over it must stash it first; a scan hash
	/// matching the row stashes nothing.
	#[test]
	fn a_pull_over_an_edit_only_the_scan_hash_saw_quarantines_first() {
		let root = temp_dir();
		let (path, base) = synced_file(&root, b"abc");
		assert!(!local_holds_unsynced_content(&path, Some(&base)));

		let matching = stash_local_target(&root, "a.txt", Some(&base), base.content_hash).unwrap();
		assert_eq!(
			matching, None,
			"a scan that read the recorded hash vouches for the copy"
		);

		let stashed =
			stash_local_target(&root, "a.txt", Some(&base), Some(Blake3Hash::from([9; 32])))
				.unwrap()
				.expect("an edit the scan hashed must be stashed");
		assert_eq!(std::fs::read(&stashed).unwrap(), b"abc");
		assert!(
			!path.exists(),
			"the edit is moved out of the download's way"
		);

		std::fs::remove_dir_all(&root).ok();
	}

	/// A `KeepLocal` resolution clears the row's local half on purpose and leaves it that way until
	/// the re-push. A move must carry that emptiness across: filling it in from the DESTINATION's
	/// remote state would record bytes nobody ever compared, and where the kept copy happens to be
	/// the same length as the remote's the scanner's `(size, mtime)` fast path would vouch for them
	/// forever — the push the row is waiting for never happens, and the next foreign edit overwrites
	/// the kept copy without even a quarantine.
	#[test]
	fn moving_a_row_with_no_local_content_records_none_at_the_new_path() {
		let kept = BaselineEntry {
			content_hash: None,
			size: None,
			local_mtime: None,
			..baseline_file("a.txt", None)
		};
		let remote = remote_node("b.txt", Some(Blake3Hash::from([9; 32])));

		let row = moved_file_row("b.txt", Some(&kept), Some(&remote), Some(4242));
		assert_eq!(
			row.content_hash, None,
			"the destination's remote hash is not a stand-in for local content nobody recorded"
		);
		assert_eq!(
			row.local_mtime, None,
			"nor is there an mtime to fast-path on"
		);
		assert_ne!(
			row.size,
			Some(remote.size),
			"nor may the remote's size stand in for it"
		);
		// The remote anchor still follows the moved row: that half of it IS on record.
		assert_eq!(row.remote_uuid, kept.remote_uuid);

		// The negative: an ordinary move carries the local half it does have across untouched.
		let synced = baseline_file("a.txt", Some(Blake3Hash::from([1; 32])));
		let row = moved_file_row("b.txt", Some(&synced), Some(&remote), Some(4242));
		assert_eq!(row.content_hash, synced.content_hash);
		assert_eq!(row.size, synced.size);
		assert_eq!(row.local_mtime, Some(4242), "the renamed file's own mtime");
	}

	#[test]
	fn quarantine_of_a_missing_source_is_a_noop() {
		let root = temp_dir();
		// e.g. the item already moved as part of an ancestor's quarantine.
		assert_eq!(
			quarantine_local(&root, "already/gone.txt").expect("missing source is a no-op"),
			None
		);
		std::fs::remove_dir_all(&root).ok();
	}

	/// A download the pause dropped (or one that failed) never writes at its target, so the copy
	/// stashed out of its way goes BACK: left in the bin, the path is empty, and the next pass
	/// reads a baseline row with nothing on disk as a local deletion — a conflict over a deletion
	/// nobody made, for a file whose only copy is in the quarantine bin.
	#[test]
	fn a_write_that_never_landed_puts_its_stashed_target_back() {
		let root = temp_dir();
		std::fs::write(root.join("a.txt"), b"a local edit").unwrap();

		let stashed = stash_local_target(&root, "a.txt", None, None).unwrap();
		assert!(
			stashed.is_some(),
			"a copy the baseline cannot vouch for must be stashed before the download"
		);
		assert!(!root.join("a.txt").exists());

		restore_stashed(stashed, &root.join("a.txt"));

		assert_eq!(
			std::fs::read(root.join("a.txt")).unwrap(),
			b"a local edit",
			"the interrupted download left the path empty"
		);
		assert!(
			!root.join(QUARANTINE_DIR).join("a.txt").exists(),
			"and nothing is left duplicated in the bin"
		);
		std::fs::remove_dir_all(&root).ok();
	}

	/// Unless something is back at the path: that is somebody's newer write, and the stashed copy
	/// stays recoverable in the bin rather than being put on top of it.
	#[test]
	fn a_restore_never_overwrites_what_took_the_path() {
		let root = temp_dir();
		std::fs::write(root.join("a.txt"), b"a local edit").unwrap();
		let stashed = stash_local_target(&root, "a.txt", None, None).unwrap();
		std::fs::write(root.join("a.txt"), b"written since").unwrap();

		restore_stashed(stashed, &root.join("a.txt"));

		assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"written since");
		assert_eq!(
			std::fs::read(root.join(QUARANTINE_DIR).join("a.txt")).unwrap(),
			b"a local edit",
			"the stashed copy must stay recoverable"
		);
		std::fs::remove_dir_all(&root).ok();
	}
}

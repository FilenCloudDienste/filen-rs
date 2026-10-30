//! The apply layer: execute a screened [`SyncAction`] plan against the remote (via `Client`) and
//! the local filesystem, advancing the baseline after each action so an interrupted pass simply
//! re-reconciles to the same plan.
//!
//! Each action is best-effort: a failure is recorded in the [`SyncReport`] and the pass continues,
//! since the actions are independent and the next pass re-plans from truth. The exception is a
//! failure no transfer to that side can get past — a full local disk or a full account — which
//! stops the pass starting any more of those, and a remote out of reach or refusing the session,
//! which stops it starting any more remote actions (see [`SyncReport::halted`]). Local deletions move
//! to the pair's quarantine dir (recoverable for 30 days); remote deletions go to Filen trash. The
//! local mtime written to the baseline after a download is read back from disk (not the value we
//! asked for) so the scanner's fast-path stays stable.

use std::{
	collections::{HashMap, HashSet},
	ffi::OsStr,
	path::{Path, PathBuf},
	sync::{
		Arc, PoisonError,
		atomic::{AtomicBool, Ordering},
	},
	time::Duration,
};

use chrono::{DateTime, NaiveDate, Utc};
use filen_types::{
	api::v3::{bulk::FileOrDir, dir, file},
	fs::StableUuid,
};
use futures::{StreamExt, future::join_all, stream::FuturesUnordered};
use reqwest::StatusCode;
use uuid::Uuid;

use super::{
	baseline::{BaselineChange, BaselineEntry, BaselineState, NodeKind, PairId, PendingRow},
	bin::quarantine_local,
	changes::FullPassReason,
	engine::{
		LockBudget, Observations, PASS_LOCK_MAX_SLEEP, PendingKind, PendingWrites, SYNC_BULK_CHUNK,
		SharedStore, locked, off_store,
	},
	events::{SyncEvent, TransferDirection},
	guard::GuardReason,
	ignore::IgnoredPath,
	outcome::{
		HaltReason, PlannedAction, PlannedConflict, PlannedNodeKind, RefuseReason, UnsyncablePath,
	},
	pause::PassGate,
	plan::{RemoteNode, SyncAction, is_under},
	rows::{Baseline, Cursor},
	scan::{
		LocalNode, PairRoot, ScanGap, collision_key, folds_onto_legacy_trash, hash_file,
		is_account_data, is_legacy_trash,
	},
	side::{NodesAt, Side},
	stage::{Carried, Pager, Run, Screen, Staged, StagedPlan},
};
use crate::{
	ErrorKind,
	api::v3::bulk::trash,
	auth::{Client, http::is_connection_lost},
	cache::{SearchResult, hydrate_cached_items_at},
	connect::ParentTargets,
	fs::{
		HasUUID,
		bulk::create_dirs::{CreatedDir, NewDir, NewDirParent},
		categories::{DirType, Normal},
		dir::meta::DirectoryMetaChanges,
		file::{FileBuilderOptionalName, meta::FileMetaChanges},
		name::ValidatedName,
	},
	io::{
		RemoteDirectory, RemoteFile,
		client_impl::{KeepLocalWhen, download_to_path},
	},
	sync::lock::{ATTEMPTS_DEFAULT, ResourceLock, confirm_or_retake},
	util::{MaybeArc, MaybeSendCallback},
};
use tokio::{sync::mpsc::UnboundedSender, time::Instant};

/// Outcome of one apply pass.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SyncReport {
	pub downloaded: usize,
	pub uploaded: usize,
	pub local_dirs_created: usize,
	pub remote_dirs_created: usize,
	/// Items moved into the pair's local quarantine bin, where they are kept for 30 days and then
	/// deleted permanently — a directory counts once, whatever its subtree holds.
	pub locally_deleted: usize,
	/// Items moved to the Filen trash — a directory counts once, whatever its subtree holds.
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
	/// Set when the pass refused to run: nothing was applied. No pass sets it any more (see
	/// [`RefuseReason`]).
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
	/// batch changes or is held for another kind of [`guard`](Self::guard) reason, so an approval
	/// can never leak onto a different set of deletions, nor onto the same ones held for a reason
	/// the caller was never shown.
	pub deletion_token: Option<String>,
	/// Per-action failures (the pass continues past them; one that ran out of room or found the
	/// remote out of reach also holds back the actions behind it, see [`halted`](Self::halted)).
	pub errors: Vec<String>,
	/// The paths that left the local read incomplete, and why: the first 100 the read met, in the
	/// order it met them. Every deletion of a pass whose read was incomplete is held as
	/// [`GuardReason::ScanIncomplete`](super::GuardReason::ScanIncomplete), and these are what a
	/// caller can show for it. Empty on a complete read, and on an incomplete one that has no path
	/// to name: a walk cut short, or a name collision, which refuses the pass with its own report.
	/// A change-scoped pass that carries an earlier incomplete read on lists that read's paths too.
	pub scan_gaps: Vec<ScanGap>,
	/// How many such paths the read met, of which `scan_gaps` lists the first 100.
	pub scan_gaps_total: usize,
	/// Paths whose local copy this pass moved into the pair's quarantine bin to clear the way for a
	/// write: a remote-wins download over a copy the baseline cannot vouch for, and a move onto an
	/// occupied destination. The bytes stay recoverable under `.filen-sync-trash`, in the bucket of
	/// the UTC day the pass moved them (`.filen-sync-trash/YYYY-MM-DD/<path>`), for 30 days; then
	/// the engine deletes that bucket permanently.
	///
	/// Nothing else in the report mentions those paths — the edit such a copy holds is by
	/// definition one no scan could see (that is why the write was planned at all), and no action
	/// failed — so without this the file reverts and the pass reads as a clean download.
	pub quarantined: Vec<String>,
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
	/// flight plus everything queued behind it — because the pass could not take the drive lock, in
	/// which case it is every planned action and `errors` says why, or because a record of work
	/// already done could not be written or the pass's staged plan could not be read, either of
	/// which stops the pass admitting anything more (see `admits_more`). Those actions recorded
	/// nothing — a row is written only after its action succeeded, and a transfer that succeeded is
	/// never dropped before its row — and the next pass re-plans them. Zero on a pass that ran to
	/// the end, including one that was merely SUSPENDED and resumed.
	pub interrupted: usize,
	/// Set, to the kind of the lock's own error, when the pass could not take the drive lock, so it
	/// applied nothing. The report is still returned (conflicts and held deletions were recorded
	/// before the lock was asked for), but a watch loop must count the pass as failed rather than
	/// healthy. The error itself is the entry in `errors` that starts with [`LOCK_FAILURE`].
	pub(super) lock_failed: Option<ErrorKind>,
	/// Set when a write that RECORDS work already done could not be persisted: a baseline row, a
	/// journal row, a journal retirement, or the pass's own record in its stage of a directory it
	/// created or of a write a later deletion must not take down. The act itself stands — the
	/// transfer, the rename, the trash — and the store's own answer is in `errors`, naming the path.
	/// Such a pass is a FAILED pass: a watch loop backs off rather than running the next one into
	/// the same wedged DB, and the pass after it re-derives or redoes what the lost record described
	/// (see the baseline store's `record_write`). So is a pass that could not read its staged plan,
	/// the only copy of what it is applying (see `note_unread_plan`).
	pub(super) store_failed: Option<StoreFailure>,
	/// How many actions this pass planned and then DROPPED before applying anything: a path it may
	/// not act on (`engine::Blocking`) or a deletion over content it cannot reach
	/// (`engine::Withholding`). Making the plan consumed both changelists, so a
	/// dropped action is named nowhere any more — the next pass reads both sides whole to find it
	/// again (see `next_pass_scope`). Distinct from [`deferred_paths`](Self::deferred_paths),
	/// which counts what the reconcile itself left alone, whether or not it planned anything there.
	pub(super) dropped_actions: usize,
	/// Set when a side ran out of room during the pass — the local disk or the account — or the
	/// remote could not be reached or refused the session, whichever was found first. The action
	/// that ran into it — and any action already running that did too — is in
	/// [`errors`](Self::errors) but counts against no path's failure streak, since the path itself
	/// is not what is broken (a remote outage counts only spaced out, see
	/// [`HaltReason::RemoteUnavailable`] and the engine's `TRANSIENT_FAILURE_SPACING`). Each
	/// condition found has its own line there, so a pass that fills both sides says so.
	///
	/// From then on the pass starts no further transfer that WRITES to a full side (see
	/// [`HaltReason::held_transfers`]): no upload once the account is full, no download once the
	/// disk is. Everything else still runs — the other direction's transfers, directory creates,
	/// moves and deletions — so a full account does not stop a two-way pair pulling the other
	/// devices' changes, and deletions that free room are not held behind the transfers that need
	/// it. A remote out of reach or refusing the session holds back every action that touches the
	/// remote — transfers both ways, remote directory creates, moves and trashes, and everything
	/// under a remote directory move it held — while the local-only ones (local directory creates,
	/// moves and deletions, baseline adoptions) still run. [`interrupted`](Self::interrupted) does
	/// not count the held actions: the next pass re-plans them. A watch treats such a pass as a
	/// failed one and backs off before the next.
	pub halted: Option<HaltReason>,
	/// Why this pass read both sides WHOLE rather than only what changed since the last one, or
	/// `None` when its changelists described every change (see [`FullPassReason`]).
	///
	/// A pass reads everything today either way — the change-scoped pass that acts on this is not
	/// in place yet — so for now the field says what the next pass's scope WOULD be, which is what
	/// makes the trigger table observable from the outside while it is still being built.
	pub full_pass: Option<FullPassReason>,
	/// How long this pass spent READING: the baseline, the local scan, the remote snapshot and the
	/// view. Zero on a pass that never got that far.
	///
	/// This — and not the pass's wall time — is what a watch's safety net scales its interval by
	/// (see [`WatchConfig::safety_net`](super::WatchConfig::safety_net)). What the net trades
	/// against is the cost of reading this pair's tree; a pass also carries its transfers, the wait
	/// for the drive-write lock and any stretch a suspension parked it in, none of which say
	/// anything about how expensive the tree is to read.
	pub(super) read_cost: Duration,
	/// The `(rel_path, error)` of every action that failed, for the engine's per-path failure
	/// bookkeeping. `errors` is the human-facing rendering of the same failures plus the pass-level
	/// ones (a refusal, a lock that could not be taken) that belong to no path, less the actions on
	/// a file another device has since replaced, which only the next pass can re-plan (see
	/// `note_action_failed`).
	pub(super) failed_paths: Vec<(String, String)>,
	/// The `(rel_path, error)` of every action that failed because the remote was out of reach or
	/// refused the session ([`HaltReason::is_remote_outage`]). Not in
	/// [`failed_paths`](Self::failed_paths): the engine counts these against their paths' streaks
	/// only spaced out (see its `TRANSIENT_FAILURE_SPACING`), so an outage does not block the paths
	/// it happened to catch, while a path the server refuses with a 5xx on every pass still is.
	pub(super) transient_failures: Vec<(String, String)>,
	/// The paths of the actions that landed this pass, among those whose success ends a failure
	/// streak ([`ApplyContext::streaked`]): what the engine clears the streaks of. Only those, so a
	/// pass of a million actions keeps a list as long as its paths with a streak, not its actions.
	pub(super) landed: Vec<String>,
}

impl SyncReport {
	/// How many deletions the mass-delete guard held back this pass.
	pub fn held_deletions(&self) -> usize {
		self.held
			.iter()
			.filter(|action| action.kind.is_deletion())
			.count()
	}

	/// Just the paths of [`conflicts`](Self::conflicts) — what
	/// [`SyncEngine::resolve_conflict`](super::SyncEngine::resolve_conflict) takes.
	pub fn conflict_paths(&self) -> impl Iterator<Item = &str> {
		self.conflicts.iter().map(|c| c.rel_path.as_str())
	}

	/// Why this pass FAILED as a whole, or `None` when it did not: the line a watch loop publishes
	/// as the pair's error and backs off on.
	///
	/// A pass that could not take the drive lock outranks the rest, since it applied nothing: its
	/// line is the entry in [`errors`](Self::errors) naming the lock's own answer. Then a side that
	/// ran out of room or a remote out of reach or refusing the session ([`halted`](Self::halted)),
	/// then a record of work done that could not be written or a staged plan that could not be
	/// read. Per-path failures alone do not fail a pass; they stay in `errors`.
	pub fn pass_error(&self) -> Option<String> {
		self.pass_failure().map(|(line, _)| line)
	}

	/// [`pass_error`](Self::pass_error) and the [`ErrorKind`] of what failed the pass: the lock's
	/// own error, the halt's kind ([`HaltReason::error_kind`]), or the store's own error.
	pub(super) fn pass_failure(&self) -> Option<(String, ErrorKind)> {
		if let Some(kind) = self.lock_failed {
			let line = self
				.errors
				.iter()
				.find(|error| error.starts_with(LOCK_FAILURE))
				.map_or_else(|| LOCK_FAILURE.to_string(), String::clone);
			return Some((line, kind));
		}
		self.halted
			.map(|halt| (halt.held_line(), halt.error_kind()))
			.or_else(|| {
				self.store_failed
					.map(|failure| (failure.line().into(), failure.kind()))
			})
	}
}

/// Everything the apply pass needs besides its actions and what each of them reads (see
/// [`PlannedEntry`]).
pub(super) struct ApplyContext<'a> {
	pub(super) client: &'a Client,
	pub(super) local_root: &'a Path,
	pub(super) pair: PairId,
	/// The pair's mode. Read only where the direction changes what an action MEANS — a two-way
	/// upload has a divergence to surface where a one-way one has an authoritative side.
	pub(super) mode: super::SyncMode,
	/// The pair's own baseline connection. Every write here is one row or one small transaction and
	/// runs on the caller's thread; the exception is a directory move's subtree re-key, which does
	/// not (see [`commit_dir_move`]).
	pub(super) store: &'a SharedStore,
	/// The sync root (every top-level parent): the drive root itself, or a directory in it.
	pub(super) root_remote: DirType<'a, Normal>,
	/// The cache DB each page of the plan reads the remote objects it acts on from (see
	/// [`PageObjects::hydrate`]); `None` when the client has no cache configured, and every action
	/// then asks the server for its own.
	///
	/// Taken from the client once, before the first action, because the client's cache slot is a
	/// lock a page turn must never wait on. A page turns with transfers in flight, which only the
	/// apply's own loop polls, and [`Client::flush_cache`] holds the slot until the cache worker
	/// has exited: a worker in the middle of a resync listing exits once its requests are answered,
	/// and those wait on request permits the transfers in flight hold. The pass and the flush would
	/// each wait on the other for good.
	pub(super) cache_db: Option<PathBuf>,
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
	/// The UTC day whose bucket of the local bin everything this pass moves aside is filed under
	/// (see [`quarantine_local`]): taken once, when the pass starts applying.
	pub(super) bin_day: NaiveDate,
	/// The paths of the pass's actions whose success ends a failure streak (the engine's
	/// `endable_streaks`): which of them landed is recorded in [`SyncReport::landed`].
	pub(super) streaked: &'a HashSet<&'a str>,
}

impl ApplyContext<'_> {
	/// Whether the pair is rooted at the drive, whose account data directory no local write may
	/// resolve into (see [`confined_local_target`]).
	fn pair_root(&self) -> PairRoot {
		match self.root_remote {
			DirType::Root(_) => PairRoot::Drive,
			DirType::Dir(_) => PairRoot::Directory,
		}
	}
}

/// One action of the plan, with what the apply reads for it as the pass started: made by
/// [`PlannedEntry::stage`] and staged with it, and read back out of the stage by the apply.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct PlannedEntry {
	pub(super) action: SyncAction,
	pub(super) start: PassStart,
}

/// What the apply reads for one action, read when the plan was made: a variant per kind of action,
/// carrying exactly what that kind reads (see [`PassStart::of`]), so the apply's arm for a kind
/// reaches nothing its entry was not given. Each input is `None` where the pass held nothing.
///
/// The inputs are: the local and the remote node at the action's path (a move's destination), the
/// baseline row there and at the path a move starts from, and `parent_uuid`, the remote directory
/// the view listed at the parent of the action's path — where it lands, unless this pass created a
/// directory there (see [`PagedPlan::parent_of`]).
///
/// Nodes and rows boxed: a kind reads at most three of them, and slots holding each inline would
/// cost every entry the room of the largest variant.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum PassStart {
	/// A local directory create, or a remote directory trash: the remote directory at its path.
	Remote { remote: Option<Box<RemoteNode>> },
	/// A download: the remote file it fetches, and the row the local copy at its path answers to.
	Download {
		remote: Option<Box<RemoteNode>>,
		base: Option<Box<BaselineEntry>>,
	},
	/// An upload: the local file it sends, the remote one it versions, the row whose agreed content
	/// it keeps, and where it lands.
	Upload {
		local: Option<Box<LocalNode>>,
		remote: Option<Box<RemoteNode>>,
		base: Option<Box<BaselineEntry>>,
		parent_uuid: Option<Uuid>,
	},
	/// A remote directory create: the local directory it mirrors, the remote item it replaces, and
	/// where it lands.
	CreateRemoteDir {
		local: Option<Box<LocalNode>>,
		remote: Option<Box<RemoteNode>>,
		parent_uuid: Option<Uuid>,
	},
	/// A remote file move: the local file at its destination, the row it moves from, and where it
	/// lands.
	MoveRemoteFile {
		local: Option<Box<LocalNode>>,
		base_from: Option<Box<BaselineEntry>>,
		parent_uuid: Option<Uuid>,
	},
	/// A remote directory move: the remote directory at its destination, and where it lands.
	MoveRemoteDir {
		remote: Option<Box<RemoteNode>>,
		parent_uuid: Option<Uuid>,
	},
	/// A local file move: the remote file at its destination, the row there, and the row it moves
	/// from.
	MoveLocalFile {
		remote: Option<Box<RemoteNode>>,
		base: Option<Box<BaselineEntry>>,
		base_from: Option<Box<BaselineEntry>>,
	},
	/// A conflict, or a baseline adoption: both nodes at its path.
	Sides {
		local: Option<Box<LocalNode>>,
		remote: Option<Box<RemoteNode>>,
	},
	/// A local deletion, a remote file trash, or a local directory move: nothing.
	Nothing,
}

/// Where [`PassStart::of`] reads an action's inputs: the pass's own trees as its plan is staged
/// ([`PassTrees`]), or the stage once it is (`stage::StagedInputs`). Asked only for what the action's
/// kind reads, and each at most once.
pub(super) trait StartInputs {
	/// The local node at `path`.
	fn local(&mut self, path: &str) -> Option<Box<LocalNode>>;
	/// The remote node at `path`.
	fn remote(&mut self, path: &str) -> Option<Box<RemoteNode>>;
	/// The baseline row at `path`, the action's own.
	fn base(&mut self, path: &str) -> Option<Box<BaselineEntry>>;
	/// The baseline row at `from_path`, where a move starts.
	fn base_from(&mut self, from_path: &str) -> Option<Box<BaselineEntry>>;
	/// The remote directory the view listed at the parent of `path`.
	fn parent_uuid(&mut self, path: &str) -> Option<Uuid>;
}

/// The pass's two sides and its rows, asked for one action at a time in plan order — which is path
/// order within each phase, so the questions in a row walk the pages the cursor reads.
struct PassTrees<'a, L, R> {
	local: &'a L,
	remote: &'a R,
	rows: Cursor<'a>,
}

impl<L, R> StartInputs for PassTrees<'_, L, R>
where
	L: NodesAt<Node = LocalNode>,
	R: NodesAt<Node = RemoteNode>,
{
	fn local(&mut self, path: &str) -> Option<Box<LocalNode>> {
		self.local.at(path).map(|node| Box::new(node.into_owned()))
	}

	fn remote(&mut self, path: &str) -> Option<Box<RemoteNode>> {
		self.remote.at(path).map(|node| Box::new(node.into_owned()))
	}

	fn base(&mut self, path: &str) -> Option<Box<BaselineEntry>> {
		self.rows.get(path).map(Box::new)
	}

	fn base_from(&mut self, from_path: &str) -> Option<Box<BaselineEntry>> {
		self.base(from_path)
	}

	fn parent_uuid(&mut self, path: &str) -> Option<Uuid> {
		dir_uuid(self.remote.at(parent_and_name(path).0).as_deref())
	}
}

impl PlannedEntry {
	/// The plan the pass screened (`screen`), each action with what the apply reads for it out of
	/// the pass's `baseline` and its two sides, staged in the same stage one at a time and ordered
	/// there for the apply (see [`StagedPlan`]), with the deletions each directory deletion carries
	/// out along with its own: what the apply holds of its plan is then the page of it it is in.
	///
	/// It takes those four, and drops them: the scan and the view are a node per item of the tree;
	/// the baseline's snapshot is the pass's read transaction, which has to END before the first
	/// action commits (see [`Baseline::close`]). Every answer an entry carries is the one the plan
	/// was made from, the pass's own commits landing in none of them.
	pub(super) fn stage(
		screen: Screen,
		baseline: Baseline,
		local: Side<LocalNode>,
		remote: Side<RemoteNode>,
	) -> rusqlite::Result<StagedPlan> {
		let staged = Self::write(screen, &baseline, local, remote);
		baseline.close();
		staged
	}

	fn write(
		screen: Screen,
		baseline: &Baseline,
		local: Side<LocalNode>,
		remote: Side<RemoteNode>,
	) -> rusqlite::Result<StagedPlan> {
		let (local, remote) = (local.of(baseline), remote.of(baseline));
		let mut trees = PassTrees {
			local: &local,
			remote: &remote,
			rows: baseline.cursor(),
		};
		screen.stage_plan(|action| PassStart::of(action, &mut trees))
	}
}

impl PassStart {
	/// What the apply reads for `action` — in `apply_one`, `apply_transfer`, `compose_creates`,
	/// `compose_trashes` and `Wanted::of` — read out of `inputs`, and nothing else.
	pub(super) fn of(action: &SyncAction, inputs: &mut impl StartInputs) -> Self {
		let path = action.rel_path();
		match action {
			SyncAction::CreateLocalDir { .. }
			| SyncAction::TrashRemote {
				kind: NodeKind::Dir,
				..
			} => Self::Remote {
				remote: inputs.remote(path),
			},
			SyncAction::DownloadFile { .. } => Self::Download {
				remote: inputs.remote(path),
				base: inputs.base(path),
			},
			SyncAction::UploadFile { .. } => Self::Upload {
				local: inputs.local(path),
				remote: inputs.remote(path),
				base: inputs.base(path),
				parent_uuid: inputs.parent_uuid(path),
			},
			SyncAction::CreateRemoteDir { .. } => Self::CreateRemoteDir {
				local: inputs.local(path),
				remote: inputs.remote(path),
				parent_uuid: inputs.parent_uuid(path),
			},
			SyncAction::MoveRemote {
				from_path,
				kind: NodeKind::File,
				..
			} => Self::MoveRemoteFile {
				local: inputs.local(path),
				base_from: inputs.base_from(from_path),
				parent_uuid: inputs.parent_uuid(path),
			},
			SyncAction::MoveRemote {
				kind: NodeKind::Dir,
				..
			} => Self::MoveRemoteDir {
				remote: inputs.remote(path),
				parent_uuid: inputs.parent_uuid(path),
			},
			SyncAction::MoveLocal {
				from_path,
				kind: NodeKind::File,
				..
			} => Self::MoveLocalFile {
				remote: inputs.remote(path),
				base: inputs.base(path),
				base_from: inputs.base_from(from_path),
			},
			SyncAction::Conflict { .. } | SyncAction::AdoptBaseline { .. } => Self::Sides {
				local: inputs.local(path),
				remote: inputs.remote(path),
			},
			SyncAction::DeleteLocal { .. }
			| SyncAction::TrashRemote {
				kind: NodeKind::File,
				..
			}
			| SyncAction::MoveLocal {
				kind: NodeKind::Dir,
				..
			} => Self::Nothing,
		}
	}
}

/// The uuid of `node`, when it is a directory.
fn dir_uuid(node: Option<&RemoteNode>) -> Option<Uuid> {
	node.filter(|node| node.kind == NodeKind::Dir)
		.map(|node| node.remote_uuid)
}

/// An entry whose inputs are not the ones its action's kind reads. [`PassStart::of`] makes every
/// entry's, so this is a bug, and fails the action rather than reading inputs it was not given.
fn unpaired() -> crate::Error {
	internal("an entry's pass-start inputs are not its action's (should be impossible)")
}

pub(super) fn local_path(root: &Path, rel_path: &str) -> PathBuf {
	rel_path
		.split('/')
		.fold(root.to_path_buf(), |p, c| p.join(c))
}

/// Resolve a local WRITE target under `root`, confined to the sync root. Rejects a `rel_path` with
/// a `.`/`..`/empty component (lexical traversal) and — to defeat a symlink in the tree that points
/// outside — canonicalizes the deepest existing ancestor and verifies it stays under the
/// canonicalized root. Without this, downloading `link/x` where `link` is a symlink to `/etc` would
/// write outside the sync folder. A target that resolves into filen-sync's trash folder is
/// rejected too, and so is one that resolves into the account's data directory in a pair
/// `pair_root` says is rooted at the drive. Reads/quarantine moves are unaffected; only writes are
/// confined.
fn confined_local_target(
	root: &Path,
	pair_root: PairRoot,
	rel_path: &str,
) -> Result<PathBuf, crate::Error> {
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
	let Ok(within) = existing_canon.strip_prefix(&root_canon) else {
		return Err(internal_owned(format!(
			"refusing to write outside the sync root via {rel_path:?}"
		)));
	};
	// Nor into filen-sync's trash folder, which the plan never names (it is never scanned or
	// viewed) but a path can still resolve into: a case variant of its name (see
	// [`folds_onto_legacy_trash`]) on a case-insensitive filesystem, or a link. The canonical path
	// carries the names on disk, as macOS and Windows resolve them; a case-insensitive Linux
	// directory (casefold) hands back the spelling asked for, and is not caught here.
	if within
		.components()
		.any(|component| is_legacy_trash(component.as_os_str()))
		|| rel_path
			.split('/')
			.any(|component| is_legacy_trash(OsStr::new(component)))
	{
		return Err(internal_owned(format!(
			"refusing to write into filen-sync's trash folder via {rel_path:?}"
		)));
	}
	// Nor into the account's data directory, which the plan never names either but a link can
	// resolve into; its name is matched case-insensitively (see [`is_account_data`]), so a case
	// variant is caught on the relative path whatever spelling the filesystem hands back.
	if is_account_data(rel_path, pair_root)
		|| within
			.components()
			.next()
			.and_then(|component| component.as_os_str().to_str())
			.is_some_and(|name| is_account_data(name, pair_root))
	{
		return Err(internal_owned(format!(
			"refusing to write into the account's data directory via {rel_path:?}"
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
pub(super) fn rename_aside(
	root: &Path,
	pair_root: PairRoot,
	rel_path: &str,
) -> Result<Option<String>, crate::Error> {
	let source = confined_local_target(root, pair_root, rel_path)?;
	if source.symlink_metadata().is_err() {
		return Ok(None);
	}
	let (rel, dest) = aside_target(root, pair_root, rel_path)?;
	std::fs::rename(&source, &dest).map_err(io_err)?;
	Ok(Some(rel))
}

/// A free `<stem>.old.<ext>` name beside `rel_path` (`<stem>.old.N.<ext>` when that is taken), with
/// its confined local path. What [`rename_aside`] moves the losing copy to, and where a keep-both
/// resolution of a buried version downloads it.
pub(super) fn aside_target(
	root: &Path,
	pair_root: PairRoot,
	rel_path: &str,
) -> Result<(String, PathBuf), crate::Error> {
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
		let dest = confined_local_target(root, pair_root, &rel)?;
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

/// Execute the staged `plan` (guard-screened, ordered in its stage) against the remote and local
/// tree, reading it a page at a time in the order the stage gives it (see [`StagedPlan`]), with the
/// remote objects each page acts on (see [`PagedPlan`]).
///
/// Each entry carries what its action reads as the pass started, so the apply reads neither the
/// pass's rows nor its two sides — and holds no read transaction open behind the commits it makes
/// (see [`PlannedEntry::stage`]).
///
/// The drive-write lock is held per bounded BATCH of remote writes (see [`Lease`]) so the per-op
/// `lock_drive` calls inside upload/create become free clones of the held lock instead of each
/// doing an acquire+release server round-trip — the dominant per-file cost at scale — while no other
/// device of the account waits on it for longer than about [`PASS_LOCK_MAX_HOLD`](super::engine::PASS_LOCK_MAX_HOLD). It is let go
/// only between actions that do not depend on one another (see [`Staged::releasable`]), once no
/// remote write is in flight, and taken again before the next remote write; a pure pull never takes
/// it.
///
/// File transfers (uploads/downloads) — the bulk of the work and independent of one another — run
/// CONCURRENTLY, bounded by the client's configured concurrency ([`ClientConfig::with_concurrency`],
/// default 16) and the global request rate limiter. Directory creates/moves (which make the
/// directories later actions land in, parent before child) and deletes (descendant-first for
/// cascade safety) stay in order; consecutive remote directory creates, and consecutive remote
/// trashes, go to the server together, a chunk at a time (see [`apply_creates`] and
/// [`apply_trashes`]).
///
/// Each action emits its [`SyncEvent`] to `observer` (and an [`ActionFailed`](SyncEvent::ActionFailed)
/// if it errors): serial actions when they start, a chunk's creates or trashes as it is sent,
/// concurrent transfers as each completes.
///
/// Pausing the pair reaches into this loop through [`ApplyContext::gate`]: a SUSPENDED pass parks
/// before its next action — and before taking the drive lock — while a transfer already running
/// finishes, since the transfer paths poll no pause signal of their own; a CANCELLED one drops the
/// transfers in flight and skips everything left, counting them in [`SyncReport::interrupted`]. A
/// transfer that got as far as FINISHING is never dropped between its network op and the row that
/// records it (see [`apply_transfer`]), so the two always agree.
pub(super) async fn apply(
	ctx: ApplyContext<'_>,
	plan: StagedPlan,
	report: &mut SyncReport,
	observer: &mut (dyn FnMut(SyncEvent) + Send),
) {
	let mut plan = PagedPlan::new(plan, ctx.root_remote.uuid());

	// What the pass still owes if it is cancelled part-way: everything it has not carried out.
	let total = plan.len();
	let mut applied = 0usize;
	let mut lease = Lease::Unheld;

	let mut failed = FailedDirMoves::default();
	// The sides found full so far: each holds back the transfers that write to it.
	let full = FullSides::default();
	// Creates and moves first, in order (a run of remote directory creates a chunk at a time): they
	// make the directories the transfers land in, and run parent before child. The deletion half of
	// a type flip runs among them, before the create at its path, or the server rejects that create
	// (the old item still exists).
	while admits_more(report)
		&& let Some(Staged {
			entry, releasable, ..
		}) = unless_unread(plan.next_in(&ctx, Run::Before).await, report)
	{
		let action = &entry.action;
		if !ctx.gate.wait_to_start().await {
			return note_interrupted(report, total - applied, observer);
		}
		if !failed.skips(action, report, observer) && !holds_serial(&full, &mut failed, action) {
			match lease.prepare(&ctx, action, releasable).await {
				Some(Ok(())) => {}
				Some(Err(error)) => {
					return note_lock_failure(report, total - applied, &error, observer);
				}
				None => return note_interrupted(report, total - applied, observer),
			}
			if is_trash(action) {
				let chunk =
					match take_chunk(&ctx, &mut plan, &mut lease, Run::Before, entry, is_trash)
						.await
					{
						Ok(chunk) => chunk,
						Err(error) => {
							note_unread_plan(report, &error);
							continue;
						}
					};
				applied +=
					apply_trashes(&ctx, chunk, &mut plan, &failed, &full, report, observer).await;
				continue;
			}
			if is_remote_dir_create(action) {
				let chunk = match take_chunk(
					&ctx,
					&mut plan,
					&mut lease,
					Run::Before,
					entry,
					is_remote_dir_create,
				)
				.await
				{
					Ok(chunk) => chunk,
					Err(error) => {
						note_unread_plan(report, &error);
						continue;
					}
				};
				let hold = lease
					.held()
					.expect("a remote write's lease holds the lock (should be impossible)");
				let chunk_ctx = ChunkContext {
					ctx: &ctx,
					hold,
					full: &full,
				};
				applied +=
					apply_creates(&chunk_ctx, chunk, &mut plan, &failed, report, observer).await;
				continue;
			}
			apply_serial(
				&ctx,
				&entry,
				&mut plan,
				&mut failed,
				&full,
				report,
				observer,
			)
			.await;
		}
		applied += 1;
	}

	// The transfers under a directory whose move just failed are failed before any transfer starts,
	// those under one that was held are held, and both are passed over below.
	if !failed.dirs.is_empty() || !failed.held.is_empty() {
		let swept = plan.sweep(Some(Run::Transfers), |entry| {
			if failed.skips(&entry.action, report, observer) {
				applied += 1;
			}
		});
		if let Err(error) = swept {
			note_unread_plan(report, &error);
		}
	}

	if admits_more(report)
		&& unless_unread(next_transfer(&mut plan, &ctx, &failed).await, report).is_some()
	{
		let concurrency = ctx.client.unauthed().state().max_concurrency().max(1);
		// Byte progress comes from inside the SDK's transfer futures, which cannot reach the
		// observer this loop holds: they queue it here and the loop delivers it — between
		// completions, and all of a transfer's progress ahead of that transfer's own event.
		let (progress, mut progress_events) = tokio::sync::mpsc::unbounded_channel();
		let mut in_flight = FuturesUnordered::new();
		// Uploads in flight use the lock, so it is let go only once none is; downloads do not.
		let mut remote_in_flight = 0usize;
		loop {
			while in_flight.len() < concurrency
				&& admits_more(report)
				&& let Some(staged) =
					unless_unread(next_transfer(&mut plan, &ctx, &failed).await, report)
			{
				let (action, releasable) = (&staged.entry.action, staged.releasable);
				// Once an action finds a side full, the transfers already running finish, and the ones
				// not started yet that write to that side never start (they record nothing); once one
				// finds the remote out of reach, none not started yet does. Held back for want of room
				// or of a remote: carried out as far as this pass can, owed to no pause.
				if full.holds(action) {
					tracing::debug!(
						"apply: {} held, its side is full or the remote is out of reach",
						action.describe()
					);
					unless_unread(plan.next_in(&ctx, Run::Transfers).await, report);
					applied += 1;
					continue;
				}
				let now = Instant::now();
				if remote_in_flight == 0 {
					lease.release_if_spent(&ctx, releasable, now);
				}
				if !lease.admits(&ctx.lock_budget, action, releasable, now) {
					break;
				}
				let Some(Staged { entry, .. }) =
					unless_unread(plan.next_in(&ctx, Run::Transfers).await, report)
				else {
					break;
				};
				let target = match plan.transfer_target(&entry) {
					Ok(target) => target,
					Err(error) => {
						applied += 1;
						note_action_failed(&entry.action, &error, &full, report, observer);
						continue;
					}
				};
				lease.note_start(&entry.action);
				remote_in_flight += usize::from(mutates_remote(&entry.action));
				in_flight.push(run_transfer(&ctx, entry, target, &progress));
			}
			let next = tokio::select! {
				biased;
				Some(event) = progress_events.recv() => {
					observer(event);
					continue;
				}
				next = in_flight.next() => next,
			};
			let Some((action, quarantined, result)) = next else {
				// Nothing in flight: the transfers are done, or the next one waits on the lock.
				if !admits_more(report) {
					break;
				}
				if unless_unread(next_transfer(&mut plan, &ctx, &failed).await, report).is_none() {
					break;
				}
				let Some(Staged {
					entry, releasable, ..
				}) = unless_unread(plan.next_in(&ctx, Run::Transfers).await, report)
				else {
					break;
				};
				let target = match plan.transfer_target(&entry) {
					Ok(target) => target,
					Err(error) => {
						applied += 1;
						note_action_failed(&entry.action, &error, &full, report, observer);
						continue;
					}
				};
				match lease.prepare(&ctx, &entry.action, releasable).await {
					Some(Ok(())) => {}
					Some(Err(error)) => {
						return note_lock_failure(report, total - applied, &error, observer);
					}
					None => return note_interrupted(report, total - applied, observer),
				}
				remote_in_flight += usize::from(mutates_remote(&entry.action));
				in_flight.push(run_transfer(&ctx, entry, target, &progress));
				continue;
			};
			remote_in_flight -= usize::from(mutates_remote(&action));
			// A transfer queues its last progress before it resolves.
			while let Ok(event) = progress_events.try_recv() {
				observer(event);
			}
			// Whatever the transfer returned: a copy it moved aside was moved aside for a write
			// that landed, and it stays in the bin. Ahead of the transfer's own event on the way
			// out, and ahead of the failure of the bookkeeping that followed the write.
			note_quarantined(report, observer, quarantined);
			match result {
				// A cancel skips a transfer that has not started and DROPS one that is mid-flight;
				// either way it recorded nothing, and the pass owes it to the next one.
				Ok(Transfer::Interrupted) => {
					tracing::debug!("apply: {} interrupted", action.describe());
				}
				Ok(Transfer::Done) => {
					applied += 1;
					note_written(&ctx, &plan, &action, report);
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
				Ok(Transfer::Overwrote {
					conflict,
					unrecorded,
				}) => {
					applied += 1;
					note_written(&ctx, &plan, &action, report);
					observer(action.to_event());
					observer(SyncEvent::Conflict {
						rel_path: conflict.rel_path.clone(),
					});
					note_overwrote(report, action.rel_path(), *conflict, unrecorded);
				}
				Err(error) => {
					applied += 1;
					note_action_failed(&action, &error, &full, report, observer);
				}
			}
		}
	}

	// The other deletions, children before parents: a local one at a time, a run of remote trashes a
	// chunk at a time.
	while admits_more(report)
		&& let Some(Staged {
			entry, releasable, ..
		}) = unless_unread(plan.next_in(&ctx, Run::After).await, report)
	{
		let action = &entry.action;
		if !ctx.gate.wait_to_start().await {
			return note_interrupted(report, total - applied, observer);
		}
		if !failed.skips(action, report, observer) && !holds_serial(&full, &mut failed, action) {
			match lease.prepare(&ctx, action, releasable).await {
				Some(Ok(())) => {}
				Some(Err(error)) => {
					return note_lock_failure(report, total - applied, &error, observer);
				}
				None => return note_interrupted(report, total - applied, observer),
			}
			if is_trash(action) {
				let chunk = match take_chunk(
					&ctx,
					&mut plan,
					&mut lease,
					Run::After,
					entry,
					is_trash,
				)
				.await
				{
					Ok(chunk) => chunk,
					Err(error) => {
						note_unread_plan(report, &error);
						continue;
					}
				};
				applied +=
					apply_trashes(&ctx, chunk, &mut plan, &failed, &full, report, observer).await;
				continue;
			}
			apply_serial(
				&ctx,
				&entry,
				&mut plan,
				&mut failed,
				&full,
				report,
				observer,
			)
			.await;
		}
		applied += 1;
	}
	note_interrupted(report, total - applied, observer);
}

/// The next transfer of `plan` to start, past the ones under a directory whose move failed this
/// pass, which `apply` has already failed.
async fn next_transfer<'p>(
	plan: &'p mut PagedPlan,
	ctx: &ApplyContext<'_>,
	failed: &FailedDirMoves,
) -> Result<Option<&'p Staged>, crate::Error> {
	while plan
		.peek_in(ctx, Run::Transfers)
		.await?
		.is_some_and(|staged| failed.holds_back(&staged.entry.action).is_some())
	{
		plan.next_in(ctx, Run::Transfers).await?;
	}
	plan.peek_in(ctx, Run::Transfers).await
}

/// One transfer, paired with the action it ran and with whatever local copy it moved aside — named
/// so every transfer of a pass is the same future type and they can share one [`FuturesUnordered`].
///
/// The quarantine travels beside the result, not inside it: the apply loop accounts for it whether
/// the transfer ended in [`Transfer::Done`] or in an error from the bookkeeping that followed its
/// landed write (see [`apply_transfer`]).
///
/// It owns its entry and its target, so the apply holds the entry and the target of each transfer
/// in flight and the page it reads from, and nothing else of its plan.
async fn run_transfer(
	ctx: &ApplyContext<'_>,
	planned: PlannedEntry,
	target: TransferTarget,
	progress: &UnboundedSender<SyncEvent>,
) -> (
	SyncAction,
	Option<(String, PathBuf)>,
	Result<Transfer, crate::Error>,
) {
	let mut quarantined = None;
	let result = apply_transfer(ctx, &planned, target, progress, &mut quarantined).await;
	(planned.action, quarantined, result)
}

/// Record an action that failed — its event and its failure (see [`note_failure`]), and the side
/// it found full, which holds back the transfers after it.
///
/// A file the server holds a newer version of than the one this pass read (see [`superseded`])
/// is not a failure the caller sees: it has no event and no line in
/// [`errors`](SyncReport::errors). It is still owed, so it stays in
/// [`failed_paths`](SyncReport::failed_paths): that makes the next pass read both sides whole and
/// re-plan the path from the newer version, and a view that keeps naming the old one still runs
/// into the path's failure streak.
fn note_action_failed(
	action: &SyncAction,
	error: &crate::Error,
	full: &FullSides,
	report: &mut SyncReport,
	observer: &mut (dyn FnMut(SyncEvent) + Send),
) {
	tracing::debug!("apply: {} FAILED — {error}", action.describe());
	if superseded(error) {
		tracing::debug!(
			"apply: {} left to the next pass: the server holds a newer version of that file than \
			 this pass read",
			action.describe()
		);
		report
			.failed_paths
			.push((action.rel_path().to_string(), error.to_string()));
		return;
	}
	observer(SyncEvent::ActionFailed {
		rel_path: action.rel_path().to_string(),
		error: error.to_string(),
	});
	if let Some(reason) = note_failure(report, action.rel_path(), error) {
		full.note(reason);
	}
}

/// Whether the server refused an action because the file it names is no longer the current one:
/// another device uploaded a newer version after this pass read it, which made the uuid the plan
/// holds an old version (`file_is_versioned`). A move or trash of that uuid is moot, since the
/// next pass plans from the newer version.
fn superseded(error: &crate::Error) -> bool {
	error.server_code().as_deref() == Some("file_is_versioned")
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

	/// Let a spent hold go, when the next action sits where a release splits nothing, and with it
	/// the links and shares looked up during it, unless another holder in the process shares it.
	fn release_if_spent(&mut self, ctx: &ApplyContext<'_>, releasable: bool, now: Instant) {
		if releasable && self.spent(&ctx.lock_budget, now) {
			*self = Self::Released { at: now };
			ctx.client.forget_ended_hold_targets();
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

	/// How many more remote writes the hold starts before its batch is spent; none without a hold.
	fn room(&self, budget: &LockBudget) -> usize {
		match self {
			Self::Held { writes, .. } => budget.batch.saturating_sub(*writes),
			Self::Unheld | Self::Released { .. } => 0,
		}
	}

	/// The lock, while it is held.
	fn held(&self) -> Option<&Arc<ResourceLock>> {
		match self {
			Self::Held { lock, .. } => Some(lock),
			Self::Unheld | Self::Released { .. } => None,
		}
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
	/// then. A suspended pass parks here instead of holding a lock it is not using. A grant to an
	/// acquisition dropped mid-request is released by that request's own task (see
	/// `request_acquire`); if that release is lost, the 60 s lease lapses on its own.
	async fn prepare(
		&mut self,
		ctx: &ApplyContext<'_>,
		action: &SyncAction,
		releasable: bool,
	) -> Option<Result<(), crate::Error>> {
		let now = Instant::now();
		self.release_if_spent(ctx, releasable, now);
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

/// Where the directory moves that failed this pass, or that it held back, were headed: nothing
/// under those paths is attempted. As many as moves failed or were held, which the fold makes one
/// per moved subtree.
#[derive(Debug, Default)]
struct FailedDirMoves {
	dirs: Vec<String>,
	/// The moves the pass held back, or that failed on the condition that halted it (see
	/// [`SyncReport::halted`]): what is under them is held too, silently and counting against no
	/// streak, and the next pass re-plans it.
	held: Vec<String>,
}

impl FailedDirMoves {
	/// The directory whose move failed or was held this pass at or above either end of `action`:
	/// the plan names that subtree by a path neither side carries.
	fn holds_back(&self, action: &SyncAction) -> Option<&str> {
		Self::above(&self.dirs, action).or_else(|| Self::above(&self.held, action))
	}

	/// The directory of `dirs` at or above either end of `action`.
	fn above<'d>(dirs: &'d [String], action: &SyncAction) -> Option<&'d str> {
		let (from, to) = action.endpoints();
		dirs.iter().map(String::as_str).find(|dir| {
			[from, to]
				.into_iter()
				.any(|path| path == *dir || is_under(path, dir))
		})
	}

	/// Skip `action` when a directory move that failed or was held this pass holds it back (see
	/// [`holds_back`](Self::holds_back)): under a failed one recording it as a failure of its own,
	/// under a held one holding it as well.
	fn skips(
		&self,
		action: &SyncAction,
		report: &mut SyncReport,
		observer: &mut (dyn FnMut(SyncEvent) + Send),
	) -> bool {
		if let Some(dir) = Self::above(&self.held, action) {
			tracing::debug!(
				"apply: {} held, under the held move of {dir:?}",
				action.describe()
			);
			return true;
		}
		let Some(dir) = Self::above(&self.dirs, action) else {
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

/// Journal a remote directory move from `from` to `to` and carry its subtree's rows along, in one
/// transaction.
///
/// The rows are re-keyed where they lie, from what the STORE holds rather than from the pass's
/// baseline — see [`BaselineChange::MoveSubtree`] for why that difference matters to a move with
/// another nested inside it.
///
/// NOT cancel-safe at the join: `spawn_blocking` runs to completion, so a caller that DROPS the
/// `sync_once` future while this transaction is committing leaves the journal row and the re-keyed
/// rows written with the in-memory journal never told. Nothing inside the engine does that — a
/// cancel is cooperative, and the watch loop joins its pass rather than aborting it — and the next
/// open reloads the journal out of the DB, so the two halves are back in step from then on.
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
	// The journal row and the re-key in ONE transaction, exactly as [`commit_remote_write`] does
	// it — but off the runtime thread, because this re-key covers every row of the moved
	// directory's subtree rather than a single row. Owned copies for the closure, and the
	// in-memory journal is published only once that transaction has committed, as it is there.
	let row = PendingRow {
		pair: ctx.pair,
		uuid,
		kind: kind.clone(),
		recorded_at: Utc::now().timestamp_millis(),
	};
	let (from, to) = (from.to_string(), to.to_string());
	let expired = ctx.pending.expire();
	off_store(ctx.store, move |store| {
		store.record_pending(
			&row,
			&[BaselineChange::MoveSubtree {
				from: &from,
				to: &to,
			}],
			&expired,
		)
	})
	.await?
	.map_err(db_err)?;
	ctx.pending.record(ctx.observed, ctx.pair, uuid, kind);
	Ok(())
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
	report.lock_failed = Some(error.kind());
	note_interrupted(report, remaining, observer);
}

/// How the error of a pass that could not take the drive lock begins in [`SyncReport::errors`].
pub(super) const LOCK_FAILURE: &str = "failed to acquire the drive lock";

/// How a pass whose baseline or journal write did not land reports that to a watch loop. The line
/// naming the path, and the store's own answer, is already in [`SyncReport::errors`].
pub(super) const STORE_FAILURE: &str = "a record of work already done could not be written";

/// How a pass whose staged plan could not be read reports that to a watch loop. The store's own
/// answer is already in [`SyncReport::errors`].
pub(super) const UNREAD_PLAN: &str = "the plan this pass staged could not be read";

/// What stopped a pass admitting more work ([`SyncReport::store_failed`]), with the kind of the
/// store's own error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StoreFailure {
	/// A write recording work already done did not land (see [`record_not_written`]). It outranks
	/// an [`UnreadPlan`](Self::UnreadPlan) in the same pass: it leaves an act no record describes,
	/// where an unread plan only stops the pass short.
	RecordNotWritten(ErrorKind),
	/// A read of the pass's staged plan failed (see [`note_unread_plan`]).
	UnreadPlan(ErrorKind),
}

impl StoreFailure {
	/// How a pass it stopped reports that to a watch loop.
	pub(super) fn line(self) -> &'static str {
		match self {
			Self::RecordNotWritten(_) => STORE_FAILURE,
			Self::UnreadPlan(_) => UNREAD_PLAN,
		}
	}

	/// The kind of the store's error that stopped the pass.
	pub(super) fn kind(self) -> ErrorKind {
		match self {
			Self::RecordNotWritten(kind) | Self::UnreadPlan(kind) => kind,
		}
	}
}

/// Whether `error` is a baseline or journal write that did not land — the record of an act that has
/// already happened, which makes the pass a failed one whatever else it managed.
///
/// Every such write answers with a `rusqlite::Error`, which [`db_err`] keeps as the source, and
/// this reads ANY of those on an action's path as a lost record — including the one baseline READ
/// an action makes (the row read back before a download quarantines what sits at its target, which
/// reaches here through the same `?`). A read that failed lost nothing, so the pass would be failed
/// for no reason; the consequence is one backoff, which is the safe direction, and separating the
/// two would mean marking every write for the sake of a read failure nobody has seen. The read of
/// where an action lands out of the pass's stage ([`stage_err`]) is failed here on purpose: the
/// stage is the only record of the directories the pass created, and the pass admits nothing more
/// once it cannot read it.
pub(super) fn record_not_written(error: &crate::Error) -> bool {
	error.downcast_ref::<rusqlite::Error>().is_some()
}

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

/// The staged plan as the apply takes it: a page at a time, with the remote objects that page acts
/// on hydrated as it is read (see [`PageObjects`]). What the apply holds of its plan, and of the
/// remote tree, is then the page in hand.
///
/// Every action is taken through [`peek_in`](Self::peek_in) and [`next_in`](Self::next_in), which
/// turn the page and hydrate it before handing out the first action of it.
struct PagedPlan {
	pager: Pager,
	objects: PageObjects,
	/// The sync root's uuid: the remote directory every top-level item lands in.
	root: Uuid,
}

impl PagedPlan {
	fn new(plan: StagedPlan, root: Uuid) -> Self {
		Self {
			pager: plan.pager(),
			objects: PageObjects::default(),
			root,
		}
	}

	fn len(&self) -> usize {
		self.pager.len()
	}

	/// The next action, when it is in `run`, the objects of its page hydrated. An error is a page
	/// that could not be read (see [`unless_unread`]).
	async fn peek_in(
		&mut self,
		ctx: &ApplyContext<'_>,
		run: Run,
	) -> Result<Option<&Staged>, crate::Error> {
		if let Some(wanted) = self.turn()? {
			self.objects.hydrate(ctx, wanted).await;
		}
		self.pager.peek_in(run).map_err(page_err)
	}

	/// Turn to the next page once the one in hand is used up (see [`Pager::turn`]): drop the objects
	/// the page before acted on, and say what this one acts on, for [`PageObjects::hydrate`]. `None`
	/// while the page in hand has actions left, and at the end of the plan.
	fn turn(&mut self) -> Result<Option<Wanted>, crate::Error> {
		let Some(page) = self.pager.turn().map_err(page_err)? else {
			return Ok(None);
		};
		let wanted = Wanted::of(page);
		self.objects.clear();
		Ok(Some(wanted))
	}

	/// Take the next action, when it is in `run`, the objects of its page hydrated.
	async fn next_in(
		&mut self,
		ctx: &ApplyContext<'_>,
		run: Run,
	) -> Result<Option<Staged>, crate::Error> {
		if self.peek_in(ctx, run).await?.is_none() {
			return Ok(None);
		}
		self.pager.next_in(run).map_err(page_err)
	}

	/// Take the actions at the front of `run` while `same` holds for them, at most `max`, turning
	/// and hydrating pages as [`next_in`](Self::next_in) does.
	async fn take_run(
		&mut self,
		ctx: &ApplyContext<'_>,
		run: Run,
		max: usize,
		same: impl Fn(&SyncAction) -> bool,
	) -> Result<Vec<Staged>, crate::Error> {
		let mut taken = Vec::new();
		while taken.len() < max
			&& self
				.peek_in(ctx, run)
				.await?
				.is_some_and(|staged| same(&staged.entry.action))
		{
			taken.extend(self.next_in(ctx, run).await?);
		}
		Ok(taken)
	}

	/// See [`Pager::sweep`].
	fn sweep(&self, run: Option<Run>, each: impl FnMut(PlannedEntry)) -> Result<(), crate::Error> {
		self.pager.sweep(run, each).map_err(page_err)
	}

	/// The remote file `uuid` an action of the page in hand acts on, if the cache held it when the
	/// page was read (else see [`file_or_fetch`]).
	///
	/// A copy, here and in [`dir`](Self::dir): a move updates the object it is handed, and
	/// a transfer runs on past the page; the page's own stays as hydrated for any other action of it
	/// that names the same object.
	fn file(&self, uuid: Uuid) -> Option<RemoteFile> {
		self.objects.files.get(&uuid).cloned()
	}

	/// The remote directory `uuid` an action of the page in hand acts on, if the cache held it when
	/// the page was read (else see [`dir_or_fetch`]).
	fn dir(&self, uuid: Uuid) -> Option<RemoteDirectory> {
		self.objects.dirs.get(&uuid).cloned()
	}

	/// The uuid of the remote directory the item at `rel_path` lands in: the sync root for a
	/// top-level item, the directory this pass created there, or else `listed`, the one the view
	/// listed there (the entry's `parent_uuid`). `None` where there is none of the three, and the
	/// action fails.
	///
	/// The plan creates a directory only where the view listed none, so at most one of the last two
	/// answers.
	///
	/// An error is a read of the stage that failed, and fails the action that asked, and with it the
	/// pass (see [`stage_err`]).
	fn parent_of(
		&self,
		rel_path: &str,
		listed: Option<Uuid>,
	) -> Result<Option<Uuid>, crate::Error> {
		let parent = match parent_and_name(rel_path).0 {
			"" => return Ok(Some(self.root)),
			parent => parent,
		};
		let created = self.pager.created_at(parent).map_err(|error| {
			stage_err(
				error,
				format!("reading the directory created at {parent:?} from the pass's stage"),
			)
		})?;
		Ok(created.or(listed))
	}

	/// Record the directory the create at `rel_path` made, or merged into, for the actions under it.
	fn record_created(&mut self, rel_path: &str, uuid: Uuid) -> Result<(), crate::Error> {
		self.pager.record_created(rel_path, uuid).map_err(|error| {
			stage_err(
				error,
				format!("recording the directory created at {rel_path:?} in the pass's stage"),
			)
		})
	}

	/// What the transfer of `planned` reads that its entry does not carry (see [`TransferTarget`]).
	/// An error is where an upload lands, which the stage could not read (see
	/// [`parent_of`](Self::parent_of)): the apply fails that upload without starting it, before it
	/// counts as a remote write or takes the drive lock for one.
	fn transfer_target(&self, planned: &PlannedEntry) -> Result<TransferTarget, crate::Error> {
		Ok(match (&planned.action, &planned.start) {
			(SyncAction::DownloadFile { remote_uuid, .. }, _) => TransferTarget {
				file: self.file(*remote_uuid),
				..TransferTarget::default()
			},
			(SyncAction::UploadFile { rel_path }, PassStart::Upload { parent_uuid, .. }) => {
				TransferTarget {
					parent: self.parent_of(rel_path, *parent_uuid)?,
					..TransferTarget::default()
				}
			}
			_ => TransferTarget::default(),
		})
	}
}

/// A read or a write of the pass's stage that failed, as the error of the action that made it. Its
/// `rusqlite::Error` stays the source, so the action fails as a record that could not be kept, and
/// the pass with it (see [`record_not_written`]): what the stage holds is the only copy of where the
/// pass's own directories are.
fn stage_err(error: rusqlite::Error, context: String) -> crate::Error {
	super::engine::db_error(error, &context)
}

/// A page of the pass's staged plan that could not be read (see [`unless_unread`]).
fn page_err(error: rusqlite::Error) -> crate::Error {
	stage_err(
		error,
		"reading a page of the pass's staged plan".to_string(),
	)
}

/// What a read of the pass's staged plan found — and for one that failed, nothing more to take,
/// with the failure recorded (see [`note_unread_plan`]).
fn unless_unread<T>(read: Result<Option<T>, crate::Error>, report: &mut SyncReport) -> Option<T> {
	read.unwrap_or_else(|error| {
		note_unread_plan(report, &error);
		None
	})
}

/// Record a read of the pass's staged plan that failed. The plan has no other copy, so the pass
/// fails as one that lost a record of its work does, for a reason of its own
/// ([`StoreFailure::UnreadPlan`]): it starts nothing more ([`admits_more`]), the transfers already
/// in flight finish and record what they did, and what it has not carried out is owed to the next
/// pass, which plans afresh ([`SyncReport::interrupted`]).
fn note_unread_plan(report: &mut SyncReport, error: &crate::Error) {
	tracing::error!("apply: {error}; the pass starts nothing more");
	report.errors.push(error.to_string());
	report
		.store_failed
		.get_or_insert(StoreFailure::UnreadPlan(error.kind()));
}

/// What a transfer reads that its entry does not carry, looked up as the apply takes it: the remote
/// file a download fetches, and the remote directory an upload lands in. Only what the action's kind
/// reads is filled in. The file is `None` where the cache held none, and the download asks the
/// server for it (see [`file_or_fetch`]); the parent is `None` where the pass has none, which fails
/// the upload.
///
/// The transfer holds its own copy rather than reading the page's: it runs on while the apply turns
/// to the next page.
#[derive(Default)]
struct TransferTarget {
	file: Option<RemoteFile>,
	parent: Option<Uuid>,
}

/// The uuids of the remote objects one page of the plan acts on (see [`PageObjects`]).
#[derive(Debug, Default, PartialEq, Eq)]
struct Wanted {
	files: HashSet<Uuid>,
	dirs: HashSet<Uuid>,
}

impl Wanted {
	/// What the actions of `page` act on: the file of a download or a file move, and the directory
	/// of a directory move — the one the view listed at its path, which its entry carries. A trash
	/// sends only a uuid, and wants nothing.
	fn of<'p>(page: impl Iterator<Item = &'p PlannedEntry>) -> Self {
		let mut wanted = Self::default();
		for PlannedEntry { action, start } in page {
			match (action, start) {
				(
					SyncAction::DownloadFile { remote_uuid, .. }
					| SyncAction::MoveRemote {
						kind: NodeKind::File,
						remote_uuid,
						..
					},
					_,
				) => {
					wanted.files.insert(*remote_uuid);
				}
				(
					SyncAction::MoveRemote {
						kind: NodeKind::Dir,
						..
					},
					PassStart::MoveRemoteDir { remote, .. },
				) => wanted.dirs.extend(dir_uuid(remote.as_deref())),
				_ => {}
			}
		}
		wanted
	}
}

/// The remote objects the actions of the page in hand act on, by uuid, as the cache holds them:
/// read as the page is turned to, and replaced when the next one is.
///
/// The remote view carries the writes this engine made that the cache has not listed yet, so an
/// action can name an item no cache row describes. Those are not here: the action that needs one
/// asks the server for it (see [`file_or_fetch`]).
///
/// Where an action LANDS — the parent of an upload, a create or a move — is not among them: that
/// takes only the directory's uuid (see [`PagedPlan::parent_of`]).
#[derive(Default)]
struct PageObjects {
	files: HashMap<Uuid, RemoteFile>,
	dirs: HashMap<Uuid, RemoteDirectory>,
}

impl PageObjects {
	/// Fill in what the cache holds of `wanted`, once [`PagedPlan::turn`] has dropped what the page
	/// before held: one local read of the cache DB the pass took at its start
	/// ([`ApplyContext::cache_db`]), and neither a server call nor a wait on the client. A page turns
	/// while transfers are in flight, and those are polled only once the apply's loop gets back to
	/// them, holding the client's request permits until then; a server call made here could wait for
	/// a permit only they can give back, and the client's cache slot can be held by a flush that
	/// waits on one too.
	async fn hydrate(&mut self, ctx: &ApplyContext<'_>, wanted: Wanted) {
		let Some(cache_db) = &ctx.cache_db else {
			tracing::debug!(
				"apply: no cache is configured; each action of this page asks the server for its own object"
			);
			return;
		};
		// The pass's snapshot carries only the columns its view reads (see `cache::enumerate`), so
		// the whole payloads are read here, by uuid — on a blocking thread, which takes its own copy
		// of the path.
		let uuids: Vec<Uuid> = wanted.files.into_iter().chain(wanted.dirs).collect();
		match hydrate_cached_items_at(cache_db.clone(), uuids).await {
			Ok(items) => self.extend(items),
			Err(error) => tracing::debug!(
				"apply: could not read the cached objects this page acts on ({error}); each of its actions asks the server for its own"
			),
		}
	}

	/// Drop what the page before held.
	fn clear(&mut self) {
		self.files.clear();
		self.dirs.clear();
	}

	/// Take in what the cache holds of what the page acts on.
	fn extend(&mut self, cached: Vec<SearchResult>) {
		for item in cached {
			match item {
				SearchResult::File(file) => {
					self.files.insert(file.uuid, RemoteFile::from(file));
				}
				SearchResult::Dir(dir) => {
					self.dirs.insert(dir.uuid, RemoteDirectory::from(dir));
				}
			}
		}
	}
}

/// The remote file `uuid` an action acts on: `hydrated`, the copy its page read from the cache, or
/// else the server's. The cache lacks a file this engine wrote that it has not listed yet, and the
/// server is authoritative about those (it minted them). Asked by the action itself, never at a page
/// turn (see [`PageObjects::hydrate`]). A file that cannot be fetched fails the action with
/// `missing`, and the pass carries on.
async fn file_or_fetch(
	ctx: &ApplyContext<'_>,
	hydrated: Option<RemoteFile>,
	uuid: Uuid,
	missing: &'static str,
) -> Result<RemoteFile, crate::Error> {
	if let Some(file) = hydrated {
		return Ok(file);
	}
	ctx.client.get_file(uuid).await.map_err(|error| {
		tracing::debug!(
			"apply: cannot resolve the file {uuid} this engine wrote ({error}); its action fails this pass"
		);
		internal(missing)
	})
}

/// The remote directory `uuid` an action acts on, as [`file_or_fetch`] finds a file. `None` for
/// `uuid` is a directory the view listed none for, and fails the action with `missing` unasked.
async fn dir_or_fetch(
	ctx: &ApplyContext<'_>,
	hydrated: Option<RemoteDirectory>,
	uuid: Option<Uuid>,
	missing: &'static str,
) -> Result<RemoteDirectory, crate::Error> {
	if let Some(dir) = hydrated {
		return Ok(dir);
	}
	let uuid = uuid.ok_or_else(|| internal(missing))?;
	ctx.client.get_dir(uuid).await.map_err(|error| {
		tracing::debug!(
			"apply: cannot resolve the remote dir {uuid} this engine wrote ({error}); its action fails this pass"
		);
		internal(missing)
	})
}

/// Whether an action touches the remote at all: a remote write, or a download.
fn touches_remote(action: &SyncAction) -> bool {
	mutates_remote(action) || matches!(action, SyncAction::DownloadFile { .. })
}

/// Whether `full` holds back the serial action `action` (see [`FullSides::holds`]). A remote
/// directory move it holds keeps everything under its destination from running too, as one that
/// failed does: the plan names that subtree by a path the remote does not carry yet.
fn holds_serial(full: &FullSides, failed: &mut FailedDirMoves, action: &SyncAction) -> bool {
	if !full.holds(action) {
		return false;
	}
	tracing::debug!(
		"apply: {} held, the remote is out of reach",
		action.describe()
	);
	if let SyncAction::MoveRemote {
		to_path,
		kind: NodeKind::Dir,
		..
	} = action
	{
		failed.held.push(to_path.clone());
	}
	true
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

/// Apply one action serially: emit its in-progress event, run it, and record success/failure (see
/// [`note_action_failed`]).
async fn apply_serial(
	ctx: &ApplyContext<'_>,
	planned: &PlannedEntry,
	plan: &mut PagedPlan,
	failed: &mut FailedDirMoves,
	full: &FullSides,
	report: &mut SyncReport,
	observer: &mut (dyn FnMut(SyncEvent) + Send),
) {
	let action = &planned.action;
	observer(action.to_event());
	tracing::debug!("apply: {}", action.describe());
	let error = match apply_one(ctx, planned, plan, report, &mut *observer).await {
		Ok(()) => {
			note_written(ctx, plan, action, report);
			return;
		}
		Err(error) => error,
	};
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
		// A move that halted the pass is the pass's failure, not the subtree's: what is under it is
		// held, as everything after it that the halt stops is, and counts against no streak.
		if pass_halt(&error).is_some() {
			failed.held.push(to_path.clone());
		} else {
			failed.dirs.push(to_path.clone());
		}
	}
	note_action_failed(action, &error, full, report, observer);
}

/// Whether `action` is one [`apply_creates`] carries out.
fn is_remote_dir_create(action: &SyncAction) -> bool {
	matches!(action, SyncAction::CreateRemoteDir { .. })
}

/// Whether `action` is one [`apply_trashes`] carries out.
fn is_trash(action: &SyncAction) -> bool {
	matches!(action, SyncAction::TrashRemote { .. })
}

/// `first`, which the lease has already been prepared and counted for, and the rest of its run in
/// `run` that `same` holds for, as one chunk: at most [`SYNC_BULK_CHUNK`] actions, and no more than
/// the hold has room for. Each is counted against the hold here, with no release inside a chunk, so
/// a tie is never split.
async fn take_chunk(
	ctx: &ApplyContext<'_>,
	plan: &mut PagedPlan,
	lease: &mut Lease,
	run: Run,
	first: PlannedEntry,
	same: impl Fn(&SyncAction) -> bool,
) -> Result<Vec<PlannedEntry>, crate::Error> {
	let more = plan
		.take_run(ctx, run, chunk_room(lease, &ctx.lock_budget), same)
		.await?;
	for staged in &more {
		lease.note_start(&staged.entry.action);
	}
	Ok(std::iter::once(first)
		.chain(more.into_iter().map(|staged| staged.entry))
		.collect())
}

/// How many more actions a chunk takes after its first, which `lease` has been counted for: what
/// fits in one bulk request and in the hold.
fn chunk_room(lease: &Lease, budget: &LockBudget) -> usize {
	(SYNC_BULK_CHUNK - 1).min(lease.room(budget))
}

/// What a chunk of directory creates reads besides its plan (see [`apply_creates`]).
struct ChunkContext<'p, 'c> {
	ctx: &'p ApplyContext<'c>,
	/// The drive lock the pass holds for the chunk, which the links and shares looked up for it
	/// are kept for (see [`Client::held_connected_targets`]).
	hold: &'p Arc<ResourceLock>,
	/// The sides found full so far.
	full: &'p FullSides,
}

/// A remote directory create of a chunk, ready to send (see [`compose_creates`]).
struct ComposedCreate<'c> {
	action: &'c SyncAction,
	/// The remote item at its path that the create replaces, for its journal row.
	replaced: Option<Uuid>,
	dir: NewDir<'c>,
}

/// Create a chunk of the plan's remote directories, in plan order and parents first, with one
/// request ([`Client::create_dirs`]): compose each create ([`compose_creates`]), send the ones
/// that composed, and record each answer ([`land_creates`]).
///
/// Returns how many of the chunk it carried out or failed. The creates it did not get to, behind a
/// stage read or a record that could not be written, are not counted (see
/// [`SyncReport::interrupted`]): the server made the ones that were sent, and the next pass adopts
/// them.
///
/// Each create's event goes out as it is composed, so the events of a chunk come before the
/// failures its answers bring.
async fn apply_creates(
	chunk_ctx: &ChunkContext<'_, '_>,
	chunk: Vec<PlannedEntry>,
	plan: &mut PagedPlan,
	failed: &FailedDirMoves,
	report: &mut SyncReport,
	observer: &mut (dyn FnMut(SyncEvent) + Send),
) -> usize {
	let (composed, settled) = compose_creates(
		chunk_ctx,
		&chunk,
		plan,
		failed,
		&mut HashMap::new(),
		report,
		observer,
	)
	.await;
	if composed.is_empty() || !admits_more(report) {
		return settled;
	}
	let dirs: Vec<NewDir<'_>> = composed.iter().map(|create| create.dir).collect();
	tracing::debug!(
		"apply: creating {} remote directories in one request",
		dirs.len()
	);
	let answers = match chunk_ctx.ctx.client.create_dirs(&dirs).await {
		Ok(answers) => answers,
		Err(error) => all_failed(error, dirs.len()),
	};
	settled + land_creates(chunk_ctx, &composed, answers, plan, report, observer).await
}

/// The answers to `count` items whose request failed as a whole with `error`: a lock that could not
/// be taken, under which nothing was sent.
fn all_failed<T>(error: crate::Error, count: usize) -> Vec<Result<T, crate::Error>> {
	let error = Arc::new(error);
	(0..count)
		.map(|_| Err(crate::Error::unshared(Arc::clone(&error))))
		.collect()
}

/// Compose each create of `chunk`, in plan order: where it goes and what it is called. A create
/// under a directory an earlier create of the chunk makes names that create; any other names the
/// directory it lands in (see [`PagedPlan::parent_of`]).
///
/// The links and shares of each directory the chunk lands in are looked up once, here, all at once,
/// before anything is created, unless `looked_up` already holds that directory's lookup. A lookup
/// that fails fails every create under that directory, so nothing is created that would miss them,
/// and the next pass plans those creates again.
///
/// A stage read that fails stops the pass (see [`admits_more`]), and so the composing: the creates
/// of the chunk after it are left uncounted, for the next pass.
///
/// Returns the creates that composed, and how many of the chunk it skipped or failed.
async fn compose_creates<'c>(
	chunk_ctx: &ChunkContext<'_, '_>,
	chunk: &'c [PlannedEntry],
	plan: &PagedPlan,
	failed: &FailedDirMoves,
	looked_up: &mut HashMap<Uuid, Result<(), Arc<crate::Error>>>,
	report: &mut SyncReport,
	observer: &mut (dyn FnMut(SyncEvent) + Send),
) -> (Vec<ComposedCreate<'c>>, usize) {
	let mut composed = Vec::with_capacity(chunk.len());
	let mut settled = 0;
	// Where each create that composed sits in `composed`, by its path.
	let mut earlier = HashMap::new();
	for planned in chunk {
		if !admits_more(report) {
			break;
		}
		let action = &planned.action;
		if failed.skips(action, report, observer) {
			settled += 1;
			continue;
		}
		observer(action.to_event());
		tracing::debug!("apply: {}", action.describe());
		match compose_create(planned, plan, &earlier) {
			Ok(create) => {
				earlier.insert(action.rel_path(), composed.len());
				composed.push(create);
			}
			Err(error) => {
				settled += 1;
				note_action_failed(action, &error, chunk_ctx.full, report, observer);
			}
		}
	}
	// Nothing is sent once the pass stops.
	if !admits_more(report) {
		return (composed, settled);
	}

	let missing: HashSet<Uuid> = composed
		.iter()
		.filter_map(|create| match create.dir.parent {
			NewDirParent::Existing(dir) if !looked_up.contains_key(&dir) => Some(dir),
			_ => None,
		})
		.collect();
	let (client, hold) = (chunk_ctx.ctx.client, chunk_ctx.hold);
	let lookups = join_all(missing.into_iter().map(|dir| async move {
		let lookup = client.held_connected_targets(hold, dir).await;
		(dir, lookup.map(|_| ()).map_err(Arc::new))
	}))
	.await;
	looked_up.extend(lookups);

	// A create dropped here moves the ones after it, so each `Earlier` parent is renumbered: by
	// where each create of `composed` went in `sent`, or the lookup error that failed it and every
	// create under it.
	let mut sent = Vec::with_capacity(composed.len());
	let mut placed: Vec<Result<usize, Arc<crate::Error>>> = Vec::with_capacity(composed.len());
	for mut create in composed {
		let place = match create.dir.parent {
			NewDirParent::Existing(dir) => match looked_up.get(&dir) {
				Some(Ok(())) => Ok(sent.len()),
				Some(Err(error)) => Err(Arc::clone(error)),
				None => Err(Arc::new(internal(
					"a parent's links and shares were not looked up (should be impossible)",
				))),
			},
			NewDirParent::Earlier(index) => match placed.get(index) {
				Some(Ok(moved)) => {
					create.dir.parent = NewDirParent::Earlier(*moved);
					Ok(sent.len())
				}
				Some(Err(error)) => Err(Arc::clone(error)),
				None => Err(Arc::new(internal(
					"a create's parent comes after it in the chunk (should be impossible)",
				))),
			},
		};
		if let Err(error) = &place {
			settled += 1;
			let error = crate::Error::unshared(Arc::clone(error));
			note_action_failed(create.action, &error, chunk_ctx.full, report, observer);
		} else {
			sent.push(create);
		}
		placed.push(place);
	}
	(sent, settled)
}

/// Compose the create `planned` (see [`compose_creates`]).
fn compose_create<'c>(
	planned: &'c PlannedEntry,
	plan: &PagedPlan,
	earlier: &HashMap<&str, usize>,
) -> Result<ComposedCreate<'c>, crate::Error> {
	let (
		action @ SyncAction::CreateRemoteDir { rel_path },
		PassStart::CreateRemoteDir {
			local,
			remote,
			parent_uuid,
		},
	) = (&planned.action, &planned.start)
	else {
		return Err(unpaired());
	};
	let (parent_path, name) = parent_and_name(rel_path);
	let parent = match earlier.get(parent_path) {
		Some(&index) => NewDirParent::Earlier(index),
		None => NewDirParent::Existing(
			plan.parent_of(rel_path, *parent_uuid)?
				.ok_or_else(|| internal("remote parent dir not yet created"))?,
		),
	};
	let created = local
		.as_deref()
		.map(|n| millis_to_dt(n.mtime_millis))
		.unwrap_or_else(Utc::now);
	Ok(ComposedCreate {
		action,
		replaced: remote.as_deref().map(|node| node.remote_uuid),
		dir: NewDir {
			name,
			created,
			parent,
		},
	})
}

/// Record each answer to the creates `composed`, in order: a directory made is recorded, and a
/// failure is recorded as one. Every directory made is first added to its parent's links and
/// shares, and one the server merged into an existing directory to those it is missing from
/// ([`propagate_chunk`]).
///
/// Returns how many it recorded. It stops at the first record that could not be written, since
/// every later one would wait out the same wedged store. The server made the directories behind
/// it, and they were added to their parents' links and shares; the next pass adopts them.
async fn land_creates(
	chunk_ctx: &ChunkContext<'_, '_>,
	composed: &[ComposedCreate<'_>],
	answers: Vec<Result<CreatedDir, crate::Error>>,
	plan: &mut PagedPlan,
	report: &mut SyncReport,
	observer: &mut (dyn FnMut(SyncEvent) + Send),
) -> usize {
	let answers = refuse_merges_into_legacy_trash(composed, answers);
	propagate_chunk(chunk_ctx, composed, &answers).await;
	let mut settled = 0;
	for (create, answer) in composed.iter().zip(answers) {
		settled += 1;
		let landed = match answer {
			Ok(created) => record_create(chunk_ctx.ctx, create, created.uuid, plan, report).await,
			Err(error) => Err(error),
		};
		if let Err(error) = landed {
			note_action_failed(create.action, &error, chunk_ctx.full, report, observer);
		}
		if !admits_more(report) {
			break;
		}
	}
	settled
}

/// `answers` to the creates `composed`, with a failure in place of each one the server merged into
/// filen-sync's trash folder, and of every create under such a one.
///
/// A create named like that folder but in another case (see [`folds_onto_legacy_trash`]) is the
/// user's directory. The view leaves the reserved folder out, so the plan cannot see that the
/// server's case-insensitive name dedup will hand the create that folder: recorded, the row would
/// name a directory no view ever shows, and the next pass would read the user's directory as
/// deleted remotely and move it into the bin. Failed instead, it records nothing, and the pass
/// reports it. The server has already made the creates under it inside the reserved folder; they
/// are empty, and in a folder neither engine syncs.
fn refuse_merges_into_legacy_trash(
	composed: &[ComposedCreate<'_>],
	answers: Vec<Result<CreatedDir, crate::Error>>,
) -> Vec<Result<CreatedDir, crate::Error>> {
	let mut refused: Vec<bool> = Vec::with_capacity(composed.len());
	composed
		.iter()
		.zip(answers)
		.map(|(create, answer)| {
			let under_refused = matches!(
				create.dir.parent,
				NewDirParent::Earlier(index) if refused.get(index).copied().unwrap_or(false)
			);
			let merged_into_trash = answer
				.as_ref()
				.is_ok_and(|created| created.merged && folds_onto_legacy_trash(create.dir.name));
			refused.push(under_refused || merged_into_trash);
			match answer {
				Ok(_) if merged_into_trash => Err(internal_owned(format!(
					"refusing to record {:?}: the server merged it into filen-sync's trash folder, \
					 which is never synced",
					create.action.rel_path()
				))),
				Ok(_) if under_refused => Err(internal_owned(format!(
					"refusing to record {:?}: its parent was merged into filen-sync's trash folder",
					create.action.rel_path()
				))),
				answer => answer,
			}
		})
		.collect()
}

/// Add each directory the creates `composed` made to its parent's links and shares
/// ([`propagate_created`]), and each one the server merged into an existing directory to those of
/// them it is missing from ([`propagate_merged`]), a level of the chunk's tree at a time, each
/// level's directories at once: a level's parents are added before it. A merged directory keeps
/// links and shares of its own; the creates under it look them up.
///
/// It runs before any create is recorded, and needs no store: the server made every directory
/// answered `Ok`, whether or not its record can be written.
async fn propagate_chunk(
	chunk_ctx: &ChunkContext<'_, '_>,
	composed: &[ComposedCreate<'_>],
	answers: &[Result<CreatedDir, crate::Error>],
) {
	// The uuid each create was answered, by its place in `composed`, for the creates under it.
	let answered: Vec<Option<Uuid>> = answers
		.iter()
		.map(|answer| answer.as_ref().ok().map(|created| created.uuid))
		.collect();
	// How far each create sits below a directory that was there before the chunk.
	let mut depths: Vec<usize> = Vec::with_capacity(composed.len());
	for create in composed {
		let depth = match create.dir.parent {
			NewDirParent::Existing(_) => 0,
			NewDirParent::Earlier(index) => depths.get(index).map_or(0, |depth| depth + 1),
		};
		depths.push(depth);
	}
	let deepest = depths.iter().copied().max().unwrap_or(0);
	let at_level =
		|level: usize| {
			composed.iter().zip(answers).zip(&depths).filter_map(
				move |((create, answer), &depth)| match answer {
					Ok(created) if depth == level => Some((create, created)),
					_ => None,
				},
			)
		};
	for level in 0..=deepest {
		// The merges first: each forgets what the hold kept once it is done, which the fresh
		// directories of the level would otherwise keep their parents' targets into for nothing.
		join_all(
			at_level(level)
				.filter(|(_, created)| created.merged)
				.map(|(_, created)| propagate_merged(chunk_ctx, created)),
		)
		.await;
		join_all(
			at_level(level)
				.filter(|(_, created)| !created.merged)
				.map(|(create, created)| propagate_created(chunk_ctx, create, created, &answered)),
		)
		.await;
	}
}

/// Add the directory `created` that `create` made to the links and shares of the directory it was
/// made in, and keep those as its own for the rest of the hold, so that what goes under it later in
/// the hold looks nothing up.
///
/// The lease of the hold is confirmed first, as for every write made under it after an await; once
/// it is lost, the lock is taken again and the links and shares are looked up under the new one.
///
/// A failure is only logged: the directory exists, and failing its create would only make the next
/// pass adopt it as it is.
async fn propagate_created(
	chunk_ctx: &ChunkContext<'_, '_>,
	create: &ComposedCreate<'_>,
	created: &CreatedDir,
	answered: &[Option<Uuid>],
) {
	let client = chunk_ctx.ctx.client;
	let retaken = match confirm_or_retake(chunk_ctx.hold, || client.lock_drive()).await {
		Ok(retaken) => retaken,
		Err(error) => {
			tracing::warn!(
				"apply: {} was created, but the drive lock could not be confirmed to add it to its \
				 parent's links and shares, so it is not added to them: {error}",
				create.action.describe()
			);
			return;
		}
	};
	let hold = retaken.as_ref().unwrap_or(chunk_ctx.hold);
	let parent = match create.dir.parent {
		NewDirParent::Existing(uuid) => Some(uuid),
		NewDirParent::Earlier(index) => answered.get(index).copied().flatten(),
	};
	let Some(parent) = parent else {
		tracing::warn!(
			"apply: {} was created under a directory with no answer (should be impossible); it is \
			 not added to that directory's links and shares",
			create.action.describe()
		);
		return;
	};
	let lookup = match client.held_connected_targets(hold, parent).await {
		Ok(lookup) => lookup,
		Err(error) => {
			tracing::warn!(
				"apply: {} was created, but the links and shares of its parent could not be looked \
				 up, so it is not added to them: {error}",
				create.action.describe()
			);
			return;
		}
	};
	if !lookup.targets.is_empty() {
		// `create_dirs` answers `Ok` only for a directory it minted, through the same validation.
		let name = ValidatedName::try_from(create.dir.name)
			.expect("create_dirs validated this name (should be impossible)");
		let dir = RemoteDirectory::new_from_parts(
			created.uuid,
			RemoteDirectory::make_meta(name, create.dir.created),
			parent.into(),
			created.timestamp,
		);
		let errors = client
			.propagate_to_targets(&lookup.targets, &[(&dir).into()])
			.await;
		for error in &errors {
			tracing::warn!(
				"failed to propagate a connected-parent update to a link or shared user: {error}"
			);
		}
		// A directory missing from some of them is not one their lookup describes.
		if !errors.is_empty() {
			return;
		}
	}
	client.inherit_connected_targets(hold, created.uuid, &lookup);
}

/// Add the existing directory `created` that a create merged into to the links and shares of its
/// parent it is missing from, as [`Client::create_dir`] does for a merge: the create of a
/// directory whose propagation failed comes back as one. That adds its whole subtree to them, so
/// nothing kept for the hold is answered while it runs, and all of it is forgotten after.
///
/// A failure is only logged, as for [`propagate_created`].
async fn propagate_merged(chunk_ctx: &ChunkContext<'_, '_>, created: &CreatedDir) {
	let client = chunk_ctx.ctx.client;
	let _change = client.connected_targets_change();
	let dir = match client.get_dir(created.uuid).await {
		Ok(dir) => dir,
		Err(error) => {
			tracing::warn!(
				"apply: a create merged into {}, which could not be read to add it to the links and \
				 shares of its parent it is missing from: {error}",
				created.uuid
			);
			return;
		}
	};
	if let Err(error) = client
		.update_item_with_maybe_connected_parent(
			chunk_ctx.hold,
			(&dir).into(),
			ParentTargets::Missing,
		)
		.await
	{
		tracing::warn!(
			"apply: a create merged into {}, which could not be added to the links and shares of \
			 its parent it is missing from: {error}",
			created.uuid
		);
	}
}

/// Record the directory `uuid` that `create` made, or merged into: its baseline row and journal
/// row, and where it is in the pass's stage.
async fn record_create(
	ctx: &ApplyContext<'_>,
	create: &ComposedCreate<'_>,
	uuid: Uuid,
	plan: &mut PagedPlan,
	report: &mut SyncReport,
) -> Result<(), crate::Error> {
	let rel_path = create.action.rel_path();
	let kind = PendingKind::Created {
		path: rel_path.to_string(),
		replaced: create.replaced,
	};
	let entry = dir_entry(rel_path, Some(uuid), None);
	commit_remote_write(ctx, uuid, kind, &[BaselineChange::Upsert(&entry)]).await?;
	// What lands under the directory finds it here, and a later deletion of the pass that names it
	// is refused: when the dedup answered with an existing directory, this is ITS uuid. A record
	// that failed fails the action, and so the pass (see `record_not_written`), before anything is
	// put under it or deleted.
	plan.record_created(rel_path, uuid)?;
	report.remote_dirs_created += 1;
	note_written(ctx, plan, create.action, report);
	Ok(())
}

/// A remote trash of a chunk, ready to send (see [`compose_trashes`]).
struct ComposedTrash<'c> {
	action: &'c SyncAction,
	/// The item its baseline row and journal row name.
	remote_uuid: Uuid,
	/// The deletions it carries out along with its own, read before it is sent.
	carried: Option<Carried>,
	/// What the request sends for it.
	item: trash::Item,
}

/// Move a chunk of the plan's remote items to the trash, in plan order, with one request
/// ([`Client::trash_items`]): compose each trash ([`compose_trashes`]), send the ones that composed,
/// and record each answer ([`land_trashes`]). The screen keeps only the topmost trashes, so no item
/// of a chunk is under another.
///
/// Returns how many of the chunk it carried out or failed, as [`apply_creates`] does.
async fn apply_trashes(
	ctx: &ApplyContext<'_>,
	chunk: Vec<PlannedEntry>,
	plan: &mut PagedPlan,
	failed: &FailedDirMoves,
	full: &FullSides,
	report: &mut SyncReport,
	observer: &mut (dyn FnMut(SyncEvent) + Send),
) -> usize {
	let (composed, settled) = compose_trashes(&chunk, plan, failed, full, report, observer);
	if composed.is_empty() || !admits_more(report) {
		return settled;
	}
	// Each item is a uuid, so the copy the request is sent from is cheap.
	let items: Vec<trash::Item> = composed.iter().map(|trash| trash.item.clone()).collect();
	tracing::debug!(
		"apply: trashing {} remote items in one request",
		items.len()
	);
	let answers = match ctx.client.trash_items(&items).await {
		Ok(answers) => answers,
		Err(error) => all_failed(error, items.len()),
	};
	settled + land_trashes(ctx, composed, answers, plan, full, report, observer).await
}

/// Compose each trash of `chunk`, in plan order: the item it names, and the deletions it carries.
/// A trash that would take down something this pass wrote is refused, and fails alone.
///
/// Returns the trashes that composed, and how many of the chunk it skipped or failed.
fn compose_trashes<'c>(
	chunk: &'c [PlannedEntry],
	plan: &PagedPlan,
	failed: &FailedDirMoves,
	full: &FullSides,
	report: &mut SyncReport,
	observer: &mut (dyn FnMut(SyncEvent) + Send),
) -> (Vec<ComposedTrash<'c>>, usize) {
	let mut composed = Vec::with_capacity(chunk.len());
	let mut settled = 0;
	for planned in chunk {
		let action = &planned.action;
		if failed.skips(action, report, observer) {
			settled += 1;
			continue;
		}
		observer(action.to_event());
		tracing::debug!("apply: {}", action.describe());
		match compose_trash(planned, plan) {
			Ok(trash) => composed.push(trash),
			Err(error) => {
				settled += 1;
				note_action_failed(action, &error, full, report, observer);
			}
		}
	}
	(composed, settled)
}

/// Compose the trash `planned` (see [`compose_trashes`]). A directory's item names the directory
/// the view listed at its path.
fn compose_trash<'c>(
	planned: &'c PlannedEntry,
	plan: &PagedPlan,
) -> Result<ComposedTrash<'c>, crate::Error> {
	let action @ SyncAction::TrashRemote {
		rel_path,
		kind,
		remote_uuid,
	} = &planned.action
	else {
		return Err(unpaired());
	};
	if plan.pager.shields_remote(rel_path, *remote_uuid) {
		return Err(internal_owned(format!(
			"refusing to trash {rel_path:?}: this pass already wrote that item, or under that name"
		)));
	}
	// Read before the act, which nothing then stands between and its record.
	let carried = plan.pager.carried(action);
	let item = match (kind, &planned.start) {
		(NodeKind::File, _) => FileOrDir::File(file::trash::Request { uuid: *remote_uuid }),
		(NodeKind::Dir, PassStart::Remote { remote }) => {
			let uuid = dir_uuid(remote.as_deref())
				.ok_or_else(|| internal("trash target dir missing from the snapshot"))?;
			FileOrDir::Dir(dir::trash::Request { uuid })
		}
		(NodeKind::Dir, _) => return Err(unpaired()),
	};
	Ok(ComposedTrash {
		action,
		remote_uuid: *remote_uuid,
		carried,
		item,
	})
}

/// Record each answer to the trashes `trashes`, in order: a trash the server answered `Ok` drops
/// its row and the rows it carried ([`commit_trash`]); a failure is recorded as one. A missing or
/// already trashed item is answered `Ok`, which is what the trash wanted.
///
/// Returns how many it recorded. It stops at the first record that could not be written, since
/// every later one would wait out the same wedged store; the trashes behind it landed, and the next
/// pass drops their rows or trashes them again, which changes nothing.
async fn land_trashes(
	ctx: &ApplyContext<'_>,
	trashes: Vec<ComposedTrash<'_>>,
	answers: Vec<Result<(), crate::Error>>,
	plan: &PagedPlan,
	full: &FullSides,
	report: &mut SyncReport,
	observer: &mut (dyn FnMut(SyncEvent) + Send),
) -> usize {
	let mut settled = 0;
	for (trash, answer) in trashes.into_iter().zip(answers) {
		settled += 1;
		let ComposedTrash {
			action,
			remote_uuid,
			carried,
			..
		} = trash;
		if let Err(error) = answer {
			note_action_failed(action, &error, full, report, observer);
			continue;
		}
		// The row is about to go, so a snapshot that has not applied the trash yet reads this item
		// as an untracked remote one with nothing local — a deletion to make all over again. Record
		// the trash so the next pass suppresses that.
		if let Err(error) = commit_trash(ctx, remote_uuid, action.rel_path(), carried).await {
			note_action_failed(action, &error, full, report, observer);
			break;
		}
		report.remotely_trashed += 1;
		note_written(ctx, plan, action, report);
		if !admits_more(report) {
			break;
		}
	}
	settled
}

/// Record that `action` landed, for the deletions after it (see [`Pager::note_written`]).
///
/// A record the stage cannot take leaves that write unshielded, so the pass then admits nothing
/// more, as it does once a baseline row is lost ([`admits_more`]): no deletion runs after it to
/// take the write down. The write itself stands, and its path keeps no failure streak for it.
///
/// It is also what the engine ends failure streaks by: an action that landed is recorded in
/// [`SyncReport::landed`] when its success ends one.
fn note_written(
	ctx: &ApplyContext<'_>,
	plan: &PagedPlan,
	action: &SyncAction,
	report: &mut SyncReport,
) {
	if ctx.streaked.contains(action.rel_path()) {
		report.landed.push(action.rel_path().to_string());
	}
	if let Err(error) = plan.pager.note_written(action) {
		let error = super::engine::db_error(error, "recording what this pass wrote in its stage");
		tracing::error!(
			"apply: {} landed, but {error}; the pass starts nothing more",
			action.describe()
		);
		report
			.errors
			.push(format!("{}: {error}", action.rel_path()));
		report.store_failed = Some(StoreFailure::RecordNotWritten(error.kind()));
	}
}

/// Record one action failure: the human-readable line AND the `(path, error)` pair the engine's
/// per-path failure counter consumes, so a path that fails every pass is eventually reported as
/// unsyncable instead of retried forever.
///
/// A failure that says a side has no room left, or that the remote is out of reach or refuses the
/// session (see [`pass_halt`]), is not the path's fault: it sets [`halted`](SyncReport::halted)
/// instead (if nothing did yet), with one line per condition saying so, and is returned so the
/// caller holds back the actions it stops. A full side counts against no streak; a remote outage
/// goes to [`transient_failures`](SyncReport::transient_failures), which the engine counts only
/// spaced out.
fn note_failure(
	report: &mut SyncReport,
	rel_path: &str,
	error: &crate::Error,
) -> Option<HaltReason> {
	report.errors.push(format!("{rel_path}: {error}"));
	if record_not_written(error) {
		report.store_failed = Some(StoreFailure::RecordNotWritten(error.kind()));
	}
	let Some(reason) = pass_halt(error) else {
		report
			.failed_paths
			.push((rel_path.to_string(), error.to_string()));
		return None;
	};
	if reason.is_remote_outage() {
		report
			.transient_failures
			.push((rel_path.to_string(), error.to_string()));
	}
	let line = reason.held_line();
	if !report.errors.contains(&line) {
		report.errors.push(line);
	}
	report.halted.get_or_insert(reason);
	Some(reason)
}

/// Record an upload that buried a concurrent edit: it counts as an upload, and the conflict it made
/// reaches the caller — even when the row holding the buried version could not be written.
///
/// That row is the only reference this engine ever has to the buried version: the remote head is
/// our own upload, and the version underneath it exists only in the file's history, which is why
/// the engine's conflict loop refuses to re-derive such a row from the current view. So a lost row
/// costs the HOLD, not the warning: the path is not excluded from planning, and the next pass finds
/// both sides holding our bytes and adopts them (`plan::reconcile`'s converged arm). This report is
/// then the only word anybody gets, which is why it is made whatever the store answered — and why
/// the answer itself still makes the pass a failed one, through [`note_failure`].
fn note_overwrote(
	report: &mut SyncReport,
	rel_path: &str,
	conflict: PlannedConflict,
	unrecorded: Option<crate::Error>,
) {
	report.uploaded += 1;
	report.conflicts.push(conflict);
	if let Some(error) = unrecorded {
		tracing::error!(
			"apply: the conflict over {rel_path:?} is REPORTED but not held — the row holding the \
			 buried version could not be written ({error}), so unless the caller resolves it from \
			 this report, the next pass reads both sides holding our bytes and adopts them"
		);
		note_failure(report, rel_path, &error);
	}
}

/// Whether the pass may still start work it has not started yet.
///
/// It may not once a write that RECORDS work already done has failed
/// ([`SyncReport::store_failed`]). Such a write fails only after SQLite's 30 s busy timeout has run
/// out on a write lock another connection is holding, so every remaining action would carry out its
/// remote or local act and then sit out another 30 s to lose its own record too — 500 uploads into
/// a wedged store is four hours of doing work nothing records. The rest are counted as
/// [`interrupted`](SyncReport::interrupted) instead: they ran nothing, the next pass re-plans them,
/// and the watch loop waits out its backoff first because the pass reports a failure.
///
/// Nor may it once a read of its staged plan has failed (see [`note_unread_plan`]): that plan is the
/// only copy of what it is applying.
///
/// Transfers already in flight are left to finish: their network op is done, and dropping one there
/// would leave the remote holding a write nothing records at all.
fn admits_more(report: &SyncReport) -> bool {
	report.store_failed.is_none()
}

/// The sides of a pass found full so far, and whether the remote was found out of reach, read by
/// the actions as they start. Atomics, because the transfers run concurrently while the apply loop
/// records their failures.
#[derive(Default)]
struct FullSides {
	local: AtomicBool,
	remote: AtomicBool,
	/// The remote could not be reached or refused the session ([`HaltReason::is_remote_outage`]).
	unreachable: AtomicBool,
}

impl FullSides {
	fn note(&self, reason: HaltReason) {
		match reason {
			HaltReason::LocalStorageFull => &self.local,
			HaltReason::RemoteStorageFull => &self.remote,
			HaltReason::RemoteUnavailable | HaltReason::Unauthenticated => &self.unreachable,
		}
		.store(true, Ordering::Relaxed);
	}

	/// Whether `action` must not start: it writes to a side found full (an upload once the account
	/// is full, a download once the local disk is), or it touches the remote once the remote was
	/// found out of reach.
	fn holds(&self, action: &SyncAction) -> bool {
		if touches_remote(action) && self.unreachable.load(Ordering::Relaxed) {
			return true;
		}
		match action {
			SyncAction::UploadFile { .. } => self.remote.load(Ordering::Relaxed),
			SyncAction::DownloadFile { .. } => self.local.load(Ordering::Relaxed),
			_ => false,
		}
	}
}

/// Whether `error` stops the pass rather than failing its own path, and why: a side with no room
/// left (the local disk, or the account), or a remote out of reach or refusing the session (see
/// [`remote_outage`]). Looked for along the whole error chain, including inside an `io::Error`
/// wrapping another error — which is how the upload writer hands on a server refusal. A full side
/// found anywhere in the chain wins over an outage found above it.
fn pass_halt(error: &crate::Error) -> Option<HaltReason> {
	let mut outage = None;
	let mut next: Option<&(dyn std::error::Error + 'static)> = Some(error);
	while let Some(current) = next {
		if current
			.downcast_ref::<crate::Error>()
			.is_some_and(|sdk| sdk.kind() == ErrorKind::MaxStorageReached)
			|| api_code(current) == Some("max_storage_reached")
		{
			return Some(HaltReason::RemoteStorageFull);
		}
		outage = outage.or_else(|| remote_outage(current));
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
	outage
}

/// The code of the server's refusal `error` is, if it is one.
fn api_code<'e>(error: &'e (dyn std::error::Error + 'static)) -> Option<&'e str> {
	match error.downcast_ref::<filen_types::error::ResponseError>() {
		Some(filen_types::error::ResponseError::ApiError {
			code: Some(code), ..
		}) => Some(code.as_str()),
		_ => None,
	}
}

/// Whether one link of an error chain says the remote is out of reach or refuses the session —
/// a condition of the whole pass that the next one may find gone — rather than anything about the
/// request's own path:
///
/// - [`HaltReason::Unauthenticated`]: an SDK error of kind [`ErrorKind::Unauthenticated`]
///   (the server does not know the API key, `api_key_not_found`), that code on its own, or an
///   HTTP 401.
/// - [`HaltReason::RemoteUnavailable`]: an HTTP 5xx, 408 or 429 the retries could not get past,
///   or a transport error with no response at all — a connect that failed (refused, unresolvable,
///   timed out), a request that timed out, or a connection that died under the request
///   ([`is_connection_lost`]).
///
/// Only the HTTP client's own error decides the second: an IO error on its own is the local disk's
/// as often as the network's, and a request error that is neither of those (a body that could not
/// be read from a local file, a malformed request) is the path's. Every other status — 404, 403,
/// 400 and the like — and every server refusal by code other than the session's is the path's
/// too.
fn remote_outage(link: &(dyn std::error::Error + 'static)) -> Option<HaltReason> {
	if link
		.downcast_ref::<crate::Error>()
		.is_some_and(|sdk| sdk.kind() == ErrorKind::Unauthenticated)
		|| api_code(link) == Some("api_key_not_found")
	{
		return Some(HaltReason::Unauthenticated);
	}
	let transport = link.downcast_ref::<reqwest::Error>()?;
	match transport.status() {
		Some(StatusCode::UNAUTHORIZED) => Some(HaltReason::Unauthenticated),
		Some(status) => (status.is_server_error()
			|| status == StatusCode::REQUEST_TIMEOUT
			|| status == StatusCode::TOO_MANY_REQUESTS)
			.then_some(HaltReason::RemoteUnavailable),
		None => (transport.is_connect() || transport.is_timeout() || is_connection_lost(transport))
			.then_some(HaltReason::RemoteUnavailable),
	}
}

/// Whether one transfer ran at all — the pass can be cancelled before it starts, or while its
/// network op is in flight (see [`PassGate`]).
#[derive(Debug)]
enum Transfer {
	/// It ran to the end and recorded what it did.
	Done,
	/// The upload landed and IS the remote head — but the server's version chain shows it went on
	/// top of a version this pass never saw. Another client edited the file between the snapshot
	/// this pass read and our upload, and our bytes buried it; the conflict says so.
	Overwrote {
		conflict: Box<PlannedConflict>,
		/// The store's answer when the row that holds the buried version could NOT be written.
		/// The conflict travels anyway: that row is the only place the buried version is named —
		/// the remote head is our own upload and the version underneath it survives only in the
		/// file's history — so dropping the conflict with the row would leave nobody, and no later
		/// pass, any word of the edit these bytes buried.
		unrecorded: Option<crate::Error>,
	},
	/// The pass was cancelled: it never started, or a cancel dropped it mid-flight. Either way it
	/// recorded nothing, and the next pass re-plans it.
	Interrupted,
}

/// Record a local copy a write moved into the quarantine bin: the path goes in the report, and the
/// caller hears about it as it happens ([`SyncEvent::Quarantined`]).
///
/// The one entrance for both writes that can move a copy aside — the pre-download stash and a move
/// onto an occupied destination — so neither can grow a report entry without the event, or the
/// event without the entry.
///
/// Called for what a transfer moved aside whatever that transfer then RETURNED: once the write has
/// landed the copy stays in the bin, and the bookkeeping that follows the write can still fail (a
/// baseline row that will not persist). Announcing only on the success path would leave the pass
/// reporting `ActionFailed` for a path whose local copy it had silently carried off.
fn note_quarantined(
	report: &mut SyncReport,
	observer: &mut (dyn FnMut(SyncEvent) + Send),
	quarantined: Option<(String, PathBuf)>,
) {
	let Some((rel_path, bin_path)) = quarantined else {
		return;
	};
	report.quarantined.push(rel_path.clone());
	observer(SyncEvent::Quarantined { rel_path, bin_path });
}

/// Perform a transfer (download or upload): the network op + baseline write only. It touches no
/// shared mutable state — what it reads of the remote tree is its own `target`, and the baseline
/// store is internally locked — so transfers run concurrently; the caller does the report/event
/// accounting as each completes.
///
/// The pair's pause gates it in two places, and only those two: nothing starts while the pass is
/// suspended, and the NETWORK op alone is raced against a cancel (for a download, that includes
/// the server read of a file the cache lacks, which comes first). The bookkeeping after that op is
/// deliberately outside the race — it waits on the store lock every other transfer of the pass
/// contends for, and a cancel landing in that window would leave the remote holding a write no
/// baseline row describes and no report counts.
///
/// `quarantined` is an out-parameter rather than part of the return value on purpose: it is set the
/// moment the write LANDS, so the path whose local copy this moved aside (and where that copy went)
/// reaches the caller even when the bookkeeping after the write fails and this returns `Err`. A
/// transfer cannot report it itself — transfers run concurrently, so neither the report nor the
/// observer is theirs to touch (see [`note_quarantined`]).
async fn apply_transfer(
	ctx: &ApplyContext<'_>,
	planned: &PlannedEntry,
	target: TransferTarget,
	progress: &UnboundedSender<SyncEvent>,
	quarantined: &mut Option<(String, PathBuf)>,
) -> Result<Transfer, crate::Error> {
	if !ctx.gate.wait_to_start().await {
		return Ok(Transfer::Interrupted);
	}
	match (&planned.action, &planned.start) {
		(
			SyncAction::DownloadFile {
				rel_path,
				remote_uuid,
			},
			PassStart::Download { remote, base },
		) => {
			// The server read for a file the cache lacks is network work like the download, and
			// leaves nothing behind when dropped, so a cancel cuts it short too.
			let Some(remote_file) = ctx
				.gate
				.guard(file_or_fetch(
					ctx,
					target.file,
					*remote_uuid,
					"download target missing from the snapshot",
				))
				.await
			else {
				return Ok(Transfer::Interrupted);
			};
			let remote_file = remote_file?;
			let path = confined_local_target(ctx.local_root, ctx.pair_root(), rel_path)?;
			if let Some(parent) = path.parent() {
				std::fs::create_dir_all(parent).map_err(io_err)?;
			}
			// Remote-wins: stash a local file the baseline cannot vouch for before the download
			// overwrites it, the same way a propagated deletion is quarantined rather than
			// destroyed. An unmodified copy still matching its baseline row is just refreshed.
			//
			// The row this action took is the PASS-START one, so there is none for a path this pass
			// moved a file onto — a remote move that carried a content edit is a `MoveLocal` paired
			// with this very download, and the move already wrote the moved file's row. Read that
			// row back from the store before deciding; without it the download would quarantine the
			// copy the move just renamed into place. Only when something is actually sitting at the
			// target: an ordinary download onto free space has nothing to lose and skips the read.
			let written = match base {
				None if path.exists() => locked(ctx.store)
					.entry(ctx.pair, rel_path)
					.map_err(db_err)?,
				_ => None,
			};
			let base = base.as_deref().or(written.as_ref());
			let stashed = stash_local_target(ctx.local_root, rel_path, base, ctx.bin_day).await?;
			// Keep what is at the path only when its hash matches the remote's: the row written
			// below records the path as holding the remote's bytes, and an mtime no older than the
			// remote's says nothing of that — a same-size remote replacement stamped no newer than
			// the local copy would leave the old bytes there, recorded as synced.
			let Some(downloaded) = ctx
				.gate
				.guard(download_to_path(
					ctx.client,
					&remote_file,
					&path,
					Some(progress_callback(
						progress,
						rel_path,
						TransferDirection::Download,
						remote.as_deref().map_or(0, |node| node.size),
					)),
					KeepLocalWhen::SameHash,
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
			// The download has landed, so a copy moved out of its way stays in the bin: say so.
			// Set BEFORE the row this writes, which can fail — the copy is in the bin either way,
			// and a failure that swallowed the only word of it is what this is out here for.
			*quarantined = stashed.map(|bin| (rel_path.clone(), bin));
			let remote = remote.as_deref();
			// The row this writes has to carry a hash of what landed. Where the remote declares one
			// it is that hash — the same one every later pass compares the path against. Where it
			// declares NONE (a client that wrote no `blake3` in the file's metadata: the common
			// cross-device case), hashing the file just written is the only way the row gets one at
			// all, and a row without one vouches for nothing: it cannot tell a later size- and
			// mtime-preserving edit from the copy it recorded, and the scan reads the file as
			// locally modified on every pass because `Some(hash) != None`.
			let landed_hash = match remote.and_then(|n| n.content_hash) {
				Some(declared) => Some(declared),
				// Off the runtime thread, like every other hash of a local file. A read that fails
				// here leaves the row as it would have been before — hashless, and so not vouched
				// for by the pre-download stash.
				None => {
					let landed = path.clone();
					tokio::task::spawn_blocking(move || hash_file(&landed))
						.await
						.map_err(|e| {
							internal_owned(format!("hashing a downloaded file panicked: {e}"))
						})?
						.inspect_err(|error| {
							tracing::debug!(
								"apply: {rel_path:?} landed but could not be hashed ({error}); its row records no content hash"
							);
						})
						.ok()
				}
			};
			upsert_file_baseline(
				ctx,
				FileRow {
					rel_path,
					remote_uuid: Some(*remote_uuid),
					remote_stable_uuid: remote.and_then(|n| n.stable_uuid),
					content_hash: landed_hash,
					size: remote.map(|n| n.size).unwrap_or(0),
					local_mtime: local_mtime_of(&path),
					remote_modified: remote.map(|n| n.modified_millis),
					// A pull is the moment both sides demonstrably hold the same bytes: the content
					// just fetched IS the agreed content. The two stay equal — a row whose recorded
					// content and agreed content differ is one holding an unconfirmed local edit
					// (`is_unconfirmed_concurrent_edit`), which a fresh pull is not.
					agreed_hash: landed_hash,
				},
			)
			.await?;
		}
		(
			SyncAction::UploadFile { rel_path },
			PassStart::Upload {
				local,
				remote,
				base,
				..
			},
		) => {
			let parent = target
				.parent
				.ok_or_else(|| internal("remote parent dir for upload is missing"))?;
			let path = local_path(ctx.local_root, rel_path);
			// The remote keeps the file's own times, as filen-sync wrote them: an EXIF date would
			// become the remote lastModified (a naive one read as UTC), skew what a pull writes
			// back, and hide an edit that kept the date from devices that pull on a newer
			// lastModified. The upload sets both times from the file's metadata, so with the
			// override off EXIF could fill nothing either, and its parse is skipped altogether.
			let mut builder = FileBuilderOptionalName::new(parent);
			builder.no_exif();
			let Some(upload) = ctx
				.gate
				.guard(ctx.client.upload_file_from_path_with_builder(
					builder,
					path,
					Some(progress_callback(
						progress,
						rel_path,
						TransferDirection::Upload,
						local.as_deref().map_or(0, |node| node.size),
					)),
				))
				.await
			else {
				return Ok(Transfer::Interrupted);
			};
			let (uploaded, _file) = upload?;
			let local = local.as_deref();
			let new_uuid: Uuid = uploaded.uuid();
			// A same-name upload versions whatever the path held: record that uuid, so the next
			// pass can tell a cache that has not caught up (the old uuid still at the path) from
			// someone ELSE having written there since (a third uuid).
			let replaced = remote.as_deref().map(|node| node.remote_uuid);
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
				// No pending-write record: the path is HELD from here on (as long as this row
				// lands), so no pass reconciles against it, and a fold would have to read the row
				// — which describes the buried version, not what the remote holds.
				//
				// A row that could not be written is REPORTED rather than thrown: `?` here would
				// turn the upload into a plain failure, and with the conflict would go the only
				// reference to the buried version there is. Nothing else holds one — the remote
				// head is our own upload — so the next pass would find both sides holding our
				// bytes and adopt them, and the concurrent edit would never be mentioned again.
				let unrecorded = upsert_baseline(ctx, &entry).await.err().map(|error| {
					error.with_context(format!(
						"holding version {} of {rel_path:?}, the concurrent edit this upload buried",
						buried.uuid()
					))
				});
				return Ok(Transfer::Overwrote {
					conflict: Box::new(PlannedConflict {
						rel_path: rel_path.clone(),
						local: local.map(|node| node.kind.into()),
						remote: Some(PlannedNodeKind::File),
					}),
					unrecorded,
				});
			}
			let entry = file_entry(FileRow {
				rel_path,
				remote_uuid: Some(new_uuid),
				// The upload reports the lineage it landed in: the same one when it versioned an
				// existing file at this name, a brand-new one when it created a file.
				remote_stable_uuid: Some(uploaded.stable_uuid()),
				content_hash: local.and_then(|n| n.content_hash),
				size: local.map(|n| n.size).unwrap_or(0),
				local_mtime: local.map(|n| n.mtime_millis),
				remote_modified: Some(uploaded.timestamp.timestamp_millis()),
				// A push does NOT make its own content agreed: the server took our bytes, but
				// another client's edit may already be on its way to the same path. The marker stays
				// on the previous agreed content until a snapshot lists this version at this path
				// (`plan::snapshot_confirms`), and the gap is what surfaces a concurrent edit.
				agreed_hash: base.as_deref().and_then(|b| b.agreed_hash),
			});
			commit_remote_write(ctx, new_uuid, kind, &[BaselineChange::Upsert(&entry)]).await?;
		}
		(SyncAction::DownloadFile { .. } | SyncAction::UploadFile { .. }, _) => {
			return Err(unpaired());
		}
		_ => return Err(internal("apply_transfer called with a non-transfer action")),
	}
	Ok(Transfer::Done)
}

/// The SDK progress callback for the transfer of `rel_path` going `direction`. The SDK hands it byte
/// deltas, already throttled to one call per 200 ms plus a final flush; it adds them up and queues a
/// [`SyncEvent::Progress`] for the apply loop to deliver. The running total is updated and sent
/// under one lock, so the queued `bytes` never go down even if two chunk futures report at once.
fn progress_callback<'a>(
	progress: &'a UnboundedSender<SyncEvent>,
	rel_path: &'a str,
	direction: TransferDirection,
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
			direction,
			bytes: *bytes,
			total,
		});
	})
}

async fn apply_one(
	ctx: &ApplyContext<'_>,
	planned: &PlannedEntry,
	plan: &mut PagedPlan,
	report: &mut SyncReport,
	observer: &mut (dyn FnMut(SyncEvent) + Send),
) -> Result<(), crate::Error> {
	let action = &planned.action;
	match (action, &planned.start) {
		(SyncAction::CreateLocalDir { rel_path }, PassStart::Remote { remote }) => {
			let path = confined_local_target(ctx.local_root, ctx.pair_root(), rel_path)?;
			std::fs::create_dir_all(&path).map_err(io_err)?;
			let remote_uuid = remote.as_deref().map(|n| n.remote_uuid);
			upsert_dir_baseline(ctx, rel_path, remote_uuid, local_mtime_of(&path)).await?;
			report.local_dirs_created += 1;
		}
		// Transfers normally run via the concurrent path in `apply`; these arms keep `apply_one`
		// total and correct if a transfer is ever applied serially.
		(SyncAction::DownloadFile { .. }, _) => {
			// Nothing drains the progress channel on this fallback, so its `Progress` events go
			// nowhere; what it moved aside still reaches the observer below.
			let mut quarantined = None;
			let outcome = apply_transfer(
				ctx,
				planned,
				plan.transfer_target(planned)?,
				&tokio::sync::mpsc::unbounded_channel().0,
				&mut quarantined,
			)
			.await;
			// Before the `?`: a landed download whose baseline row would not persist still moved a
			// local copy into the bin, and that is the pass's last word on those bytes.
			note_quarantined(report, observer, quarantined);
			if matches!(outcome?, Transfer::Done) {
				report.downloaded += 1;
			}
		}
		(SyncAction::DeleteLocal { rel_path, .. }, _) => {
			if plan.pager.shields_local(rel_path) {
				return Err(internal_owned(format!(
					"refusing to quarantine {rel_path:?}: this pass already wrote there, under this or another spelling"
				)));
			}
			// Read before the act, which nothing then stands between and its record.
			let carried = plan.pager.carried(action);
			let _stashed = quarantine_local(ctx.local_root, rel_path, ctx.bin_day)?;
			match carried {
				None => delete_baseline(ctx, rel_path).await?,
				// The carried rows scale with the subtree, so they are dropped off the runtime thread.
				Some(carried) => {
					let (pair, path) = (ctx.pair, rel_path.clone());
					off_store(ctx.store, move |store| {
						store.apply_changes_carrying(
							pair,
							&[BaselineChange::Delete(&path)],
							|drop| carried.each(drop),
						)
					})
					.await?
					.map_err(db_err)?;
				}
			}
			report.locally_deleted += 1;
		}
		(SyncAction::UploadFile { .. }, _) => {
			let mut quarantined = None;
			let outcome = apply_transfer(
				ctx,
				planned,
				plan.transfer_target(planned)?,
				&tokio::sync::mpsc::unbounded_channel().0,
				&mut quarantined,
			)
			.await;
			note_quarantined(report, observer, quarantined);
			match outcome? {
				Transfer::Done => {
					report.uploaded += 1;
				}
				Transfer::Overwrote {
					conflict,
					unrecorded,
				} => note_overwrote(report, action.rel_path(), *conflict, unrecorded),
				Transfer::Interrupted => {}
			}
		}
		(
			SyncAction::MoveRemote {
				from_path,
				to_path,
				kind: NodeKind::File,
				remote_uuid,
			},
			PassStart::MoveRemoteFile {
				local,
				base_from,
				parent_uuid,
			},
		) => {
			let mut remote_file = file_or_fetch(
				ctx,
				plan.file(*remote_uuid),
				*remote_uuid,
				"move-source file missing from the snapshot",
			)
			.await?;
			let (from_parent, from_name) = parent_and_name(from_path);
			let (to_parent, to_name) = parent_and_name(to_path);
			if to_parent != from_parent {
				let parent = plan
					.parent_of(to_path, *parent_uuid)?
					.ok_or_else(|| internal("move-target parent dir is missing"))?;
				ctx.client.inner_move_file(&mut remote_file, parent).await?;
			}
			if to_name != from_name {
				let changes = FileMetaChanges::default()
					.name(to_name)
					.map_err(crate::Error::from)?;
				ctx.client
					.update_file_metadata(&mut remote_file, changes)
					.await?;
			}
			let local = local.as_deref();
			let kind = PendingKind::Moved {
				from: from_path.clone(),
				to: to_path.clone(),
			};
			let entry = file_entry(FileRow {
				rel_path: to_path,
				remote_uuid: Some(*remote_uuid),
				remote_stable_uuid: Some(remote_file.stable_uuid()),
				content_hash: local.and_then(|n| n.content_hash),
				size: local.map(|n| n.size).unwrap_or(0),
				local_mtime: local.map(|n| n.mtime_millis),
				remote_modified: Some(remote_file.timestamp.timestamp_millis()),
				// A move changes no content, so whatever the two sides agreed on at the old path
				// they still agree on at the new one.
				agreed_hash: base_from.as_deref().and_then(|b| b.agreed_hash),
			});
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
		(
			SyncAction::MoveLocal {
				from_path,
				to_path,
				kind: NodeKind::File,
			},
			PassStart::MoveLocalFile {
				remote,
				base,
				base_from,
			},
		) => {
			let from = local_path(ctx.local_root, from_path);
			let to = confined_local_target(ctx.local_root, ctx.pair_root(), to_path)?;
			// The reconciler only plans a move onto a destination the SCAN saw free, but a pass
			// waits on the drive lock in between and a file can land there meanwhile — and
			// `rename` would destroy it without a trace. Same window, and same remedy, as the
			// pre-download stash above; this one reports what it moved itself.
			stash_move_target(
				ctx.local_root,
				to_path,
				base.as_deref(),
				ctx.bin_day,
				report,
				&mut *observer,
			)
			.await?;
			if let Some(parent) = to.parent() {
				std::fs::create_dir_all(parent).map_err(io_err)?;
			}
			std::fs::rename(&from, &to).map_err(io_err)?;
			let row = moved_file_row(
				to_path,
				base_from.as_deref(),
				remote.as_deref(),
				local_mtime_of(&to),
			);
			// One transaction, delete before upsert: a crash between the two would leave the file
			// with no row at either path, and the next pass would read it as newly created.
			locked(ctx.store)
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
		(
			SyncAction::MoveRemote {
				from_path,
				to_path,
				kind: NodeKind::Dir,
				remote_uuid,
			},
			PassStart::MoveRemoteDir {
				remote,
				parent_uuid,
			},
		) => {
			// The view already lists the directory at the path it moves to.
			let uuid = dir_uuid(remote.as_deref()).filter(|uuid| uuid == remote_uuid);
			let mut dir = dir_or_fetch(
				ctx,
				uuid.and_then(|uuid| plan.dir(uuid)),
				uuid,
				"move-source dir missing from the snapshot",
			)
			.await?;
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
				let parent = plan
					.parent_of(to_path, *parent_uuid)?
					.ok_or_else(|| internal("move-target parent dir is missing"))?;
				ctx.client.inner_move_dir(&mut dir, parent).await?;
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
			commit_dir_move(ctx, *remote_uuid, from_path, to_path).await?;
			report.moved_remote += 1;
		}
		(
			SyncAction::MoveLocal {
				from_path,
				to_path,
				kind: NodeKind::Dir,
			},
			_,
		) => {
			let from = local_path(ctx.local_root, from_path);
			let to = confined_local_target(ctx.local_root, ctx.pair_root(), to_path)?;
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
			// Two seeking statements over every row of the moved directory's subtree, so this is
			// the one baseline write of a local action that does not run on a runtime thread. The
			// paths are owned by the closure that runs them.
			let pair = ctx.pair;
			let (from, to) = (from_path.clone(), to_path.clone());
			off_store(ctx.store, move |store| {
				store.apply_changes(
					pair,
					&[BaselineChange::MoveSubtree {
						from: &from,
						to: &to,
					}],
				)
			})
			.await?
			.map_err(db_err)?;
			report.moved_local += 1;
		}
		(SyncAction::Conflict { rel_path }, PassStart::Sides { local, remote }) => {
			// The engine screens conflicts out of the applied plan and records them itself; this
			// arm keeps `apply_one` total and correct if one is ever routed through here.
			record_conflict(
				ctx.store,
				ctx.pair,
				rel_path,
				local.as_deref(),
				remote.as_deref(),
			)
			.await?;
		}
		(SyncAction::AdoptBaseline { rel_path }, PassStart::Sides { local, remote }) => {
			// Record the converged state into the baseline with no transfer, so a later one-sided
			// change at this path is classified correctly. Both sides agree here (the reconciler
			// only emits this when they do).
			match adopt_outcome(
				rel_path,
				local.as_deref(),
				remote.as_deref(),
				ctx.absence_trusted,
			) {
				AdoptOutcome::Record(entry) => upsert_baseline(ctx, &entry).await?,
				AdoptOutcome::DropRow => delete_baseline(ctx, rel_path).await?,
				AdoptOutcome::Keep => {}
			}
		}
		(
			SyncAction::CreateLocalDir { .. }
			| SyncAction::CreateRemoteDir { .. }
			| SyncAction::TrashRemote { .. }
			| SyncAction::MoveRemote { .. }
			| SyncAction::MoveLocal {
				kind: NodeKind::File,
				..
			}
			| SyncAction::Conflict { .. }
			| SyncAction::AdoptBaseline { .. },
			_,
		) => return Err(unpaired()),
	}
	Ok(())
}

/// Stash whatever sits at a local write target that the baseline cannot vouch for, before the
/// write lands on top of it. Every local write that can OVERWRITE — a remote-wins download, and a
/// move whose destination is occupied — goes through here, so a local edit is never destroyed
/// silently; it lands in the quarantine bin the same way a propagated deletion does.
///
/// Returns where it was stashed (nothing stashed: `None`), for a caller whose write may still not
/// happen — see [`restore_stashed`].
///
/// The decision reads the target's bytes (see [`local_holds_unsynced_content`]), so it is made off
/// the runtime thread — the same reason the local scan runs on a blocking thread.
async fn stash_local_target(
	local_root: &Path,
	rel_path: &str,
	base: Option<&BaselineEntry>,
	bin_day: NaiveDate,
) -> Result<Option<PathBuf>, crate::Error> {
	let path = local_path(local_root, rel_path);
	let base = base.cloned();
	let unsynced =
		tokio::task::spawn_blocking(move || local_holds_unsynced_content(&path, base.as_ref()))
			.await
			.map_err(|e| internal_owned(format!("reading a local write target panicked: {e}")))?;
	if unsynced {
		return quarantine_local(local_root, rel_path, bin_day);
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
/// must not be overwritten.
///
/// `(size, mtime)` cannot answer it on its own. An edit that kept both — `touch -r`, an `rsync -a`
/// restore, an editor that puts the timestamp back — is indistinguishable from the synced copy by
/// stat alone, and the scan's own fast-path (`scan::fast_path_hash`) reads it as unchanged for
/// ever, so no plan ever mentions that path. This is therefore the one place those bytes are still
/// read: when the stat matches the row, the file is HASHED and compared. It costs one local read
/// of a file this pass is about to replace with a download of the same path, and it is all that
/// stands between such an edit and a silent overwrite.
///
/// A row that records NO hash is not vouched for either — quarantined, not refreshed in place.
/// There is nothing to compare the bytes against, and the stat it does record is exactly what such
/// an edit preserves. Every row a write of this engine's own produces carries a hash (a download
/// hashes what landed where the remote declared none; an upload records what the scan hashed), and
/// the rows that deliberately hold no local content — a `KeepLocal` resolution, a move that
/// carried one across — record no size either, so they never reach this branch. What is left is a
/// row from some earlier build, and a spare copy in the bin is the cheap side of that trade.
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
	if base.kind != NodeKind::File
		|| !meta.is_file()
		|| base.size != Some(meta.len())
		|| base.local_mtime != Some(FilenMetaExt::modified(&meta).timestamp_millis())
	{
		return true;
	}
	match base.content_hash {
		Some(recorded) => match hash_file(path) {
			Ok(hash) => hash != recorded,
			// Unreadable right now: never overwrite on the strength of a stat that may be stale.
			Err(_) => true,
		},
		// Nothing to compare against, and the stat it does record is what the edit this exists to
		// catch preserves. Quarantine rather than write over it.
		None => true,
	}
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
	store: &SharedStore,
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
	locked(store).upsert_entry(pair, &entry).map_err(db_err)
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

/// What a file write decides about the synced baseline row it leaves behind; the rest of the row
/// is the same for every write (see [`file_entry`]). Named fields, because two of them are
/// `Option<i64>` timestamps that a positional argument list would let a caller swap.
///
/// `agreed_hash` is the caller's to decide, because only the caller knows what its write proved:
/// a pull records the content it just fetched (both sides demonstrably hold it), a push carries
/// the PREVIOUS agreed content forward (the upload says nothing about what the remote holds by the
/// time we look again), and a move carries the moved row's marker to its new path. See
/// [`BaselineEntry::agreed_hash`].
struct FileRow<'p> {
	rel_path: &'p str,
	remote_uuid: Option<Uuid>,
	remote_stable_uuid: Option<StableUuid>,
	content_hash: Option<filen_types::crypto::Blake3Hash>,
	size: u64,
	local_mtime: Option<i64>,
	remote_modified: Option<i64>,
	agreed_hash: Option<filen_types::crypto::Blake3Hash>,
}

/// The synced baseline row a file write leaves behind.
fn file_entry(row: FileRow<'_>) -> BaselineEntry {
	BaselineEntry {
		rel_path: row.rel_path.to_string(),
		kind: NodeKind::File,
		remote_uuid: row.remote_uuid,
		content_hash: row.content_hash,
		size: Some(row.size),
		local_mtime: row.local_mtime,
		remote_modified: row.remote_modified,
		state: BaselineState::Synced,
		local_kind: None,
		remote_kind: None,
		remote_hash: None,
		remote_size: None,
		remote_stable_uuid: row.remote_stable_uuid,
		agreed_hash: row.agreed_hash,
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
	file_entry(FileRow {
		rel_path: to_path,
		remote_uuid: base
			.and_then(|b| b.remote_uuid)
			.or_else(|| remote.map(|n| n.remote_uuid)),
		remote_stable_uuid: base
			.and_then(|b| b.remote_stable_uuid)
			.or_else(|| remote.and_then(|n| n.stable_uuid)),
		content_hash,
		size,
		local_mtime,
		remote_modified: remote.map(|n| n.modified_millis),
		// The row follows the moved file, and so does its agreed content. A move that carried an
		// edit is paired with a download, which re-records both.
		agreed_hash: base.and_then(|b| b.agreed_hash),
	})
}

async fn upsert_dir_baseline(
	ctx: &ApplyContext<'_>,
	rel_path: &str,
	remote_uuid: Option<Uuid>,
	local_mtime: Option<i64>,
) -> Result<(), crate::Error> {
	upsert_baseline(ctx, &dir_entry(rel_path, remote_uuid, local_mtime)).await
}

async fn upsert_file_baseline(
	ctx: &ApplyContext<'_>,
	row: FileRow<'_>,
) -> Result<(), crate::Error> {
	upsert_baseline(ctx, &file_entry(row)).await
}

/// Journal a remote write together with the baseline rows it produced — ONE transaction, so a
/// crash cannot leave the write recorded without its state or the state without its record — and
/// then publish the write to the in-memory journal the current process folds from.
///
/// Every journalled write first drops the records past their grace, from memory and then, in that
/// same transaction, from the DB ([`PendingWrites::expire`]): a pass keeps about one grace window
/// of its writes however long it runs. A transaction that fails leaves those rows in the DB, and
/// the next open drops them as expired.
async fn commit_remote_write(
	ctx: &ApplyContext<'_>,
	uuid: Uuid,
	kind: PendingKind,
	changes: &[BaselineChange<'_>],
) -> Result<(), crate::Error> {
	let expired = ctx.pending.expire();
	let row = PendingRow {
		pair: ctx.pair,
		uuid,
		kind,
		recorded_at: Utc::now().timestamp_millis(),
	};
	locked(ctx.store)
		.record_pending(&row, changes, &expired)
		.map_err(db_err)?;
	ctx.pending.record(ctx.observed, ctx.pair, uuid, row.kind);
	Ok(())
}

async fn upsert_baseline(
	ctx: &ApplyContext<'_>,
	entry: &BaselineEntry,
) -> Result<(), crate::Error> {
	locked(ctx.store)
		.upsert_entry(ctx.pair, entry)
		.map_err(db_err)
}

/// Journal a remote trash at `rel_path` and drop the rows it removes — its own, and those of the
/// deletions it carried out along with its own (`carried`), read out of the pass's stage as they
/// are dropped — in one transaction. Those rows scale with the trashed subtree, so a commit that
/// carries them runs off the runtime thread and is published to the in-memory journal once it has
/// committed, as [`commit_dir_move`]'s is (see there for what that means for a caller that drops the
/// pass mid-commit).
async fn commit_trash(
	ctx: &ApplyContext<'_>,
	uuid: Uuid,
	rel_path: &str,
	carried: Option<Carried>,
) -> Result<(), crate::Error> {
	let kind = PendingKind::Trashed {
		path: rel_path.to_string(),
	};
	let Some(carried) = carried else {
		return commit_remote_write(ctx, uuid, kind, &[BaselineChange::Delete(rel_path)]).await;
	};
	// Owned for the store thread, as the journal's own copy is.
	let (path, row) = (
		rel_path.to_owned(),
		PendingRow {
			pair: ctx.pair,
			uuid,
			kind: kind.clone(),
			recorded_at: Utc::now().timestamp_millis(),
		},
	);
	let expired = ctx.pending.expire();
	off_store(ctx.store, move |store| {
		store.record_pending_carrying(&row, &[BaselineChange::Delete(&path)], &expired, |drop| {
			carried.each(drop)
		})
	})
	.await?
	.map_err(db_err)?;
	ctx.pending.record(ctx.observed, ctx.pair, uuid, kind);
	Ok(())
}

async fn delete_baseline(ctx: &ApplyContext<'_>, rel_path: &str) -> Result<(), crate::Error> {
	locked(ctx.store)
		.delete_entry(ctx.pair, rel_path)
		.map_err(db_err)
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
///
/// An occupant it moves is announced from in HERE, through [`note_quarantined`], rather than
/// handed back for the caller to announce: the rename this clears the way for is two statements
/// further on, and a stash whose announcement is the caller's business is a stash the caller can
/// stop announcing. Nothing to hand back, so there is nothing to forget.
async fn stash_move_target(
	local_root: &Path,
	to_path: &str,
	base: Option<&BaselineEntry>,
	bin_day: NaiveDate,
	report: &mut SyncReport,
	observer: &mut (dyn FnMut(SyncEvent) + Send),
) -> Result<(), crate::Error> {
	if !has_own_directory_entry(&local_path(local_root, to_path)) {
		return Ok(());
	}
	// The rename that follows always lands, so nothing here is ever put back.
	let stashed = stash_local_target(local_root, to_path, base, bin_day).await?;
	note_quarantined(
		report,
		observer,
		stashed.map(|bin| (to_path.to_string(), bin)),
	);
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

pub(super) fn io_err(error: std::io::Error) -> crate::Error {
	crate::Error::custom_with_source(crate::ErrorKind::IO, error, None::<String>)
}

fn db_err(error: rusqlite::Error) -> crate::Error {
	super::engine::db_error(error, "baseline")
}

fn internal(message: &'static str) -> crate::Error {
	crate::Error::custom(crate::ErrorKind::Internal, message)
}

fn internal_owned(message: String) -> crate::Error {
	crate::Error::custom(crate::ErrorKind::Internal, message)
}

/// A plan the tests stage for the apply without screening it, and entries with nothing read.
#[cfg(test)]
mod test_support;

#[cfg(test)]
mod tests;

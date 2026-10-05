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
	collections::{HashMap, HashSet},
	path::{Path, PathBuf},
	sync::{
		Arc, PoisonError,
		atomic::{AtomicBool, Ordering},
	},
	time::Duration,
};

use chrono::{DateTime, Utc};
use filen_types::fs::StableUuid;
use futures::{StreamExt, stream::FuturesUnordered};
use uuid::Uuid;

use super::{
	baseline::{BaselineChange, BaselineEntry, BaselineState, NodeKind, PairId},
	changes::FullPassReason,
	engine::{
		LockBudget, Observations, PASS_LOCK_MAX_SLEEP, PendingKind, PendingWrites, SharedStore,
		locked, off_store,
	},
	events::{SyncEvent, TransferDirection},
	guard::GuardReason,
	ignore::IgnoredPath,
	outcome::{
		HaltReason, PlannedAction, PlannedActionKind, PlannedConflict, PlannedNodeKind,
		RefuseReason, UnsyncablePath,
	},
	pause::PassGate,
	plan::{RemoteNode, Subsumed, SyncAction, is_under},
	rows::{Baseline, Cursor},
	scan::{LocalNode, QUARANTINE_DIR, collision_key, hash_file},
	side::{NodesAt, Side},
	stage::{Pager, PlanWriter, Run, Stage, Staged, StagedPlan},
};
use crate::{
	auth::Client,
	cache::{SearchResult, hydrate_cached_items_at},
	fs::{
		HasUUID,
		dir::meta::DirectoryMetaChanges,
		file::{FileBuilderOptionalName, meta::FileMetaChanges},
	},
	io::{RemoteDirectory, RemoteFile, client_impl::IoSharedClientExt},
	sync::lock::{ATTEMPTS_DEFAULT, ResourceLock},
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
	/// Items moved into the pair's local quarantine bin — a directory counts once, whatever its
	/// subtree holds.
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
	/// batch changes or is held for another kind of [`guard`](Self::guard) reason, so an approval
	/// can never leak onto a different set of deletions, nor onto the same ones held for a reason
	/// the caller was never shown.
	pub deletion_token: Option<String>,
	/// Per-action failures (the pass continues past them; one that ran out of room also holds back
	/// the transfers behind it, see [`halted`](Self::halted)).
	pub errors: Vec<String>,
	/// Paths whose local copy this pass moved into the pair's quarantine bin to clear the way for a
	/// write: a remote-wins download over a copy the baseline cannot vouch for, and a move onto an
	/// occupied destination. The bytes stay recoverable under `.filen-sync-trash`.
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
	/// Set when the pass could not take the drive lock, so it applied nothing. The report is still
	/// returned (conflicts and held deletions were recorded before the lock was asked for), but a
	/// watch loop must count the pass as failed rather than healthy. The error itself is the entry
	/// in `errors` that starts with [`LOCK_FAILURE`].
	pub(super) lock_failed: bool,
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
	/// not act on (`drop_blocked`) or a deletion over content it cannot reach
	/// (`withhold_deletions_over_unreachable`). Making the plan consumed both changelists, so a
	/// dropped action is named nowhere any more — the next pass reads both sides whole to find it
	/// again (see `next_pass_scope`). Distinct from [`deferred_paths`](Self::deferred_paths),
	/// which counts what the reconcile itself left alone, whether or not it planned anything there.
	pub(super) dropped_actions: usize,
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
	/// ones (a refusal, a lock that could not be taken) that belong to no path.
	pub(super) failed_paths: Vec<(String, String)>,
}

impl SyncReport {
	/// How many deletions the mass-delete guard held back this pass.
	pub fn held_deletions(&self) -> usize {
		self.held
			.iter()
			.filter(|action| match action.kind {
				PlannedActionKind::TrashRemote | PlannedActionKind::DeleteLocal => true,
				PlannedActionKind::UploadFile
				| PlannedActionKind::DownloadFile
				| PlannedActionKind::CreateRemoteDir
				| PlannedActionKind::CreateLocalDir
				| PlannedActionKind::MoveRemote { .. }
				| PlannedActionKind::MoveLocal { .. }
				| PlannedActionKind::AdoptBaseline => false,
			})
			.count()
	}

	/// Just the paths of [`conflicts`](Self::conflicts) — what
	/// [`SyncEngine::resolve_conflict`](super::SyncEngine::resolve_conflict) takes.
	pub fn conflict_paths(&self) -> impl Iterator<Item = &str> {
		self.conflicts.iter().map(|c| c.rel_path.as_str())
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
	/// The sync root resolved to a remote directory (every top-level parent).
	pub(super) root_remote: RemoteDirectory,
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
	/// The deletions each directory deletion of the plan carries out along with its own, whose
	/// baseline rows it drops (see [`plan::fold_subsumed_deletes`](super::plan)).
	pub(super) subsumed: &'a Subsumed,
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
	/// The plan's `actions`, each with what the apply reads for it out of the pass's `baseline` and
	/// its two sides, staged in `stage` one at a time and ordered there for the apply (see
	/// [`StagedPlan`]): what the apply holds of its plan is then the page of it it is in.
	///
	/// It takes those three, and drops them: the scan and the view are a node per item of the tree,
	/// and the baseline's snapshot is the pass's read transaction, which has to END before the first
	/// action commits (see [`Baseline::close`]). Every answer an entry carries is the one the plan was
	/// made from, the pass's own commits landing in none of them.
	pub(super) fn stage(
		actions: Vec<SyncAction>,
		baseline: Baseline,
		local: Side<LocalNode>,
		remote: Side<RemoteNode>,
		stage: Stage,
	) -> rusqlite::Result<StagedPlan> {
		let staged = Self::write(actions, &baseline, local, remote, stage);
		baseline.close();
		staged
	}

	fn write(
		actions: Vec<SyncAction>,
		baseline: &Baseline,
		local: Side<LocalNode>,
		remote: Side<RemoteNode>,
		stage: Stage,
	) -> rusqlite::Result<StagedPlan> {
		let (local, remote) = (local.of(baseline), remote.of(baseline));
		let mut trees = PassTrees {
			local: &local,
			remote: &remote,
			rows: baseline.cursor(),
		};
		let mut plan = PlanWriter::new(stage)?;
		for action in actions {
			let start = PassStart::of(&action, &mut trees);
			plan.push(&Self { action, start })?;
		}
		plan.finish()
	}
}

impl PassStart {
	/// What the apply reads for `action` — in `apply_one`, `apply_transfer` and `Wanted::of` — read
	/// out of `inputs`, and nothing else.
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
/// cascade safety) stay serial.
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
	// Creates and moves first, one at a time: they make the directories the transfers land in, and
	// run parent before child. The deletion half of a type flip runs among them, before the create at
	// its path, or the server rejects that create (the old item still exists).
	while admits_more(report)
		&& let Some(Staged {
			entry, releasable, ..
		}) = unless_unread(plan.next_in(&ctx, Run::Before).await, report)
	{
		let action = &entry.action;
		if !ctx.gate.wait_to_start().await {
			return note_interrupted(report, total - applied, observer);
		}
		if !failed.skips(action, report, observer) {
			match lease.prepare(&ctx, action, releasable).await {
				Some(Ok(())) => {}
				Some(Err(error)) => {
					return note_lock_failure(report, total - applied, &error, observer);
				}
				None => return note_interrupted(report, total - applied, observer),
			}
			if let Some(reason) =
				apply_serial(&ctx, &entry, &mut plan, &mut failed, report, observer).await
			{
				full.note(reason);
			}
		}
		applied += 1;
	}

	// The transfers under a directory whose move just failed are failed before any transfer starts,
	// and passed over below.
	if !failed.dirs.is_empty() {
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
				// not started yet that write to that side never start (they record nothing). Held back
				// for want of room: carried out as far as this pass can, owed to no pause.
				if full.holds(action) {
					tracing::debug!("apply: {} held, its side is full", action.describe());
					unless_unread(plan.next_in(&ctx, Run::Transfers).await, report);
					applied += 1;
					continue;
				}
				let now = Instant::now();
				if remote_in_flight == 0 {
					lease.release_if_spent(&ctx.lock_budget, releasable, now);
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
						note_transfer_failed(&entry.action, &error, &full, report, observer);
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
						note_transfer_failed(&entry.action, &error, &full, report, observer);
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
					note_written(&plan, &action, report);
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
					note_written(&plan, &action, report);
					observer(action.to_event());
					observer(SyncEvent::Conflict {
						rel_path: conflict.rel_path.clone(),
					});
					note_overwrote(report, action.rel_path(), *conflict, unrecorded);
				}
				Err(error) => {
					applied += 1;
					note_transfer_failed(&action, &error, &full, report, observer);
				}
			}
		}
	}

	// The other deletions, one at a time, children before parents.
	while admits_more(report)
		&& let Some(Staged {
			entry, releasable, ..
		}) = unless_unread(plan.next_in(&ctx, Run::After).await, report)
	{
		let action = &entry.action;
		if !ctx.gate.wait_to_start().await {
			return note_interrupted(report, total - applied, observer);
		}
		if !failed.skips(action, report, observer) {
			match lease.prepare(&ctx, action, releasable).await {
				Some(Ok(())) => {}
				Some(Err(error)) => {
					return note_lock_failure(report, total - applied, &error, observer);
				}
				None => return note_interrupted(report, total - applied, observer),
			}
			apply_serial(&ctx, &entry, &mut plan, &mut failed, report, observer).await;
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

/// Record a transfer that failed — its event and its failure (see [`note_failure`]), and the side
/// it found full, which holds back the transfers after it.
fn note_transfer_failed(
	action: &SyncAction,
	error: &crate::Error,
	full: &FullSides,
	report: &mut SyncReport,
	observer: &mut (dyn FnMut(SyncEvent) + Send),
) {
	tracing::debug!("apply: {} FAILED — {error}", action.describe());
	observer(SyncEvent::ActionFailed {
		rel_path: action.rel_path().to_string(),
		error: error.to_string(),
	});
	if let Some(reason) = note_failure(report, action.rel_path(), error) {
		full.note(reason);
	}
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

/// Where the directory moves that failed this pass were headed: nothing under those paths is
/// attempted. As many as moves failed, which the fold makes one per moved subtree.
#[derive(Debug, Default)]
struct FailedDirMoves {
	dirs: Vec<String>,
}

impl FailedDirMoves {
	/// The directory whose move failed this pass at or above either end of `action`: the plan names
	/// that subtree by a path neither side carries.
	fn holds_back(&self, action: &SyncAction) -> Option<&str> {
		let (from, to) = action.endpoints();
		self.dirs.iter().map(String::as_str).find(|dir| {
			[from, to]
				.into_iter()
				.any(|path| path == *dir || is_under(path, dir))
		})
	}

	/// Skip `action` when a directory move that failed this pass holds it back (see
	/// [`holds_back`](Self::holds_back)), recording it as a failure of its own.
	fn skips(
		&self,
		action: &SyncAction,
		report: &mut SyncReport,
		observer: &mut (dyn FnMut(SyncEvent) + Send),
	) -> bool {
		let Some(dir) = self.holds_back(action) else {
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
	let pair = ctx.pair;
	let committed = kind.clone();
	let (from, to) = (from.to_string(), to.to_string());
	let at = Utc::now().timestamp_millis();
	let expired = ctx.pending.expire();
	off_store(ctx.store, move |store| {
		store.record_pending(
			pair,
			uuid,
			&committed,
			at,
			&[BaselineChange::MoveSubtree {
				from: &from,
				to: &to,
			}],
			&expired,
		)
	})
	.await?
	.map_err(db_err)?;
	ctx.pending.record(ctx.observed, pair, uuid, kind);
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
	report.lock_failed = true;
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

/// What stopped a pass admitting more work ([`SyncReport::store_failed`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StoreFailure {
	/// A write recording work already done did not land (see [`record_not_written`]). It outranks
	/// an [`UnreadPlan`](Self::UnreadPlan) in the same pass: it leaves an act no record describes,
	/// where an unread plan only stops the pass short.
	RecordNotWritten,
	/// A read of the pass's staged plan failed (see [`note_unread_plan`]).
	UnreadPlan,
}

impl StoreFailure {
	/// How a pass it stopped reports that to a watch loop.
	pub(super) fn line(self) -> &'static str {
		match self {
			Self::RecordNotWritten => STORE_FAILURE,
			Self::UnreadPlan => UNREAD_PLAN,
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

	/// See [`Pager::sweep`].
	fn sweep(&self, run: Option<Run>, each: impl FnMut(PlannedEntry)) -> Result<(), crate::Error> {
		self.pager.sweep(run, each).map_err(page_err)
	}

	/// The remote file `uuid` an action of the page in hand acts on, if the cache held it when the
	/// page was read (else see [`file_or_fetch`]).
	///
	/// A copy, here and in [`dir`](Self::dir): a trash or a move updates the object it is handed, and
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
	report.store_failed.get_or_insert(StoreFailure::UnreadPlan);
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
	/// What the actions of `page` act on: the file of a download, a file trash or a file move, and
	/// the directory of a directory trash or move — the one the view listed at its path, which its
	/// entry carries.
	fn of<'p>(page: impl Iterator<Item = &'p PlannedEntry>) -> Self {
		let mut wanted = Self::default();
		for PlannedEntry { action, start } in page {
			match (action, start) {
				(
					SyncAction::DownloadFile { remote_uuid, .. }
					| SyncAction::TrashRemote {
						kind: NodeKind::File,
						remote_uuid,
						..
					}
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
					SyncAction::TrashRemote {
						kind: NodeKind::Dir,
						..
					},
					PassStart::Remote { remote },
				)
				| (
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

/// Apply one action serially: emit its in-progress event, run it, and record success/failure.
/// Returns the side its failure found full, if it did (see [`note_failure`]).
async fn apply_serial(
	ctx: &ApplyContext<'_>,
	planned: &PlannedEntry,
	plan: &mut PagedPlan,
	failed: &mut FailedDirMoves,
	report: &mut SyncReport,
	observer: &mut (dyn FnMut(SyncEvent) + Send),
) -> Option<HaltReason> {
	let action = &planned.action;
	observer(action.to_event());
	tracing::debug!("apply: {}", action.describe());
	let error = match apply_one(ctx, planned, plan, report, &mut *observer).await {
		Ok(()) => {
			note_written(plan, action, report);
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
		failed.dirs.push(to_path.clone());
	}
	observer(SyncEvent::ActionFailed {
		rel_path: action.rel_path().to_string(),
		error: error.to_string(),
	});
	note_failure(report, action.rel_path(), &error)
}

/// Record that `action` landed, for the deletions after it (see [`Pager::note_written`]).
///
/// A record the stage cannot take leaves that write unshielded, so the pass then admits nothing
/// more, as it does once a baseline row is lost ([`admits_more`]): no deletion runs after it to
/// take the write down. The write itself stands, and its path keeps no failure streak for it.
fn note_written(plan: &PagedPlan, action: &SyncAction, report: &mut SyncReport) {
	if let Err(error) = plan.pager.note_written(action) {
		let error = super::engine::db_error(error, "recording what this pass wrote in its stage");
		tracing::error!(
			"apply: {} landed, but {error}; the pass starts nothing more",
			action.describe()
		);
		report
			.errors
			.push(format!("{}: {error}", action.rel_path()));
		report.store_failed = Some(StoreFailure::RecordNotWritten);
	}
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
	if record_not_written(error) {
		report.store_failed = Some(StoreFailure::RecordNotWritten);
	}
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
			let path = confined_local_target(ctx.local_root, rel_path)?;
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
			let stashed = stash_local_target(ctx.local_root, rel_path, base).await?;
			let Some(downloaded) = ctx
				.gate
				.guard(ctx.client.download_file_to_path(
					&remote_file,
					&path,
					Some(progress_callback(
						progress,
						rel_path,
						TransferDirection::Download,
						remote.as_deref().map_or(0, |node| node.size),
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
			let Some(upload) = ctx
				.gate
				.guard(ctx.client.upload_file_from_path_with_builder(
					FileBuilderOptionalName::new(parent),
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
				// (`plan::confirm_agreed_content`), and the gap is what surfaces a concurrent edit.
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
			let path = confined_local_target(ctx.local_root, rel_path)?;
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
			let _stashed = quarantine_local(ctx.local_root, rel_path)?;
			match ctx.subsumed.under(action) {
				[] => delete_baseline(ctx, rel_path).await?,
				subsumed => {
					let (pair, paths) = (ctx.pair, dropped_paths(rel_path, subsumed));
					off_store(ctx.store, move |store| {
						store.apply_changes(pair, &deletes(&paths))
					})
					.await?
					.map_err(db_err)?;
				}
			}
			report.locally_deleted += 1;
		}
		(
			SyncAction::CreateRemoteDir { rel_path },
			PassStart::CreateRemoteDir {
				local,
				remote,
				parent_uuid,
			},
		) => {
			let parent = plan
				.parent_of(rel_path, *parent_uuid)?
				.ok_or_else(|| internal("remote parent dir not yet created"))?;
			let created = local
				.as_deref()
				.map(|n| millis_to_dt(n.mtime_millis))
				.unwrap_or_else(Utc::now);
			let new_uuid: Uuid = ctx
				.client
				.inner_create_dir_with_created(parent, parent_and_name(rel_path).1, created)
				.await?
				.uuid();
			let kind = PendingKind::Created {
				path: rel_path.clone(),
				replaced: remote.as_deref().map(|node| node.remote_uuid),
			};
			let entry = dir_entry(rel_path, Some(new_uuid), None);
			commit_remote_write(ctx, new_uuid, kind, &[BaselineChange::Upsert(&entry)]).await?;
			// What lands under the directory finds it here, and a later deletion of the pass that
			// names it is refused: when the dedup answered with an existing directory, this is ITS
			// uuid. A record that failed fails the action, and so the pass (see
			// `record_not_written`), before anything is put under it or deleted.
			plan.record_created(rel_path, new_uuid)?;
			report.remote_dirs_created += 1;
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
			SyncAction::TrashRemote {
				rel_path,
				kind,
				remote_uuid,
			},
			start,
		) => {
			if plan.pager.shields_remote(rel_path, *remote_uuid) {
				return Err(internal_owned(format!(
					"refusing to trash {rel_path:?}: this pass already wrote that item, or under that name"
				)));
			}
			match (kind, start) {
				(NodeKind::File, _) => {
					let mut remote_file = file_or_fetch(
						ctx,
						plan.file(*remote_uuid),
						*remote_uuid,
						"trash target file missing from the snapshot",
					)
					.await?;
					ctx.client.trash_file(&mut remote_file).await?;
				}
				(NodeKind::Dir, PassStart::Remote { remote }) => {
					let uuid = dir_uuid(remote.as_deref());
					let mut remote_dir = dir_or_fetch(
						ctx,
						uuid.and_then(|uuid| plan.dir(uuid)),
						uuid,
						"trash target dir missing from the snapshot",
					)
					.await?;
					ctx.client.trash_dir(&mut remote_dir).await?;
				}
				(NodeKind::Dir, _) => return Err(unpaired()),
			}
			// The row is about to go, so a snapshot that has not applied the trash yet reads this
			// item as an untracked remote file with nothing local — a deletion to make all over
			// again. Record the trash so the next pass suppresses that.
			commit_trash(ctx, *remote_uuid, rel_path, ctx.subsumed.under(action)).await?;
			report.remotely_trashed += 1;
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
			let to = confined_local_target(ctx.local_root, to_path)?;
			// The reconciler only plans a move onto a destination the SCAN saw free, but a pass
			// waits on the drive lock in between and a file can land there meanwhile — and
			// `rename` would destroy it without a trace. Same window, and same remedy, as the
			// pre-download stash above; this one reports what it moved itself.
			stash_move_target(
				ctx.local_root,
				to_path,
				base.as_deref(),
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
) -> Result<Option<PathBuf>, crate::Error> {
	let path = local_path(local_root, rel_path);
	let base = base.cloned();
	let unsynced =
		tokio::task::spawn_blocking(move || local_holds_unsynced_content(&path, base.as_ref()))
			.await
			.map_err(|e| internal_owned(format!("reading a local write target panicked: {e}")))?;
	if unsynced {
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
	locked(ctx.store)
		.record_pending(
			ctx.pair,
			uuid,
			&kind,
			Utc::now().timestamp_millis(),
			changes,
			&expired,
		)
		.map_err(db_err)?;
	ctx.pending.record(ctx.observed, ctx.pair, uuid, kind);
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
/// deletions it carried out along with its own (`subsumed`) — in one transaction. Those rows scale
/// with the trashed subtree, so a commit that carries them runs off the runtime thread and is
/// published to the in-memory journal once it has committed, as [`commit_dir_move`]'s is (see there
/// for what that means for a caller that drops the pass mid-commit).
async fn commit_trash(
	ctx: &ApplyContext<'_>,
	uuid: Uuid,
	rel_path: &str,
	subsumed: &[String],
) -> Result<(), crate::Error> {
	let kind = PendingKind::Trashed {
		path: rel_path.to_string(),
	};
	if subsumed.is_empty() {
		return commit_remote_write(ctx, uuid, kind, &[BaselineChange::Delete(rel_path)]).await;
	}
	let (pair, committed, paths) = (ctx.pair, kind.clone(), dropped_paths(rel_path, subsumed));
	let at = Utc::now().timestamp_millis();
	let expired = ctx.pending.expire();
	off_store(ctx.store, move |store| {
		store.record_pending(pair, uuid, &committed, at, &deletes(&paths), &expired)
	})
	.await?
	.map_err(db_err)?;
	ctx.pending.record(ctx.observed, pair, uuid, kind);
	Ok(())
}

/// `rel_path` and the paths of the deletions it carried out along with its own: the rows a
/// directory deletion drops, owned for the store thread.
fn dropped_paths(rel_path: &str, subsumed: &[String]) -> Vec<String> {
	let mut paths = Vec::with_capacity(subsumed.len() + 1);
	paths.push(rel_path.to_owned());
	paths.extend_from_slice(subsumed);
	paths
}

fn deletes(paths: &[String]) -> Vec<BaselineChange<'_>> {
	paths
		.iter()
		.map(|path| BaselineChange::Delete(path))
		.collect()
}

async fn delete_baseline(ctx: &ApplyContext<'_>, rel_path: &str) -> Result<(), crate::Error> {
	locked(ctx.store)
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
///
/// An occupant it moves is announced from in HERE, through [`note_quarantined`], rather than
/// handed back for the caller to announce: the rename this clears the way for is two statements
/// further on, and a stash whose announcement is the caller's business is a stash the caller can
/// stop announcing. Nothing to hand back, so there is nothing to forget.
async fn stash_move_target(
	local_root: &Path,
	to_path: &str,
	base: Option<&BaselineEntry>,
	report: &mut SyncReport,
	observer: &mut (dyn FnMut(SyncEvent) + Send),
) -> Result<(), crate::Error> {
	if !has_own_directory_entry(&local_path(local_root, to_path)) {
		return Ok(());
	}
	// The rename that follows always lands, so nothing here is ever put back.
	let stashed = stash_local_target(local_root, to_path, base).await?;
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
	super::engine::db_error(error, "baseline")
}

fn internal(message: &'static str) -> crate::Error {
	crate::Error::custom(crate::ErrorKind::Internal, message)
}

fn internal_owned(message: String) -> crate::Error {
	crate::Error::custom(crate::ErrorKind::Internal, message)
}

/// Entries with nothing read, for the tests.
#[cfg(test)]
mod test_support {
	use uuid::Uuid;

	use super::{
		super::{
			baseline::BaselineEntry,
			plan::{RemoteNode, SyncAction},
			scan::LocalNode,
		},
		PassStart, PlannedEntry, StartInputs,
	};

	/// A pass that held nothing at any path.
	struct Unread;

	impl StartInputs for Unread {
		fn local(&mut self, _: &str) -> Option<Box<LocalNode>> {
			None
		}

		fn remote(&mut self, _: &str) -> Option<Box<RemoteNode>> {
			None
		}

		fn base(&mut self, _: &str) -> Option<Box<BaselineEntry>> {
			None
		}

		fn base_from(&mut self, _: &str) -> Option<Box<BaselineEntry>> {
			None
		}

		fn parent_uuid(&mut self, _: &str) -> Option<Uuid> {
			None
		}
	}

	impl PassStart {
		/// What `action`'s kind reads, with nothing read at any of it.
		pub(in crate::sync_engine) fn bare(action: &SyncAction) -> Self {
			Self::of(action, &mut Unread)
		}
	}

	impl PlannedEntry {
		/// `action`, with nothing read for it (see [`PassStart::bare`]).
		pub(in crate::sync_engine) fn bare(action: SyncAction) -> Self {
			Self {
				start: PassStart::bare(&action),
				action,
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use std::{sync::Mutex, time::Duration};

	use filen_types::crypto::Blake3Hash;
	use uuid::Uuid;

	use super::*;
	use crate::{
		alloc_meter,
		auth::test_support::offline_client,
		cache::{CacheState, hydrate_by_uuids},
		sync_engine::{
			SyncMode,
			baseline::BaselineStore,
			changes::tests::{cacheable_dir, cacheable_file},
			stage::PAGE_ROWS,
		},
	};

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

	fn trash(rel_path: &str, kind: NodeKind) -> SyncAction {
		SyncAction::TrashRemote {
			rel_path: rel_path.to_string(),
			kind,
			remote_uuid: Uuid::new_v4(),
		}
	}

	/// Each entry carries what its action reads, read as the pass started, and nothing else: a
	/// download its remote node and its row; an upload its local node, the remote node it versions
	/// and the directory it lands in; a remote file move its local node, the row it moves from and
	/// the directory it lands in; a local deletion nothing at all.
	#[test]
	fn each_entry_carries_what_its_action_reads() {
		let docs = Uuid::from_u128(1);
		let pulled = RemoteNode {
			remote_uuid: Uuid::from_u128(2),
			..remote_node("docs/a.txt", None)
		};
		let versioned = RemoteNode {
			remote_uuid: Uuid::from_u128(3),
			..remote_node("docs/new.txt", None)
		};
		let remote = Side::Whole(HashMap::from([
			(
				"docs".to_string(),
				RemoteNode {
					kind: NodeKind::Dir,
					remote_uuid: docs,
					..remote_node("docs", None)
				},
			),
			("docs/a.txt".to_string(), pulled.clone()),
			("docs/new.txt".to_string(), versioned.clone()),
		]));
		let local_at = |path: &str| Box::new(local_node(path, 10, None));
		let local = Side::Whole(HashMap::from(
			["docs/new.txt", "docs/renamed.txt", "gone.txt"]
				.map(|path| (path.to_string(), *local_at(path))),
		));
		let (row_a, row_old) = (
			baseline_file("docs/a.txt", None),
			baseline_file("old.txt", None),
		);
		let baseline = Baseline::from_rows([
			row_a.clone(),
			row_old.clone(),
			baseline_file("gone.txt", None),
		]);
		let actions = vec![
			SyncAction::DownloadFile {
				rel_path: "docs/a.txt".to_string(),
				remote_uuid: pulled.remote_uuid,
			},
			upload("docs/new.txt"),
			SyncAction::MoveRemote {
				from_path: "old.txt".to_string(),
				to_path: "docs/renamed.txt".to_string(),
				kind: NodeKind::File,
				remote_uuid: Uuid::from_u128(4),
			},
			SyncAction::DeleteLocal {
				rel_path: "gone.txt".to_string(),
				kind: NodeKind::File,
			},
		];
		let (_dir, stage) = Stage::in_temp_dir();
		let plan = PlannedEntry::stage(actions, baseline, local, remote, stage).unwrap();
		let mut entries = Vec::new();
		plan.pager()
			.sweep(None, |entry| entries.push(entry))
			.unwrap();
		let read: Vec<(&str, &PassStart)> = entries
			.iter()
			.map(|PlannedEntry { action, start }| (action.rel_path(), start))
			.collect();
		// In the order the apply runs them: the move, the two transfers (the download first), and
		// the deletion last.
		assert_eq!(
			read,
			[
				(
					"docs/renamed.txt",
					&PassStart::MoveRemoteFile {
						local: Some(local_at("docs/renamed.txt")),
						base_from: Some(Box::new(row_old)),
						parent_uuid: Some(docs),
					}
				),
				(
					"docs/a.txt",
					&PassStart::Download {
						remote: Some(Box::new(pulled)),
						base: Some(Box::new(row_a)),
					}
				),
				(
					"docs/new.txt",
					&PassStart::Upload {
						local: Some(local_at("docs/new.txt")),
						remote: Some(Box::new(versioned)),
						base: None,
						parent_uuid: Some(docs),
					}
				),
				("gone.txt", &PassStart::Nothing),
			]
		);
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
	/// the transfer's direction and the planned size alongside.
	#[test]
	fn progress_callback_reports_a_running_total() {
		let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
		let callback = progress_callback(&sender, "dir/f.bin", TransferDirection::Upload, 12);
		callback(5);
		callback(7);
		drop(callback);
		let progress = |bytes| SyncEvent::Progress {
			rel_path: "dir/f.bin".to_string(),
			direction: TransferDirection::Upload,
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

	/// The store's own answer to a write another connection will not let go of: the error a record
	/// write really fails with, mapped exactly as an action's path maps it.
	fn contended_write_error() -> crate::Error {
		let path =
			std::env::temp_dir().join(format!("filen_apply_contended_{}.db", Uuid::new_v4()));
		let holder = BaselineStore::open(&path).unwrap();
		let (pair, _) = holder
			.create_pair("/root", Uuid::new_v4(), super::super::SyncMode::TwoWay)
			.unwrap();
		let writer = BaselineStore::open(&path).unwrap();
		writer.busy_timeout_for_test(Duration::from_millis(50));
		let entry = dir_entry("held", Some(Uuid::new_v4()), None);
		let error = {
			let _held = holder.hold_write_lock(pair, &entry).unwrap();
			writer
				.upsert_entry(pair, &entry)
				.map_err(db_err)
				.expect_err("a write contended past the busy timeout must fail rather than wait")
		};
		drop(writer);
		drop(holder);
		for suffix in ["", "-wal", "-shm"] {
			std::fs::remove_file(format!("{}{suffix}", path.display())).ok();
		}
		error
	}

	fn conflict(rel_path: &str) -> PlannedConflict {
		PlannedConflict {
			rel_path: rel_path.to_string(),
			local: Some(PlannedNodeKind::File),
			remote: Some(PlannedNodeKind::File),
		}
	}

	/// The row that holds a version an upload of ours buried is the only reference to that version
	/// anywhere — no view can re-derive it. So when it cannot be written the conflict must still
	/// reach the report, and the lost row must still make the pass a failed one.
	#[test]
	fn a_buried_version_whose_row_could_not_be_written_is_still_reported() {
		let mut report = SyncReport::default();
		note_overwrote(
			&mut report,
			"a.txt",
			conflict("a.txt"),
			Some(contended_write_error()),
		);
		assert_eq!(report.uploaded, 1, "the upload landed and still counts");
		assert_eq!(
			report
				.conflicts
				.iter()
				.map(|conflict| conflict.rel_path.as_str())
				.collect::<Vec<_>>(),
			["a.txt"],
			"the buried version must be surfaced even though its row was lost: {:?}",
			report.errors
		);
		assert_eq!(
			report.store_failed,
			Some(StoreFailure::RecordNotWritten),
			"and the lost row must still fail the pass: {:?}",
			report.errors
		);
		assert!(
			report.errors.iter().any(|line| line.starts_with("a.txt: ")),
			"naming the path whose row was lost: {:?}",
			report.errors
		);

		let mut recorded = SyncReport::default();
		note_overwrote(&mut recorded, "b.txt", conflict("b.txt"), None);
		assert_eq!(recorded.conflicts.len(), 1);
		assert!(
			recorded.store_failed.is_none() && recorded.errors.is_empty(),
			"a row that landed is no failure: {:?}",
			recorded.errors
		);
	}

	/// Once a record of work already done could not be written, the pass admits nothing more: every
	/// action it still holds would carry out its act and then wait out another busy timeout to lose
	/// its own record. What it does not get to is owed to the next pass instead.
	#[test]
	fn a_lost_record_stops_the_pass_admitting_more_work() {
		let mut report = SyncReport::default();
		assert!(admits_more(&report), "a healthy pass admits its actions");
		note_failure(
			&mut report,
			"docs/a.txt",
			&internal("the server refused this one"),
		);
		assert!(
			admits_more(&report),
			"an action that failed on its own leaves the pass running: {:?}",
			report.errors
		);
		note_failure(&mut report, "docs/b.txt", &contended_write_error());
		assert!(
			!admits_more(&report),
			"a record that did not land stops the pass: {:?}",
			report.errors
		);
		note_interrupted(&mut report, 7, &mut |_| {});
		assert_eq!(
			report.interrupted, 7,
			"what it did not get to is owed to the next pass"
		);
	}

	/// A plan the pass could not read stops it as a lost record does, but for a reason of its own;
	/// a lost record in the same pass, before or after, is what it reports.
	#[test]
	fn an_unread_plan_stops_the_pass_for_a_reason_a_lost_record_outranks() {
		let unread = internal("reading a page of the pass's staged plan");
		let mut report = SyncReport::default();
		note_unread_plan(&mut report, &unread);
		assert_eq!(report.store_failed, Some(StoreFailure::UnreadPlan));
		assert!(!admits_more(&report));
		note_failure(&mut report, "docs/a.txt", &contended_write_error());
		assert_eq!(report.store_failed, Some(StoreFailure::RecordNotWritten));
		note_unread_plan(&mut report, &unread);
		assert_eq!(report.store_failed, Some(StoreFailure::RecordNotWritten));
	}

	/// The baseline write of an applied action records something that has already happened, so when
	/// it cannot land the pass is a failed one: the store's answer reaches the report under the path,
	/// and `store_failed` is what tells a watch loop to back off instead of reading the pass as
	/// healthy. Both halves here are the production path — the store's own contended answer, mapped
	/// by `db_err` and recorded by `note_failure`.
	#[test]
	fn a_baseline_row_that_could_not_be_written_makes_the_pass_a_failed_one() {
		let path = std::env::temp_dir().join(format!("filen_apply_busy_{}.db", Uuid::new_v4()));
		let holder = BaselineStore::open(&path).unwrap();
		let (pair, _) = holder
			.create_pair("/root", Uuid::new_v4(), super::super::SyncMode::TwoWay)
			.unwrap();
		let writer = BaselineStore::open(&path).unwrap();
		writer.busy_timeout_for_test(Duration::from_millis(50));
		let entry = dir_entry("docs", Some(Uuid::new_v4()), None);

		let mut report = SyncReport::default();
		{
			let _held = holder.hold_write_lock(pair, &entry).unwrap();
			let error = writer
				.upsert_entry(pair, &entry)
				.map_err(db_err)
				.expect_err("the contended row write must fail rather than wait");
			assert!(record_not_written(&error), "{error}");
			note_failure(&mut report, "docs", &error);
		}
		assert_eq!(
			report.store_failed,
			Some(StoreFailure::RecordNotWritten),
			"a lost record must mark the pass failed: {:?}",
			report.errors
		);
		assert!(
			report.errors.iter().any(|line| line.starts_with("docs: ")),
			"the report must name the path whose record was lost: {:?}",
			report.errors
		);
		assert_eq!(
			report.failed_paths.len(),
			1,
			"and the path's own streak still counts it"
		);

		drop(writer);
		drop(holder);
		for suffix in ["", "-wal", "-shm"] {
			std::fs::remove_file(format!("{}{suffix}", path.display())).ok();
		}
	}

	fn temp_dir() -> PathBuf {
		let dir = std::env::temp_dir().join(format!("filen_confine_test_{}", Uuid::new_v4()));
		std::fs::create_dir_all(&dir).unwrap();
		dir
	}

	/// `actions` staged as they stand, with nothing read for any of them, for the apply to page
	/// through; the sync root is `u128::MAX`.
	fn paged(actions: impl IntoIterator<Item = SyncAction>) -> (tempfile::TempDir, PagedPlan) {
		let (dir, stage) = Stage::in_temp_dir();
		let mut writer = PlanWriter::new(stage).unwrap();
		for action in actions {
			writer.push(&PlannedEntry::bare(action)).unwrap();
		}
		let plan = PagedPlan::new(writer.finish().unwrap(), Uuid::from_u128(u128::MAX));
		(dir, plan)
	}

	fn delete_local(rel_path: &str, kind: NodeKind) -> SyncAction {
		SyncAction::DeleteLocal {
			rel_path: rel_path.to_string(),
			kind,
		}
	}

	/// The shape a case-only directory rename planned per path takes on the remote: the create at
	/// the new spelling is answered by the dedup with the EXISTING directory's uuid, and the trash
	/// at the old spelling then names that same uuid. The trash must be refused.
	#[test]
	fn a_trash_of_a_directory_this_pass_created_is_refused() {
		let existing = Uuid::from_u128(1);
		let create = SyncAction::CreateRemoteDir {
			rel_path: "docs".to_string(),
		};
		let (_dir, mut plan) = paged([create.clone(), trash("Docs", NodeKind::Dir)]);
		plan.record_created("docs", existing).unwrap();
		plan.pager.note_written(&create).unwrap();
		assert!(
			plan.pager.shields_remote("Other", existing),
			"the directory the dedup answered with, under any name"
		);
		assert!(
			plan.pager.shields_remote("Docs", Uuid::from_u128(2)),
			"same name under another spelling"
		);
		assert!(
			!plan.pager.shields_remote("Other", Uuid::from_u128(2)),
			"an unrelated trash is not held"
		);
	}

	/// A trash or quarantine of a directory that now holds something this pass wrote — a child
	/// moved or downloaded into it under another spelling — would take that write down with it, and
	/// a trash of the item a move moved would take down the item itself.
	#[test]
	fn a_delete_above_a_write_of_this_pass_is_refused() {
		let moved = Uuid::from_u128(1);
		let writes = [
			SyncAction::MoveRemote {
				from_path: "Docs/a.txt".to_string(),
				to_path: "docs/a.txt".to_string(),
				kind: NodeKind::File,
				remote_uuid: moved,
			},
			SyncAction::DownloadFile {
				rel_path: "docs/b.txt".to_string(),
				remote_uuid: Uuid::from_u128(2),
			},
		];
		let (_dir, plan) = paged(
			writes
				.iter()
				.cloned()
				.chain([delete_local("Docs", NodeKind::Dir)]),
		);
		for write in &writes {
			plan.pager.note_written(write).unwrap();
		}
		assert!(plan.pager.shields_remote("Docs", Uuid::from_u128(3)));
		assert!(
			plan.pager.shields_remote("elsewhere.txt", moved),
			"the moved item"
		);
		assert!(plan.pager.shields_local("Docs"));
		assert!(
			!plan.pager.shields_local("Docs/c.txt"),
			"a sibling of the write is not held"
		);
		assert!(
			!plan.pager.shields_local("doc"),
			"a name sharing only a prefix is not held"
		);
		assert!(
			!plan.pager.shields_remote("docs/b.txt", Uuid::from_u128(3)),
			"a local write does not hold a remote trash"
		);
	}

	#[test]
	fn a_key_sorting_between_a_dir_and_its_children_does_not_hide_them() {
		// `docs-x` sorts after `docs` but before `docs/`, so a range read has to start at the
		// child prefix rather than at the key itself.
		let shields_docs = |written: &[&str]| {
			let downloads: Vec<SyncAction> = written.iter().map(|path| download(path)).collect();
			let (_dir, plan) = paged(
				downloads
					.iter()
					.cloned()
					.chain([delete_local("Docs", NodeKind::Dir)]),
			);
			for download in &downloads {
				plan.pager.note_written(download).unwrap();
			}
			plan.pager.shields_local("Docs")
		};
		assert!(shields_docs(&["docs-x", "docs/a.txt"]));
		assert!(!shields_docs(&["docs-x"]));
	}

	/// A write the stage cannot record is left unshielded, so the pass admits nothing more after
	/// it. The write itself landed: its path keeps no failure streak for it.
	#[test]
	fn a_write_the_stage_cannot_record_fails_the_pass_but_not_its_path() {
		let (_dir, plan) = paged([upload("up.txt"), trash("Up.txt", NodeKind::File)]);
		plan.pager.break_stage();
		let mut report = SyncReport::default();
		note_written(&plan, &upload("up.txt"), &mut report);
		assert_eq!(report.store_failed, Some(StoreFailure::RecordNotWritten));
		assert!(!admits_more(&report));
		assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
		assert!(
			report.errors[0].starts_with("up.txt: ")
				&& report.errors[0].contains("recording what this pass wrote in its stage"),
			"{}",
			report.errors[0]
		);
		assert!(report.failed_paths.is_empty());
	}

	/// A pass that deletes nothing has nothing a write of its own needs shielding from: a first push
	/// of 10,000 files keeps none of them, in memory or in its stage.
	#[test]
	fn a_plan_that_deletes_nothing_keeps_nothing_of_its_writes() {
		let uploads: Vec<SyncAction> = (0..10_000)
			.map(|n| upload(&format!("d{:02}/f{n:05}.txt", n % 100)))
			.collect();
		let (_dir, plan) = paged(uploads.iter().cloned());
		let ((), peak) = alloc_meter::peak_bytes(|| {
			for action in &uploads {
				plan.pager.note_written(action).unwrap();
			}
		});
		assert_eq!(peak, 0, "10_000 uploads landed and nothing is deleted");
		assert!(
			!plan.pager.shields_remote("d00", Uuid::from_u128(1)),
			"the stage recorded the uploads"
		);
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

	/// A file on disk plus a baseline row that vouches for exactly it — its content hash included,
	/// which is what a pull-over decision compares once the stat alone says "unchanged".
	fn synced_file(root: &Path, bytes: &[u8]) -> (PathBuf, BaselineEntry) {
		let path = root.join("a.txt");
		std::fs::write(&path, bytes).unwrap();
		let mut base = baseline_file("a.txt", Some(blake3::hash(bytes).into()));
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
	#[tokio::test]
	async fn a_local_move_onto_an_unsynced_file_stashes_it_first() {
		let root = temp_dir();
		std::fs::write(root.join("dest.txt"), b"never synced").unwrap();

		let _stashed = stash_local_target(&root, "dest.txt", None).await.unwrap();

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

	/// Clear a move's destination exactly as the `MoveLocal` arm does, and collect what the pass
	/// said while doing it.
	async fn cleared_move_target(
		root: &Path,
		to_path: &str,
		base: Option<&BaselineEntry>,
	) -> (SyncReport, Vec<SyncEvent>) {
		let mut report = SyncReport::default();
		let mut events = Vec::new();
		stash_move_target(root, to_path, base, &mut report, &mut |event| {
			events.push(event)
		})
		.await
		.expect("clearing a move's destination");
		(report, events)
	}

	/// A case-only rename resolves both ends of the move to one file on a case-insensitive
	/// filesystem: stashing "the destination" would carry the source away and leave the rename
	/// with nothing to rename.
	#[tokio::test]
	async fn a_case_only_rename_is_not_treated_as_an_occupied_destination() {
		let root = temp_dir();
		std::fs::write(root.join("report.txt"), b"the source").unwrap();

		let (report, events) = cleared_move_target(&root, "REPORT.TXT", None).await;
		assert!(
			report.quarantined.is_empty() && events.is_empty(),
			"nothing of its own sits at the destination, so nothing is moved and nothing is said: {report:?} {events:?}"
		);

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
	#[tokio::test]
	async fn a_destination_that_only_looks_like_the_source_is_still_stashed() {
		let root = temp_dir();
		std::fs::write(root.join("REPORT.TXT"), b"never synced").unwrap();

		let (report, events) = cleared_move_target(&root, "REPORT.TXT", None).await;
		assert_eq!(
			report.quarantined,
			vec!["REPORT.TXT".to_string()],
			"somebody else's file was moved aside, and the pass has to say so"
		);
		assert_eq!(
			events,
			vec![SyncEvent::Quarantined {
				rel_path: "REPORT.TXT".to_string(),
				bin_path: root.join(QUARANTINE_DIR).join("REPORT.TXT"),
			}]
		);

		assert_eq!(
			std::fs::read(root.join(QUARANTINE_DIR).join("REPORT.TXT")).unwrap(),
			b"never synced"
		);
		std::fs::remove_dir_all(&root).ok();
	}

	/// The other side of it: a destination holding exactly what the baseline records is just an
	/// unmodified copy, and moving over it stashes nothing.
	#[tokio::test]
	async fn a_local_move_onto_a_synced_copy_stashes_nothing() {
		let root = temp_dir();
		let (path, base) = synced_file(&root, b"abc");

		let stashed = stash_local_target(&root, "a.txt", Some(&base))
			.await
			.unwrap();
		assert_eq!(stashed, None, "nothing was stashed, so nothing to put back");

		assert!(path.exists(), "an unmodified copy is left where it is");
		assert!(
			!root.join(QUARANTINE_DIR).exists(),
			"nothing is quarantined"
		);

		std::fs::remove_dir_all(&root).ok();
	}

	/// The edit no stat can see: same length, with the mtime put back (`touch -r`, an `rsync -a`
	/// restore, an editor that restores the timestamp). The scan's fast-path reads such a file as
	/// unchanged for ever, so no plan ever names that path and nothing else re-reads its bytes —
	/// this decision has to hash them, or the next remote change is pulled straight over the edit.
	#[tokio::test]
	async fn a_pull_over_a_size_and_mtime_preserving_edit_quarantines_first() {
		let root = temp_dir();
		let (path, base) = synced_file(&root, b"abc");
		let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();

		std::fs::write(&path, b"XYZ").unwrap();
		std::fs::File::options()
			.write(true)
			.open(&path)
			.unwrap()
			.set_times(std::fs::FileTimes::new().set_modified(mtime))
			.unwrap();
		assert_eq!(
			(base.size, base.local_mtime),
			(Some(3), local_mtime_of(&path)),
			"the stat has to be indistinguishable from the synced copy, or this proves nothing"
		);

		assert!(
			local_holds_unsynced_content(&path, Some(&base)),
			"only the bytes tell this edit from the copy the row vouches for"
		);
		let stashed = stash_local_target(&root, "a.txt", Some(&base))
			.await
			.unwrap()
			.expect("an edit no stat can see must still be stashed");
		assert_eq!(
			std::fs::read(&stashed).unwrap(),
			b"XYZ",
			"the edit stays recoverable instead of going under the download"
		);
		assert!(
			!path.exists(),
			"the edit is moved out of the download's way"
		);

		std::fs::remove_dir_all(&root).ok();
	}

	/// A row recording no content hash vouches for nothing: the stat it does record is exactly what
	/// a size- and mtime-preserving edit preserves, so there is no evidence the copy on disk is the
	/// one that was synced. It is quarantined rather than written over.
	#[tokio::test]
	async fn a_row_without_a_recorded_hash_is_not_vouched_for() {
		let root = temp_dir();
		let (path, mut base) = synced_file(&root, b"abc");
		base.content_hash = None;

		assert!(local_holds_unsynced_content(&path, Some(&base)));
		let stashed = stash_local_target(&root, "a.txt", Some(&base))
			.await
			.unwrap()
			.expect("a copy no row vouches for must be kept");
		assert_eq!(std::fs::read(&stashed).unwrap(), b"abc");
		assert!(!path.exists(), "it moved out of the download's way");

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
	#[tokio::test]
	async fn a_write_that_never_landed_puts_its_stashed_target_back() {
		let root = temp_dir();
		std::fs::write(root.join("a.txt"), b"a local edit").unwrap();

		let stashed = stash_local_target(&root, "a.txt", None).await.unwrap();
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
	#[tokio::test]
	async fn a_restore_never_overwrites_what_took_the_path() {
		let root = temp_dir();
		std::fs::write(root.join("a.txt"), b"a local edit").unwrap();
		let stashed = stash_local_target(&root, "a.txt", None).await.unwrap();
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

	/// A copy in the bin nobody is told about is a file that vanished from the tree. Pinned on the
	/// edit only a hash can see, which is the case where nothing else the pass says names that
	/// path: the report lists it AND the caller hears it as it happens.
	#[tokio::test]
	async fn a_pull_over_an_invisible_edit_announces_the_copy_it_moved_aside() {
		let root = temp_dir();
		let (path, base) = synced_file(&root, b"abc");
		let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
		// Same length, mtime put back: indistinguishable from the synced copy by stat alone.
		std::fs::write(&path, b"XYZ").unwrap();
		std::fs::File::options()
			.write(true)
			.open(&path)
			.unwrap()
			.set_times(std::fs::FileTimes::new().set_modified(mtime))
			.unwrap();

		let bin = stash_local_target(&root, "a.txt", Some(&base))
			.await
			.unwrap()
			.expect("an edit no stat can see must still be stashed");

		let mut report = SyncReport::default();
		let mut events = Vec::new();
		note_quarantined(
			&mut report,
			&mut |event| events.push(event),
			Some(("a.txt".to_string(), bin.clone())),
		);

		assert_eq!(
			events,
			vec![SyncEvent::Quarantined {
				rel_path: "a.txt".to_string(),
				bin_path: bin.clone(),
			}],
			"the caller has to HEAR about the copy, not only find it in the final report"
		);
		assert_eq!(
			report.quarantined,
			vec!["a.txt".to_string()],
			"and the report still lists it"
		);
		assert_eq!(
			std::fs::read(&bin).unwrap(),
			b"XYZ",
			"the announced path must be where the edit actually went"
		);

		std::fs::remove_dir_all(&root).ok();
	}

	/// The other write that moves a copy aside — a move onto a destination something took since
	/// the scan — has quarantined silently for longer, and clearing the destination is what says
	/// so: the occupant is announced by the same call that moves it, with the path it went to.
	#[tokio::test]
	async fn a_move_onto_an_occupied_destination_announces_it_too() {
		let root = temp_dir();
		std::fs::write(root.join("dest.txt"), b"never synced").unwrap();

		let (report, events) = cleared_move_target(&root, "dest.txt", None).await;

		let bin = root.join(QUARANTINE_DIR).join("dest.txt");
		assert_eq!(
			events,
			vec![SyncEvent::Quarantined {
				rel_path: "dest.txt".to_string(),
				bin_path: bin.clone(),
			}],
			"the rename cleared the destination without a word"
		);
		assert_eq!(report.quarantined, vec!["dest.txt".to_string()]);
		assert!(
			!root.join("dest.txt").exists(),
			"the destination is cleared for the rename"
		);
		assert_eq!(
			std::fs::read(&bin).unwrap(),
			b"never synced",
			"the occupant is recoverable at the announced path"
		);

		std::fs::remove_dir_all(&root).ok();
	}

	/// A page wants the file each download, file trash and file move names, and the directory the
	/// view listed at the path of each directory trash and move; nothing for what only lands in a
	/// directory, nor for a directory action the view listed no directory for.
	#[test]
	fn a_page_wants_the_objects_its_actions_act_on() {
		let uuid = Uuid::from_u128;
		let at_dir = |remote_uuid| {
			Some(Box::new(RemoteNode {
				kind: NodeKind::Dir,
				remote_uuid,
				..remote_node("d", None)
			}))
		};
		let entry = |action, start| PlannedEntry { action, start };
		let page = [
			PlannedEntry::bare(SyncAction::DownloadFile {
				rel_path: "a".to_string(),
				remote_uuid: uuid(1),
			}),
			PlannedEntry::bare(SyncAction::TrashRemote {
				rel_path: "b".to_string(),
				kind: NodeKind::File,
				remote_uuid: uuid(2),
			}),
			PlannedEntry::bare(SyncAction::MoveRemote {
				from_path: "c0".to_string(),
				to_path: "c".to_string(),
				kind: NodeKind::File,
				remote_uuid: uuid(3),
			}),
			entry(
				SyncAction::TrashRemote {
					rel_path: "d".to_string(),
					kind: NodeKind::Dir,
					remote_uuid: uuid(14),
				},
				PassStart::Remote {
					remote: at_dir(uuid(4)),
				},
			),
			entry(
				SyncAction::MoveRemote {
					from_path: "e0".to_string(),
					to_path: "e".to_string(),
					kind: NodeKind::Dir,
					remote_uuid: uuid(5),
				},
				PassStart::MoveRemoteDir {
					remote: at_dir(uuid(5)),
					parent_uuid: None,
				},
			),
			PlannedEntry::bare(SyncAction::TrashRemote {
				rel_path: "f".to_string(),
				kind: NodeKind::Dir,
				remote_uuid: uuid(6),
			}),
			entry(
				upload("g"),
				PassStart::Upload {
					local: None,
					remote: at_dir(uuid(7)),
					base: None,
					parent_uuid: None,
				},
			),
			entry(
				SyncAction::CreateRemoteDir {
					rel_path: "h".to_string(),
				},
				PassStart::CreateRemoteDir {
					local: None,
					remote: at_dir(uuid(8)),
					parent_uuid: None,
				},
			),
		];
		assert_eq!(
			Wanted::of(page.iter()),
			Wanted {
				files: HashSet::from([uuid(1), uuid(2), uuid(3)]),
				dirs: HashSet::from([uuid(4), uuid(5)]),
			}
		);
	}

	/// What lands in a directory lands in the sync root at the top, in the directory this pass
	/// created at its parent once that create has run, and otherwise in the one the view listed
	/// there; with none of the three it has nowhere to go.
	#[test]
	fn an_item_lands_in_the_root_a_directory_this_pass_created_or_the_views() {
		let (root, made, listed) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
		let (_dir, stage) = Stage::in_temp_dir();
		let mut writer = PlanWriter::new(stage).unwrap();
		for action in [
			SyncAction::CreateRemoteDir {
				rel_path: "made".to_string(),
			},
			SyncAction::CreateRemoteDir {
				rel_path: "pending".to_string(),
			},
			upload("uploaded"),
		] {
			writer.push(&PlannedEntry::bare(action)).unwrap();
		}
		let mut plan = PagedPlan::new(writer.finish().unwrap(), root);
		plan.record_created("made", made).unwrap();
		// Only a directory create records what it made.
		plan.record_created("uploaded", Uuid::from_u128(4)).unwrap();
		let (viewed, unlisted) = (Some(listed), None);
		let landing = [
			plan.parent_of("top.txt", viewed).unwrap(),
			plan.parent_of("made/x.txt", viewed).unwrap(),
			plan.parent_of("made/x.txt", unlisted).unwrap(),
			plan.parent_of("pending/x.txt", viewed).unwrap(),
			plan.parent_of("pending/x.txt", unlisted).unwrap(),
			plan.parent_of("uploaded/x.txt", unlisted).unwrap(),
			plan.parent_of("made/deeper/x.txt", unlisted).unwrap(),
		];
		assert_eq!(
			landing,
			[
				Some(root),
				Some(made),
				Some(made),
				Some(listed),
				None,
				None,
				None
			]
		);
		let target = plan
			.transfer_target(&PlannedEntry::bare(upload("made/x.txt")))
			.unwrap();
		assert_eq!((target.file, target.parent), (None, Some(made)));
	}

	/// Hydrating a 20,000-download pull page by page holds the objects of a page at a time, not the
	/// pull's: every download finds its file in the page it is in. The plan is the apply's own
	/// [`PagedPlan`], turned and read through the methods the apply uses; only the cache read is
	/// made here, directly, where the apply makes it on a blocking thread the meter cannot see. The
	/// cache holds every file, so the server is never asked.
	#[test]
	fn hydrating_a_page_at_a_time_holds_a_page_of_the_objects() {
		const DOWNLOADS: u32 = 20_000;
		let cache_dir = tempfile::tempdir().unwrap();
		let cache = cache_dir.path().join("cache.db");
		let root = Uuid::from_u128(u128::MAX);
		let file_uuid = |n: u32| Uuid::from_u128(u128::from(n) + 1);
		let name = |n: u32| format!("f{n:05}.txt");
		let mut state = CacheState::new_on_path(&cache, root);
		state
			.upsert_files((0..DOWNLOADS).map(|n| cacheable_file(file_uuid(n), root, &name(n))))
			.unwrap();
		let (_dir, stage) = Stage::in_temp_dir();
		let mut writer = PlanWriter::new(stage).unwrap();
		for n in 0..DOWNLOADS {
			let remote = RemoteNode {
				remote_uuid: file_uuid(n),
				..remote_node(&name(n), None)
			};
			writer
				.push(&PlannedEntry {
					action: SyncAction::DownloadFile {
						rel_path: name(n),
						remote_uuid: file_uuid(n),
					},
					start: PassStart::Download {
						remote: Some(Box::new(remote)),
						base: None,
					},
				})
				.unwrap();
		}
		let staged = writer.finish().unwrap();
		let (found, peak) = alloc_meter::peak_bytes(|| {
			let mut plan = PagedPlan::new(staged, root);
			let mut found = 0u32;
			loop {
				if let Some(wanted) = plan.turn().unwrap() {
					let uuids: Vec<Uuid> =
						wanted.files.iter().chain(&wanted.dirs).copied().collect();
					plan.objects
						.extend(hydrate_by_uuids(&cache, &uuids).unwrap());
				}
				let Some(staged) = plan.pager.next_in(Run::Transfers).unwrap() else {
					break;
				};
				let SyncAction::DownloadFile { remote_uuid, .. } = staged.entry.action else {
					panic!("only downloads were staged");
				};
				found += u32::from(plan.file(remote_uuid).is_some());
			}
			found
		});
		assert_eq!(found, DOWNLOADS);
		assert!(
			peak < 2 * 1024 * 1024,
			"hydrating held {peak} bytes at once"
		);
	}

	/// What an [`ApplyContext`] borrows, for steps of the apply run with no account behind them: an
	/// offline client, an empty baseline, an empty local root and a gate nothing pauses.
	struct Borrowed {
		client: Client,
		local: tempfile::TempDir,
		store: SharedStore,
		pending: PendingWrites,
		observed: Observations,
		gate: PassGate,
		subsumed: Subsumed,
	}

	impl Borrowed {
		fn new() -> Self {
			Self {
				client: offline_client(),
				local: tempfile::tempdir().unwrap(),
				store: Arc::new(Mutex::new(BaselineStore::open_in_memory().unwrap())),
				pending: PendingWrites::default(),
				observed: Observations::default(),
				gate: PassGate::default(),
				subsumed: Subsumed::default(),
			}
		}

		/// A pass over the sync root `root` that took `cache_db` as its cache.
		fn ctx(&self, root: Uuid, cache_db: Option<PathBuf>) -> ApplyContext<'_> {
			ApplyContext {
				client: &self.client,
				local_root: self.local.path(),
				pair: 1,
				mode: SyncMode::TwoWay,
				store: &self.store,
				root_remote: RemoteDirectory::from(cacheable_dir(root, root, "root")),
				cache_db,
				absence_trusted: true,
				pending: &self.pending,
				observed: &self.observed,
				gate: &self.gate,
				lock_budget: LockBudget::default(),
				subsumed: &self.subsumed,
			}
		}
	}

	/// A page turn reads the cache DB the pass took at its start, and never waits on the client's
	/// cache slot: a flush holds that until the cache worker exits, which can wait on the request
	/// permits of the transfers in flight, and only the loop turning the page polls those.
	#[tokio::test]
	async fn a_page_turn_reads_the_cache_while_the_client_holds_its_cache_slot() {
		let cache_dir = tempfile::tempdir().unwrap();
		let cache = cache_dir.path().join("cache.db");
		let (root, file) = (Uuid::from_u128(1), Uuid::from_u128(2));
		let mut state = CacheState::new_on_path(&cache, root);
		state
			.upsert_files(std::iter::once(cacheable_file(file, root, "a.txt")))
			.unwrap();
		let (_dir, stage) = Stage::in_temp_dir();
		let mut writer = PlanWriter::new(stage).unwrap();
		writer
			.push(&PlannedEntry::bare(SyncAction::DownloadFile {
				rel_path: "a.txt".to_string(),
				remote_uuid: file,
			}))
			.unwrap();
		let mut plan = PagedPlan::new(writer.finish().unwrap(), root);
		let borrowed = Borrowed::new();
		let ctx = borrowed.ctx(root, Some(cache));
		// Held through the turn, as a flush waiting out the cache worker holds it.
		let _slot = borrowed.client.cache_slot.lock().await;
		let staged =
			tokio::time::timeout(Duration::from_secs(30), plan.next_in(&ctx, Run::Transfers))
				.await
				.expect("the page turn waited on the client's cache slot")
				.expect("reading the plan's one page")
				.expect("the download is the plan's one transfer");
		assert_eq!(staged.entry.action.rel_path(), "a.txt");
		assert!(
			plan.file(file).is_some(),
			"the page did not hydrate the file its download fetches"
		);
	}

	/// Where an upload lands, when the pass's stage cannot be read, fails that upload — as a record
	/// the pass could not keep, so it starts nothing more — rather than panicking in the loop that
	/// holds the other transfers in flight.
	#[test]
	fn a_landing_the_stage_cannot_read_fails_the_upload_and_the_pass() {
		let root = Uuid::from_u128(1);
		let (_dir, stage) = Stage::in_temp_dir();
		let mut writer = PlanWriter::new(stage).unwrap();
		writer
			.push(&PlannedEntry::bare(SyncAction::CreateRemoteDir {
				rel_path: "made".to_string(),
			}))
			.unwrap();
		let plan = PagedPlan::new(writer.finish().unwrap(), root);
		plan.pager.break_stage();
		let entry = PlannedEntry::bare(upload("made/x.txt"));
		let Err(error) = plan.transfer_target(&entry) else {
			panic!("an upload with nowhere to land found somewhere to");
		};
		let mut report = SyncReport::default();
		let mut events = Vec::new();
		note_transfer_failed(
			&entry.action,
			&error,
			&FullSides::default(),
			&mut report,
			&mut |event| events.push(event),
		);
		assert!(matches!(
			events.as_slice(),
			[SyncEvent::ActionFailed { rel_path, .. }] if rel_path == "made/x.txt"
		));
		assert_eq!(report.failed_paths.len(), 1);
		assert_eq!(report.failed_paths[0].0, "made/x.txt");
		assert_eq!(report.halted, None);
		assert_eq!(report.store_failed, Some(StoreFailure::RecordNotWritten));
		assert!(!admits_more(&report));
	}

	/// A page of the plan the stage cannot read fails the pass, rather than panicking in the loop
	/// that holds the transfers in flight as it turns to that page: each transfer already started
	/// finishes and records what it did, and the action on the unread page is owed to the next
	/// pass.
	///
	/// A page of downloads and one more, so the loop turns to the second page once, with the default
	/// concurrency's worth of the first page's downloads still in flight. Each download finds its
	/// file in the cache and fails before it reaches the network, on a path no write may take.
	#[tokio::test]
	async fn a_page_the_stage_cannot_read_fails_the_pass_once_the_transfers_in_flight_finish() {
		let downloads = PAGE_ROWS + 1;
		let cache_dir = tempfile::tempdir().unwrap();
		let cache = cache_dir.path().join("cache.db");
		let root = Uuid::from_u128(u128::MAX);
		let file_uuid = |n: usize| Uuid::from_u128(u128::try_from(n).unwrap() + 1);
		let rel_path = |n: usize| format!("up/../f{n}");
		let mut state = CacheState::new_on_path(&cache, root);
		state
			.upsert_files(
				(0..downloads).map(|n| cacheable_file(file_uuid(n), root, &format!("f{n}"))),
			)
			.unwrap();
		let (_dir, stage) = Stage::in_temp_dir();
		let mut writer = PlanWriter::new(stage).unwrap();
		for n in 0..downloads {
			writer
				.push(&PlannedEntry::bare(SyncAction::DownloadFile {
					rel_path: rel_path(n),
					remote_uuid: file_uuid(n),
				}))
				.unwrap();
		}
		let plan = writer.finish().unwrap();
		plan.spoil_from(PAGE_ROWS);
		let borrowed = Borrowed::new();
		let mut report = SyncReport::default();
		let mut events = Vec::new();
		apply(
			borrowed.ctx(root, Some(cache)),
			plan,
			&mut report,
			&mut |event| events.push(event),
		)
		.await;
		let mut failed: Vec<&str> = report
			.failed_paths
			.iter()
			.map(|(path, _)| path.as_str())
			.collect();
		failed.sort_unstable();
		let mut started: Vec<String> = (0..PAGE_ROWS).map(rel_path).collect();
		started.sort_unstable();
		assert_eq!(failed, started);
		assert_eq!(
			events
				.iter()
				.filter(|event| matches!(event, SyncEvent::ActionFailed { .. }))
				.count(),
			PAGE_ROWS
		);
		assert_eq!(report.downloaded, 0);
		assert_eq!(report.interrupted, 1);
		assert!(matches!(
			events.last(),
			Some(SyncEvent::Interrupted { actions: 1 })
		));
		assert_eq!(report.store_failed, Some(StoreFailure::UnreadPlan));
		assert!(
			report
				.errors
				.iter()
				.any(|line| line.contains("reading a page of the pass's staged plan")),
			"{:?}",
			report.errors.last()
		);
	}
}

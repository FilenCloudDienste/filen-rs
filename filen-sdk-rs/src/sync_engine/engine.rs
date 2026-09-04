//! The engine orchestration: register pairs, plan a pass (read-only), and run one (plan + apply).
//!
//! `prepare` runs the read-only half — load the baseline, scan the local tree (fast-path),
//! enumerate the remote subtree from the cache, build the remote view — shared by `plan_pair` (a
//! dry run) and `sync_once` (plan + guard + apply + baseline advance).

use std::{
	collections::HashMap,
	path::{Path, PathBuf},
	sync::Arc,
	time::{Duration, Instant},
};

use chrono::Utc;
use tokio::sync::Mutex;
use uuid::Uuid;

use super::{
	SyncEvent, SyncMode,
	apply::{self, ApplyContext, SyncReport},
	baseline::{
		BaselineEntry, BaselineState, BaselineStore, NodeKind, PairId, PairRecord, PendingRow,
	},
	guard::{self, DeleteGuard, GuardReason},
	outcome::{
		PlanOutcome, PlannedAction, PlannedConflict, RefuseReason, UnsyncablePath, planned_action,
		planned_conflict,
	},
	plan::{self, RemoteNode, RemoteView, SyncAction},
	scan::{self, LocalScan, ScanError},
};
use crate::{
	Error, ErrorKind,
	auth::Client,
	cache::{CacheEvent, CacheEventType, DirEvent, FileEvent, SyncRootCallback, SyncRootHandle},
	fs::dir::cache::CacheableDir,
	fs::file::cache::CacheableFile,
};

/// The ceiling on how long a remote write this engine made stays trusted over the cache snapshot.
///
/// A write normally retires long before this: the cache announces the item (see [`Observations`])
/// or the snapshot shows what happened at its path. The window is the last resort for the case
/// where neither ever arrives — past it the snapshot is believed again, so an item genuinely
/// deleted elsewhere is picked up late, never ignored.
const PENDING_CREATE_GRACE: Duration = Duration::from_secs(180);

/// One remote write this engine made, and how to tell whether the cache has caught up to it.
#[derive(Debug)]
struct PendingWrite {
	at: Instant,
	/// The pair whose pass made the write. Only that pair's remote snapshot covers the item, so
	/// only that pair's pass may read a snapshot as evidence about it.
	pair: PairId,
	/// The observation counter as of the moment this write was recorded. Only a cache event seen
	/// AFTER that proves the cache caught up to THIS write: a move's uuid was necessarily
	/// announced earlier, when the item was first created.
	seq: u64,
	kind: PendingKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PendingKind {
	/// An item created at `path`. `replaced` is the remote uuid that held the path immediately
	/// before — a same-name upload versions the existing file, minting a new uuid — or `None` for
	/// a path nothing occupied.
	Created {
		path: String,
		replaced: Option<Uuid>,
	},
	/// An item moved from `from` to `to`. The source path is what makes "the cache has not applied
	/// our move yet" distinguishable from "someone else moved the item after us", which must be
	/// reconciled at once rather than waited out; the destination is where the view has to show it
	/// in the meantime.
	Moved { from: String, to: String },
	/// An item this engine sent to the remote trash. Its baseline row is gone, so a snapshot that
	/// has not applied the trash yet reads as "present remotely, untracked, absent locally" — a
	/// deletion to make, which would trash it a second time.
	Trashed,
}

/// Uuids the cache has announced to this engine, each stamped with the observation counter at the
/// time it arrived.
///
/// This is what retires a pending write in the ordinary case. Waiting for the written uuid to
/// APPEAR in a snapshot only works while the item survives, and two everyday events destroy it:
/// a re-upload (ours or another client's) versions the file under a fresh uuid, and a deletion
/// removes it outright. Either way the cache announces the uuid — as a `New`, a `Trashed`, an
/// `Archived` or a `Removed` — and that announcement alone proves the cache is no longer behind
/// on our write.
#[derive(Debug, Default)]
pub(super) struct Observations(std::sync::Mutex<ObservationState>);

#[derive(Debug, Default)]
struct ObservationState {
	seq: u64,
	seen: HashMap<Uuid, u64>,
}

impl Observations {
	/// Record every uuid a committed cache batch touched. Runs on the cache worker thread, so it
	/// does nothing but take a lock and insert.
	fn note(&self, uuids: impl IntoIterator<Item = Uuid>) {
		let mut state = self.state();
		for uuid in uuids {
			state.seq += 1;
			let seq = state.seq;
			state.seen.insert(uuid, seq);
		}
	}

	/// The current counter — the stamp a write recorded now must be beaten by to retire.
	fn stamp(&self) -> u64 {
		self.state().seq
	}

	/// A copy to settle against. Taken BEFORE the remote snapshot is read: an event committed
	/// after the snapshot must not retire a write this pass, or the pass would go on to read the
	/// just-written item as deleted.
	fn snapshot(&self) -> HashMap<Uuid, u64> {
		self.state().seen.clone()
	}

	/// Forget the observations no live pending write can ever consult — only one stamped LATER
	/// than a write retires it, so everything up to the oldest live write's stamp is dead weight
	/// (and with no write left, so is the whole map).
	fn prune_before(&self, oldest: Option<u64>) {
		let mut state = self.state();
		match oldest {
			Some(seq) => state.seen.retain(|_, stamp| *stamp > seq),
			None => state.seen.clear(),
		}
	}

	fn state(&self) -> std::sync::MutexGuard<'_, ObservationState> {
		// Plain data behind the lock: a panic while holding it cannot leave the state inconsistent.
		self.0
			.lock()
			.unwrap_or_else(|poisoned| poisoned.into_inner())
	}
}

/// The uuids one cache event touches: the item's own, plus the successor a trash or archive of an
/// edited file carries (the same file re-minted under a new id).
fn event_uuids(event: &CacheEvent<'_>) -> [Option<Uuid>; 2] {
	match &event.event {
		CacheEventType::File(file) => match file {
			FileEvent::New(f) | FileEvent::Move(f) | FileEvent::Changed(f) => [Some(f.uuid), None],
			FileEvent::Archived { uuid, new_uuid, .. }
			| FileEvent::Trashed { uuid, new_uuid, .. } => [Some(*uuid), *new_uuid],
			FileEvent::Removed(uuid) | FileEvent::MetadataChanged { uuid, .. } => {
				[Some(*uuid), None]
			}
		},
		CacheEventType::Dir(dir) => match dir {
			DirEvent::New(d) | DirEvent::Move(d) | DirEvent::Changed(d) => [Some(d.uuid), None],
			DirEvent::Removed(uuid)
			| DirEvent::MetadataChanged { uuid, .. }
			| DirEvent::ColorChanged { uuid, .. } => [Some(*uuid), None],
		},
		CacheEventType::Global(_) | CacheEventType::NoOp => [None, None],
	}
}

/// The sync-root callback the engine registers per pair: it records the uuids of every committed
/// cache batch and nothing else. It runs on the cache worker thread, so it never awaits and never
/// touches the database.
fn observation_callback(observations: Arc<Observations>) -> SyncRootCallback {
	Box::new(move |events| {
		observations.note(events.flat_map(event_uuids).flatten());
	})
}

/// Remote writes this engine made recently. The apply layer records every item it creates or
/// moves; [`settle`](PendingWrites::settle) drops the records the cache has demonstrably caught up
/// to and the ones past [`PENDING_CREATE_GRACE`], so the map only ever holds one grace window of
/// writes.
#[derive(Debug, Default)]
pub(super) struct PendingWrites(std::sync::Mutex<HashMap<Uuid, PendingWrite>>);

impl PendingWrites {
	/// Record a remote write this pass just made. The apply layer persists the same write to the
	/// baseline DB FIRST (see [`BaselineStore::record_pending`]), so a restart re-learns it.
	pub(super) fn record(
		&self,
		observations: &Observations,
		pair: PairId,
		uuid: Uuid,
		kind: PendingKind,
	) {
		self.insert(pair, uuid, kind, observations.stamp(), Instant::now());
	}

	/// Take a write a PREVIOUS engine journalled back into memory, `age` after it was made.
	///
	/// Its stamp is 0: this process has observed nothing yet, so the first cache announcement of
	/// the uuid — stamped 1 or later — retires it, exactly as it would have retired the original.
	/// The announcements that arrived before the restart are gone with the process, so a write
	/// whose item is no longer in a snapshot (superseded, trashed) now waits for a fresh
	/// announcement, a foreign uuid at its path, or the grace ceiling — bounded, and never longer
	/// than a write made right now.
	pub(super) fn restore(&self, pair: PairId, uuid: Uuid, kind: PendingKind, age: Duration) {
		// `checked_sub` only fails for an age older than this machine's uptime — a journal carried
		// across a reboot — and its fallback is the conservative answer anyway: a full grace
		// window from now.
		let at = Instant::now().checked_sub(age).unwrap_or_else(Instant::now);
		self.insert(pair, uuid, kind, 0, at);
	}

	fn insert(&self, pair: PairId, uuid: Uuid, kind: PendingKind, seq: u64, at: Instant) {
		self.map().insert(
			uuid,
			PendingWrite {
				at,
				pair,
				seq,
				kind,
			},
		);
	}

	/// The uuids currently journalled, for diffing what a [`settle`](Self::settle) retired.
	fn uuids(&self) -> std::collections::HashSet<Uuid> {
		self.map().keys().copied().collect()
	}

	/// Drop every record for `pair` — the pair is gone, so nothing may fold on its behalf (a
	/// re-registration of the same roots gets the same id back).
	fn forget_pair(&self, pair: PairId) {
		self.map().retain(|_, write| write.pair != pair);
	}

	/// The oldest live write's observation stamp, for pruning [`Observations`].
	fn oldest_stamp(&self) -> Option<u64> {
		self.map().values().map(|write| write.seq).min()
	}

	/// Drop the records the cache has caught up to and the ones past [`PENDING_CREATE_GRACE`], and
	/// return the uuids that remain — the ones the snapshot demonstrably still shows in their
	/// pre-write state — split by what the reconciler has to do about each.
	///
	/// `observed` must have been copied BEFORE `remote` was read, so a cache batch committed after
	/// the snapshot cannot retire a record whose item that snapshot still predates. `remote` is
	/// `pair`'s subtree alone, so it is only evidence about `pair`'s own writes.
	fn settle(
		&self,
		pair: PairId,
		observed: &HashMap<Uuid, u64>,
		remote: &HashMap<String, RemoteNode>,
	) -> plan::PassHolds {
		let now = Instant::now();
		let snapshot_path: HashMap<Uuid, &str> = remote
			.iter()
			.map(|(path, node)| (node.remote_uuid, path.as_str()))
			.collect();
		let mut map = self.map();
		map.retain(|uuid, write| {
			// The cache announced this uuid after we wrote it: it has caught up, whether the item
			// still exists, was superseded by a re-upload, or has since been trashed.
			if observed.get(uuid).is_some_and(|stamp| *stamp > write.seq) {
				return false;
			}
			if now.duration_since(write.at) >= PENDING_CREATE_GRACE {
				return false;
			}
			// Another pair's write: this snapshot covers a different remote subtree and says
			// nothing about it, least of all that it is gone. (The two rules above are about the
			// uuid alone, so they hold whichever pair's pass applies them.)
			if write.pair != pair {
				return true;
			}
			match &write.kind {
				PendingKind::Created { path, replaced } => {
					if snapshot_path.contains_key(uuid) {
						return false;
					}
					// Someone else's item holds the path: the snapshot is not behind on our write,
					// it is showing a foreign one — which the reconciler must act on at once.
					match remote.get(path.as_str()).map(|node| node.remote_uuid) {
						Some(occupant) => Some(occupant) == *replaced,
						None => true,
					}
				}
				PendingKind::Moved { from, .. } => snapshot_path
					.get(uuid)
					.is_none_or(|path| *path == from.as_str()),
				PendingKind::Trashed => snapshot_path.contains_key(uuid),
			}
		});
		plan::PassHolds {
			trashed: map
				.iter()
				.filter(|(_, write)| {
					write.pair == pair && matches!(write.kind, PendingKind::Trashed)
				})
				.map(|(uuid, _)| *uuid)
				.collect(),
			..Default::default()
		}
	}

	/// Fold this pair's surviving records into `nodes` — the path-keyed remote view built from the
	/// cache snapshot — and return how many were applied.
	///
	/// The cache learns of this engine's own writes only through socket events and resyncs, so a
	/// pass run seconds after the previous one reads a remote that is missing what that pass just
	/// wrote. The engine knows exactly what it wrote, so its remote view is the snapshot PLUS its
	/// own unacknowledged writes: that is what lets the pass act on the path — push a second edit,
	/// carry the file on — instead of leaving it alone until the cache agrees.
	///
	/// `baseline` supplies the written state: the apply layer writes each row immediately after
	/// the write it describes, so the row IS the post-write truth. A record only survives
	/// [`settle`](Self::settle) while the cache demonstrably still shows the pre-write state, so a
	/// fold never paints over somebody else's write.
	pub(super) fn fold_into(
		&self,
		pair: PairId,
		baseline: &HashMap<String, BaselineEntry>,
		nodes: &mut HashMap<String, RemoteNode>,
	) -> usize {
		let map = self.map();
		let mut writes: Vec<(&Uuid, &PendingWrite)> =
			map.iter().filter(|(_, write)| write.pair == pair).collect();
		if writes.is_empty() {
			return 0;
		}
		// Oldest first: two writes can name one path (a re-upload on the very next pass), and the
		// later one has to land on top.
		writes.sort_unstable_by_key(|(_, write)| write.at);
		// uuid -> where the view holds it, kept current as the fold edits the view; a scan per
		// record would be quadratic on a large tree right after a large pass.
		let mut path_of: HashMap<Uuid, String> = nodes
			.iter()
			.map(|(path, node)| (node.remote_uuid, path.clone()))
			.collect();
		let mut folded = 0;
		for (uuid, write) in writes {
			let applied = match &write.kind {
				PendingKind::Created { path, replaced } => {
					fold_create(nodes, &mut path_of, baseline, *uuid, path, *replaced)
				}
				PendingKind::Moved { from, to } => {
					fold_move(nodes, &mut path_of, baseline, *uuid, from, to)
				}
				PendingKind::Trashed => fold_trash(nodes, &mut path_of, *uuid),
			};
			folded += usize::from(applied);
		}
		folded
	}

	fn map(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, PendingWrite>> {
		// Plain data behind the lock: a panic while holding it cannot leave the map inconsistent.
		self.0
			.lock()
			.unwrap_or_else(|poisoned| poisoned.into_inner())
	}
}

/// [`PENDING_CREATE_GRACE`] as the journal stores it: wall-clock millis.
fn grace_millis() -> i64 {
	PENDING_CREATE_GRACE.as_millis() as i64
}

/// Take the journal a previous engine persisted back into memory, each row aged by the wall clock
/// so it expires on the same ceiling as a write recorded in this process.
/// [`BaselineStore::load_pending`] has already dropped the rows that ceiling ran out on; the clamp
/// here only guards a clock that moved between the two.
fn restore_journal(pending: &PendingWrites, rows: Vec<PendingRow>, now: i64) {
	for row in rows {
		let age = now.saturating_sub(row.recorded_at).clamp(0, grace_millis()) as u64;
		pending.restore(row.pair, row.uuid, row.kind, Duration::from_millis(age));
	}
}

/// The remote node a baseline row describes — the state the write it records left behind.
fn written_node(entry: Option<&BaselineEntry>) -> Option<RemoteNode> {
	let entry = entry?;
	Some(RemoteNode {
		rel_path: entry.rel_path.clone(),
		kind: entry.kind,
		remote_uuid: entry.remote_uuid?,
		content_hash: entry.content_hash,
		size: entry.size.unwrap_or(0),
		modified_millis: entry.remote_modified.unwrap_or(0),
	})
}

/// Put `node` at `path`, keeping the uuid index in step with whatever it displaces.
fn place_node(
	nodes: &mut HashMap<String, RemoteNode>,
	path_of: &mut HashMap<Uuid, String>,
	path: String,
	node: RemoteNode,
) {
	let uuid = node.remote_uuid;
	if let Some(previous) = nodes.insert(path.clone(), node) {
		path_of.remove(&previous.remote_uuid);
	}
	path_of.insert(uuid, path);
}

/// Show what we wrote at `path`, over the uuid it replaced there.
///
/// The node comes from the baseline row at `path` whatever uuid that row names, not from the
/// record's own uuid: a second write to the same path inside the window supersedes the first, and
/// its record is retired by the very snapshot lag this fold exists for (the path holds neither of
/// our uuids, so it reads as foreign). The surviving record still proves the path is ours, and the
/// row is where the latest write recorded itself.
fn fold_create(
	nodes: &mut HashMap<String, RemoteNode>,
	path_of: &mut HashMap<Uuid, String>,
	baseline: &HashMap<String, BaselineEntry>,
	uuid: Uuid,
	path: &str,
	replaced: Option<Uuid>,
) -> bool {
	let Some(node) = written_node(baseline.get(path)) else {
		return false;
	};
	match nodes.get(path).map(|node| node.remote_uuid) {
		// The cache is showing what our baseline records: it has caught up.
		Some(current) if current == node.remote_uuid => return false,
		// Ours — the write itself, or the version it superseded — so the cache is behind on us.
		Some(current) if current == uuid || Some(current) == replaced => {}
		None => {}
		// Somebody else's item: the pass has to reconcile against it, and `settle` retires such a
		// record anyway.
		Some(_) => return false,
	}
	place_node(nodes, path_of, path.to_string(), node);
	true
}

/// Show the item we moved at its destination rather than where the cache still lists it. Only
/// files are ever moved on the remote, so there is no subtree to carry along.
fn fold_move(
	nodes: &mut HashMap<String, RemoteNode>,
	path_of: &mut HashMap<Uuid, String>,
	baseline: &HashMap<String, BaselineEntry>,
	uuid: Uuid,
	from: &str,
	to: &str,
) -> bool {
	let vacated = match path_of.get(&uuid).map(String::as_str) {
		// Already where we moved it.
		Some(at) if at == to => return false,
		// The pre-move path the cache still shows: take the node off it, whatever the destination
		// turns out to hold — the one thing this move makes certain is that the item is not here.
		Some(at) if at == from => {
			path_of.remove(&uuid);
			nodes.remove(from)
		}
		// Somewhere else entirely — not our move lagging, so the snapshot stands.
		Some(_) => return false,
		None => None,
	};
	// Somebody else's item holds the destination: it superseded ours there while the move was in
	// flight, so leave the path showing what really sits on it. Vacating the pre-move path was
	// still right — without it the reconciler reads the item we moved as an untracked remote entry
	// and trashes it.
	if nodes.get(to).is_some_and(|node| node.remote_uuid != uuid) {
		return vacated.is_some();
	}
	let node =
		vacated.or_else(|| written_node(baseline.get(to)).filter(|node| node.remote_uuid == uuid));
	let Some(mut node) = node else {
		return false;
	};
	node.rel_path = to.to_string();
	place_node(nodes, path_of, to.to_string(), node);
	true
}

/// Take the item we trashed out of the view — and, for a directory, everything under it, which the
/// server trashed with it.
fn fold_trash(
	nodes: &mut HashMap<String, RemoteNode>,
	path_of: &mut HashMap<Uuid, String>,
	uuid: Uuid,
) -> bool {
	let Some(path) = path_of.remove(&uuid) else {
		return false;
	};
	let Some(node) = nodes.remove(&path) else {
		return false;
	};
	if node.kind == NodeKind::Dir {
		nodes.retain(|key, node| {
			let keep = !plan::is_under(key, &path);
			if !keep {
				path_of.remove(&node.remote_uuid);
			}
			keep
		});
	}
	true
}

/// A configured sync engine: an `Arc<Client>` (whose cache supplies the remote view) plus the
/// per-pair baseline store.
pub struct SyncEngine {
	pub(super) client: Arc<Client>,
	pub(super) store: Mutex<BaselineStore>,
	/// Remote writes this engine made that the cache may not reflect yet (see [`PendingWrites`]).
	pub(super) pending: PendingWrites,
	/// Uuids the cache has announced, shared with the per-pair sync-root callbacks — the evidence
	/// that retires a pending write (see [`Observations`]).
	pub(super) observed: Arc<Observations>,
	/// One cache sync-root registration per pair, kept alive for as long as the pair is
	/// registered; dropping a handle unsubscribes it.
	roots: Mutex<HashMap<PairId, SyncRootHandle>>,
	/// One-shot mass-delete approvals: pair -> the batch token the caller approved. The next pass
	/// whose held batch hashes to that token executes it; any other batch is held again.
	approvals: Mutex<HashMap<PairId, String>>,
}

/// The read-only inputs to a pass, shared by planning and applying.
struct Prepared {
	record: PairRecord,
	baseline: Arc<HashMap<String, BaselineEntry>>,
	local_scan: LocalScan,
	remote_view: RemoteView,
	/// Whether the cache's remote view has converged at least once (the snapshot carried a
	/// watermark). When false, the snapshot's emptiness is untrustworthy and remote-driven
	/// deletions are held by the guard.
	remote_converged: bool,
	/// Whether the cache SNAPSHOT — before this engine's own unacknowledged writes were folded
	/// into it — was wholly empty while the baseline still tracks remote items: what a transient
	/// backend/cache fault looks like. Read from the snapshot, never the folded view, so folding
	/// our own writes back in cannot mask the fault and let the guard through.
	remote_emptied: bool,
	/// What this pass must not act on (see [`plan::PassHolds`]).
	holds: plan::PassHolds,
	dirs: Vec<CacheableDir<'static>>,
	files: Vec<CacheableFile<'static>>,
}

impl Prepared {
	/// Map internal actions onto the public [`PlannedAction`] shape, resolving each one's size from
	/// whichever side of this pass knows it.
	fn planned(&self, actions: &[SyncAction]) -> Vec<PlannedAction> {
		actions
			.iter()
			.map(|action| planned_action(action, &self.local_scan.nodes, &self.remote_view.nodes))
			.collect()
	}

	/// Every path this pass will not act on, and why — reported identically by the dry run and by
	/// the pass itself, so a caller sees the same list either way.
	fn unsyncable(&self) -> Vec<UnsyncablePath> {
		Vec::new()
	}
}

/// Which side wins when a caller resolves a held two-way conflict via
/// [`SyncEngine::resolve_conflict`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictResolution {
	/// The local copy wins: the next pass pushes it to the remote (or, if the conflict was a
	/// local deletion, propagates that deletion).
	KeepLocal,
	/// The remote copy wins: the next pass pulls it over the local copy (or, if the conflict was a
	/// remote deletion, propagates that deletion — quarantining the local file).
	KeepRemote,
	/// Keep both: the local copy is renamed aside to `<stem>.old.<ext>` (`<stem>.old.N.<ext>` on
	/// collision) so it uploads as a new file, and the conflicting path itself then resolves as
	/// [`KeepRemote`](Self::KeepRemote).
	KeepBoth,
}

/// A `Synced` baseline row for `rel_path` with every side-specific field cleared — the base the
/// resolution branches fill in with the winning side's anchor.
fn synced_shell(rel_path: &str) -> BaselineEntry {
	BaselineEntry {
		rel_path: rel_path.to_string(),
		kind: super::baseline::NodeKind::File,
		remote_uuid: None,
		content_hash: None,
		size: None,
		local_mtime: None,
		remote_modified: None,
		state: BaselineState::Synced,
		local_kind: None,
		remote_kind: None,
		remote_hash: None,
		remote_size: None,
	}
}

/// The baseline row that resolving a held conflict writes, or `None` when the winning side had
/// nothing at the path (the row is dropped instead, so the other side reads as a fresh create).
///
/// Decided purely so the policy is unit-testable. `winner` is already normalized:
/// [`KeepBoth`](ConflictResolution::KeepBoth) has moved the local copy aside and become
/// [`KeepRemote`](ConflictResolution::KeepRemote).
fn resolution_entry(
	rel_path: &str,
	held: &BaselineEntry,
	winner: ConflictResolution,
) -> Option<BaselineEntry> {
	match winner {
		// The local side wins: anchor the baseline to the REMOTE's recorded state, with no
		// local evidence, so the next scan reads the local copy (or its absence) as the change
		// and pushes it.
		//
		// Unless the two sides converged on identical content while the conflict was held: there
		// is nothing left to push, so record the whole converged state at once instead of leaving
		// a row a further pass has to adopt.
		ConflictResolution::KeepLocal => held.remote_kind.map(|kind| {
			let converged = kind == super::baseline::NodeKind::File
				&& held.content_hash.is_some()
				&& held.content_hash == held.remote_hash
				&& held.size == held.remote_size;
			BaselineEntry {
				kind,
				remote_uuid: held.remote_uuid,
				remote_modified: held.remote_modified,
				content_hash: converged.then_some(held.content_hash).flatten(),
				size: converged.then_some(held.size).flatten(),
				local_mtime: converged.then_some(held.local_mtime).flatten(),
				..synced_shell(rel_path)
			}
		}),
		// The remote side wins: anchor to the LOCAL's recorded state with no remote evidence,
		// so the remote (or its absence) reads as the change and is pulled.
		//
		// With the same convergence exception, mirrored: identical content on both sides leaves
		// nothing to pull, so the remote anchor is recorded too rather than left for a later
		// adopt pass.
		ConflictResolution::KeepRemote => held.local_kind.map(|kind| {
			let converged = kind == super::baseline::NodeKind::File
				&& held.content_hash.is_some()
				&& held.content_hash == held.remote_hash
				&& held.size == held.remote_size;
			BaselineEntry {
				kind,
				content_hash: held.content_hash,
				size: held.size,
				local_mtime: held.local_mtime,
				remote_uuid: converged.then_some(held.remote_uuid).flatten(),
				remote_modified: converged.then_some(held.remote_modified).flatten(),
				..synced_shell(rel_path)
			}
		}),
		ConflictResolution::KeepBoth => unreachable!("normalized to KeepRemote above"),
	}
}

impl SyncEngine {
	/// Open the engine, creating the baseline DB at `db_path` if needed. The `client`'s cache must
	/// be configured (it supplies the remote view).
	pub async fn open(client: Arc<Client>, db_path: PathBuf) -> Result<Self, Error> {
		let store = tokio::task::spawn_blocking(move || BaselineStore::open(&db_path))
			.await
			.map_err(|e| {
				Error::custom(ErrorKind::Internal, format!("baseline open panicked: {e}"))
			})??;
		let engine = Self {
			client,
			store: Mutex::new(store),
			pending: PendingWrites::default(),
			observed: Arc::new(Observations::default()),
			roots: Mutex::new(HashMap::new()),
			approvals: Mutex::new(HashMap::new()),
		};
		// Pairs registered by an earlier session are live again from here on, so they need their
		// cache subscription back too.
		for record in engine.list_pairs().await? {
			engine.observe_pair(record.id, record.remote_root).await;
		}
		// An engine that went down inside the cache-lag window left the writes it had made in the
		// journal; folding them is what stops this one from making them a second time.
		let now = Utc::now().timestamp_millis();
		let rows = engine
			.store
			.lock()
			.await
			.load_pending(now, grace_millis())
			.map_err(|e| db_error(e, "loading the pending-write journal"))?;
		if !rows.is_empty() {
			tracing::debug!(
				"sync engine: restoring {} unacknowledged write(s)",
				rows.len()
			);
		}
		restore_journal(&engine.pending, rows, now);
		Ok(engine)
	}

	/// Subscribe to the cache's notifications for a pair's remote root, so a write this engine
	/// made retires as soon as the cache commits any event for it — including the trash and
	/// supersede events a deletion or a re-upload produces, which never make the written uuid
	/// appear in a snapshot at all.
	///
	/// Best-effort: a root the cache refuses (deleted, unreachable) leaves that pair on the
	/// [`PENDING_CREATE_GRACE`] fallback rather than failing the whole engine. The watch loop
	/// registers its own subscription for wakeups; the two are independent.
	async fn observe_pair(&self, pair: PairId, remote_root: Uuid) {
		let callback = observation_callback(Arc::clone(&self.observed));
		match Arc::clone(&self.client)
			.add_sync_root(remote_root, callback)
			.await
		{
			Ok(handle) => {
				self.roots.lock().await.insert(pair, handle);
			}
			Err(error) => tracing::warn!(
				"sync pair {pair}: cache notifications are unavailable ({error}); its pending writes will retire on the grace window alone"
			),
		}
	}

	/// Register a sync pair (idempotent for the same `(local_root, remote_root)`), returning its id.
	/// `remote_root` must be a sync-rooted SUBFOLDER the cache covers (not the account root).
	///
	/// `local_root` must already EXIST and be a directory; it is canonicalized before being stored,
	/// so the pair's identity (and the write-confinement anchor) is symlink-stable. A missing or
	/// non-directory root is rejected here rather than surfacing a pass later as an
	/// incomplete-scan guard hold with every deletion withheld.
	pub async fn add_pair(
		&self,
		local_root: PathBuf,
		remote_root: Uuid,
		mode: SyncMode,
	) -> Result<PairId, Error> {
		let local_root = std::fs::canonicalize(&local_root).map_err(|error| {
			Error::custom_with_source(
				ErrorKind::IO,
				error,
				Some(format!(
					"sync pair local root {local_root:?} cannot be resolved"
				)),
			)
		})?;
		if !local_root.is_dir() {
			return Err(Error::custom(
				ErrorKind::InvalidState,
				format!("sync pair local root {local_root:?} is not a directory"),
			));
		}
		let local = local_root.to_string_lossy().into_owned();
		let pair = self
			.store
			.lock()
			.await
			.create_pair(&local, remote_root, mode)
			.map_err(|e| db_error(e, "registering a sync pair"))?;
		self.observe_pair(pair, remote_root).await;
		Ok(pair)
	}

	/// Resolve a two-way conflict the engine is holding at `rel_path` (one reported in
	/// [`SyncReport::conflicts`]) by naming the winning side. The resolution takes effect on the
	/// NEXT pass: the baseline is re-anchored so the winner reads as the changed side and is
	/// propagated normally, and the path (with its subtree) stops being held.
	///
	/// [`KeepBoth`](ConflictResolution::KeepBoth) additionally renames the local copy aside to
	/// `<stem>.old.<ext>` first, so that copy uploads as a new file instead of being overwritten.
	///
	/// Errors if the pair is unknown or no conflict is currently held at `rel_path`.
	pub async fn resolve_conflict(
		&self,
		pair: PairId,
		rel_path: &str,
		resolution: ConflictResolution,
	) -> Result<(), Error> {
		let store = self.store.lock().await;
		let record = store
			.pair(pair)
			.map_err(|e| db_error(e, "loading the sync pair"))?
			.ok_or_else(|| Error::custom(ErrorKind::InvalidState, "unknown sync pair"))?;
		let mut held = store
			.entry(pair, rel_path)
			.map_err(|e| db_error(e, "loading the held conflict"))?
			.filter(|entry| entry.state == BaselineState::Conflicted)
			.ok_or_else(|| {
				Error::custom(
					ErrorKind::InvalidState,
					format!("no conflict is being held at {rel_path:?} for this pair"),
				)
			})?;

		let mut winner = resolution;
		if winner == ConflictResolution::KeepBoth {
			// Move the losing local copy out of the way FIRST, then resolve the path itself to the
			// remote: the moved-aside copy has no baseline row, so it uploads as a new file.
			if held.local_kind.is_some() {
				let moved = apply::rename_aside(Path::new(&record.local_root), rel_path)?;
				tracing::debug!(
					"resolve_conflict[pair {pair}]: kept the local copy of {rel_path:?} aside as {moved:?}"
				);
				held.local_kind = None;
			}
			// Either way the local side is now empty at this path, so the remote copy lands there.
			winner = ConflictResolution::KeepRemote;
		}

		match resolution_entry(rel_path, &held, winner) {
			Some(entry) => store.upsert_entry(pair, &entry),
			// The winner's side had nothing at this path: drop the row entirely, so the other
			// side reads as a fresh create (or as already-gone) rather than as a second conflict.
			None => store.delete_entry(pair, rel_path),
		}
		.map_err(|e| db_error(e, "resolving a conflict"))
	}

	/// Run the read-only half: load the baseline, scan, enumerate the remote, build the view.
	async fn prepare(&self, pair: PairId) -> Result<Prepared, Error> {
		let (record, baseline_entries) = {
			let store = self.store.lock().await;
			let record = store
				.pair(pair)
				.map_err(|e| db_error(e, "loading the sync pair"))?
				.ok_or_else(|| Error::custom(ErrorKind::InvalidState, "unknown sync pair"))?;
			let entries = store
				.entries(pair)
				.map_err(|e| db_error(e, "loading the baseline"))?;
			(record, entries)
		};

		let baseline: Arc<HashMap<String, BaselineEntry>> = Arc::new(
			baseline_entries
				.into_iter()
				.map(|entry| (entry.rel_path.clone(), entry))
				.collect(),
		);

		let local_root = PathBuf::from(&record.local_root);
		let scan_baseline = Arc::clone(&baseline);
		let local_scan =
			tokio::task::spawn_blocking(move || scan::scan_local(&local_root, &scan_baseline))
				.await
				.map_err(|e| {
					Error::custom(ErrorKind::Internal, format!("local scan panicked: {e}"))
				})?;

		// Copied BEFORE the snapshot is read: an event the cache commits afterwards describes a
		// state this snapshot predates, so it must not retire a pending write this pass.
		let observed = self.observed.snapshot();
		let snapshot = self
			.client
			.enumerate_sync_root_snapshot(record.remote_root)
			.await?;
		let mut remote_view =
			plan::build_remote_view(record.remote_root, &snapshot.dirs, &snapshot.files);

		let remote_emptied =
			remote_view.nodes.is_empty() && baseline.values().any(|e| e.remote_uuid.is_some());
		// The rows `settle` retires have to leave the DB too, or a restart would fold writes the
		// cache has demonstrably caught up to. Diffed around the call so `settle` itself stays a
		// pure in-memory operation.
		let before = self.pending.uuids();
		let mut holds = self.pending.settle(pair, &observed, &remote_view.nodes);
		let retired: Vec<Uuid> = before.difference(&self.pending.uuids()).copied().collect();
		if !retired.is_empty() {
			self.store
				.lock()
				.await
				.delete_pending(&retired)
				.map_err(|e| db_error(e, "retiring pending writes"))?;
		}
		// Correct the view with what this engine wrote and the cache has not shown yet, BEFORE
		// anything reconciles or detects moves against it.
		let folded = self
			.pending
			.fold_into(pair, &baseline, &mut remote_view.nodes);
		if folded > 0 {
			tracing::debug!(
				"sync_once[pair {pair}]: folding {folded} unacknowledged write(s) into the remote view"
			);
		}
		holds.held_remote = remote_view.held_paths.clone();
		self.observed.prune_before(self.pending.oldest_stamp());

		Ok(Prepared {
			record,
			baseline,
			local_scan,
			remote_view,
			remote_converged: snapshot.watermark.is_some(),
			remote_emptied,
			holds,
			dirs: snapshot.dirs,
			files: snapshot.files,
		})
	}

	/// Reconcile + guard-screen a pass WITHOUT applying it: a dry run that reads both sides and
	/// reports what a [`sync_once`](Self::sync_once) would do, mutating neither tree nor the
	/// baseline. A pending deletion approval is neither consumed nor honoured here.
	pub async fn plan_pair(&self, pair: PairId) -> Result<PlanOutcome, Error> {
		let prep = self.prepare(pair).await?;
		if let Some(reason) = refusal(&prep) {
			return Ok(PlanOutcome {
				refused: Some(reason),
				..PlanOutcome::default()
			});
		}
		let screened = reconcile_and_screen(&prep, screen_state(&prep));
		Ok(PlanOutcome {
			actions: prep.planned(&screened.decision.safe),
			held: prep.planned(&screened.decision.held),
			held_reason: screened.decision.reason,
			pass_token: screened.pass_token,
			conflicts: screened.conflicts,
			unsyncable: prep.unsyncable(),
			refused: None,
		})
	}

	/// Set `pair`'s mass-delete threshold, from the next pass on. Persisted with the pair, so it
	/// survives a restart; read it back on the pair's [`PairRecord`].
	///
	/// The threshold is the only part of the guard that is configurable. The other holds — an
	/// incomplete local scan, a first sync against a populated destination, a remote view that has
	/// never converged or came back wholly empty — are about whether this pass's evidence can be
	/// TRUSTED at all, and stay in force under every setting including
	/// [`DeleteGuard::unlimited`].
	pub async fn set_delete_guard(&self, pair: PairId, guard: DeleteGuard) -> Result<(), Error> {
		let changed = self
			.store
			.lock()
			.await
			.set_delete_guard(pair, guard)
			.map_err(|e| db_error(e, "setting the delete guard"))?;
		if changed == 0 {
			return Err(Error::custom(ErrorKind::InvalidState, "unknown sync pair"));
		}
		Ok(())
	}

	/// Every sync pair this engine has registered, in registration order.
	pub async fn list_pairs(&self) -> Result<Vec<PairRecord>, Error> {
		self.store
			.lock()
			.await
			.list_pairs()
			.map_err(|e| db_error(e, "listing sync pairs"))
	}

	/// Forget `pair` and every baseline row under it.
	///
	/// This is a REGISTRY operation only: NO file is touched on either side. The local tree and the
	/// remote folder are left exactly as they are — a removed pair simply stops syncing. Adding the
	/// same roots again later starts from an empty baseline, i.e. with first-sync semantics.
	/// Removing an unknown pair is a no-op.
	pub async fn remove_pair(&self, pair: PairId) -> Result<(), Error> {
		self.approvals.lock().await.remove(&pair);
		// Dropping the handle unsubscribes the pair's cache notifications.
		self.roots.lock().await.remove(&pair);
		// The pair's journal rows go with it (`ON DELETE CASCADE`); drop the in-memory copies too.
		// Re-registering the same roots hands back the SAME pair id, and a stale `Trashed` record
		// would then hide a remote item from the fresh pair's very first view.
		self.pending.forget_pair(pair);
		self.store
			.lock()
			.await
			.delete_pair(pair)
			.map_err(|e| db_error(e, "removing a sync pair"))
	}

	/// Approve the mass-delete batch the guard is currently holding for `pair`, naming it by the
	/// `pass_token` the hold reported ([`SyncReport::deletion_token`] or
	/// [`SyncEvent::DeletionsHeld`]).
	///
	/// The approval is ONE-SHOT and batch-specific: the next pass whose held deletions hash to the
	/// same token applies them; any other batch — different paths, or a different reason to hold —
	/// is held again under a fresh token, so an approval can never leak onto deletions the caller
	/// never saw.
	pub async fn approve_deletions(&self, pair: PairId, pass_token: &str) {
		self.approvals
			.lock()
			.await
			.insert(pair, pass_token.to_string());
	}

	/// Consume a pending approval for `pair` if it names exactly this batch.
	async fn take_approval(&self, pair: PairId, pass_token: &str) -> bool {
		let mut approvals = self.approvals.lock().await;
		if approvals.get(&pair).is_some_and(|held| held == pass_token) {
			approvals.remove(&pair);
			true
		} else {
			false
		}
	}

	/// Run one full sync pass: plan, screen, and apply against the remote and local tree.
	pub async fn sync_once(&self, pair: PairId) -> Result<SyncReport, Error> {
		self.sync_once_observed(pair, &mut |_| {}).await
	}

	/// Like [`sync_once`](Self::sync_once), but reports live progress: `observer` is invoked with
	/// each [`SyncEvent`] as the pass plans and applies its actions (see [`SyncEvent`] for the
	/// event order). The observer is called synchronously between async steps, so keep it quick.
	pub async fn sync_once_observed(
		&self,
		pair: PairId,
		observer: &mut (dyn FnMut(SyncEvent) + Send),
	) -> Result<SyncReport, Error> {
		let prep = self.prepare(pair).await?;
		let mut report = SyncReport::default();

		tracing::debug!(
			"sync_once[pair {pair}]: mode {:?} — local scan {} node(s) (complete={}), remote view {} node(s) (converged={})",
			prep.record.mode,
			prep.local_scan.nodes.len(),
			prep.local_scan.complete,
			prep.remote_view.nodes.len(),
			prep.remote_converged,
		);
		observer(SyncEvent::PassStarted {
			mode: prep.record.mode,
		});

		report.unsyncable = prep.unsyncable();

		if let Some(refusal) = refusal(&prep) {
			tracing::debug!("sync_once[pair {pair}]: refused — {refusal:?}");
			let reason = refusal.to_string();
			report.refused = Some(refusal);
			observer(SyncEvent::Refused {
				reason: reason.clone(),
			});
			report
				.errors
				.push(format!("refused: {reason}; resolve it and retry"));
			observer(SyncEvent::PassCompleted {
				report: report.clone(),
			});
			return Ok(report);
		}

		let state = screen_state(&prep);
		let mut screened = reconcile_and_screen(&prep, state);
		// A one-shot approval releases the held batch it names — and only that batch.
		if let Some(token) = screened.pass_token.clone()
			&& self.take_approval(pair, &token).await
		{
			tracing::debug!(
				"sync_once[pair {pair}]: approval {token} matched — applying {} held deletion(s)",
				screened.decision.held.len(),
			);
			screened.decision = guard::GuardDecision {
				safe: std::mem::take(&mut screened.all),
				held: Vec::new(),
				reason: None,
			};
			screened.pass_token = None;
		}
		let decision = screened.decision;
		report.conflicts = screened.conflicts;
		// `held` can also carry the create half of a held type flip; `held_deletions()` counts only
		// the deletions.
		report.held = prep.planned(&decision.held);
		report.guard = decision.reason.clone();
		report.deletion_token = screened.pass_token;
		report.deferred_paths = screened.deferred_paths;

		for conflict in &report.conflicts {
			let rel_path = &conflict.rel_path;
			// HOLD the conflict in the baseline: the path (and its subtree) is excluded from
			// planning until `resolve_conflict` picks a winner, instead of being re-surfaced,
			// unresolvable, on every pass.
			if let Err(error) = apply::record_conflict(
				&self.store,
				pair,
				rel_path,
				prep.local_scan.nodes.get(rel_path),
				prep.remote_view.nodes.get(rel_path),
			)
			.await
			{
				report.errors.push(format!("{rel_path}: {error}"));
			}
			observer(SyncEvent::Conflict {
				rel_path: rel_path.clone(),
			});
		}
		if report.held_deletions() > 0 {
			observer(SyncEvent::DeletionsHeld {
				count: report.held_deletions(),
				reason: report
					.guard
					.as_ref()
					.map(GuardReason::to_string)
					.unwrap_or_default(),
				pass_token: report.deletion_token.clone().unwrap_or_default(),
			});
		}
		observer(SyncEvent::Planned {
			actions: decision.safe.len(),
		});

		if decision.safe.is_empty() {
			tracing::debug!(
				"sync_once[pair {pair}]: nothing to apply ({} deletion(s) held, {} conflict(s), {} path(s) deferred)",
				report.held_deletions(),
				report.conflicts.len(),
				report.deferred_paths,
			);
			observer(SyncEvent::PassCompleted {
				report: report.clone(),
			});
			return Ok(report);
		}

		// Resolve the sync root to a remote directory (every top-level parent). Errors if the root
		// is not a reachable subfolder.
		let root_remote = self.client.get_dir(prep.record.remote_root).await?;

		let local_root = PathBuf::from(&prep.record.local_root);
		let ctx = ApplyContext {
			client: &self.client,
			local_root: &local_root,
			pair,
			store: &self.store,
			local: &prep.local_scan.nodes,
			baseline: &prep.baseline,
			remote: &prep.remote_view.nodes,
			root_remote,
			absence_trusted: state.absence_trusted(),
			dirs: &prep.dirs,
			files: &prep.files,
			pending: &self.pending,
			observed: &self.observed,
		};
		apply::apply(ctx, decision.safe, &mut report, observer).await;
		tracing::debug!(
			"sync_once[pair {pair}]: done — {} uploaded, {} downloaded, {} remote dir(s), {} local dir(s), {} trashed, {} locally deleted, {} moved remote, {} moved local, {} conflict(s), {} held, {} deferred, {} error(s)",
			report.uploaded,
			report.downloaded,
			report.remote_dirs_created,
			report.local_dirs_created,
			report.remotely_trashed,
			report.locally_deleted,
			report.moved_remote,
			report.moved_local,
			report.conflicts.len(),
			report.held_deletions(),
			report.deferred_paths,
			report.errors.len(),
		);
		observer(SyncEvent::PassCompleted {
			report: report.clone(),
		});
		Ok(report)
	}
}

/// A name collision (local or remote) makes a 1:1 mapping impossible — refuse the pass.
fn refusal(prep: &Prepared) -> Option<RefuseReason> {
	if prep
		.local_scan
		.errors
		.iter()
		.any(|e| matches!(e, ScanError::DuplicateName { .. }))
	{
		return Some(RefuseReason::LocalCollision);
	}
	if prep.remote_view.has_collisions {
		return Some(RefuseReason::RemoteCollision);
	}
	None
}

/// A reconciled, guard-screened pass.
struct Screened {
	/// Paths surfaced as two-way conflicts (held, never applied).
	conflicts: Vec<PlannedConflict>,
	/// Every executable action in apply order — what an APPROVED pass runs, held deletions and all.
	all: Vec<SyncAction>,
	decision: guard::GuardDecision,
	/// Identifies the held batch, when the guard held one.
	pass_token: Option<String>,
	/// Paths this pass deliberately left alone (see [`SyncReport::deferred_paths`]).
	deferred_paths: usize,
}

/// Reconcile the prepared inputs and screen deletions through the guard, splitting out conflicts.
fn reconcile_and_screen(prep: &Prepared, state: guard::ScreenState) -> Screened {
	let plan = plan::reconcile(
		prep.record.mode,
		&prep.baseline,
		&prep.local_scan.nodes,
		&prep.remote_view.nodes,
		&prep.holds,
	);
	let deferred_paths = plan.deferred_paths;
	let (conflict_actions, executable): (Vec<_>, Vec<_>) = plan
		.actions
		.into_iter()
		.partition(|a| matches!(a, SyncAction::Conflict { .. }));
	let conflicts = conflict_actions
		.iter()
		.map(|a| {
			planned_conflict(
				a.rel_path(),
				&prep.local_scan.nodes,
				&prep.remote_view.nodes,
			)
		})
		.collect();
	// The unscreened list is only needed to release an approved batch, which cannot happen unless
	// the pass plans a deletion at all — so a pure-transfer pass (a first sync, say) never pays for
	// the copy.
	let all = if executable.iter().any(SyncAction::is_delete) {
		executable.clone()
	} else {
		Vec::new()
	};
	let decision = guard::screen(executable, state, prep.record.delete_guard);
	let pass_token = (!decision.held.is_empty()).then(|| deletion_batch_token(&decision.held));
	Screened {
		conflicts,
		all,
		decision,
		pass_token,
		deferred_paths,
	}
}

/// A stable identifier for one held deletion batch: the sorted `(side, path)` lines hashed. The
/// same set of held deletions always yields the same token, and adding, dropping or re-siding a
/// single deletion yields a different one — which is what makes an approval batch-specific.
fn deletion_batch_token(held: &[SyncAction]) -> String {
	let mut lines: Vec<String> = held
		.iter()
		.map(|action| match action {
			SyncAction::DeleteLocal { rel_path, .. } => format!("local\t{rel_path}"),
			SyncAction::TrashRemote { rel_path, .. } => format!("remote\t{rel_path}"),
			other => format!("other\t{}", other.rel_path()),
		})
		.collect();
	lines.sort_unstable();
	let mut hasher = blake3::Hasher::new();
	for line in &lines {
		hasher.update(line.as_bytes());
		hasher.update(b"\n");
	}
	hasher.finalize().to_hex()[..16].to_string()
}

/// How trustworthy this pass's inputs are, for the guard.
fn screen_state(prep: &Prepared) -> guard::ScreenState {
	guard::ScreenState {
		scan_complete: prep.local_scan.complete,
		remote_converged: prep.remote_converged,
		// A wholly empty remote snapshot while the baseline still tracks remote items: what a
		// transient backend/cache fault looks like, and it would otherwise delete the whole pair.
		remote_emptied: prep.remote_emptied,
		first_sync: prep.baseline.is_empty(),
		tracked: prep.baseline.len(),
	}
}

fn db_error(error: rusqlite::Error, context: &str) -> Error {
	Error::custom_with_source(ErrorKind::Internal, error, Some(context.to_string()))
}

#[cfg(test)]
mod tests {
	use std::collections::HashSet;

	use base64::{Engine as _, prelude::BASE64_STANDARD};
	use filen_types::crypto::Blake3Hash;
	use rsa::{RsaPrivateKey, pkcs8::EncodePrivateKey};

	use super::*;
	use crate::{
		auth::{StringifiedClient, http::ClientConfig, unauth::UnauthClient},
		sync_engine::{
			baseline::{BaselineChange, NodeKind},
			plan::RemoteNode,
			scan::LocalNode,
		},
	};

	/// The pair whose writes the pending-write tests record.
	const PAIR: PairId = 1;

	/// The reconciler's own one-line rendering of an action — what these tests assert plans by.
	/// (The public [`PlannedAction`] rendering is pinned in `outcome.rs`.)
	fn describe(action: &SyncAction) -> String {
		action.describe()
	}

	/// A client with no account and no network behind it. [`SyncEngine::open`] only hands it to
	/// the cache to subscribe each pair's remote root, and a subscription the cache refuses leaves
	/// that pair on the grace window instead of failing the open (see
	/// [`SyncEngine::observe_pair`]) — which is exactly what an unconfigured cache answers.
	fn offline_client() -> Arc<Client> {
		let private_key = RsaPrivateKey::new(&mut old_rng::thread_rng(), 512).unwrap();
		let unauthed = UnauthClient::from_config(ClientConfig::default()).unwrap();
		Arc::new(
			unauthed
				.from_stringified(StringifiedClient {
					email: "sync-engine@example.invalid".to_string(),
					user_id: 1,
					root_uuid: Uuid::nil().to_string(),
					auth_info: "0".repeat(64),
					private_key: BASE64_STANDARD
						.encode(private_key.to_pkcs8_der().unwrap().as_bytes()),
					api_key: String::new(),
					auth_version: 2,
					max_parallel_requests: None,
					max_io_memory_usage: None,
				})
				.unwrap(),
		)
	}

	fn created(path: &str, replaced: Option<Uuid>) -> PendingKind {
		PendingKind::Created {
			path: path.to_string(),
			replaced,
		}
	}

	fn moved(from: &str, to: &str) -> PendingKind {
		PendingKind::Moved {
			from: from.to_string(),
			to: to.to_string(),
		}
	}

	fn hash(byte: u8) -> Blake3Hash {
		Blake3Hash::from([byte; 32])
	}

	/// A held conflict row for `a.txt` whose two sides carry the same content evidence — what
	/// `record_conflict` writes once both sides have converged while the conflict was held.
	fn converged_conflict(uuid: Uuid) -> BaselineEntry {
		BaselineEntry {
			rel_path: "a.txt".to_string(),
			kind: NodeKind::File,
			remote_uuid: Some(uuid),
			content_hash: Some(hash(3)),
			size: Some(5),
			local_mtime: Some(111),
			remote_modified: Some(222),
			state: BaselineState::Conflicted,
			local_kind: Some(NodeKind::File),
			remote_kind: Some(NodeKind::File),
			remote_hash: Some(hash(3)),
			remote_size: Some(5),
		}
	}

	fn local_map(hash: Blake3Hash) -> HashMap<String, LocalNode> {
		HashMap::from([(
			"a.txt".to_string(),
			LocalNode {
				rel_path: "a.txt".to_string(),
				kind: NodeKind::File,
				size: 5,
				mtime_millis: 111,
				content_hash: Some(hash),
			},
		)])
	}

	fn remote_map(uuid: Uuid, hash: Blake3Hash) -> HashMap<String, RemoteNode> {
		HashMap::from([(
			"a.txt".to_string(),
			RemoteNode {
				rel_path: "a.txt".to_string(),
				kind: NodeKind::File,
				remote_uuid: uuid,
				content_hash: Some(hash),
				size: 5,
				modified_millis: 222,
			},
		)])
	}

	/// Both sides converged while the conflict was held: keeping the local copy has nothing left
	/// to push, so the resolution must record the whole synced state rather than a half row the
	/// next pass reads as a local modification.
	#[test]
	fn keeping_local_on_a_converged_conflict_records_a_clean_synced_row() {
		let uuid = Uuid::new_v4();
		let entry = resolution_entry(
			"a.txt",
			&converged_conflict(uuid),
			ConflictResolution::KeepLocal,
		)
		.expect("the remote side had a kind, so a row is written");

		assert_eq!(entry.state, BaselineState::Synced);
		assert_eq!(
			entry.content_hash,
			Some(hash(3)),
			"the converged content is recorded"
		);
		assert_eq!(entry.size, Some(5));
		assert_eq!(entry.local_mtime, Some(111));
		assert_eq!(entry.remote_uuid, Some(uuid));

		let baseline = HashMap::from([("a.txt".to_string(), entry)]);
		assert!(
			plan::reconcile(
				SyncMode::TwoWay,
				&baseline,
				&local_map(hash(3)),
				&remote_map(uuid, hash(3)),
				&plan::PassHolds::default(),
			)
			.actions
			.is_empty(),
			"the resolved path is settled: the next pass plans nothing at all"
		);
	}

	/// The mirror image: keeping the REMOTE copy of a converged conflict also has nothing left to
	/// transfer, so it too must record the whole synced state instead of a half row.
	#[test]
	fn keeping_remote_on_a_converged_conflict_records_a_clean_synced_row() {
		let uuid = Uuid::new_v4();
		let entry = resolution_entry(
			"a.txt",
			&converged_conflict(uuid),
			ConflictResolution::KeepRemote,
		)
		.expect("the local side had a kind, so a row is written");

		assert_eq!(entry.state, BaselineState::Synced);
		assert_eq!(
			entry.remote_uuid,
			Some(uuid),
			"the converged remote version is recorded"
		);
		assert_eq!(entry.remote_modified, Some(222));
		assert_eq!(entry.content_hash, Some(hash(3)));

		let baseline = HashMap::from([("a.txt".to_string(), entry)]);
		assert!(
			plan::reconcile(
				SyncMode::TwoWay,
				&baseline,
				&local_map(hash(3)),
				&remote_map(uuid, hash(3)),
				&plan::PassHolds::default(),
			)
			.actions
			.is_empty(),
			"the resolved path is settled: the next pass plans nothing at all"
		);
	}

	/// And the diverged case still pulls: with the sides genuinely different the row keeps no
	/// remote anchor, so the remote copy reads as the change.
	#[test]
	fn keeping_remote_on_a_diverged_conflict_still_pulls_the_remote_copy() {
		let uuid = Uuid::new_v4();
		let held = BaselineEntry {
			remote_hash: Some(hash(9)),
			..converged_conflict(uuid)
		};
		let entry = resolution_entry("a.txt", &held, ConflictResolution::KeepRemote)
			.expect("the local side had a kind, so a row is written");
		assert_eq!(
			entry.remote_uuid, None,
			"a diverged remote side must still read as changed"
		);

		let baseline = HashMap::from([("a.txt".to_string(), entry)]);
		let actions = plan::reconcile(
			SyncMode::TwoWay,
			&baseline,
			&local_map(hash(3)),
			&remote_map(uuid, hash(9)),
			&plan::PassHolds::default(),
		)
		.actions;
		assert_eq!(
			actions.iter().map(describe).collect::<Vec<_>>(),
			vec!["download file \"a.txt\"".to_string()],
		);
	}

	/// The ordinary case is unchanged: the sides really do differ, so the row is anchored to the
	/// remote alone and the local copy is pushed on the next pass.
	#[test]
	fn keeping_local_on_a_diverged_conflict_still_pushes_the_local_copy() {
		let uuid = Uuid::new_v4();
		let held = BaselineEntry {
			remote_hash: Some(hash(9)),
			..converged_conflict(uuid)
		};
		let entry = resolution_entry("a.txt", &held, ConflictResolution::KeepLocal)
			.expect("the remote side had a kind, so a row is written");
		assert_eq!(
			entry.content_hash, None,
			"a diverged local side must still read as changed"
		);

		let baseline = HashMap::from([("a.txt".to_string(), entry)]);
		let actions = plan::reconcile(
			SyncMode::TwoWay,
			&baseline,
			&local_map(hash(3)),
			&remote_map(uuid, hash(9)),
			&plan::PassHolds::default(),
		)
		.actions;
		assert_eq!(
			actions.iter().map(describe).collect::<Vec<_>>(),
			vec!["upload file \"a.txt\"".to_string()],
		);
	}
	/// A remote node the snapshot holds at `path`.
	fn node_at(path: &str, uuid: Uuid) -> HashMap<String, RemoteNode> {
		HashMap::from([(
			path.to_string(),
			RemoteNode {
				rel_path: path.to_string(),
				kind: NodeKind::File,
				remote_uuid: uuid,
				content_hash: None,
				size: 0,
				modified_millis: 0,
			},
		)])
	}

	/// A baseline row as the apply layer writes it straight after a remote write.
	fn written_at(
		path: &str,
		uuid: Uuid,
		kind: NodeKind,
		hash: Option<Blake3Hash>,
	) -> BaselineEntry {
		BaselineEntry {
			rel_path: path.to_string(),
			kind,
			remote_uuid: Some(uuid),
			content_hash: hash,
			size: Some(5),
			local_mtime: Some(111),
			remote_modified: Some(222),
			state: BaselineState::Synced,
			local_kind: None,
			remote_kind: None,
			remote_hash: None,
			remote_size: None,
		}
	}

	/// The baseline an upload of `a.txt` leaves behind.
	fn written_row(uuid: Uuid, hash: Blake3Hash) -> HashMap<String, BaselineEntry> {
		HashMap::from([(
			"a.txt".to_string(),
			written_at("a.txt", uuid, NodeKind::File, Some(hash)),
		)])
	}

	/// The restart this journal exists for: the engine that made the write is gone, and the one
	/// that reopens its baseline DB seconds later has nothing in memory to fold. Loading the
	/// persisted journal is what keeps it from reading the just-uploaded file as absent and
	/// uploading it a second time.
	#[tokio::test]
	async fn a_create_a_previous_engine_journalled_is_folded_after_a_reopen() {
		let path = std::env::temp_dir().join(format!("filen_sync_journal_{}.db", Uuid::new_v4()));
		let uuid = Uuid::new_v4();
		let baseline = written_row(uuid, hash(1));
		let pair = {
			let store = BaselineStore::open(&path).unwrap();
			let pair = store
				.create_pair("/root", Uuid::new_v4(), SyncMode::LocalToRemote)
				.unwrap();
			store
				.record_pending(
					pair,
					uuid,
					&created("a.txt", None),
					Utc::now().timestamp_millis(),
					&[BaselineChange::Upsert(&baseline["a.txt"])],
				)
				.unwrap();
			pair
			// The engine "exits" here, taking its in-memory journal with it.
		};

		// The restart itself: a real `open` on the same DB, so what is under test is the wiring
		// the reopened engine actually runs — loading the journal and aging it — not a
		// hand-assembled stand-in for it.
		let engine = SyncEngine::open(offline_client(), path.clone())
			.await
			.unwrap();

		// The cache is still behind: its snapshot has nothing at all under the root yet.
		let mut remote = HashMap::new();
		let holds = engine
			.pending
			.settle(pair, &engine.observed.snapshot(), &remote);
		assert_eq!(
			engine.pending.fold_into(pair, &baseline, &mut remote),
			1,
			"the reopened engine folds the write its predecessor made"
		);

		let actions = plan::reconcile(
			SyncMode::LocalToRemote,
			&baseline,
			&local_map(hash(1)),
			&remote,
			&holds,
		)
		.actions;
		assert!(
			actions.is_empty(),
			"the restart re-does the pass's writes: {:?}",
			actions.iter().map(describe).collect::<Vec<_>>()
		);
		drop(engine);
		std::fs::remove_file(&path).ok();
	}

	/// The everyday complaint: a second edit within a few seconds of the first. The cache has not
	/// listed our upload yet, but the engine knows exactly what it wrote there — so the pass must
	/// push the new content rather than leave the path alone until the cache agrees.
	#[test]
	fn a_local_edit_at_a_path_whose_create_is_pending_is_pushed() {
		let observations = Observations::default();
		let pending = PendingWrites::default();
		let uuid = Uuid::new_v4();
		pending.record(&observations, PAIR, uuid, created("a.txt", None));

		let baseline = written_row(uuid, hash(1));
		// The cache is behind: its snapshot has nothing at all under the root yet.
		let mut remote = HashMap::new();
		let holds = pending.settle(PAIR, &observations.snapshot(), &remote);
		pending.fold_into(PAIR, &baseline, &mut remote);

		let actions = plan::reconcile(
			SyncMode::LocalToRemote,
			&baseline,
			&local_map(hash(2)),
			&remote,
			&holds,
		)
		.actions;
		assert_eq!(
			actions.iter().map(describe).collect::<Vec<_>>(),
			vec!["upload file \"a.txt\"".to_string()],
			"the edit is pushed against what we wrote, not deferred until the cache catches up"
		);
	}

	/// The everyday case the snapshot alone cannot settle: the file we uploaded is gone again —
	/// trashed by the user, or superseded by another client's re-upload — so its uuid will NEVER
	/// appear in a snapshot. Waiting for it to appear froze the path for the whole grace window
	/// and every pass in between reported an all-zero no-op. The cache announcing the uuid is the
	/// evidence that it has caught up.
	#[test]
	fn a_write_the_cache_has_announced_retires_even_though_the_item_is_gone() {
		let observations = Observations::default();
		let pending = PendingWrites::default();
		let uuid = Uuid::new_v4();
		pending.record(&observations, PAIR, uuid, created("a.txt", None));

		observations.note([uuid]);
		let observed = observations.snapshot();

		let mut remote = HashMap::new();
		pending.settle(PAIR, &observed, &remote);
		assert_eq!(
			pending.fold_into(PAIR, &written_row(uuid, hash(1)), &mut remote),
			0,
			"an announced uuid retires the write even with nothing left at the path"
		);
		assert!(
			remote.is_empty(),
			"and the snapshot is believed again: the item really is gone"
		);
	}

	/// The race the pre-snapshot copy exists for: an event committed AFTER the snapshot was read
	/// describes a state that snapshot predates. Retiring on it would let this very pass go on to
	/// read the just-written item as a remote-side deletion.
	#[test]
	fn an_announcement_made_after_the_snapshot_was_read_does_not_retire_the_write() {
		let observations = Observations::default();
		let pending = PendingWrites::default();
		let uuid = Uuid::new_v4();
		pending.record(&observations, PAIR, uuid, created("a.txt", None));

		// The pass copies the observations, THEN reads the snapshot; the event lands in between.
		let observed = observations.snapshot();
		observations.note([uuid]);

		let mut remote = HashMap::new();
		pending.settle(PAIR, &observed, &remote);
		assert_eq!(
			pending.fold_into(PAIR, &written_row(uuid, hash(1)), &mut remote),
			1,
			"only what was known before the snapshot may retire a write against it"
		);
		assert_eq!(remote["a.txt"].remote_uuid, uuid);
	}

	/// A move's uuid was necessarily announced earlier, when the item was created. Only an
	/// announcement made after the move itself proves the cache applied the move.
	#[test]
	fn an_announcement_predating_a_move_does_not_retire_it() {
		let observations = Observations::default();
		let pending = PendingWrites::default();
		let uuid = Uuid::new_v4();

		observations.note([uuid]);
		pending.record(&observations, PAIR, uuid, moved("a.txt", "b.txt"));

		let baseline = HashMap::from([(
			"b.txt".to_string(),
			written_at("b.txt", uuid, NodeKind::File, Some(hash(1))),
		)]);
		let mut remote = node_at("a.txt", uuid);
		pending.settle(PAIR, &observations.snapshot(), &remote);
		assert_eq!(
			pending.fold_into(PAIR, &baseline, &mut remote),
			1,
			"the snapshot still shows the pre-move path and nothing new has been announced"
		);

		observations.note([uuid]);
		let mut remote = node_at("a.txt", uuid);
		pending.settle(PAIR, &observations.snapshot(), &remote);
		assert_eq!(
			pending.fold_into(PAIR, &baseline, &mut remote),
			0,
			"an announcement made after the move retires it"
		);
	}

	/// The pass moved the file on; the cache still lists it where it was. The view has to show it
	/// where we put it, or the pass reads the move as a remote-side delete plus a local create.
	#[test]
	fn a_pending_move_is_folded_to_its_destination() {
		let observations = Observations::default();
		let pending = PendingWrites::default();
		let uuid = Uuid::new_v4();
		pending.record(&observations, PAIR, uuid, moved("a.txt", "b.txt"));

		let baseline = HashMap::from([(
			"b.txt".to_string(),
			written_at("b.txt", uuid, NodeKind::File, Some(hash(3))),
		)]);
		let mut remote = node_at("a.txt", uuid);
		let holds = pending.settle(PAIR, &observations.snapshot(), &remote);
		assert_eq!(pending.fold_into(PAIR, &baseline, &mut remote), 1);

		assert!(
			!remote.contains_key("a.txt"),
			"the pre-move path is vacated"
		);
		assert_eq!(remote["b.txt"].remote_uuid, uuid);
		let local = HashMap::from([(
			"b.txt".to_string(),
			LocalNode {
				rel_path: "b.txt".to_string(),
				kind: NodeKind::File,
				size: 5,
				mtime_millis: 111,
				content_hash: Some(hash(3)),
			},
		)]);
		assert!(
			plan::reconcile(SyncMode::TwoWay, &baseline, &local, &remote, &holds)
				.actions
				.is_empty(),
			"both sides agree once the move is folded in: the pass has nothing to do"
		);
	}

	/// The destination was taken while our move was in flight (another client uploaded over it,
	/// and the cache learned of that before it learned of the move). We cannot claim `b.txt` — but
	/// we know for certain the file is no longer at `a.txt`, and leaving it there hands the
	/// reconciler an untracked remote item to trash: the very file we just moved.
	#[test]
	fn a_move_whose_destination_was_taken_still_vacates_the_pre_move_path() {
		let observations = Observations::default();
		let pending = PendingWrites::default();
		let (uuid, foreign) = (Uuid::new_v4(), Uuid::new_v4());
		pending.record(&observations, PAIR, uuid, moved("a.txt", "b.txt"));

		let baseline = HashMap::from([(
			"b.txt".to_string(),
			written_at("b.txt", uuid, NodeKind::File, Some(hash(3))),
		)]);
		let mut remote = node_at("a.txt", uuid);
		remote.extend(node_at("b.txt", foreign));
		let holds = pending.settle(PAIR, &observations.snapshot(), &remote);
		assert_eq!(pending.fold_into(PAIR, &baseline, &mut remote), 1);

		assert!(
			!remote.contains_key("a.txt"),
			"the pre-move path is vacated even though the destination is somebody else's"
		);
		assert_eq!(
			remote["b.txt"].remote_uuid, foreign,
			"and the destination is left showing what really sits there"
		);
		let local = HashMap::from([(
			"b.txt".to_string(),
			LocalNode {
				rel_path: "b.txt".to_string(),
				kind: NodeKind::File,
				size: 5,
				mtime_millis: 111,
				content_hash: Some(hash(3)),
			},
		)]);
		let actions =
			plan::reconcile(SyncMode::LocalToRemote, &baseline, &local, &remote, &holds).actions;
		assert!(
			!actions
				.iter()
				.any(|action| describe(action).contains("\"a.txt\"")),
			"the file we moved is not trashed at the path we moved it off: {:?}",
			actions.iter().map(describe).collect::<Vec<_>>()
		);
	}

	/// A trash the cache has not applied reads as an untracked remote item with nothing local —
	/// a deletion to make all over again. Folding takes it out of the view instead, subtree and
	/// all, since the server trashes a directory whole.
	#[test]
	fn a_pending_trash_takes_the_item_out_of_the_view() {
		let observations = Observations::default();
		let pending = PendingWrites::default();
		let (dir, child) = (Uuid::new_v4(), Uuid::new_v4());
		pending.record(&observations, PAIR, dir, PendingKind::Trashed);

		let mut remote = HashMap::from([
			(
				"d".to_string(),
				RemoteNode {
					rel_path: "d".to_string(),
					kind: NodeKind::Dir,
					remote_uuid: dir,
					content_hash: None,
					size: 0,
					modified_millis: 0,
				},
			),
			(
				"d/x.txt".to_string(),
				RemoteNode {
					rel_path: "d/x.txt".to_string(),
					kind: NodeKind::File,
					remote_uuid: child,
					content_hash: Some(hash(4)),
					size: 5,
					modified_millis: 0,
				},
			),
		]);
		// Trashing dropped both baseline rows, and the local side is gone too.
		let (baseline, local) = (HashMap::new(), HashMap::new());
		let holds = pending.settle(PAIR, &observations.snapshot(), &remote);
		assert_eq!(pending.fold_into(PAIR, &baseline, &mut remote), 1);
		assert!(
			remote.is_empty(),
			"the trashed directory takes its subtree with it"
		);
		assert!(
			holds.trashed.contains(&dir),
			"the deletion of that uuid is still suppressed, whatever a view shows"
		);
		assert!(
			plan::reconcile(SyncMode::LocalToRemote, &baseline, &local, &remote, &holds)
				.actions
				.is_empty(),
			"nothing is trashed a second time"
		);
	}

	/// The upload replaced `old` at the path: the cache is simply behind on us, so the view has to
	/// show OUR item there — the one the next edit is measured against.
	#[test]
	fn a_pending_create_is_folded_over_the_uuid_it_replaced() {
		let observations = Observations::default();
		let pending = PendingWrites::default();
		let (old, new) = (Uuid::new_v4(), Uuid::new_v4());
		pending.record(&observations, PAIR, new, created("a.txt", Some(old)));

		let baseline = written_row(new, hash(1));
		let mut remote = node_at("a.txt", old);
		let holds = pending.settle(PAIR, &observations.snapshot(), &remote);
		assert_eq!(pending.fold_into(PAIR, &baseline, &mut remote), 1);
		assert_eq!(
			remote["a.txt"].remote_uuid, new,
			"the pre-write occupant is what a lagging cache shows; ours supersedes it"
		);
		assert_eq!(remote.len(), 1, "and does not leave the old one behind");

		// A further local edit is then pushed rather than left alone.
		let actions = plan::reconcile(
			SyncMode::LocalToRemote,
			&baseline,
			&local_map(hash(2)),
			&remote,
			&holds,
		)
		.actions;
		assert_eq!(
			actions.iter().map(describe).collect::<Vec<_>>(),
			vec!["upload file \"a.txt\"".to_string()],
		);
	}

	/// Two uploads to one path inside the window: the second's record is retired by the very lag
	/// the fold exists for (the cache still shows the version the FIRST one replaced, which reads
	/// as foreign to it), so the view has to be corrected from the surviving record. Left to the
	/// snapshot, a two-way pass reads both sides as changed and raises a conflict nobody caused.
	#[test]
	fn two_writes_to_one_path_inside_the_window_show_the_latest() {
		let observations = Observations::default();
		let pending = PendingWrites::default();
		let (old, first, second) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
		pending.record(&observations, PAIR, first, created("a.txt", Some(old)));
		pending.record(&observations, PAIR, second, created("a.txt", Some(first)));

		let baseline = written_row(second, hash(2));
		let mut remote = node_at("a.txt", old);
		let holds = pending.settle(PAIR, &observations.snapshot(), &remote);
		assert_eq!(pending.fold_into(PAIR, &baseline, &mut remote), 1);
		assert_eq!(
			remote["a.txt"].remote_uuid, second,
			"the latest write is what the path holds"
		);
		assert!(
			plan::reconcile(
				SyncMode::TwoWay,
				&baseline,
				&local_map(hash(2)),
				&remote,
				&holds
			)
			.actions
			.is_empty(),
			"and both sides agree, so there is no conflict to raise"
		);
	}

	/// A third uuid at the path is not our write lagging: somebody else wrote there after us, and
	/// the reconciler has to see that immediately rather than wait out the grace window.
	#[test]
	fn a_foreign_uuid_at_the_path_retires_the_write() {
		let observations = Observations::default();
		let pending = PendingWrites::default();
		let (old, new) = (Uuid::new_v4(), Uuid::new_v4());
		pending.record(&observations, PAIR, new, created("a.txt", Some(old)));

		let foreign = Uuid::new_v4();
		let mut remote = node_at("a.txt", foreign);
		pending.settle(PAIR, &observations.snapshot(), &remote);
		assert_eq!(
			pending.fold_into(PAIR, &written_row(new, hash(1)), &mut remote),
			0,
			"a uuid that is neither ours nor the one we replaced is a foreign write"
		);
		assert_eq!(
			remote["a.txt"].remote_uuid, foreign,
			"and it must stand, so the pass reconciles against it at once"
		);
	}

	/// A path held in conflict is re-reported every pass until the caller resolves it, and its
	/// subtree stays suppressed — including when this engine has just written there.
	#[test]
	fn a_conflicted_path_is_re_reported_even_with_a_write_folded_at_it() {
		let observations = Observations::default();
		let pending = PendingWrites::default();
		let uuid = Uuid::new_v4();
		pending.record(&observations, PAIR, uuid, created("d", None));

		let baseline = HashMap::from([(
			"d".to_string(),
			BaselineEntry {
				state: BaselineState::Conflicted,
				..written_at("d", uuid, NodeKind::Dir, None)
			},
		)]);
		// A new local file under the held path: an upload the pass must not attempt while the
		// name itself is unresolved.
		let local = HashMap::from([(
			"d/x.txt".to_string(),
			LocalNode {
				rel_path: "d/x.txt".to_string(),
				kind: NodeKind::File,
				size: 5,
				mtime_millis: 111,
				content_hash: Some(hash(2)),
			},
		)]);

		let mut remote = HashMap::new();
		let holds = pending.settle(PAIR, &observations.snapshot(), &remote);
		assert_eq!(pending.fold_into(PAIR, &baseline, &mut remote), 1);
		let actions = plan::reconcile(SyncMode::TwoWay, &baseline, &local, &remote, &holds).actions;
		assert_eq!(
			actions.iter().map(describe).collect::<Vec<_>>(),
			vec!["conflict \"d\"".to_string()],
			"the held conflict is still reported, and nothing under it is acted on"
		);
	}

	/// Each pair enumerates its OWN remote subtree, so another pair's snapshot is silent about a
	/// write it never covers. Reading that silence as evidence retired the record — and a retired
	/// trash is trashed a second time by the owning pair's next pass.
	#[test]
	fn another_pairs_pass_leaves_a_write_it_cannot_see_alone() {
		let observations = Observations::default();
		let pending = PendingWrites::default();
		let (trashed, created) = (Uuid::new_v4(), Uuid::new_v4());
		pending.record(&observations, PAIR, trashed, PendingKind::Trashed);
		pending.record(
			&observations,
			PAIR,
			created,
			PendingKind::Created {
				path: "note.txt".to_string(),
				replaced: None,
			},
		);
		// The other pair happens to hold a path of the same name — its own file, under its own
		// root — and never lists the trashed uuid at all.
		let foreign = node_at("note.txt", Uuid::new_v4());

		let holds = pending.settle(PAIR + 1, &observations.snapshot(), &foreign);
		assert!(
			holds.trashed.is_empty(),
			"a pass holds nothing on behalf of another pair"
		);
		assert_eq!(
			pending.fold_into(PAIR + 1, &HashMap::new(), &mut foreign.clone()),
			0,
			"nor does it fold another pair's writes into its own view"
		);

		let mut remote = node_at("gone.txt", trashed);
		let holds = pending.settle(PAIR, &observations.snapshot(), &remote);
		assert_eq!(
			holds.trashed,
			HashSet::from([trashed]),
			"the owning pair's snapshot still shows the item untrashed"
		);
		let baseline = HashMap::from([(
			"note.txt".to_string(),
			written_at("note.txt", created, NodeKind::File, Some(hash(1))),
		)]);
		assert_eq!(
			pending.fold_into(PAIR, &baseline, &mut remote),
			2,
			"and still has not caught up to either write"
		);
		assert!(!remote.contains_key("gone.txt"), "the trash is folded out");
		assert_eq!(remote["note.txt"].remote_uuid, created);
	}

	/// The oldest surviving write's stamp bounds what still has to be remembered; with nothing
	/// pending, nothing does.
	#[test]
	fn observations_are_pruned_to_the_oldest_live_write() {
		let observations = Observations::default();
		let pending = PendingWrites::default();
		observations.note([Uuid::new_v4(), Uuid::new_v4()]);

		let uuid = Uuid::new_v4();
		pending.record(&observations, PAIR, uuid, created("a.txt", None));
		observations.prune_before(pending.oldest_stamp());
		assert!(
			observations.snapshot().is_empty(),
			"observations older than every live write are unreachable"
		);

		observations.note([uuid]);
		pending.settle(PAIR, &observations.snapshot(), &HashMap::new());
		observations.prune_before(pending.oldest_stamp());
		assert!(
			observations.snapshot().is_empty(),
			"with no pending write left there is nothing to remember at all"
		);
	}
}

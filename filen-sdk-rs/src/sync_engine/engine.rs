//! The engine orchestration: register pairs, plan a pass (read-only), and run one (plan + apply).
//!
//! `prepare` runs the read-only half — load the baseline, scan the local tree (fast-path),
//! enumerate the remote subtree from the cache, build the remote view — shared by `plan_pair` (a
//! dry run) and `sync_once` (plan + guard + apply + baseline advance).

use std::{
	collections::{BTreeSet, HashMap},
	path::{Path, PathBuf},
	sync::Arc,
	time::{Duration, Instant},
};

use chrono::Utc;
use tokio::sync::Mutex;
use uuid::Uuid;

use super::{
	Backlog, SyncEvent, SyncMode,
	apply::{self, ApplyContext, SyncReport},
	baseline::{
		BaselineEntry, BaselineState, BaselineStore, NodeKind, PairId, PairRecord, PendingRow,
	},
	guard::{self, DeleteGuard, GuardReason},
	outcome::{
		PlanOutcome, PlannedAction, PlannedConflict, RefuseReason, UnsyncablePath,
		UnsyncableReason, planned_action, planned_conflict,
	},
	pause::{PassControl, PassGate, PauseOptions, cancel_suspension},
	plan::{self, RemoteNode, RemoteView, SyncAction},
	scan::{self, LocalScan, ScanError},
};
use crate::{
	Error, ErrorKind,
	auth::Client,
	cache::{CacheEvent, CacheEventType, DirEvent, FileEvent, SyncRootCallback, SyncRootHandle},
	fs::HasUUID,
	fs::dir::cache::CacheableDir,
	fs::file::cache::CacheableFile,
	io::client_impl::IoSharedClientExt,
};

/// The ceiling on how long a remote write this engine made stays trusted over the cache snapshot.
///
/// A write normally retires long before this: the cache announces the item (see [`Observations`])
/// or the snapshot shows what happened at its path. The window is the last resort for the case
/// where neither ever arrives — past it the snapshot is believed again, so an item genuinely
/// deleted elsewhere is picked up late, never ignored.
const PENDING_CREATE_GRACE: Duration = Duration::from_secs(180);

/// How many CONSECUTIVE failures at one path the engine retries before it stops planning that path
/// and reports it as [`UnsyncableReason::RepeatedFailure`].
///
/// Small on purpose: the point is to stop a permanently-broken path (a local file the OS will not
/// let us read, a remote item the account may not write) from failing on every pass forever, while
/// still riding out the transient failures a couple of retries cover. A path is unblocked by a
/// success or by [`SyncEngine::retry_path`].
const MAX_PATH_FAILURES: u32 = 3;

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

/// How long a push of ours must stand as the remote head before the engine takes its content as
/// AGREED, with no snapshot ever having listed it.
///
/// A push proves only that the server took our bytes. What makes the content agreed is the remote
/// still holding it a while later — long enough that another client's edit of the same file would
/// have to have been made against our version rather than beside it. Thirty seconds is that "a
/// while": comfortably longer than the socket round trip that announces a foreign write (so a
/// genuinely concurrent edit lands INSIDE the window and stays a conflict), and short enough that
/// the ordinary "someone edited it minutes later" case confirms without a pass having to catch our
/// version as the head.
pub const CONFIRM_TENURE: Duration = Duration::from_secs(30);

/// How long a push record survives with nothing ever resolving it.
///
/// A pass forgets the record of every row it persisted as agreed, so this only catches the ones no
/// pass will ever look at again — a row whose pair was removed, a conflict resolved out from under
/// it. Generous, because a paused pair's records are what confirm it on resume.
const PUSH_RECORD_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// How long the engine remembers that a version uuid came out of one of its OWN uploads.
///
/// Only the version chain of a path this engine has just written asks, and only about versions
/// around that write, so this needs to outlive a pass and nothing more. Sized like the pending-write
/// grace for the same reason: past it, a write of ours is not something a pass still reasons about.
const MINTED_TTL: Duration = PENDING_CREATE_GRACE;

/// What the cache's announcements say about one push of ours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PushVerdict {
	/// Our version stood as the remote head for [`CONFIRM_TENURE`] before anything superseded it
	/// (or still stands): its content is what both sides hold.
	Confirmed,
	/// Evidence exists and does not confirm — something superseded our version inside the window,
	/// or it has not stood long enough yet.
	Unconfirmed,
	/// No evidence either way: the announcement was never seen, or this engine was restarted since.
	Unknown,
}

/// One push of ours awaiting confirmation: when the cache announced our version and when anything
/// superseded it. The difference is the tenure the confirmation rule reads.
#[derive(Debug)]
struct PushTenure {
	/// The file's whole-life id, so a NEW version of the same file recognisably supersedes ours
	/// even when no archive/trash event names our uuid (a resync, say).
	lineage: Option<filen_types::fs::StableUuid>,
	recorded: Instant,
	announced: Option<Instant>,
	superseded: Option<Instant>,
}

impl PushTenure {
	fn verdict(&self, now: Instant) -> PushVerdict {
		match self.stood_for(now) {
			Some(stood_for) if stood_for >= CONFIRM_TENURE => PushVerdict::Confirmed,
			Some(_) => PushVerdict::Unconfirmed,
			None => PushVerdict::Unknown,
		}
	}

	/// How long our version has stood as the remote head — until whatever superseded it, or until
	/// now if nothing has. `None` while the cache has not announced it at all.
	fn stood_for(&self, now: Instant) -> Option<std::time::Duration> {
		let announced = self.announced?;
		Some(
			self.superseded
				.unwrap_or(now)
				.saturating_duration_since(announced),
		)
	}
}

/// Uuids the cache has announced to this engine, each stamped with the observation counter and the
/// moment it arrived — plus the tenure of every push of ours still awaiting confirmation.
///
/// The counter is what retires a pending write in the ordinary case. Waiting for the written uuid
/// to APPEAR in a snapshot only works while the item survives, and two everyday events destroy it:
/// a re-upload (ours or another client's) versions the file under a fresh uuid, and a deletion
/// removes it outright. Either way the cache announces the uuid — as a `New`, a `Trashed`, an
/// `Archived` or a `Removed` — and that announcement alone proves the cache is no longer behind
/// on our write.
///
/// The timestamps are what confirms a push no snapshot ever caught as the head (see
/// [`CONFIRM_TENURE`]). Both are recorded on the cache worker thread, so this type never does more
/// than take a lock and touch two maps.
#[derive(Debug, Default)]
pub(super) struct Observations(std::sync::Mutex<ObservationState>);

#[derive(Debug, Default)]
struct ObservationState {
	seq: u64,
	seen: HashMap<Uuid, (u64, Instant)>,
	/// Pushes of ours whose content is not agreed yet, keyed by the version uuid the upload minted.
	pushes: HashMap<Uuid, PushTenure>,
	/// Every version uuid this engine's own uploads minted recently, confirmed or not.
	///
	/// The version chain cannot say who made a version and the server stamps them to the second,
	/// so our own previous upload and another client's concurrent edit are indistinguishable there
	/// whenever both fall in one second. This is the difference: a uuid in here came out of an
	/// upload of ours, so it is not an edit anybody interleaved (see
	/// [`plan::interleaved_version`]).
	///
	/// It is process-local, so a restart forgets what it wrote and a chain question asked
	/// within one second of a pre-restart upload can read that upload as a stranger's — one
	/// spurious conflict, cleared with KeepLocal. Persisting it would mean a table for a fact that
	/// matters for [`MINTED_TTL`].
	minted: HashMap<Uuid, Instant>,
}

impl Observations {
	/// Record every uuid a committed cache batch touched, and time-stamp what it says about a push
	/// of ours. Runs on the cache worker thread, so it does nothing but take a lock and insert.
	fn note(&self, events: &mut dyn Iterator<Item = &CacheEvent<'_>>) {
		let now = Instant::now();
		let mut state = self.state();
		for event in events {
			for uuid in event_uuids(event).into_iter().flatten() {
				state.see(uuid, now);
			}
			state.note_tenure(event, now);
		}
	}

	/// Record an announcement directly, for the tests that have no cache event to hand.
	#[cfg(test)]
	fn note_uuids(&self, uuids: impl IntoIterator<Item = Uuid>) {
		let now = Instant::now();
		let mut state = self.state();
		for uuid in uuids {
			state.see(uuid, now);
		}
	}

	/// Start watching a push of ours: it is unconfirmed until the cache says otherwise.
	///
	/// `replaced` is the version this upload went on top of — its own record, if it still has one,
	/// answers for a path the baseline has since moved past, so it goes.
	pub(super) fn watch_push(
		&self,
		uuid: Uuid,
		lineage: Option<filen_types::fs::StableUuid>,
		replaced: Option<Uuid>,
	) {
		let now = Instant::now();
		let mut state = self.state();
		if let Some(replaced) = replaced {
			state.pushes.remove(&replaced);
		}
		// The cache can announce our own upload before the call that made it has returned; the
		// announcement map is the only place that moment survives.
		let announced = state.seen.get(&uuid).map(|(_, at)| *at);
		state.minted.insert(uuid, now);
		state.pushes.insert(
			uuid,
			PushTenure {
				lineage,
				recorded: now,
				announced,
				superseded: None,
			},
		);
	}

	/// What the announcements say about the push that minted `uuid`.
	fn push_verdict(&self, uuid: Uuid, now: Instant) -> PushVerdict {
		let state = self.state();
		let Some(push) = state.pushes.get(&uuid) else {
			return PushVerdict::Unknown;
		};
		let verdict = push.verdict(now);
		tracing::debug!(
			"sync engine: the push that minted {uuid} stood as the head for {:?} — {verdict:?}",
			push.stood_for(now)
		);
		verdict
	}

	/// Whether this engine's own upload minted `uuid` recently (see [`MINTED_TTL`]).
	pub(super) fn minted(&self, uuid: Uuid) -> bool {
		self.state().minted.contains_key(&uuid)
	}

	/// Forget the push records a pass has finished with, and any that no pass will ever read again.
	fn forget_pushes(&self, decided: &[Uuid], now: Instant) {
		let mut state = self.state();
		for uuid in decided {
			state.pushes.remove(uuid);
		}
		state
			.pushes
			.retain(|_, push| now.saturating_duration_since(push.recorded) < PUSH_RECORD_TTL);
		state
			.minted
			.retain(|_, at| now.saturating_duration_since(*at) < MINTED_TTL);
	}

	/// The current counter — the stamp a write recorded now must be beaten by to retire.
	fn stamp(&self) -> u64 {
		self.state().seq
	}

	/// A copy to settle against. Taken BEFORE the remote snapshot is read: an event committed
	/// after the snapshot must not retire a write this pass, or the pass would go on to read the
	/// just-written item as deleted.
	fn snapshot(&self) -> HashMap<Uuid, u64> {
		self.state()
			.seen
			.iter()
			.map(|(uuid, (seq, _))| (*uuid, *seq))
			.collect()
	}

	/// Forget the observations no live pending write can ever consult — only one stamped LATER
	/// than a write retires it, so everything up to the oldest live write's stamp is dead weight
	/// (and with no write left, so is the whole map).
	fn prune_before(&self, oldest: Option<u64>) {
		let mut state = self.state();
		match oldest {
			Some(seq) => state.seen.retain(|_, (stamp, _)| *stamp > seq),
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

impl ObservationState {
	/// Stamp one announced uuid with the next observation counter and the moment it arrived.
	fn see(&mut self, uuid: Uuid, now: Instant) {
		self.seq += 1;
		let seq = self.seq;
		self.seen.insert(uuid, (seq, now));
	}

	/// Time-stamp what one cache event says about the pushes being watched: a live announcement of
	/// our own version, or something taking its place.
	///
	/// The scan over the watched pushes is bounded by how many pushes this engine has made since the
	/// last pass persisted what it confirmed — every one of those records is retired there, whether
	/// it was a snapshot or a tenure that vouched for it — so in steady state it is a handful of
	/// comparisons on the cache worker thread.
	///
	/// ponytail: O(pushes) per foreign event, which a first sync of a huge tree can make O(n) while
	/// its uploads are still unsettled. A `lineage -> uuids` index beside `pushes` makes it O(1) if
	/// that ever shows up in a profile.
	fn note_tenure(&mut self, event: &CacheEvent<'_>, now: Instant) {
		let CacheEventType::File(file) = &event.event else {
			return;
		};
		match file {
			FileEvent::New(f) | FileEvent::Changed(f) | FileEvent::Move(f) => {
				if let Some(push) = self.pushes.get_mut(&f.uuid) {
					push.announced.get_or_insert(now);
					return;
				}
				// A different version of a file we pushed: whatever announced it, ours is no longer
				// what the remote holds.
				for (uuid, push) in self.pushes.iter_mut() {
					if *uuid != f.uuid && push.lineage == Some(f.stable_uuid) {
						push.superseded.get_or_insert(now);
					}
				}
			}
			// An edit (either versioning mode) or a deletion of our own version: either way it stopped
			// standing at this moment.
			FileEvent::Archived { uuid, .. } | FileEvent::Trashed { uuid, .. } => {
				if let Some(push) = self.pushes.get_mut(uuid) {
					push.superseded.get_or_insert(now);
				}
			}
			FileEvent::Removed(_) | FileEvent::MetadataChanged { .. } => {}
		}
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
	Box::new(move |events| observations.note(events))
}

/// Read what the cache's announcements say about every unconfirmed push in `baseline`: the version
/// uuids they confirm, and the rows (`rel_path`, ours, the foreign version at the path) they say
/// nothing about that are worth asking the server about.
///
/// Purely a READ of the observations. The records it consulted stay put, because the caller may be
/// a dry run: [`SyncEngine::plan_pair`] reaches this through `prepare` and persists nothing, and a
/// record dropped there would leave the pass that follows with no evidence at all that the push was
/// ever confirmed. Only the callers that write the advanced rows retire the records
/// ([`Observations::forget_pushes`]).
fn observed_confirmations(
	observed: &Observations,
	baseline: &HashMap<String, BaselineEntry>,
	raw_remote: &HashMap<String, RemoteNode>,
	now: Instant,
) -> (std::collections::HashSet<Uuid>, Vec<(String, Uuid, Uuid)>) {
	let mut confirmed = std::collections::HashSet::new();
	// rel_path is only for the log; the lookup runs on the foreign version sitting at it.
	let mut ask_server = Vec::new();
	for (rel_path, entry) in baseline.iter() {
		if !plan::awaits_confirmation(entry) {
			continue;
		}
		let Some(ours) = entry.remote_uuid else {
			continue;
		};
		match observed.push_verdict(ours, now) {
			PushVerdict::Confirmed => {
				confirmed.insert(ours);
			}
			PushVerdict::Unconfirmed => {}
			// Nothing observed. Worth a round trip only where the answer changes this pass:
			// a foreign version of the same file already sitting at the row's path.
			PushVerdict::Unknown => {
				if let Some(node) = raw_remote.get(rel_path)
					&& node.remote_uuid != ours
					&& node.stable_uuid.is_some()
					&& node.stable_uuid == entry.remote_stable_uuid
				{
					ask_server.push((rel_path.clone(), ours, node.remote_uuid));
				}
			}
		}
	}
	(confirmed, ask_server)
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

	/// Drop the records the cache has demonstrably caught up to and the ones past
	/// [`PENDING_CREATE_GRACE`], and return what the reconciler still has to suppress on their
	/// behalf. Whether a surviving create may actually correct the view is [`fold_create`]'s
	/// question, not this one's.
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
				PendingKind::Created { .. } => {
					// The snapshot lists what we wrote: the cache has caught up. What it lists
					// INSTEAD is a question of identity — the version our upload superseded reads
					// exactly like somebody else's write from here — and the row that answers it is
					// the fold's ([`fold_create`]), which refuses to paint over an item that is not
					// ours. Keeping the record until then costs nothing: the announcement above or
					// the grace ceiling retires it either way.
					!snapshot_path.contains_key(uuid)
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
		stable_uuid: entry.remote_stable_uuid,
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

/// Show what we wrote at `path`, over the version it superseded there — and decide, for a path
/// showing something else entirely, whether this engine may claim it at all. [`PendingWrites`]
/// keeps a create's record while the cache has announced nothing of it; this is where what the
/// snapshot shows at its path is weighed against it.
///
/// The node comes from the baseline row at `path` whatever uuid that row names, not from the
/// record's own uuid: a second write to the same path inside the window supersedes the first, and
/// the row is where the latest write recorded itself, so both records fold the same node and the
/// first one to run wins.
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
	match nodes.get(path) {
		// The cache is showing what our baseline records: it has caught up.
		Some(current) if current.remote_uuid == node.remote_uuid => return false,
		// Ours — the write itself, or the version it superseded — so the cache is behind on us.
		Some(current) if current.remote_uuid == uuid || Some(current.remote_uuid) == replaced => {}
		// Another VERSION of the same file. A record only reaches the fold while the cache has
		// announced nothing of our write, and the cache cannot have applied a version that landed
		// AFTER ours without announcing ours on the way — so what it shows here is a version our
		// upload went on top of, whichever client made it. Left standing it reads as a foreign edit
		// over an unconfirmed push and conflicts the client whose copy is the remote head. (A
		// resync that skips straight past our version can still show a genuinely newer one; that
		// costs a pass or two at this path until the grace ceiling retires the record.)
		Some(current)
			if current.stable_uuid.is_some() && current.stable_uuid == node.stable_uuid => {}
		None => {}
		// Somebody else's item — a different file has taken the path over, and the pass has to
		// reconcile against it at once.
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
	/// Serializes [`add_pair`](SyncEngine::add_pair): its overlap check reads the registry and then
	/// inserts into it, with an `await` in between, so two concurrent registrations would otherwise
	/// both find the roots free and both commit — the exact overlap the check exists to refuse.
	registrations: Mutex<()>,
	/// The live pause state of every pair the engine has had to look at, one
	/// [`watch`](tokio::sync::watch) channel each (see [`SyncEngine::pause_pair_with`]). It mirrors
	/// the persisted `paused` column — which stays the source of truth across restarts — and adds
	/// what the pass IN FLIGHT must do about the pause, which is process-local by nature: a restart
	/// has no pass to suspend. Bounded by the pair count; an entry goes with its pair.
	paused: Mutex<HashMap<PairId, tokio::sync::watch::Sender<PassControl>>>,
	/// One removal signal per pair that has been watched: [`remove_pair`](SyncEngine::remove_pair)
	/// flips it and drops it, so the pair's watch loops stop instead of failing every pass against a
	/// pair that no longer exists — and a pair re-registered under the same id (sqlite reuses one)
	/// gets a fresh signal rather than an already-tripped one.
	removals: Mutex<HashMap<PairId, tokio::sync::watch::Sender<bool>>>,
}

/// Why [`SyncEngine::add_pair`] refused to register a pair: its roots overlap one already
/// registered, so the two pairs would fight over the same items — each reading the other's writes
/// as foreign changes, re-uploading and re-deleting them without ever converging.
///
/// Carried as the source of the returned [`Error`], so a caller that wants to react per case can
/// recover it with [`Error::downcast`].
///
/// # The remote-nesting gap
///
/// Remote nesting is detected from the CACHE's ancestry of the two roots, which is the only cheap
/// way to relate two uuids without a round trip per pair. That makes the nested-remote checks
/// BEST-EFFORT: a remote root the cache has not learned about yet (a folder created seconds ago, a
/// cache still converging) has no cached ancestry, so a nesting it is part of is not seen and the
/// registration is allowed. Equal remote roots are caught regardless — that comparison needs no
/// cache. A nesting missed here surfaces later as the two pairs disagreeing about the shared
/// subtree; re-checking it on every pass is the fix if that ever proves to matter in practice.
#[derive(Debug, thiserror::Error)]
pub enum PairOverlap {
	/// Another pair already syncs this exact local folder.
	#[error("sync pair {pair} already syncs the local root {existing:?}")]
	LocalRootInUse { pair: PairId, existing: String },
	/// The new local root is INSIDE an existing pair's local root.
	#[error("the local root is inside sync pair {pair}'s root {existing:?}")]
	LocalRootNested { pair: PairId, existing: String },
	/// The new local root CONTAINS an existing pair's local root.
	#[error("the local root contains sync pair {pair}'s root {existing:?}")]
	LocalRootContains { pair: PairId, existing: String },
	/// Another pair already syncs this exact remote folder.
	#[error("sync pair {pair} already syncs the remote root {existing}")]
	RemoteRootInUse { pair: PairId, existing: Uuid },
	/// The new remote root is INSIDE an existing pair's remote root.
	#[error("the remote root is inside sync pair {pair}'s remote root {existing}")]
	RemoteRootNested { pair: PairId, existing: Uuid },
	/// The new remote root CONTAINS an existing pair's remote root.
	#[error("the remote root contains sync pair {pair}'s remote root {existing}")]
	RemoteRootContains { pair: PairId, existing: Uuid },
}

/// How a candidate local root relates to an existing pair's, or `None` when they are disjoint.
/// Both paths are canonical (every stored root was canonicalized when it was registered), so a
/// prefix comparison is a real containment test rather than a string coincidence.
fn local_overlap(candidate: &Path, existing: &Path, pair: PairId) -> Option<PairOverlap> {
	let shown = || existing.to_string_lossy().into_owned();
	if candidate == existing {
		Some(PairOverlap::LocalRootInUse {
			pair,
			existing: shown(),
		})
	} else if candidate.starts_with(existing) {
		Some(PairOverlap::LocalRootNested {
			pair,
			existing: shown(),
		})
	} else if existing.starts_with(candidate) {
		Some(PairOverlap::LocalRootContains {
			pair,
			existing: shown(),
		})
	} else {
		None
	}
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
	/// The pair's live per-path failure streaks: `rel_path -> (attempts, last error)`.
	failures: HashMap<String, (u32, String)>,
	/// Baseline rows whose agreed-content marker this pass's raw snapshot advanced (see
	/// [`plan::confirm_agreed_content`]). Already applied to `baseline`, so planning reads them
	/// either way; a real pass persists them, a dry run writes nothing.
	confirmed: Vec<BaselineEntry>,
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
		let names = self
			.local_scan
			.invalid_names
			.iter()
			.map(|(rel_path, detail)| UnsyncablePath {
				rel_path: rel_path.clone(),
				reason: UnsyncableReason::InvalidName {
					detail: detail.clone(),
				},
			});
		let mut all: Vec<UnsyncablePath> = names
			.chain(
				self.exhausted_paths()
					.into_iter()
					.map(|rel_path| UnsyncablePath {
						reason: UnsyncableReason::RepeatedFailure {
							attempts: self.failures[&rel_path].0,
							last_error: self.failures[&rel_path].1.clone(),
						},
						rel_path,
					}),
			)
			.collect();
		// One stable order, so a caller diffing consecutive reports sees only real changes.
		all.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
		all
	}

	/// Every path this pass must not plan an action for: a name the remote would reject, and a path
	/// whose failure streak ran out. Both are reported by [`unsyncable`](Self::unsyncable).
	fn blocked_paths(&self) -> BTreeSet<String> {
		let mut blocked = self.exhausted_paths();
		blocked.extend(self.local_scan.invalid_names.keys().cloned());
		blocked
	}

	/// The paths whose failure streak has reached [`MAX_PATH_FAILURES`] — no longer planned.
	fn exhausted_paths(&self) -> BTreeSet<String> {
		self.failures
			.iter()
			.filter(|(_, (attempts, _))| *attempts >= MAX_PATH_FAILURES)
			.map(|(rel_path, _)| rel_path.clone())
			.collect()
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
		remote_stable_uuid: None,
		agreed_hash: None,
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
				remote_stable_uuid: held.remote_stable_uuid,
				remote_modified: held.remote_modified,
				content_hash: converged.then_some(held.content_hash).flatten(),
				size: converged.then_some(held.size).flatten(),
				local_mtime: converged.then_some(held.local_mtime).flatten(),
				// Resolving is an act of agreement: whatever the row now records as this side's
				// content is what both sides are taken to hold from here on, so the next foreign
				// edit reads as an ordinary one instead of re-conflicting forever.
				agreed_hash: converged.then_some(held.content_hash).flatten(),
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
				remote_stable_uuid: converged.then_some(held.remote_stable_uuid).flatten(),
				remote_modified: converged.then_some(held.remote_modified).flatten(),
				// As above — and here the row's content is the local copy either way, so the marker
				// records it whether or not the two sides converged while the conflict was held.
				agreed_hash: held.content_hash,
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
		// A pair paused by an earlier session stays paused: read the flags before the store moves
		// into the engine, so no pass can run against an empty map in the meantime. The pass that
		// session had in flight died with it, so every pair comes back on the DEFAULT pause options
		// — there is nothing left for a mode to apply to.
		let paused = store
			.paused_pairs()
			.map_err(|e| db_error(e, "loading paused sync pairs"))?
			.into_iter()
			.map(|pair| {
				(
					pair,
					tokio::sync::watch::channel(PassControl::pausing(PauseOptions::default())).0,
				)
			})
			.collect();
		let engine = Self {
			client,
			store: Mutex::new(store),
			pending: PendingWrites::default(),
			observed: Arc::new(Observations::default()),
			roots: Mutex::new(HashMap::new()),
			approvals: Mutex::new(HashMap::new()),
			registrations: Mutex::new(()),
			paused: Mutex::new(paused),
			removals: Mutex::new(HashMap::new()),
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

	/// Refuse a pair whose roots overlap one already registered (see [`PairOverlap`]). The pair
	/// being re-registered unchanged is exempt: identical roots are the idempotent case, not an
	/// overlap.
	async fn check_roots_free(
		&self,
		local_root: &Path,
		local: &str,
		remote_root: Uuid,
	) -> Result<(), Error> {
		let existing = self.list_pairs().await?;
		let same_pair =
			|record: &PairRecord| record.local_root == local && record.remote_root == remote_root;

		for record in existing.iter().filter(|r| !same_pair(r)) {
			if let Some(overlap) =
				local_overlap(local_root, Path::new(&record.local_root), record.id)
			{
				return Err(overlap_error(overlap));
			}
			if record.remote_root == remote_root {
				return Err(overlap_error(PairOverlap::RemoteRootInUse {
					pair: record.id,
					existing: record.remote_root,
				}));
			}
		}

		// Nested remote roots need the cache's ancestry (see `PairOverlap`'s note on the gap). One
		// walk for the candidate covers "the new root is inside an existing one"; one per existing
		// root covers the other direction. An ancestry the cache cannot supply is skipped, never
		// treated as proof the roots are unrelated.
		let candidate_chain = self.ancestry_or_unknown(remote_root).await;
		for record in existing.iter().filter(|r| !same_pair(r)) {
			if candidate_chain.contains(&record.remote_root) {
				return Err(overlap_error(PairOverlap::RemoteRootNested {
					pair: record.id,
					existing: record.remote_root,
				}));
			}
			if self
				.ancestry_or_unknown(record.remote_root)
				.await
				.contains(&remote_root)
			{
				return Err(overlap_error(PairOverlap::RemoteRootContains {
					pair: record.id,
					existing: record.remote_root,
				}));
			}
		}
		Ok(())
	}

	/// `uuid`'s cached ancestor chain, or an empty chain when the cache cannot answer. Best-effort
	/// by design: a root the cache has not learned about must not block a registration.
	async fn ancestry_or_unknown(&self, uuid: Uuid) -> Vec<Uuid> {
		match self.client.cached_ancestors(uuid).await {
			Ok(chain) => chain,
			Err(error) => {
				tracing::debug!(
					"add_pair: cannot read the cached ancestry of {uuid} ({error}); nested-remote-root detection is skipped for it"
				);
				Vec::new()
			}
		}
	}

	/// Register a sync pair, returning its id. `remote_root` must be a sync-rooted SUBFOLDER the
	/// cache covers (not the account root).
	///
	/// Idempotent for the same `(local_root, remote_root, mode)`: the existing pair's id comes back
	/// and nothing is disturbed. The same roots with a DIFFERENT mode is an ERROR, not a silent
	/// switch — an established pair's direction decides whether the next pass overwrites the local
	/// copy or the remote one, which is far too consequential to change as a side effect of a
	/// registration call. Use [`reconfigure_pair`](Self::reconfigure_pair) to change it.
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
		// Held across the check AND the insert: concurrent registrations must not both read a
		// registry neither has written to yet.
		let _registering = self.registrations.lock().await;
		self.check_roots_free(&local_root, &local, remote_root)
			.await?;
		let (pair, stored_mode) = self
			.store
			.lock()
			.await
			.create_pair(&local, remote_root, mode)
			.map_err(|e| db_error(e, "registering a sync pair"))?;
		if stored_mode != mode {
			return Err(Error::custom(
				ErrorKind::InvalidState,
				format!(
					"sync pair {pair} is already registered for these roots in {stored_mode:?} \
					 mode; call reconfigure_pair to change it to {mode:?}"
				),
			));
		}
		self.observe_pair(pair, remote_root).await;
		Ok(pair)
	}

	/// Change `pair`'s [`SyncMode`]. The new mode applies from the NEXT pass; nothing is re-run
	/// under it and no baseline row is dropped, so an established pair does not re-transfer its
	/// contents and a held conflict stays held for
	/// [`resolve_conflict`](Self::resolve_conflict).
	///
	/// "Prospective" is about the PASSES, not about the divergence between the two sides. The next
	/// pass reconciles whatever state the sides are in right now under the new rules, so a
	/// divergence the old mode deliberately left standing is acted on as soon as the new mode says
	/// to act on it. Concretely:
	///
	/// - to `LocalToRemote`: the remote stops being an independent side — a remote-only edit is
	///   overwritten by the local copy rather than pulled — and, going the other way, a
	///   `RemoteToLocal` switch has the local copy overwritten instead.
	/// - a backup mode (`LocalBackup` / `RemoteBackup`) to a deletion-propagating one: every source
	///   deletion the backup mode had left standing on the destination is a pending deletion under
	///   the new mode. This is what `backlog` decides — see below.
	/// - to a backup mode: deletions simply stop propagating from the next pass on; nothing already
	///   deleted comes back.
	///
	/// # The standing backlog
	///
	/// [`Backlog::Propagate`] takes the sides as they are: the whole set of source deletions the
	/// backup mode left standing is pending at once, and the next pass propagates it. The
	/// mass-delete guard still screens it (see [`set_delete_guard`](Self::set_delete_guard)), so a
	/// large backlog is held for approval rather than applied unasked — but a small one is not.
	///
	/// [`Backlog::AdoptDestination`] reads both sides FIRST (one snapshot + one local scan, the same
	/// pair a pass reads) and re-seeds the baseline from the destination for every tracked path the
	/// source no longer has, in the same transaction as the mode change. Those copies then count as
	/// intended: a one-way mirror neither deletes them nor pushes them back to the source, and
	/// `TwoWay` reads them as newly created on the side that still has them and flows them back. A
	/// destination item the pair never tracked is not adopted — the new mode's ordinary rules apply
	/// to it, switch or no switch.
	///
	/// Either way, [`plan_pair`](Self::plan_pair) after the switch shows exactly what the first pass
	/// will do.
	///
	/// Errors if the pair is unknown, and — for [`Backlog::AdoptDestination`] only — if that read
	/// cannot be trusted to say what either side no longer has: a name collision, an incomplete local
	/// scan, or a remote view that has never converged or came back wholly empty. The mode is left
	/// exactly as it was, so the call is safe to retry. Re-registering an existing pair through
	/// [`add_pair`](Self::add_pair) with a different mode is an error rather than a silent switch,
	/// so a mode change is always this explicit call.
	pub async fn reconfigure_pair(
		&self,
		pair: PairId,
		mode: SyncMode,
		backlog: Backlog,
	) -> Result<(), Error> {
		// Read both sides before touching the mode: the adoption is anchored to what the
		// destination holds NOW, and the switch must not take effect without it.
		let adopted = match backlog {
			Backlog::Propagate => Vec::new(),
			Backlog::AdoptDestination => {
				let prep = self.prepare(pair).await?;
				// The adoption rewrites baseline rows from this one read, and every row it writes is
				// for a path it reads as GONE on the source side — so it needs the same evidence a
				// pass needs before acting on an absence. Read through an incomplete scan or an
				// unconverged/transiently-empty remote view it does the opposite of what was asked:
				// it adopts nothing and the switch then propagates the very backlog the caller wanted
				// kept, or it adopts a whole pair from whichever side still lists something and drops
				// the other side's anchors. Refuse instead — the mode is left as it was, and the
				// caller can retry once the view is healthy.
				if let Some(reason) = adoption_refusal(refusal(&prep), &screen_state(&prep)) {
					return Err(Error::custom(
						ErrorKind::InvalidState,
						format!("cannot adopt the destination: {reason}"),
					));
				}
				plan::adopt_destination_rows(
					mode,
					&prep.baseline,
					&prep.local_scan.nodes,
					&prep.remote_view.nodes,
				)
			}
		};
		let changed = self
			.store
			.lock()
			.await
			.set_mode(pair, mode, &adopted)
			.map_err(|e| db_error(e, "reconfiguring a sync pair"))?;
		if changed == 0 {
			return Err(Error::custom(ErrorKind::InvalidState, "unknown sync pair"));
		}
		tracing::debug!(
			"sync pair {pair}: mode changed to {mode:?} from the next pass ({backlog:?}, {} destination item(s) adopted)",
			adopted.len()
		);
		Ok(())
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
		let (record, mut held) = {
			let store = self.store.lock().await;
			let record = store
				.pair(pair)
				.map_err(|e| db_error(e, "loading the sync pair"))?
				.ok_or_else(|| Error::custom(ErrorKind::InvalidState, "unknown sync pair"))?;
			let held = store
				.entry(pair, rel_path)
				.map_err(|e| db_error(e, "loading the held conflict"))?
				.filter(|entry| entry.state.is_conflict())
				.ok_or_else(|| {
					Error::custom(
						ErrorKind::InvalidState,
						format!("no conflict is being held at {rel_path:?} for this pair"),
					)
				})?;
			(record, held)
		};

		// A divergence this engine's own push created is resolved against the SERVER's version
		// history rather than against what the remote holds now — our upload is what it holds.
		if held.state == BaselineState::Overwritten {
			return self
				.resolve_overwritten(pair, &record, rel_path, &held, resolution)
				.await;
		}

		let store = self.store.lock().await;
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

	/// Resolve a divergence this engine's own upload created — it buried another client's edit (see
	/// [`BaselineState::Overwritten`]).
	///
	/// The shape is the mirror of an ordinary conflict: the winning side is already the remote head
	/// and the losing side is a version in the file's history, so every resolution acts on the
	/// history rather than on the current head.
	///
	/// - [`KeepLocal`](ConflictResolution::KeepLocal): nothing to do on the remote — our bytes are
	///   the head and theirs stay in the version history. The row goes, and the next pass records
	///   the two sides as converged (they hold the same content) with no transfer.
	/// - [`KeepRemote`](ConflictResolution::KeepRemote): restore the buried version, so it is the
	///   head again, and quarantine our own copy. The next pass downloads what we restored; our
	///   bytes are recoverable from the bin and from the history.
	/// - [`KeepBoth`](ConflictResolution::KeepBoth): fetch the buried version beside ours as
	///   `<stem>.old.<ext>`. It has no baseline row, so the next pass uploads it — and both
	///   clients end up holding both files instead of one of them silently losing an edit.
	async fn resolve_overwritten(
		&self,
		pair: PairId,
		record: &PairRecord,
		rel_path: &str,
		held: &BaselineEntry,
		resolution: ConflictResolution,
	) -> Result<(), Error> {
		let buried_uuid = held.remote_uuid.ok_or_else(|| {
			Error::custom(
				ErrorKind::InvalidState,
				format!("the conflict held at {rel_path:?} names no buried version"),
			)
		})?;
		let local_root = Path::new(&record.local_root);
		match resolution {
			ConflictResolution::KeepLocal => {}
			ConflictResolution::KeepBoth => {
				let buried = self.client.get_file(buried_uuid).await?;
				let (aside, path) = apply::aside_target(local_root, rel_path)?;
				self.client
					.download_file_to_path(&buried, &path, None)
					.await?;
				tracing::debug!(
					"resolve_conflict[pair {pair}]: kept the buried version of {rel_path:?} as {aside:?}"
				);
			}
			ConflictResolution::KeepRemote => {
				// The head by LINEAGE, never the version chain's first entry: the chain is sorted by
				// each version's original upload time, to the second, so the two uploads of the race
				// that made this row tie and their order in the listing is arbitrary — half the time
				// its first entry would be the buried version itself, and the restore below would be
				// skipped as a no-op while the local copy went to the bin.
				let lineage = held.remote_stable_uuid.ok_or_else(|| {
					Error::custom(
						ErrorKind::InvalidState,
						format!("the conflict held at {rel_path:?} names no file to restore into"),
					)
				})?;
				let mut head = self.client.get_file_by_stable_uuid(lineage).await?;
				if head.uuid() != buried_uuid {
					let version = self
						.client
						.list_file_versions(&head)
						.await?
						.into_iter()
						.find(|version| version.uuid() == buried_uuid)
						.ok_or_else(|| {
							Error::custom(
								ErrorKind::InvalidState,
								format!(
									"the version buried at {rel_path:?} is no longer restorable"
								),
							)
						})?;
					self.client.restore_file_version(&mut head, version).await?;
				}
				// Our own copy goes to the bin rather than under the download: the restored version
				// is what this resolution asked for, and the bytes it replaces are not lost.
				apply::quarantine_local(local_root, rel_path)?;
				tracing::debug!(
					"resolve_conflict[pair {pair}]: restored the version buried at {rel_path:?} and quarantined the local copy"
				);
			}
		}
		// Either way the row has said all it has to say: dropping it lets the next pass reconcile
		// the path from what the two sides now actually hold.
		self.store
			.lock()
			.await
			.delete_entry(pair, rel_path)
			.map_err(|e| db_error(e, "resolving a conflict"))
	}

	/// Retire the confirmation gap this engine's own pushes leave, from the evidence a snapshot
	/// listing our version at its path ([`plan::confirm_agreed_content`]) cannot supply.
	///
	/// Two sources, cheapest first:
	/// - the cache's announcements. A version of ours that stood as the remote head for
	///   [`CONFIRM_TENURE`] before anything superseded it is content both sides held, whether or
	///   not a pass ever happened to run while it was the head. This is what confirms the backlog a
	///   PAUSED pair accumulated: no pass runs, so nothing else ever could.
	/// - the server's version chain, for a row the announcements say nothing about (this engine was
	///   restarted, or the announcement never arrived) whose path now carries a foreign version of
	///   the same file. The two versions' timestamps say how long ours stood. One lookup per such
	///   row, and only for a row that is actually about to be reconciled against a foreign version;
	///   a lookup that fails leaves the row unconfirmed, exactly as before.
	///
	/// Returns the rows that moved, for the caller to persist — and to retire their push records
	/// with ([`Observations::forget_pushes`]) once it has. Reading a verdict does not consume it:
	/// `prepare` runs for [`plan_pair`](Self::plan_pair) too, which writes nothing.
	///
	/// `raw_remote` must be the RAW snapshot: a view with this engine's own writes folded in shows
	/// our version at its own path and would confirm every push against itself.
	async fn confirm_pushes(
		&self,
		baseline: &mut HashMap<String, BaselineEntry>,
		raw_remote: &HashMap<String, RemoteNode>,
		files: &[CacheableFile<'static>],
	) -> Vec<BaselineEntry> {
		let (mut confirmed, ask_server) =
			observed_confirmations(&self.observed, baseline, raw_remote, Instant::now());

		for (rel_path, ours, foreign) in ask_server {
			let Some(cacheable) = files.iter().find(|f| f.uuid == foreign) else {
				continue;
			};
			let file = crate::io::RemoteFile::from(cacheable.clone());
			let versions = match self.client.list_file_versions(&file).await {
				Ok(versions) => versions,
				Err(error) => {
					// Offline, or the endpoint refused: the row stays unconfirmed, which is the
					// safe direction — the pass surfaces a conflict rather than burying our bytes.
					tracing::debug!(
						"sync_once: could not read the version chain of {rel_path:?} to date this engine's push — {error}"
					);
					continue;
				}
			};
			let chain: Vec<(Uuid, chrono::DateTime<Utc>)> = versions
				.iter()
				.map(|version| (version.uuid(), version.timestamp()))
				.collect();
			match plan::version_chain_verdict(&chain, ours, foreign, CONFIRM_TENURE) {
				Some(true) => {
					tracing::debug!(
						"sync_once: the server's version chain confirms this engine's push of {rel_path:?}"
					);
					confirmed.insert(ours);
				}
				Some(false) => tracing::debug!(
					"sync_once: {rel_path:?} — a foreign version landed on this engine's push inside the confirmation window"
				),
				None => tracing::debug!(
					"sync_once: {rel_path:?} — the version chain does not carry this engine's push, so it stays unconfirmed"
				),
			}
		}

		plan::confirm_agreed_pushes(baseline, &confirmed)
	}

	/// Advance what the cache's announcements have confirmed since the last pass, and persist it.
	///
	/// The announcement half of [`confirm_pushes`](Self::confirm_pushes) alone: with no snapshot
	/// there is no foreign version to date a push against, so nothing here talks to the server.
	/// A pass does this itself; this is for the stretches where none runs.
	async fn sweep_confirmations(&self, pair: PairId) -> Result<(), Error> {
		let mut baseline: HashMap<String, BaselineEntry> = {
			let store = self.store.lock().await;
			store
				.entries(pair)
				.map_err(|e| db_error(e, "loading the baseline"))?
				.into_iter()
				.map(|entry| (entry.rel_path.clone(), entry))
				.collect()
		};
		let advanced = self
			.confirm_pushes(&mut baseline, &HashMap::new(), &[])
			.await;
		if advanced.is_empty() {
			return Ok(());
		}
		{
			let store = self.store.lock().await;
			for entry in &advanced {
				store
					.upsert_entry(pair, entry)
					.map_err(|e| db_error(e, "recording the confirmed agreed content"))?;
			}
		}
		self.forget_settled_pushes(&advanced);
		Ok(())
	}

	/// Retire the push records of rows this caller has just PERSISTED as agreed — the ones a
	/// snapshot listed at their own path as well as the ones a tenure dated — plus any record no
	/// pass will ever read again (see [`PUSH_RECORD_TTL`]).
	///
	/// Only a caller that wrote the rows may do this. The record is the only evidence a push was
	/// ever confirmed, and dropping it without recording what it proved leaves the next pass
	/// reading the row as an unconfirmed push again — with a foreign edit on top, a false conflict.
	fn forget_settled_pushes(&self, persisted: &[BaselineEntry]) {
		let decided: Vec<Uuid> = persisted
			.iter()
			.filter_map(|entry| entry.remote_uuid)
			.collect();
		self.observed.forget_pushes(&decided, Instant::now());
	}

	/// Run the read-only half: load the baseline, scan, enumerate the remote, build the view.
	async fn prepare(&self, pair: PairId) -> Result<Prepared, Error> {
		let (record, baseline_entries, failures) = {
			let store = self.store.lock().await;
			let record = store
				.pair(pair)
				.map_err(|e| db_error(e, "loading the sync pair"))?
				.ok_or_else(|| Error::custom(ErrorKind::InvalidState, "unknown sync pair"))?;
			let entries = store
				.entries(pair)
				.map_err(|e| db_error(e, "loading the baseline"))?;
			let failures = store
				.failures(pair)
				.map_err(|e| db_error(e, "loading the per-path failure counts"))?;
			(record, entries, failures)
		};

		let mut baseline_map: HashMap<String, BaselineEntry> = baseline_entries
			.into_iter()
			.map(|entry| (entry.rel_path.clone(), entry))
			.collect();

		// Copied BEFORE the snapshot is read: an event the cache commits afterwards describes a
		// state this snapshot predates, so it must not retire a pending write this pass.
		let observed = self.observed.snapshot();
		let snapshot = self
			.client
			.enumerate_sync_root_snapshot(record.remote_root)
			.await?;
		let mut remote_view =
			plan::build_remote_view(record.remote_root, &snapshot.dirs, &snapshot.files);

		// Read the snapshot BEFORE the local scan, so the confirmation below runs against the RAW
		// view — before this engine's own writes are folded into it, and before the baseline is
		// shared (immutably) with the scan. A row the snapshot confirms is one both sides
		// demonstrably hold, which is what a later foreign edit is measured against.
		let mut confirmed = plan::confirm_agreed_content(&mut baseline_map, &remote_view.nodes);
		confirmed.extend(
			self.confirm_pushes(&mut baseline_map, &remote_view.nodes, &snapshot.files)
				.await,
		);
		let baseline: Arc<HashMap<String, BaselineEntry>> = Arc::new(baseline_map);

		let local_root = PathBuf::from(&record.local_root);
		let scan_baseline = Arc::clone(&baseline);
		let local_scan =
			tokio::task::spawn_blocking(move || scan::scan_local(&local_root, &scan_baseline))
				.await
				.map_err(|e| {
					Error::custom(ErrorKind::Internal, format!("local scan panicked: {e}"))
				})?;

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
			failures,
			confirmed,
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
	///
	/// A watch running on the pair STOPS: its loop has nothing left to sync, and every pass it tried
	/// would fail against a pair that is gone. The loop finishes whatever pass is in flight, then
	/// ends and publishes a [`PairRemoved`](super::WatchState::PairRemoved) status; its
	/// [`WatchHandle`](super::WatchHandle) stays valid, so a caller holding one can still drop or
	/// stop it. That last pass may still be writing when this returns — its baseline and journal
	/// writes name a pair id this retires for good, so they fail rather than land in a pair added
	/// afterwards.
	pub async fn remove_pair(&self, pair: PairId) -> Result<(), Error> {
		self.approvals.lock().await.remove(&pair);
		// Dropping the handle unsubscribes the pair's cache notifications.
		self.roots.lock().await.remove(&pair);
		// The pair's journal rows go with it (`ON DELETE CASCADE`); drop the in-memory copies too,
		// so nothing of the removed pair is left to be consulted or re-persisted.
		self.pending.forget_pair(pair);
		let store = self.store.lock().await;
		// Under the store lock, which is also the lock a watch reads the pair and subscribes under
		// (see `watchable_pair`): a watch setting up right now either registered before this and
		// gets tripped here, or reads the deleted row afterwards and is refused. Before the delete,
		// so a loop sitting between two passes learns about the removal at once rather than starting
		// one more against the rows this is about to take away.
		if let Some(signal) = self.removals.lock().await.remove(&pair) {
			let _ = signal.send(true);
		}
		store
			.delete_pair(pair)
			.map_err(|e| db_error(e, "removing a sync pair"))?;
		// Under the same lock as the delete, and as `set_control`'s own write: a pause that landed
		// in between would leave the map holding an id whose row is gone. A pass PARKED on this
		// pair's pause is cut loose rather than left waiting for a resume that can no longer be
		// asked for — the pair it would sync is gone.
		if let Some(control) = self.paused.lock().await.remove(&pair) {
			cancel_suspension(&control);
		}
		Ok(())
	}

	/// Pause `pair`: its watch loop stops running passes, and [`sync_once`](Self::sync_once)
	/// returns a report marked [`paused`](SyncReport::paused) without scanning, planning or
	/// touching either side. Changes made meanwhile are not lost — the watch loop leaves its
	/// change signal pending, so the first pass after [`resume_pair`](Self::resume_pair) catches
	/// up on everything at once.
	///
	/// A pass that is ALREADY RUNNING is suspended: it parks before its next action and resumes
	/// exactly where it stopped, giving up its transfers only if the pause outlives
	/// [`DEFAULT_CANCEL_AFTER`](super::DEFAULT_CANCEL_AFTER) —
	/// [`pause_pair_with`](Self::pause_pair_with) is where that choice lives, and this is
	/// [`PauseOptions::default`].
	///
	/// A pause does NOT tear the pair down: its filesystem watcher and its cache sync-root
	/// subscription stay registered. Dropping the subscription would untrack the root in the cache,
	/// and re-registering it would then relist the whole root under the drive lock — a resync
	/// nobody asked for as the price of resuming.
	///
	/// The flag is persisted with the pair, so an engine reopened on the same baseline DB comes
	/// back paused (a suspended PASS is process-local: a restart has nothing to resume, and nothing
	/// was half-written). Pausing an already-paused pair re-applies the options; an unknown pair is
	/// an error. A pair that is already paused cannot be WATCHED, either — [`watch`](Self::watch)
	/// refuses it rather than hand out a handle whose loop does nothing.
	pub async fn pause_pair(&self, pair: PairId) -> Result<(), Error> {
		self.pause_pair_with(pair, PauseOptions::default()).await
	}

	/// [`pause_pair`](Self::pause_pair), choosing what happens to the pass already in flight.
	///
	/// * [`PauseMode::Suspend`](super::PauseMode::Suspend) parks that pass between actions and
	///   keeps it alive: a resume continues the same pass, with the same plan, the same drive lock
	///   and everything it had already done. A transfer already running finishes — the native
	///   upload/download paths poll no pause signal of their own, so there is no mid-transfer park
	///   to take, and the parking happens at the next action boundary.
	/// * [`PauseMode::Cancel`](super::PauseMode::Cancel) drops the transfers in flight, skips the
	///   rest of the plan, and lets the pass return a report whose
	///   [`interrupted`](SyncReport::interrupted) counts what it did not do. An interrupted action
	///   records nothing: an interrupted download removes its temp file and puts back whatever it
	///   had stashed out of its way, an interrupted upload
	///   normally never reaches the `upload/done` that would make it a file, and a transfer that DID
	///   finish is never dropped between that and the baseline row recording it. The next pass
	///   re-plans the remainder — including an upload the server committed as it was dropped, which
	///   it re-uploads over the same name.
	///
	/// [`cancel_after`](PauseOptions::cancel_after) is what converts the first into the second, so a
	/// UI can offer one button: a short pause resumes its transfers where they were, a long one
	/// gives them up rather than sit on the drive-write lock. It is counted FROM THIS CALL — the
	/// deadline is stamped here and shared by every action of the pass, so a countdown shown next to
	/// the button is the one the pass keeps, whether it parks at once or is still finishing a 500 MB
	/// upload when the window runs out. Re-issuing the pause with new options restarts it. `None`
	/// never escalates — the pass then parks until [`resume_pair`](Self::resume_pair) or
	/// [`cancel_paused_actions`](Self::cancel_paused_actions), holding the drive lock throughout,
	/// and a [`sync_once`](Self::sync_once) call waiting on that pass waits with it.
	pub async fn pause_pair_with(&self, pair: PairId, options: PauseOptions) -> Result<(), Error> {
		self.set_control(pair, PassControl::pausing(options)).await
	}

	/// Turn a [`suspended`](super::PauseMode::Suspend) pause into a
	/// [`cancelling`](super::PauseMode::Cancel) one: the pass parked on this pair unwinds now
	/// instead of waiting out its [`cancel_after`](PauseOptions::cancel_after) window. The pair
	/// STAYS paused.
	///
	/// A pair that is not paused has no pass to unwind and is left alone (pause it first if that is
	/// what you meant); an unknown pair is an error.
	pub async fn cancel_paused_actions(&self, pair: PairId) -> Result<(), Error> {
		cancel_suspension(&self.pair_control(pair).await?);
		Ok(())
	}

	/// [`cancel_paused_actions`](Self::cancel_paused_actions) for a caller inside the engine, on a
	/// pair it need not have checked first: it cuts loose a pass parked on a suspension without
	/// touching the persisted paused flag. A watch being stopped uses it, so a loop whose pass is
	/// parked ends instead of waiting for a resume that whoever stopped it is not going to send.
	pub(super) async fn cancel_suspended_pass(&self, pair: PairId) {
		if let Some(control) = self.paused.lock().await.get(&pair) {
			cancel_suspension(control);
		}
	}

	/// Resume a [`paused`](Self::pause_pair) pair. Nothing is re-listed and no baseline is
	/// invalidated: whatever changed on either side while the pair was paused is reconciled by the
	/// next pass exactly as if it had changed a moment ago — and a pass SUSPENDED mid-way carries
	/// on from where it parked. Resuming a running pair is a no-op; an unknown pair is an error.
	///
	/// A pause is also the one stretch where no pass runs to confirm the pushes made just before
	/// it, so the resume sweeps those (see [`CONFIRM_TENURE`]) — a `plan_pair` between the resume
	/// and the next pass then reads the same agreed content the pass will.
	pub async fn resume_pair(&self, pair: PairId) -> Result<(), Error> {
		self.set_control(pair, PassControl::Run).await?;
		self.sweep_confirmations(pair).await
	}

	/// Whether `pair` is currently [`paused`](Self::pause_pair) — in either flavour. An unknown
	/// pair reads as not paused (it has no state to be paused in).
	pub async fn is_paused(&self, pair: PairId) -> bool {
		self.paused
			.lock()
			.await
			.get(&pair)
			.is_some_and(|control| control.borrow().is_paused())
	}

	/// The checkpoint an apply pass of `pair` parks on, cloned per pass (see the pause module).
	async fn pass_gate(&self, pair: PairId) -> PassGate {
		PassGate::new(self.control_channel(pair).await)
	}

	/// `pair`'s control channel, created (running) if this is the first time anyone asked.
	async fn control_channel(&self, pair: PairId) -> tokio::sync::watch::Sender<PassControl> {
		self.paused
			.lock()
			.await
			.entry(pair)
			.or_insert_with(|| tokio::sync::watch::channel(PassControl::Run).0)
			.clone()
	}

	/// `pair`'s control channel, refusing a pair the registry does not know.
	async fn pair_control(
		&self,
		pair: PairId,
	) -> Result<tokio::sync::watch::Sender<PassControl>, Error> {
		let store = self.store.lock().await;
		if store
			.pair(pair)
			.map_err(|e| db_error(e, "loading the sync pair"))?
			.is_none()
		{
			return Err(Error::custom(ErrorKind::InvalidState, "unknown sync pair"));
		}
		drop(store);
		Ok(self.control_channel(pair).await)
	}

	/// The pair a watch is about to start on, together with the signal that ends its loop.
	///
	/// Both come out of ONE store-lock acquisition, and [`remove_pair`](Self::remove_pair) trips the
	/// signal under that same lock, so a removal racing a watch's setup has exactly two outcomes:
	/// either it deleted the row first and this reports an unknown pair, or it finds the
	/// subscription already registered and trips it. Subscribing afterwards — the setup does real
	/// work, a cache registration and a filesystem watcher, in between — would let a removal fall
	/// into the gap and go unheard, leaving the loop retrying a pair that is gone.
	pub(super) async fn watchable_pair(
		&self,
		pair: PairId,
	) -> Result<(PairRecord, tokio::sync::watch::Receiver<bool>), Error> {
		let store = self.store.lock().await;
		let record = store
			.pair(pair)
			.map_err(|e| db_error(e, "loading pair"))?
			.ok_or_else(|| Error::custom(ErrorKind::InvalidState, "unknown sync pair"))?;
		if record.paused {
			return Err(Error::custom(
				ErrorKind::InvalidState,
				format!("sync pair {pair} is paused: resume it before starting a watch on it"),
			));
		}
		let removed = self.removal_signal(pair).await;
		drop(store);
		Ok((record, removed))
	}

	/// A signal for `pair`'s watch loop to stop on: it flips to `true` when the pair is removed (see
	/// [`remove_pair`](Self::remove_pair)). Watches on the same pair share one signal.
	pub(super) async fn removal_signal(&self, pair: PairId) -> tokio::sync::watch::Receiver<bool> {
		self.removals
			.lock()
			.await
			.entry(pair)
			.or_insert_with(|| tokio::sync::watch::channel(false).0)
			.subscribe()
	}

	async fn set_control(&self, pair: PairId, control: PassControl) -> Result<(), Error> {
		// The persisted flag FIRST: an in-memory pause the DB never learned about would silently
		// un-pause on the next open, which is the one direction that loses data protection. Both
		// writes happen under the store lock, so a concurrent `remove_pair` cannot land between
		// them and leave the map holding a pair it has already deleted.
		let store = self.store.lock().await;
		let known = store
			.set_paused(pair, control.is_paused())
			.map_err(|e| db_error(e, "persisting a sync pair's paused flag"))?;
		if !known {
			return Err(Error::custom(ErrorKind::InvalidState, "unknown sync pair"));
		}
		self.control_channel(pair).await.send_replace(control);
		Ok(())
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

	/// Record what this pass's attempted paths did: a failure extends that path's streak, a success
	/// ends it. Once a streak reaches [`MAX_PATH_FAILURES`] the path stops being planned and is
	/// reported as [`UnsyncableReason::RepeatedFailure`] instead of failing on every pass forever.
	async fn note_path_outcomes(
		&self,
		pair: PairId,
		attempted: &[String],
		report: &mut SyncReport,
	) {
		if attempted.is_empty() {
			return;
		}
		let failed: HashMap<&str, &str> = report
			.failed_paths
			.iter()
			.map(|(path, error)| (path.as_str(), error.as_str()))
			.collect();
		let store = self.store.lock().await;
		let mut problems = Vec::new();
		for (path, error) in &failed {
			if let Err(e) = store.record_failure(pair, path, error) {
				problems.push(format!("{path}: recording the failure count failed: {e}"));
			}
		}
		// A pass cut short by a cancel cannot tell which of its actions ran, and an action that
		// never ran proves nothing about the path: clearing its streak would hand a permanently
		// broken path a fresh set of retries every time someone pauses. Only the failures count.
		if report.interrupted == 0 {
			for path in attempted
				.iter()
				.filter(|p| !failed.contains_key(p.as_str()))
			{
				if let Err(e) = store.clear_failure(pair, path) {
					problems.push(format!("{path}: clearing the failure count failed: {e}"));
				}
			}
		}
		report.errors.extend(problems);
	}

	/// Plan `rel_path` again on the next pass, whatever its failure history: it clears the
	/// consecutive-failure count the engine stopped planning it on (see
	/// [`UnsyncableReason::RepeatedFailure`]).
	///
	/// Idempotent — a path with no failure streak is left alone rather than erroring, so a caller
	/// can retry a whole reported list without checking each entry first. Errors only if the pair
	/// is unknown.
	pub async fn retry_path(&self, pair: PairId, rel_path: &str) -> Result<(), Error> {
		let store = self.store.lock().await;
		if store
			.pair(pair)
			.map_err(|e| db_error(e, "loading the sync pair"))?
			.is_none()
		{
			return Err(Error::custom(ErrorKind::InvalidState, "unknown sync pair"));
		}
		store
			.clear_failure(pair, rel_path)
			.map_err(|e| db_error(e, "clearing a path's failure count"))
	}

	/// Run one full sync pass: plan, screen, and apply against the remote and local tree. A
	/// [`paused`](Self::pause_pair) pair returns a report marked
	/// [`paused`](SyncReport::paused) instead, having read neither side.
	///
	/// A pause that lands WHILE this runs reaches into the pass (see
	/// [`pause_pair_with`](Self::pause_pair_with)): a suspension parks it — this call keeps waiting,
	/// so whoever wants it back must resume the pair or cancel its actions — and a cancel returns
	/// the pass's report early, with [`interrupted`](SyncReport::interrupted) counting what it did
	/// not do.
	pub async fn sync_once(&self, pair: PairId) -> Result<SyncReport, Error> {
		self.sync_once_observed(pair, &mut |_| {}).await
	}

	/// Like [`sync_once`](Self::sync_once), but reports live progress: `observer` is invoked with
	/// each [`SyncEvent`] as the pass plans and applies its actions (see [`SyncEvent`] for the
	/// event order). The observer is called synchronously between async steps, so keep it quick.
	///
	/// A [`paused`](Self::pause_pair) pair emits NO events at all — there was no pass to report on
	/// — and returns a report marked [`paused`](SyncReport::paused).
	pub async fn sync_once_observed(
		&self,
		pair: PairId,
		observer: &mut (dyn FnMut(SyncEvent) + Send),
	) -> Result<SyncReport, Error> {
		if self.is_paused(pair).await {
			tracing::debug!("sync_once[pair {pair}]: paused — neither side was read");
			return Ok(SyncReport {
				paused: true,
				..SyncReport::default()
			});
		}

		let prep = self.prepare(pair).await?;
		// Persist what this pass confirmed. Only a real pass writes it: `plan_pair` stays a pure
		// read, so a dry run inside the confirmation window just leaves it for the next pass — and
		// leaves the evidence with it, which is why the records are retired HERE and not in the
		// reading step.
		if !prep.confirmed.is_empty() {
			let store = self.store.lock().await;
			for entry in &prep.confirmed {
				store
					.upsert_entry(pair, entry)
					.map_err(|e| db_error(e, "recording the confirmed agreed content"))?;
			}
		}
		self.forget_settled_pushes(&prep.confirmed);
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
			// A row held because THIS engine's push buried a concurrent edit is the one kind the
			// current view cannot describe: the remote head is our own upload, and the version the
			// row is holding for the caller only exists in the file's history. Re-recording it from
			// the view would put our own copy on both sides of the conflict and lose the only
			// reference to the buried one.
			if prep
				.baseline
				.get(rel_path)
				.is_some_and(|row| row.state == BaselineState::Overwritten)
			{
				observer(SyncEvent::Conflict {
					rel_path: rel_path.clone(),
				});
				continue;
			}
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
		// Pausing the pair from here on reaches INTO this pass (see `pause_pair_with`).
		let gate = self.pass_gate(pair).await;
		let ctx = ApplyContext {
			client: &self.client,
			local_root: &local_root,
			pair,
			mode: prep.record.mode,
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
			gate: &gate,
		};
		let attempted: Vec<String> = decision
			.safe
			.iter()
			.map(|action| action.rel_path().to_string())
			.collect();
		apply::apply(ctx, decision.safe, &mut report, observer).await;
		self.note_path_outcomes(pair, &attempted, &mut report).await;
		tracing::debug!(
			"sync_once[pair {pair}]: done — {} uploaded, {} downloaded, {} remote dir(s), {} local dir(s), {} trashed, {} locally deleted, {} moved remote, {} moved local, {} conflict(s), {} held, {} deferred, {} interrupted, {} error(s)",
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
			report.interrupted,
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

/// Why a [`Backlog::AdoptDestination`] switch must not act on the read it just made — `None` when
/// it can. Decided purely (no I/O) so the policy is unit-testable.
///
/// The bar is the pass's own: a 1:1 path mapping (no name collision on either side) plus
/// [`absence_trusted`](guard::ScreenState::absence_trusted), since the adoption is entirely a
/// judgement about which paths one side no longer has.
fn adoption_refusal(refused: Option<RefuseReason>, state: &guard::ScreenState) -> Option<String> {
	if let Some(reason) = refused {
		return Some(reason.to_string());
	}
	if state.absence_trusted() {
		return None;
	}
	Some(
		if !state.scan_complete {
			"the local scan is incomplete"
		} else if !state.remote_converged {
			"the remote view has never converged"
		} else {
			"the remote view came back wholly empty"
		}
		.to_string(),
	)
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
	let actions = drop_blocked(plan.actions, &prep.blocked_paths());
	let (conflict_actions, executable): (Vec<_>, Vec<_>) = actions
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

/// Drop every action at a blocked path, and everything under it.
///
/// A path is blocked when the remote would reject its name, or when its failure streak ran out (and
/// stays blocked until a [`retry_path`](SyncEngine::retry_path) or a rename clears it). The SUBTREE
/// goes with it either way: a name the remote refuses can hold no remote children, and the failures
/// that get this far are structural — a directory that cannot be created can hold no children, a
/// local tree that cannot be written to cannot take a file — so planning the descendants would just
/// start the same streak one level down.
///
/// Dropping the destination half of a MOVE takes the deletions that would strand its source with it
/// (see below): an unrelocatable item must not be deleted from the side that still holds it.
fn drop_blocked(actions: Vec<SyncAction>, blocked: &BTreeSet<String>) -> Vec<SyncAction> {
	if blocked.is_empty() {
		return actions;
	}
	// The sources of the moves this dropped: their content is staying exactly where it is.
	let mut stranded: Vec<String> = Vec::new();
	let kept: Vec<SyncAction> = actions
		.into_iter()
		.filter(|action| {
			let path = action.rel_path();
			if !blocked.contains(path) && !blocked.iter().any(|root| plan::is_under(path, root)) {
				return true;
			}
			tracing::debug!(
				"reconcile: skipping {} — its path is blocked (a name the remote rejects, or \
				 {MAX_PATH_FAILURES} consecutive failures)",
				action.describe()
			);
			if let SyncAction::MoveRemote { from_path, .. }
			| SyncAction::MoveLocal { from_path, .. } = action
			{
				stranded.push(from_path.clone());
			}
			false
		})
		.collect();
	if stranded.is_empty() {
		return kept;
	}
	// A move the pass could not make leaves its source in place — and the source reads as "gone" on
	// the other side, which is what planned the deletion in the first place. Deleting it now (or
	// deleting the directory above it, which is recursive) would destroy the only copy of content
	// this pass just refused to relocate. Renaming a synced item into a name the remote rejects is
	// exactly that shape.
	kept.into_iter()
		.filter(|action| {
			let path = action.rel_path();
			let strands = action.is_delete()
				&& stranded
					.iter()
					.any(|from| from == path || plan::is_under(from, path));
			if strands {
				tracing::debug!(
					"reconcile: skipping {} — it still holds content a blocked move could not relocate",
					action.describe()
				);
			}
			!strands
		})
		.collect()
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

fn overlap_error(overlap: PairOverlap) -> Error {
	Error::custom_with_source(
		ErrorKind::InvalidState,
		overlap,
		Some("registering a sync pair whose roots overlap an existing one".to_string()),
	)
}

fn db_error(error: rusqlite::Error, context: &str) -> Error {
	Error::custom_with_source(ErrorKind::Internal, error, Some(context.to_string()))
}

#[cfg(test)]
mod tests {
	use std::collections::HashSet;

	use base64::{Engine as _, prelude::BASE64_STANDARD};
	use filen_types::{crypto::Blake3Hash, fs::StableUuid};
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

	fn upload(rel: &str) -> SyncAction {
		SyncAction::UploadFile {
			rel_path: rel.to_string(),
		}
	}

	#[test]
	fn a_blocked_path_takes_its_subtree_but_not_its_lookalikes_out_of_the_plan() {
		let actions = vec![
			upload("ok.txt"),
			upload("bad"),
			upload("bad/inner.txt"),
			upload("bad/deep/x.txt"),
			// A sibling whose name merely STARTS with the blocked one: a different path entirely.
			upload("badly.txt"),
			upload("badge/y.txt"),
		];
		let blocked = BTreeSet::from(["bad".to_string()]);
		let kept: Vec<String> = drop_blocked(actions.clone(), &blocked)
			.iter()
			.map(|a| a.rel_path().to_string())
			.collect();
		assert_eq!(kept, vec!["ok.txt", "badly.txt", "badge/y.txt"]);

		// Nothing blocked -> the plan is untouched.
		assert_eq!(drop_blocked(actions.clone(), &BTreeSet::new()), actions);
	}

	/// A mode switch that adopts the destination's standing copies rewrites baseline rows for every
	/// path it reads as gone on the source side, so it may only run on evidence a pass would act on.
	/// Untrusted, the switch is worse than useless: it adopts nothing and then propagates the whole
	/// backlog the caller asked to keep, or adopts a whole pair from whichever side a transient fault
	/// left listing something.
	#[test]
	fn adopting_the_destination_is_refused_on_evidence_a_pass_would_not_act_on() {
		let healthy = guard::ScreenState {
			scan_complete: true,
			remote_converged: true,
			remote_emptied: false,
			first_sync: false,
			tracked: 5,
		};
		assert_eq!(adoption_refusal(None, &healthy), None);

		for (state, expected) in [
			(
				guard::ScreenState {
					scan_complete: false,
					..healthy
				},
				"the local scan is incomplete",
			),
			(
				guard::ScreenState {
					remote_converged: false,
					..healthy
				},
				"the remote view has never converged",
			),
			(
				guard::ScreenState {
					remote_emptied: true,
					..healthy
				},
				"the remote view came back wholly empty",
			),
		] {
			assert_eq!(adoption_refusal(None, &state).as_deref(), Some(expected));
		}

		// A name collision makes a 1:1 mapping impossible, so it refuses the switch for the same
		// reason it refuses a pass — even with everything else healthy.
		assert_eq!(
			adoption_refusal(Some(RefuseReason::LocalCollision), &healthy).as_deref(),
			Some(RefuseReason::LocalCollision.to_string().as_str())
		);
	}

	fn scan_root(tag: &str) -> PathBuf {
		let root = std::env::temp_dir().join(format!("filen_engine_{tag}_{}", Uuid::new_v4()));
		std::fs::create_dir_all(&root).unwrap();
		root
	}

	/// Reconcile a two-way pass over a real local scan, screened exactly as a pass screens it.
	fn planned_over_scan(
		root: &Path,
		baseline: &HashMap<String, BaselineEntry>,
		remote: &HashMap<String, RemoteNode>,
	) -> Vec<SyncAction> {
		let scan = scan::scan_local(root, &HashMap::new());
		let plan = plan::reconcile(
			SyncMode::TwoWay,
			baseline,
			&scan.nodes,
			remote,
			&plan::PassHolds::default(),
		);
		drop_blocked(plan.actions, &scan.invalid_names.keys().cloned().collect())
	}

	fn synced_file(rel: &str, uuid: Uuid, hash: Blake3Hash, size: u64) -> BaselineEntry {
		BaselineEntry {
			remote_uuid: Some(uuid),
			content_hash: Some(hash),
			size: Some(size),
			local_mtime: Some(0),
			..synced_shell(rel)
		}
	}

	fn remote_file(rel: &str, uuid: Uuid, hash: Blake3Hash, size: u64) -> RemoteNode {
		RemoteNode {
			rel_path: rel.to_string(),
			kind: NodeKind::File,
			remote_uuid: uuid,
			stable_uuid: Some(StableUuid::new_for_test(uuid)),
			content_hash: Some(hash),
			size,
			modified_millis: 0,
		}
	}

	/// Renaming an already-SYNCED file to a name the remote would reject must not read as a local
	/// deletion. Nothing can be pushed under the new name — but the copy the remote already holds
	/// is the user's data, and a rename is not a request to destroy it.
	#[test]
	fn renaming_a_synced_file_into_a_rejected_name_leaves_the_remote_copy_alone() {
		let root = scan_root("rename_file");
		// `report.txt` was synced; the user has just renamed it to the reserved device name `CON`.
		std::fs::write(root.join("CON"), b"payload").unwrap();
		let hash: Blake3Hash = blake3::hash(b"payload").into();
		let uuid = Uuid::new_v4();

		let actions = planned_over_scan(
			&root,
			&HashMap::from([(
				"report.txt".to_string(),
				synced_file("report.txt", uuid, hash, 7),
			)]),
			&HashMap::from([(
				"report.txt".to_string(),
				remote_file("report.txt", uuid, hash, 7),
			)]),
		);
		assert!(
			actions.is_empty(),
			"a rename into a rejected name plans nothing at all — and above all no deletion of the \
			 remote copy: {actions:?}"
		);

		std::fs::remove_dir_all(&root).ok();
	}

	/// The same for a whole synced DIRECTORY: a remote directory deletion is recursive, so reading
	/// the rename as a deletion would take the entire remote subtree with it.
	#[test]
	fn renaming_a_synced_dir_into_a_rejected_name_leaves_its_remote_subtree_alone() {
		let root = scan_root("rename_dir");
		// `docs/` was synced; the user has just renamed it to `bad.` (a trailing dot the remote
		// rejects), carrying its contents along.
		std::fs::create_dir_all(root.join("bad.")).unwrap();
		std::fs::write(root.join("bad.").join("a.txt"), b"payload").unwrap();
		let hash: Blake3Hash = blake3::hash(b"payload").into();
		let (dir_uuid, file_uuid) = (Uuid::new_v4(), Uuid::new_v4());

		let actions = planned_over_scan(
			&root,
			&HashMap::from([
				(
					"docs".to_string(),
					BaselineEntry {
						kind: NodeKind::Dir,
						remote_uuid: Some(dir_uuid),
						..synced_shell("docs")
					},
				),
				(
					"docs/a.txt".to_string(),
					synced_file("docs/a.txt", file_uuid, hash, 7),
				),
			]),
			&HashMap::from([
				(
					"docs".to_string(),
					RemoteNode {
						rel_path: "docs".to_string(),
						kind: NodeKind::Dir,
						remote_uuid: dir_uuid,
						stable_uuid: None,
						content_hash: None,
						size: 0,
						modified_millis: 0,
					},
				),
				(
					"docs/a.txt".to_string(),
					remote_file("docs/a.txt", file_uuid, hash, 7),
				),
			]),
		);
		assert!(
			!actions.iter().any(SyncAction::is_delete),
			"the remote subtree of a locally-renamed directory must survive: {actions:?}"
		);

		std::fs::remove_dir_all(&root).ok();
	}

	/// The streak only blocks once it reaches the threshold, and reports the attempts and the last
	/// error it saw. Below the threshold the path is still planned, so a transient failure is
	/// retried rather than parked.
	#[test]
	fn a_failure_streak_blocks_only_at_the_threshold() {
		let mut failures = HashMap::new();
		for attempts in 0..MAX_PATH_FAILURES {
			failures.insert("flaky.txt".to_string(), (attempts, "boom".to_string()));
			let blocked = blocked_from(&failures);
			assert!(
				blocked.is_empty(),
				"{attempts} failure(s) is under the threshold, so the path is still planned"
			);
		}
		failures.insert(
			"flaky.txt".to_string(),
			(MAX_PATH_FAILURES, "last words".to_string()),
		);
		assert_eq!(
			blocked_from(&failures),
			BTreeSet::from(["flaky.txt".to_string()])
		);
	}

	/// `exhausted_paths`/`unsyncable` read the same map; this exercises the threshold rule without
	/// building a whole `Prepared`.
	fn blocked_from(failures: &HashMap<String, (u32, String)>) -> BTreeSet<String> {
		failures
			.iter()
			.filter(|(_, (attempts, _))| *attempts >= MAX_PATH_FAILURES)
			.map(|(rel_path, _)| rel_path.clone())
			.collect()
	}

	#[test]
	fn local_roots_overlap_only_when_one_actually_contains_the_other() {
		let existing = Path::new("/sync/data");
		assert!(matches!(
			local_overlap(Path::new("/sync/data"), existing, 1),
			Some(PairOverlap::LocalRootInUse { pair: 1, .. })
		));
		assert!(matches!(
			local_overlap(Path::new("/sync/data/sub/deep"), existing, 1),
			Some(PairOverlap::LocalRootNested { pair: 1, .. })
		));
		assert!(matches!(
			local_overlap(Path::new("/sync"), existing, 1),
			Some(PairOverlap::LocalRootContains { pair: 1, .. })
		));
		// A sibling is disjoint, and so is a name that merely SHARES a prefix with the root: the
		// comparison is per path component, not per byte.
		assert!(local_overlap(Path::new("/sync/other"), existing, 1).is_none());
		assert!(local_overlap(Path::new("/sync/database"), existing, 1).is_none());
		assert!(local_overlap(Path::new("/elsewhere"), existing, 1).is_none());
	}

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
			remote_stable_uuid: Some(StableUuid::new_for_test(uuid)),
			agreed_hash: None,
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
				stable_uuid: Some(StableUuid::new_for_test(uuid)),
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
		assert_eq!(
			entry.agreed_hash, entry.content_hash,
			"resolving records what the two sides now agree on"
		);

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
		assert_eq!(
			entry.agreed_hash, entry.content_hash,
			"resolving records what the two sides now agree on"
		);

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
		assert_eq!(
			entry.agreed_hash,
			Some(hash(3)),
			"the resolution settles on the row's content, so the pull below is not re-read as a \
			 concurrent edit and the path does not conflict forever"
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
		assert_eq!(
			entry.agreed_hash, None,
			"there is nothing agreed yet: the local copy has still to be pushed"
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
				stable_uuid: Some(StableUuid::new_for_test(uuid)),
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
			remote_stable_uuid: Some(StableUuid::new_for_test(uuid)),
			agreed_hash: None,
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
			let (pair, _) = store
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

	/// Watching a paused pair is refused rather than started idle: a handle whose loop runs nothing
	/// looks exactly like a broken watch, and the caller who paused the pair is the one who can say
	/// whether it should be running.
	#[tokio::test]
	async fn a_watch_on_a_paused_pair_is_refused() {
		let path =
			std::env::temp_dir().join(format!("filen_sync_watch_paused_{}.db", Uuid::new_v4()));
		let engine = Arc::new(
			SyncEngine::open(offline_client(), path.clone())
				.await
				.unwrap(),
		);
		let (pair, _) = engine
			.store
			.lock()
			.await
			.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		engine.pause_pair(pair).await.unwrap();

		let error = match Arc::clone(&engine).watch(pair).await {
			Ok(_) => panic!("a paused pair must not be watchable"),
			Err(error) => error.to_string(),
		};
		assert!(
			error.contains("paused") && error.contains("resume"),
			"the refusal must say what to do about it: {error}"
		);
		// An id nobody registered is still reported as unknown, not as paused.
		let unknown = match Arc::clone(&engine).watch(pair + 9_999).await {
			Ok(_) => panic!("an unknown pair must not be watchable"),
			Err(error) => error.to_string(),
		};
		assert!(unknown.contains("unknown sync pair"), "{unknown}");

		drop(engine);
		std::fs::remove_file(&path).ok();
	}

	/// A watch starting on a pair while a removal of that same pair is in flight. Registering the
	/// watch's stop signal after the existence check — with a cache registration and a filesystem
	/// watcher in between — would let the removal land in the gap: it would find no signal to trip,
	/// delete the row, and the watch would then subscribe to a fresh signal nobody will ever trip,
	/// leaving a loop that fails every pass against a pair that is gone and never stops.
	#[tokio::test]
	async fn a_removal_racing_a_watchs_setup_is_never_lost() {
		let path =
			std::env::temp_dir().join(format!("filen_sync_watch_race_{}.db", Uuid::new_v4()));
		let engine = Arc::new(
			SyncEngine::open(offline_client(), path.clone())
				.await
				.unwrap(),
		);
		let (pair, _) = engine
			.store
			.lock()
			.await
			.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();

		// The store lock fixes the interleaving, as in the pause/removal race above: both verbs
		// queue on it while this guard is held, and tokio hands the mutex out in request order, so
		// the watch's setup runs first and the removal second — the order in which the removal has
		// nothing registered to trip yet.
		let guard = engine.store.lock().await;
		let watching = tokio::spawn({
			let engine = Arc::clone(&engine);
			async move { engine.watchable_pair(pair).await }
		});
		tokio::task::yield_now().await;
		let removing = tokio::spawn({
			let engine = Arc::clone(&engine);
			async move { engine.remove_pair(pair).await }
		});
		tokio::task::yield_now().await;
		drop(guard);

		removing.await.unwrap().unwrap();
		// The watch either never starts (the removal won the lock) or starts holding a signal the
		// removal has already tripped. What must not happen is a live signal for a dead pair.
		if let Ok((_, removed)) = watching.await.unwrap() {
			assert!(
				*removed.borrow(),
				"the watch subscribed after the removal had already passed by: its loop would \
				 retry a pair that no longer exists forever"
			);
		}

		drop(engine);
		std::fs::remove_file(&path).ok();
	}

	/// A pause and a removal of the same pair, in flight at once from two callers of one engine. An
	/// id left behind in the paused set describes a pair that no longer exists: it makes
	/// [`SyncEngine::is_paused`] report a phantom, and the set grows for as long as the engine is
	/// open.
	#[tokio::test]
	async fn a_pause_racing_a_removal_leaves_no_paused_id_behind() {
		let path =
			std::env::temp_dir().join(format!("filen_sync_pause_race_{}.db", Uuid::new_v4()));
		let engine = Arc::new(
			SyncEngine::open(offline_client(), path.clone())
				.await
				.unwrap(),
		);
		let (pair, _) = engine
			.store
			.lock()
			.await
			.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();

		// The store lock fixes the interleaving: both verbs queue on it while this guard is held,
		// and tokio's mutex hands it out in request order, so the pause runs first and the removal
		// second — the order in which a pause that persisted its flag can land after the row it
		// describes is already gone.
		let guard = engine.store.lock().await;
		let pausing = tokio::spawn({
			let engine = Arc::clone(&engine);
			async move { engine.pause_pair(pair).await }
		});
		tokio::task::yield_now().await;
		let removing = tokio::spawn({
			let engine = Arc::clone(&engine);
			async move { engine.remove_pair(pair).await }
		});
		tokio::task::yield_now().await;
		drop(guard);

		// The pause itself may legitimately succeed or be refused, depending on which verb the
		// store lock served first; what must hold either way is that nothing stays paused.
		let _ = pausing.await.unwrap();
		removing.await.unwrap().unwrap();
		assert!(
			!engine.is_paused(pair).await,
			"the removed pair left its id in the paused set; the next pair sqlite gives that id \
			 would never sync"
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

		observations.note_uuids([uuid]);
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
		observations.note_uuids([uuid]);

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

		observations.note_uuids([uuid]);
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

		observations.note_uuids([uuid]);
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
					stable_uuid: None,
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
					stable_uuid: Some(StableUuid::new_for_test(child)),
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

	/// The other side of the same-round race, on the client that WON it. Its upload is the remote
	/// head, but the cache applied the version that upload superseded and has not announced ours
	/// yet — so the path shows a foreign uuid of our own lineage. Reading that as somebody else's
	/// write left the winner reconciling against the version it had already replaced: a foreign
	/// edit over an unconfirmed push, i.e. a persisted conflict on the client whose copy IS the
	/// head. Our write has to stand in the view instead.
	#[test]
	fn the_version_our_own_upload_superseded_does_not_read_as_a_foreign_write() {
		let observations = Observations::default();
		let pending = PendingWrites::default();
		let (base, ours, theirs) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
		pending.record(&observations, PAIR, ours, created("a.txt", Some(base)));

		// The row our upload left: our bytes under our uuid, the file's lineage, and the shared
		// base still the last content both sides were known to hold.
		let lineage = StableUuid::new_for_test(base);
		let mut baseline = written_row(ours, hash(2));
		let row = baseline.get_mut("a.txt").unwrap();
		row.remote_stable_uuid = Some(lineage);
		row.agreed_hash = Some(hash(0));

		// What the lagging cache shows: the other client's version of the SAME file, which landed
		// first and which our upload went on top of.
		let mut remote = node_at("a.txt", theirs);
		let node = remote.get_mut("a.txt").unwrap();
		node.stable_uuid = Some(lineage);
		node.content_hash = Some(hash(1));

		let holds = pending.settle(PAIR, &observations.snapshot(), &remote);
		assert_eq!(
			pending.fold_into(PAIR, &baseline, &mut remote),
			1,
			"the cache has announced nothing of ours, so what it shows is the version we replaced"
		);
		assert_eq!(remote["a.txt"].remote_uuid, ours);
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
			"the client holding the head must not conflict with the version it superseded"
		);
	}

	/// A third uuid at the path is not our write lagging: somebody else wrote there after us, and
	/// the reconciler has to see that immediately rather than wait out the grace window.
	#[test]
	fn a_foreign_uuid_at_the_path_is_never_painted_over() {
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
		observations.note_uuids([Uuid::new_v4(), Uuid::new_v4()]);

		let uuid = Uuid::new_v4();
		pending.record(&observations, PAIR, uuid, created("a.txt", None));
		observations.prune_before(pending.oldest_stamp());
		assert!(
			observations.snapshot().is_empty(),
			"observations older than every live write are unreachable"
		);

		observations.note_uuids([uuid]);
		pending.settle(PAIR, &observations.snapshot(), &HashMap::new());
		observations.prune_before(pending.oldest_stamp());
		assert!(
			observations.snapshot().is_empty(),
			"with no pending write left there is nothing to remember at all"
		);
	}

	/// One push of ours, announced `announced_ago` before "now" and superseded (if at all) that
	/// long after the announcement.
	fn tenure(announced_ago: Option<u64>, superseded_after: Option<u64>) -> (PushTenure, Instant) {
		// Built forwards from a real `Instant`, never backwards: subtracting from `Instant::now()`
		// underflows on a machine that has been up for less than the offset.
		let announced = Instant::now();
		let now = announced + Duration::from_secs(announced_ago.unwrap_or(0));
		(
			PushTenure {
				lineage: None,
				recorded: announced,
				announced: announced_ago.map(|_| announced),
				superseded: superseded_after.map(|after| announced + Duration::from_secs(after)),
			},
			now,
		)
	}

	#[test]
	fn a_push_that_stood_as_the_head_long_enough_is_confirmed() {
		let (push, now) = tenure(Some(40), None);
		assert_eq!(push.verdict(now), PushVerdict::Confirmed);
	}

	#[test]
	fn a_push_superseded_inside_the_window_is_not_confirmed() {
		let (push, now) = tenure(Some(40), Some(5));
		assert_eq!(
			push.verdict(now),
			PushVerdict::Unconfirmed,
			"another client was editing at the same time; its version is not an edit made after ours"
		);
	}

	#[test]
	fn a_push_superseded_after_the_window_is_still_confirmed() {
		let (push, now) = tenure(Some(40), Some(35));
		assert_eq!(
			push.verdict(now),
			PushVerdict::Confirmed,
			"our version stood for the full window before anything landed on it"
		);
	}

	#[test]
	fn a_push_that_is_still_ripening_is_not_confirmed_yet() {
		let (push, now) = tenure(Some(5), None);
		assert_eq!(push.verdict(now), PushVerdict::Unconfirmed);
	}

	#[test]
	fn a_push_the_cache_never_announced_has_no_verdict() {
		let (push, now) = tenure(None, None);
		assert_eq!(
			push.verdict(now),
			PushVerdict::Unknown,
			"with no observation the server's version chain is the only evidence left"
		);
	}

	#[test]
	fn watching_a_push_picks_up_an_announcement_that_beat_it() {
		let observations = Observations::default();
		let uuid = Uuid::new_v4();
		// The cache can commit our own upload's event before the upload call has returned.
		observations.note_uuids([uuid]);
		observations.watch_push(uuid, None, None);
		assert_eq!(
			observations.push_verdict(uuid, Instant::now() + Duration::from_secs(40)),
			PushVerdict::Confirmed,
			"the announcement that arrived first still dates the push"
		);
	}

	#[test]
	fn a_push_forgotten_after_a_pass_read_it_has_no_verdict_left() {
		let observations = Observations::default();
		let uuid = Uuid::new_v4();
		observations.note_uuids([uuid]);
		observations.watch_push(uuid, None, None);
		observations.forget_pushes(&[uuid], Instant::now());
		assert_eq!(
			observations.push_verdict(uuid, Instant::now() + Duration::from_secs(40)),
			PushVerdict::Unknown
		);
	}

	/// Reading the verdicts is what `plan_pair` does too, and it persists nothing. If the read
	/// retired the records, a dry run inside the confirmation window would leave the pass after it
	/// with an unconfirmed row and no evidence left to confirm it — a false conflict against an
	/// edit made long after ours.
	#[test]
	fn reading_the_announcements_leaves_them_for_the_pass_that_persists() {
		let observations = Observations::default();
		let uuid = Uuid::new_v4();
		observations.note_uuids([uuid]);
		observations.watch_push(uuid, None, None);
		let baseline = written_row(uuid, hash(1));
		let ripe = Instant::now() + Duration::from_secs(40);

		let read = || observed_confirmations(&observations, &baseline, &HashMap::new(), ripe).0;
		assert_eq!(read(), std::collections::HashSet::from([uuid]));
		assert_eq!(
			read(),
			std::collections::HashSet::from([uuid]),
			"a dry run that wrote nothing must not have consumed the evidence"
		);

		// The pass that persisted the row is the one that retires it.
		observations.forget_pushes(&[uuid], Instant::now());
		assert!(read().is_empty());
	}

	/// What the version chain cannot say: which versions came out of THIS engine. Two uploads a
	/// second apart carry the same server stamp, so without this the previous one reads as a
	/// stranger's edit that our push buried.
	#[test]
	fn a_version_this_engine_uploaded_is_remembered_as_its_own() {
		let observations = Observations::default();
		let (ours, theirs) = (Uuid::new_v4(), Uuid::new_v4());
		observations.watch_push(ours, None, None);

		assert!(observations.minted(ours));
		assert!(!observations.minted(theirs), "another client's version");

		// Confirming the push retires its tenure record, not the fact that we made it: the pass after
		// it still has to recognise the version as ours.
		observations.forget_pushes(&[ours], Instant::now());
		assert!(observations.minted(ours));

		// Past the window a pass reasons about, it goes.
		observations.forget_pushes(&[], Instant::now() + MINTED_TTL);
		assert!(!observations.minted(ours));
	}

	#[test]
	fn a_re_push_of_the_same_path_retires_the_version_it_replaced() {
		let observations = Observations::default();
		let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
		observations.note_uuids([first]);
		observations.watch_push(first, None, None);
		observations.watch_push(second, None, Some(first));
		assert_eq!(
			observations.push_verdict(first, Instant::now() + Duration::from_secs(40)),
			PushVerdict::Unknown,
			"the row moved on to the new version, so the old record answers for nothing"
		);
	}
}

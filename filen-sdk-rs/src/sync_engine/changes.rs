//! What changed on each side of a pair since its last pass — and why a pass has to read both
//! sides whole anyway.
//!
//! Two changelists per pair, both fed from notifications the engine already receives and until now
//! threw away: the filesystem watcher's events (`watch.rs`) and the cache's per-sync-root callback
//! (`SyncEngine::observe_pair`). A pass takes them with [`PairChanges::take`] BEFORE it reads
//! either side, and [`PassScope::full_pass_reason`] says whether what it took describes every
//! change since the last pass.
//!
//! A pass with no reason to read everything narrows itself to what these lists name
//! (`SyncEngine::prepare_scoped`); a pass with one reads both sides whole and records the reason on
//! its [`SyncReport`](super::SyncReport). Either way the events behind the lists are unrecoverable
//! — they exist only for the instant the callback runs, and the cache's `events` table is a DRAINED
//! queue (`commit_drain_batch` deletes every consumed row in the same transaction that advances the
//! watermark, and a resync applies its synthetics straight from RAM), so nothing can replay them
//! later.
//!
//! # What may and may not be expressed here
//!
//! A local changelist carries PATHS TO LOOK AT, never verdicts: there is no way to say "this is
//! gone" with a [`LocalDirty`] entry, so a lost event can only ever delay a deletion, never invent
//! one. On the remote side the same rule is a type: [`Gone`] has private fields and no public
//! constructor, so only [`RemoteChange::from_event`] — reading an actual cache removal — can mint
//! one. No other module can express remote absence.
//!
//! # Alignment with the pass that took the list
//!
//! [`take`](PairChanges::take) drains both lists before the pass reads the cache snapshot, so
//! everything it took is either already reflected in that snapshot (a duplicate, and every delta
//! entry is idempotent — an upsert of the same item, a `Gone` for an absent uuid) or not yet (and
//! the pass has it). What arrives after the take stays queued for the next pass, which is the same
//! argument `Observations::snapshot` makes for the announcement map. Replaying a superset is
//! harmless; replaying a SUBSET is the unsafe direction, and the take-before-read order rules it
//! out — which is also why no observation counter is needed here.
//!
//! # What an interrupted pass hands back
//!
//! A pass cut short part-way through its plan carried out only some of it, and the lists it
//! drained to make that plan are gone. Both halves come back: the paths the plan named, through
//! [`note_owed`](PairChanges::note_owed), and the announced changes it consumed, through
//! [`note_owed_remote`](PairChanges::note_owed_remote). BOTH — or the hand-back loses the very
//! action it exists for. The next pass derives the remote side of a locally-dirty path from that
//! path's baseline row, which still says the item is on the server, so an action owed from a
//! remote ABSENCE (the local delete answering a `Gone`, the local move answering a remote one)
//! would go unplanned until the safety net. Replaying the delta is what puts the absence back,
//! and replaying it a second time costs nothing, since every entry is idempotent.
//!
//! A pass that read both sides WHOLE has nothing to hand back that would do: its evidence was the
//! snapshot, which no changelist can restore, so it forces the next pass full
//! ([`FullPassReason::InterruptedPass`]).
//!
//! # Rule files
//!
//! A `.filenignore` is not an ordinary path: its CONTENTS decide what is hidden for its whole
//! directory and everything under it, on both sides. One path cannot describe that, so a change to
//! one collapses its side of the list to a whole-tree read
//! ([`FullPassReason::RulesChanged`]) rather than being recorded as a path to look at.
//!
//! Two shapes cannot be recognized from the notification alone, and are the CONSUMING pass's to
//! check: a removal (a cache removal event names a uuid and no name at all) and a rename or move
//! AWAY from the rule file's name (the event carries the new name, not the old one). The pass holds
//! the baseline, which is where that uuid's path is: `RemoteObservation::touch` puts both the path
//! an item left and the one it arrived at through `rule_file_dir`, so either shape lands in
//! `rule_dirs` and `prepare_scoped` answers it with [`FullPassReason::RulesChanged`].
//!
//! # Bounds
//!
//! Both lists are bounded (see [`dirty_cap`]) and collapse to a full pass past it: a `git checkout`
//! of half the tree costs a full pass, which is what it would cost anyway.

use std::{
	collections::BTreeSet,
	fmt, mem,
	path::Path,
	sync::{Mutex, MutexGuard, PoisonError},
};

use filen_types::crypto::Blake3Hash;
use notify::{
	Event, EventKind,
	event::{AccessKind, AccessMode},
};
use uuid::Uuid;

use super::{ignore::FILENIGNORE, scan::normalize_rel_path};
use crate::cache::{CacheEvent, CacheEventType, DirEvent, FileEvent, GlobalEvent, RemoteItem};

/// The most entries a changelist holds before it collapses to a full pass, whatever the tree size.
/// Chosen so the list itself can never be the pass's memory problem.
const DIRTY_CAP: usize = 50_000;

/// The FSEvents `info` string marking the watch root itself renamed or removed (`notify` 8.2.0's
/// `fsevent.rs`, `StreamFlags::ROOT_CHANGED`). It is part of that backend's interface.
const ROOT_CHANGED_INFO: &str = "root changed";

/// How many changes a tree of `last_items` tracked items justifies collecting before a whole-tree
/// read is the cheaper answer: a quarter of the tree, capped at [`DIRTY_CAP`]. At least one, so a
/// pair whose size is not known yet — or is tiny, where a full pass costs nothing — still narrows
/// a single change down instead of collapsing on the first event.
fn dirty_cap(last_items: usize) -> usize {
	(last_items / 4).clamp(1, DIRTY_CAP)
}

/// Why a pass read both sides whole instead of only what changed.
///
/// Every variant is a row of one table, decided in one place ([`PassScope::full_pass_reason`]) and
/// reported on the pass's [`SyncReport::full_pass`](super::SyncReport::full_pass), so a caller —
/// and a test — can see why a pass cost what it cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FullPassReason {
	/// The pair's first pass of this process (after `open`, `add_pair` or `resume_pair`): there is
	/// no changelist yet, and a paused pair's events may have overflowed while it waited.
	FirstPass,
	/// The baseline is empty, so this is a first sync: every deletion is held by the guard, and the
	/// whole tree is the evidence for it.
	EmptyBaseline,
	/// The kernel dropped events (FSEvents `MustScanSubDirs`, inotify `IN_Q_OVERFLOW`): what the
	/// watcher missed is unknown, so nothing may be narrowed.
	KernelDropped,
	/// The watch root itself was renamed or removed under the watcher.
	RootChanged,
	/// A local event named a path that cannot be keyed against the scan — outside the watch root,
	/// or not a plain UTF-8 path.
	UnmappedLocalPath,
	/// More local paths changed than [`dirty_cap`] allows.
	LocalOverflow,
	/// More remote changes arrived than [`dirty_cap`] allows.
	RemoteOverflow,
	/// The account was wiped (`GlobalEvent::DeleteAll`).
	RemoteWiped,
	/// Applying the announced changes left the derived remote view empty while the baseline still
	/// records remote items. An emptied remote is the shape a backend fault takes, and the guard
	/// weighs it against a whole-tree read (`remote_emptied`), never against a derivation.
	RemoteEmptied,
	/// An announced change named something the derived view cannot place: an item whose ancestry
	/// the cache does not know, a tracked item renamed to a name no local path can hold, or a
	/// directory displaced at its own path. Deriving a path for it would be a guess, and guessing
	/// on this side ends in a deletion.
	RemoteUnplaceable,
	/// The filesystem watcher does not see every local change, for the life of the watch: it could
	/// not start, could not cover the whole tree, or reported an error. Permanent, so every pass
	/// stays full.
	LocalEventsDegraded,
	/// The cache refused this pair's sync-root subscription, so no remote change is announced.
	/// Permanent, like [`LocalEventsDegraded`](Self::LocalEventsDegraded).
	RemoteEventsDegraded,
	/// No filesystem watcher is attached to this pair, so nothing is recording what changes
	/// locally: a pair only ever passed over by [`sync_once`](super::SyncEngine::sync_once), one
	/// whose watch has not started yet, and one whose watch has ended. Unlike
	/// [`LocalEventsDegraded`](Self::LocalEventsDegraded) it is not permanent — a watcher that
	/// starts and covers the whole tree clears it.
	LocalEventsUnwatched,
	/// The periodic safety-net tick — the backstop for anything both changelists silently missed.
	SafetyNet,
	/// The previous pass refused to run (a name collision on either side), which is whole-tree
	/// state.
	PreviousRefusal,
	/// The previous pass held deletions back.
	///
	/// Three of the holds want evidence only a whole-tree read supplies: an incomplete scan, an
	/// unconverged remote, or a remote that listed nothing at all. The other two — a volume
	/// threshold, a first sync against a populated destination — want something else: the approval
	/// a caller gives names ONE exact batch by its token, so the pass that offers that batch again
	/// has to reproduce it exactly, and the absences it was built from were observations the
	/// holding pass consumed. Until a carry-over set carries those paths, reading everything is
	/// the only way to reproduce them.
	DeletionHold,
	/// The previous pass was cut short while reading both sides WHOLE, so what it did not get to
	/// is evidence no changelist holds.
	///
	/// A change-scoped pass cut short is NOT this: it planned from its two lists and hands both of
	/// them back on its way out (see the module docs), so the next pass re-plans what it missed
	/// from the same evidence. A cancel that landed before there was a plan at all still is —
	/// there was nothing to hand back, and the lists went with the pass.
	InterruptedPass,
	/// The device-wide ignore patterns or the pair's mode changed: what is hidden, or what an
	/// action MEANS, changed for every path.
	RulesChanged,
	/// The previous pass planned work and did not apply it: it could not take the drive-write lock,
	/// a side filled up, a record of what it had already done did not land, or an action failed.
	/// Making that plan consumed both changelists, so the paths it named are in no list any more —
	/// only a whole-tree read finds them again.
	UnappliedWork,
	/// A conflict was resolved
	/// ([`resolve_conflict`](super::SyncEngine::resolve_conflict)): the baseline row at that path
	/// is re-anchored so the winning side reads as the changed one, which is a change neither a
	/// filesystem event nor a cache announcement carries.
	ConflictResolved,
	/// A change-scoped pass could not read one of the paths it set out to re-observe. What that
	/// path would have said is unknown, and the changelist that named it is drained, so the next
	/// pass reads everything rather than leaving it to the safety net.
	IncompleteObservation,
	/// A change-scoped pass's assembled local map did not account for the rows and observations it
	/// was built from (see `engine::assembly_bounds`). Deriving again from the same rows would
	/// reproduce the miscount, so the next pass reads both sides.
	AssemblyMismatch,
}

impl fmt::Display for FullPassReason {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(match self {
			Self::FirstPass => "the pair's first pass, so there is no changelist yet",
			Self::EmptyBaseline => "the baseline is empty, so this is a first sync",
			Self::KernelDropped => "the kernel dropped filesystem events",
			Self::RootChanged => "the watch root itself was renamed or removed",
			Self::UnmappedLocalPath => "a filesystem event named a path that cannot be keyed",
			Self::LocalOverflow => "too many local paths changed to track individually",
			Self::RemoteOverflow => "too many remote changes arrived to track individually",
			Self::RemoteWiped => "the account was wiped",
			Self::RemoteEmptied => "the announced changes emptied the remote view",
			Self::RemoteUnplaceable => "an announced remote change could not be placed at a path",
			Self::LocalEventsDegraded => "the filesystem watcher does not see every local change",
			Self::RemoteEventsDegraded => "no remote changes are announced for this pair",
			Self::LocalEventsUnwatched => "no filesystem watcher is attached to this pair",
			Self::SafetyNet => "the periodic safety-net pass",
			Self::PreviousRefusal => "the previous pass refused to run",
			Self::DeletionHold => "the previous pass held deletions for whole-tree evidence",
			Self::InterruptedPass => "the previous pass was cancelled part-way through",
			Self::RulesChanged => "the ignore patterns or the pair's mode changed",
			Self::UnappliedWork => "the previous pass planned work it did not apply",
			Self::ConflictResolved => "a conflict resolution has to be applied at its path",
			Self::IncompleteObservation => {
				"the previous pass could not re-observe every path it narrowed its read to"
			}
			Self::AssemblyMismatch => {
				"the previous pass's derived maps did not account for what they were built from"
			}
		})
	}
}

/// What one filesystem event means for the local changelist. Pure, so the whole table is testable
/// without a watcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalEventScope {
	/// Nothing can be narrowed down any more; the reason says what was lost.
	Collapse(FullPassReason),
	/// Re-observe the event's paths.
	Paths,
	/// Noise: a read, an open, a close-after-read. Nothing changed.
	Ignore,
}

/// What one filesystem event says, by itself.
///
/// `need_rescan` and the root-changed notice come first: both mean the events around them cannot be
/// trusted. Everything that mutates — create, remove, data/metadata/name modify, and the catch-all
/// kinds every backend falls back to — names paths to re-observe. Only non-mutating access is
/// dropped, except inotify's `CLOSE_WRITE`, which is how a finished write announces itself.
fn local_event_scope(event: &Event) -> LocalEventScope {
	if event.need_rescan() {
		return LocalEventScope::Collapse(FullPassReason::KernelDropped);
	}
	if event.info() == Some(ROOT_CHANGED_INFO) {
		return LocalEventScope::Collapse(FullPassReason::RootChanged);
	}
	match event.kind {
		EventKind::Access(AccessKind::Close(AccessMode::Write)) => LocalEventScope::Paths,
		EventKind::Access(_) => LocalEventScope::Ignore,
		EventKind::Any
		| EventKind::Create(_)
		| EventKind::Modify(_)
		| EventKind::Remove(_)
		| EventKind::Other => LocalEventScope::Paths,
	}
}

/// Whether a root-relative key names a rule file — the one file whose own change rewrites what is
/// hidden for its whole directory and below (see the module docs).
fn is_rule_file(rel: &str) -> bool {
	rel.rsplit('/').next() == Some(FILENIGNORE)
}

/// The paths a pass has to re-observe locally, or the reason it cannot narrow them down.
///
/// Paths are root-relative, NFC, `/`-joined — the keys [`scan_local`](super::scan::scan_local)
/// produces — so a path recorded here names the same node the scan and the baseline do. There is
/// deliberately nothing here to express "absent": a path is something to LOOK at.
#[derive(Debug, Default)]
struct LocalDirty {
	paths: BTreeSet<String>,
	/// Sticky once set: the paths are dropped and further events are not collected until a pass
	/// takes the list.
	full: Option<FullPassReason>,
}

impl LocalDirty {
	fn collapse(&mut self, reason: FullPassReason) {
		if self.full.is_none() {
			self.full = Some(reason);
		}
		// The paths cannot narrow anything down any more, and holding them would only cost memory
		// until the pass that ignores them.
		self.paths.clear();
	}
}

/// The remote changes the cache announced for a pair since its last pass, in dispatch order.
#[derive(Debug, Default)]
struct RemoteDelta {
	entries: Vec<RemoteDeltaEntry>,
	full: Option<FullPassReason>,
}

impl RemoteDelta {
	fn collapse(&mut self, reason: FullPassReason) {
		if self.full.is_none() {
			self.full = Some(reason);
		}
		self.entries.clear();
	}
}

/// One announced remote change, as dispatched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RemoteDeltaEntry {
	/// The event's `drive_message_id`; `None` for a resync synthetic, which carries none. The
	/// cache's snapshot watermark is the contiguous prefix of REAL ids only
	/// (`SubtreeSnapshot::watermark`), so a synthetic can never be aligned by id.
	///
	/// No pass reads it: both changelists are taken BEFORE either side is read, which aligns them
	/// without an id (see the module docs). It is kept for the id-based rebase a full pass will do
	/// against its own snapshot watermark, and needs no `dead_code` suppression to be kept — the
	/// derived impls and this module's own tests read it.
	pub(super) id: Option<u64>,
	pub(super) change: RemoteChange,
}

/// What one announced remote change does to the remote view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RemoteChange {
	/// The item is at this parent under this name with this content: a create, a move, or a content
	/// change. Exactly the projection the remote view is built from.
	Upsert(RemoteItem),
	/// A metadata patch: the new name, and for a file the content stamps that ride with it. Carries
	/// no parent — a metadata event names none, and the item did not move.
	Renamed {
		uuid: Uuid,
		name: String,
		/// `None` for a directory, whose metadata is only its name and creation stamp.
		content: Option<FileContent>,
	},
	/// The item is no longer in the tree.
	Gone(Gone),
}

/// The content stamps a file's metadata patch carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FileContent {
	pub(super) size: u64,
	pub(super) hash: Option<Blake3Hash>,
	pub(super) modified_millis: i64,
}

/// Evidence that a remote item is GONE — trashed, removed or archived.
///
/// Private fields and no constructor: only [`RemoteChange::from_event`], reading an actual cache
/// removal event, can mint one. That is the type-level half of the rule that a change-scoped pass
/// may MISS a change but must never fabricate an absence; the other half is that the local
/// changelist cannot express absence at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Gone {
	uuid: Uuid,
	/// The uuid this item was superseded by, where the removal names one: a versioning edit
	/// re-mints the same file under a new id, which is an identity update rather than a deletion.
	successor: Option<Uuid>,
}

impl Gone {
	pub(super) fn uuid(&self) -> Uuid {
		self.uuid
	}

	pub(super) fn successor(&self) -> Option<Uuid> {
		self.successor
	}
}

impl RemoteChange {
	/// Whether this change is to a rule file, whose contents decide what is hidden for its whole
	/// directory on both sides.
	///
	/// A [`Gone`] never is, and a rename AWAY from the name never is: a removal event carries a
	/// uuid and no name, and a rename carries the new name only. Both are the consuming pass's to
	/// check against the baseline (see the module docs) — this answers only what the event itself
	/// shows.
	fn touches_rule_file(&self) -> bool {
		match self {
			Self::Upsert(item) => item.name == FILENIGNORE,
			Self::Renamed { name, .. } => name == FILENIGNORE,
			Self::Gone(_) => false,
		}
	}

	/// What `event` changes about the remote tree: `None` for an event the sync engine reads
	/// nothing from (a favourite, a folder colour, the cache's own no-ops).
	///
	/// `Err(reason)` is the one shape no delta can describe — an account-wide wipe, which affects
	/// every path of every pair.
	fn from_event(event: &CacheEvent<'_>) -> Result<Option<Self>, FullPassReason> {
		let change = match &event.event {
			CacheEventType::File(file) => match file {
				FileEvent::New(f) | FileEvent::Move(f) | FileEvent::Changed(f) => {
					Some(Self::Upsert(RemoteItem {
						uuid: f.uuid,
						parent: f.parent,
						name: f.name.to_string(),
						stable_uuid: Some(f.stable_uuid),
						hash: f.hash,
						size: f.size,
						modified_millis: f.last_modified.timestamp_millis(),
					}))
				}
				FileEvent::MetadataChanged { uuid, meta } => Some(Self::Renamed {
					uuid: *uuid,
					name: meta.name.to_string(),
					content: Some(FileContent {
						size: meta.size,
						hash: meta.hash,
						modified_millis: meta.last_modified.timestamp_millis(),
					}),
				}),
				// A trash or archive of an EDITED file names the successor it was re-minted as; a
				// genuine removal names none. The distinction is that field, not a different kind
				// of evidence, so both arrive as one shape.
				FileEvent::Trashed { uuid, new_uuid, .. }
				| FileEvent::Archived { uuid, new_uuid, .. } => Some(Self::Gone(Gone {
					uuid: *uuid,
					successor: *new_uuid,
				})),
				FileEvent::Removed(uuid) => Some(Self::Gone(Gone {
					uuid: *uuid,
					successor: None,
				})),
			},
			CacheEventType::Dir(dir) => match dir {
				DirEvent::New(d) | DirEvent::Move(d) | DirEvent::Changed(d) => {
					Some(Self::Upsert(RemoteItem {
						uuid: d.uuid,
						parent: d.parent,
						name: d.name.to_string(),
						// A directory has no whole-life id and no content; the view tells the two
						// kinds apart by exactly that.
						stable_uuid: None,
						hash: None,
						size: 0,
						modified_millis: d.created.map_or(0, |at| at.timestamp_millis()),
					}))
				}
				DirEvent::MetadataChanged { uuid, meta } => Some(Self::Renamed {
					uuid: *uuid,
					name: meta.name.to_string(),
					content: None,
				}),
				DirEvent::Removed(uuid) => Some(Self::Gone(Gone {
					uuid: *uuid,
					successor: None,
				})),
				// Nothing the sync engine reads.
				DirEvent::ColorChanged { .. } => None,
			},
			CacheEventType::Global(GlobalEvent::DeleteAll) => {
				return Err(FullPassReason::RemoteWiped);
			}
			// Ignored exactly as the engine's own observation callback ignores them.
			CacheEventType::Global(GlobalEvent::TrashEmpty | GlobalEvent::DeleteVersioned)
			| CacheEventType::NoOp => None,
		};
		Ok(change)
	}
}

/// One pair's two changelists, plus the reasons its next pass must read everything.
///
/// Shared: the filesystem watcher's thread and the cache's worker thread both write to it, and a
/// pass takes from it. Everything under the lock is plain data and every method is a lock, a
/// compare and a push — both callbacks run on threads that must not be held up (`notify` delivers
/// on its own thread; the cache callback runs inline on the worker between commits).
#[derive(Debug, Default)]
pub(super) struct PairChanges(Mutex<ChangeState>);

#[derive(Debug, Default)]
struct ChangeState {
	local: LocalDirty,
	remote: RemoteDelta,
	/// Recorded by the engine for the NEXT pass — the previous pass's outcome, a rules change, the
	/// safety-net tick. Keeps the FIRST reason recorded (the one that made the pass mandatory) and
	/// is TAKEN by the next pass to start, in the same breath as the changelists
	/// ([`PairChanges::take`]).
	///
	/// Taken rather than cleared at the end, because a pass cannot honour a demand it never saw: a
	/// reason recorded while it was already reading (a `set_user_ignore` that commits mid-pass, a
	/// second pass forcing one while this one applies) belongs to the pass AFTER it. Clearing it at
	/// the end dropped exactly those. Nothing is lost by taking it early either — a pass that took
	/// a reason and then failed, or did not apply what it planned, forces one again on its way out
	/// (`next_pass_scope`).
	forced: Option<FullPassReason>,
	/// A trigger source that no longer reports everything. Never cleared: the coverage does not
	/// come back for the life of the watch.
	degraded: Option<FullPassReason>,
	/// Whether a filesystem watcher is currently recording local changes onto this list.
	///
	/// False until one starts and true only while it runs, because the local list is evidence only
	/// for as long as something is filling it: a pair passed over by `sync_once` alone, a pair
	/// whose watch has not started yet, and a pair whose watch has ended all have an EMPTY local
	/// list for the same reason a quiet tree does, and nothing in the list itself tells the two
	/// apart. So the answer is the watcher's presence, not the list's contents.
	local_covered: bool,
	/// Items the last full pass tracked — what [`dirty_cap`] scales with.
	last_items: usize,
	/// What the previous pass's plan left owing (`facts::carry_over`), unioned into the next pass's
	/// local set by [`take`](PairChanges::take).
	///
	/// Deliberately NOT part of the capped local list: [`dirty_cap`] scales with the tree as the
	/// pair last RECORDED it, which is 1 before a first sync, so feeding a plan's paths through the
	/// cap would collapse the next pass to a whole-tree read and report work as owing that was in
	/// fact applied. Replaced by every pass that made a plan, never accumulated: it is rebuilt from
	/// that pass's own plan.
	owed: BTreeSet<String>,
	/// The other half of [`owed`](Self::owed): the announced changes the previous pass consumed,
	/// handed back when it was cut short before it applied them (see the module docs).
	///
	/// Outside the tree-scaled cap for the same reason `owed` is, and REPLAYED IN FRONT of
	/// whatever arrived since, which is the order the cache dispatched them in. Bounded all the
	/// same — see [`note_owed_remote`](PairChanges::note_owed_remote).
	owed_remote: Vec<RemoteDeltaEntry>,
}

impl PairChanges {
	pub(super) fn new() -> Self {
		let changes = Self::default();
		// `notify`'s ReadDirectoryChangesW backend (8.2.0 `windows.rs`) sets no `Flag::Rescan` and
		// no info string on any event, so a dropped event there is indistinguishable from a quiet
		// tree and a local changelist can never be known to be complete. Treat the source as
		// degraded from the start rather than narrow a pass down on it.
		#[cfg(target_os = "windows")]
		changes.degrade(FullPassReason::LocalEventsDegraded);
		changes
	}

	/// Record what one filesystem event means. `is_staging` names the engine's own download temp
	/// files and quarantine bin, whose paths are dropped rather than re-observed — the rename that
	/// commits a download is an event on the destination, which is not staging and is collected.
	pub(super) fn note_local_event(
		&self,
		root: &Path,
		event: &Event,
		is_staging: impl Fn(&Path) -> bool,
	) {
		match local_event_scope(event) {
			LocalEventScope::Collapse(reason) => self.state().local.collapse(reason),
			LocalEventScope::Ignore => {}
			LocalEventScope::Paths => {
				let mut state = self.state();
				if state.local.full.is_some() {
					return;
				}
				let cap = dirty_cap(state.last_items);
				for path in event.paths.iter().filter(|path| !is_staging(path)) {
					// A path the scan could not key either: there is nothing to narrow to.
					let Some(rel) = relative_key(root, path) else {
						state.local.collapse(FullPassReason::UnmappedLocalPath);
						return;
					};
					if is_rule_file(&rel) {
						// Its contents hide (or stop hiding) everything below its directory, on
						// both sides: there is no path set that describes that.
						state.local.collapse(FullPassReason::RulesChanged);
						return;
					}
					state.local.paths.insert(rel);
					if state.local.paths.len() > cap {
						state.local.collapse(FullPassReason::LocalOverflow);
						return;
					}
				}
			}
		}
	}

	/// Record one committed cache batch's events for this pair, in dispatch order.
	pub(super) fn note_remote_batch(&self, events: &mut dyn Iterator<Item = &CacheEvent<'_>>) {
		let mut state = self.state();
		let cap = dirty_cap(state.last_items);
		for event in events {
			if state.remote.full.is_some() {
				return;
			}
			match RemoteChange::from_event(event) {
				Ok(None) => {}
				Ok(Some(change)) => {
					if change.touches_rule_file() {
						state.remote.collapse(FullPassReason::RulesChanged);
						return;
					}
					state.remote.entries.push(RemoteDeltaEntry {
						id: event.id,
						change,
					});
					if state.remote.entries.len() > cap {
						state.remote.collapse(FullPassReason::RemoteOverflow);
						return;
					}
				}
				Err(reason) => state.remote.collapse(reason),
			}
		}
	}

	/// Record that a filesystem watcher is now recording local changes here (it started and covers
	/// the whole tree), so a pass may narrow its local half down to what the list holds.
	pub(super) fn cover_local(&self) {
		self.state().local_covered = true;
	}

	/// Record that no watcher is recording local changes any more (a watch ended, its handle went
	/// away, or the pair was taken out from under it). What the list holds is still true; what it
	/// does NOT hold stops being evidence, so every pass reads the local side whole again.
	pub(super) fn uncover_local(&self) {
		self.state().local_covered = false;
	}

	/// The trigger source this pair has permanently lost, if any — for a caller that wants to SEE
	/// that its watch's events are not trusted (see `WatchStatus::local_events_degraded`).
	pub(super) fn degraded(&self) -> Option<FullPassReason> {
		self.state().degraded
	}

	/// Record that a trigger source no longer reports every change, for the life of the watch.
	pub(super) fn degrade(&self, reason: FullPassReason) {
		let mut state = self.state();
		if state.degraded.is_none() {
			state.degraded = Some(reason);
		}
	}

	/// Record what the pass that just ran leaves the next one owing (see
	/// [`carry_over`](super::facts::carry_over)). REPLACES the previous set rather than adding to
	/// it: each pass rebuilds it from its own plan, so a path drops out as soon as a pass gets
	/// through it.
	pub(super) fn note_owed(&self, owed: BTreeSet<String>) {
		self.state().owed = owed;
	}

	/// Record the announced changes a pass consumed and was cut short before it applied — the
	/// remote half of [`note_owed`](Self::note_owed), and the half without which the next pass
	/// derives those paths' remote side from rows that still say the item is there (see the module
	/// docs). REPLACES what the last pass left, exactly as the local half does.
	///
	/// Bounded like the lists themselves: a RUN of cut-short passes hands the same changes on
	/// again with whatever arrived between them, and a queue that only grows is the one thing a
	/// changelist may not become. Against the absolute [`DIRTY_CAP`] rather than the tree-scaled
	/// [`dirty_cap`] — one pass's delta is already under that, so this can only answer a run of
	/// them, and never the first sync whose cap sits at its floor.
	pub(super) fn note_owed_remote(&self, delta: Vec<RemoteDeltaEntry>) {
		let mut state = self.state();
		if delta.len() > DIRTY_CAP {
			state.remote.collapse(FullPassReason::RemoteOverflow);
			return;
		}
		state.owed_remote = delta;
	}

	/// Whether something has ALREADY made the next pass a whole-tree one — the same table
	/// [`take`](Self::take) reads, asked without taking anything.
	///
	/// A pass that narrowed its own read asks this before it trusts an absence: a reason recorded
	/// while it was reading (a kernel-dropped event, a watcher that stopped) says the list it
	/// narrowed itself with may be missing exactly what it is about to act on.
	pub(super) fn full_pending(&self) -> bool {
		let state = self.state();
		state.degraded.is_some()
			|| !state.local_covered
			|| state.local.full.is_some()
			|| state.remote.full.is_some()
			|| state.forced.is_some()
	}

	/// Record that the NEXT pass must read both sides whole. Keeps the first reason recorded.
	pub(super) fn force(&self, reason: FullPassReason) {
		let mut state = self.state();
		if state.forced.is_none() {
			state.forced = Some(reason);
		}
	}

	/// Take both changelists for a pass — called BEFORE it reads either side (see the module docs
	/// on alignment). What arrives afterwards belongs to the next pass.
	pub(super) fn take(&self) -> PassScope {
		let mut state = self.state();
		let local = mem::take(&mut state.local);
		let remote = mem::take(&mut state.remote);
		// The previous pass's unfinished business: owed whatever the changelists hold, and not
		// subject to their cap (see `ChangeState::owed`).
		let owed = mem::take(&mut state.owed);
		let owed_remote = mem::take(&mut state.owed_remote);
		// Taken with the lists, so what is recorded from here on is the NEXT pass's (see `forced`).
		let forced = mem::take(&mut state.forced);
		// Most specific first: a permanently degraded source, then no watcher at all, then evidence
		// that events were lost, then what the engine itself recorded about the previous pass.
		let full = state
			.degraded
			.or((!state.local_covered).then_some(FullPassReason::LocalEventsUnwatched))
			.or(local.full)
			.or(remote.full)
			.or(forced);
		let mut paths = local.paths;
		paths.extend(owed);
		// Replayed FIRST: they were dispatched before anything that arrived while the pass ran,
		// and the delta is applied in dispatch order.
		let entries = if owed_remote.is_empty() {
			remote.entries
		} else {
			let mut entries = owed_remote;
			entries.extend(remote.entries);
			entries
		};
		PassScope {
			local: paths,
			remote: entries,
			full,
		}
	}

	/// Record what a pass that read the whole tree found there: `items` is what the changelist caps
	/// scale with from now on.
	///
	/// It clears nothing. The reason that pass ran full was taken when it took the lists, and a
	/// collapse or a forced reason that landed DURING it belongs to the next pass — that pass did
	/// not cover it.
	pub(super) fn note_tree_size(&self, items: usize) {
		self.state().last_items = items;
	}

	fn state(&self) -> MutexGuard<'_, ChangeState> {
		// Plain data behind the lock: a panic while holding it cannot leave the state inconsistent.
		self.0.lock().unwrap_or_else(PoisonError::into_inner)
	}
}

/// What one pass took from the changelists.
#[derive(Debug)]
pub(super) struct PassScope {
	local: BTreeSet<String>,
	remote: Vec<RemoteDeltaEntry>,
	full: Option<FullPassReason>,
}

impl PassScope {
	/// Why this pass must read both sides whole, or `None` when what it took describes every change
	/// since the last pass.
	///
	/// The whole trigger table in one place: the changelists' own reasons (fixed at
	/// [`PairChanges::take`]), then the one fact only the pass knows — an empty baseline is a first
	/// sync, and the whole tree is the evidence for it.
	pub(super) fn full_pass_reason(&self, baseline_items: usize) -> Option<FullPassReason> {
		self.full
			.or((baseline_items == 0).then_some(FullPassReason::EmptyBaseline))
	}

	/// The local paths this pass took, moved out — the input to
	/// [`from_baseline`](super::derive::from_baseline), which owns the dirty set from there on and
	/// grows it with the rows it cannot carry. Read [`sizes`](Self::sizes) first: what is taken
	/// here is gone from the scope.
	pub(super) fn take_local(&mut self) -> BTreeSet<String> {
		mem::take(&mut self.local)
	}

	/// The remote changes this pass took, in dispatch order, moved out — the input to
	/// [`observe_remote`](super::remote::observe_remote), which applies them to the derived view.
	/// Moved for the same reason as [`take_local`](Self::take_local): the applier runs on a
	/// blocking thread and owns what it is given.
	pub(super) fn take_remote(&mut self) -> Vec<RemoteDeltaEntry> {
		mem::take(&mut self.remote)
	}

	/// Whether this pass has nothing to look at: nothing announced on either side, nothing owed by
	/// the last plan, and no reason forcing a whole read.
	///
	/// The one row of the trigger table it cannot see is the empty baseline, which is why a caller
	/// that acts on this is deciding whether to READ the pair at all — a pair with an empty
	/// baseline and nothing announced has nothing to do either.
	pub(super) fn is_idle(&self) -> bool {
		self.full.is_none() && self.local.is_empty() && self.remote.is_empty()
	}

	/// How many local paths and remote changes this pass took, for its log line.
	pub(super) fn sizes(&self) -> (usize, usize) {
		(self.local.len(), self.remote.len())
	}
}

/// `path` as the key the local scan would give it: root-relative, NFC, `/`-joined. `None` for a
/// path outside `root`, or one the scan could not key either (a non-UTF-8 or non-plain component).
/// The root itself is `""`.
fn relative_key(root: &Path, path: &Path) -> Option<String> {
	normalize_rel_path(path.strip_prefix(root).ok()?)
}

#[cfg(test)]
pub(super) mod tests {
	use std::{
		borrow::Cow,
		ffi::OsStr,
		path::{Path, PathBuf},
	};

	use chrono::{DateTime, Utc};
	use filen_types::{api::v3::dir::color::DirColor, auth::FileEncryptionVersion, fs::StableUuid};
	use notify::event::{
		CreateKind, DataChange, Flag, MetadataKind, ModifyKind, RemoveKind, RenameMode,
	};

	use super::{FullPassReason as R, *};
	use crate::{
		crypto::file::FileKey,
		fs::{
			dir::{DecryptedDirectoryMeta, cache::CacheableDir},
			file::{cache::CacheableFile, meta::DecryptedFileMeta},
		},
	};

	const ROOT: &str = "/sync/root";

	fn fs_event(kind: EventKind, paths: &[&str]) -> Event {
		Event {
			kind,
			paths: paths.iter().map(PathBuf::from).collect(),
			attrs: Default::default(),
		}
	}

	/// A kernel-dropped notice: pathless, and flagged for a rescan.
	fn rescan() -> Event {
		let mut event = fs_event(EventKind::Other, &[]);
		event.attrs.set_flag(Flag::Rescan);
		event
	}

	/// The FSEvents notice that the watch root itself was renamed or removed.
	fn root_changed() -> Event {
		let mut event = fs_event(
			EventKind::Modify(ModifyKind::Name(RenameMode::From)),
			&[ROOT],
		);
		event.attrs.set_info(ROOT_CHANGED_INFO);
		event
	}

	/// A WATCHED pair whose changelists are sized for a tree big enough not to collapse.
	fn sized_pair() -> PairChanges {
		let changes = PairChanges::new();
		changes.cover_local();
		changes.note_tree_size(4_000);
		changes
	}

	fn note(changes: &PairChanges, event: &Event) {
		changes.note_local_event(Path::new(ROOT), event, |_| false);
	}

	fn create(name: &str) -> Event {
		fs_event(
			EventKind::Create(CreateKind::File),
			&[&format!("{ROOT}/{name}")],
		)
	}

	/// The carry-over set is not a changelist entry, and the cap must not touch it: [`dirty_cap`]
	/// scales with the tree as the pair last RECORDED it, which is nothing before a first sync, so
	/// a plan of any size would otherwise collapse the next pass to a whole-tree read and report
	/// work as owing that the pass had in fact applied.
	#[test]
	fn the_carry_over_set_joins_the_next_pass_outside_the_cap() {
		let changes = PairChanges::new();
		changes.cover_local();
		// Nothing recorded yet, so `dirty_cap` sits at its floor of one path.
		let owed: BTreeSet<String> = (0..50).map(|i| format!("owed{i:02}.txt")).collect();
		changes.note_owed(owed.clone());

		let (paths, full) = taken(&changes);
		assert_eq!(
			paths.len(),
			owed.len(),
			"the cap swallowed the carry-over set"
		);
		assert_eq!(
			full, None,
			"paths the last plan owes are something to look at, not a reason to read everything"
		);
		// Taken with the lists, like everything else a pass consumes.
		assert_eq!(taken(&changes), (Vec::<String>::new(), None));
	}

	/// A pass rebuilds the set from its own plan, so recording one REPLACES the last.
	#[test]
	fn each_pass_replaces_what_the_last_one_left_owing() {
		let changes = sized_pair();
		changes.note_owed(BTreeSet::from(["first.txt".to_string()]));
		changes.note_owed(BTreeSet::from(["second.txt".to_string()]));
		assert_eq!(taken(&changes).0, vec!["second.txt".to_string()]);
	}

	/// A removal of `uuid`, as the cache announces one.
	fn removed(uuid: u128) -> CacheEvent<'static> {
		cache_event(
			Some(u64::try_from(uuid).expect("a test uuid fits an event id")),
			CacheEventType::File(FileEvent::Removed(Uuid::from_u128(uuid))),
		)
	}

	/// The delta entries a pass takes for those removals — minted the one way a `Gone` can be.
	fn minted_delta(uuids: &[u128]) -> Vec<RemoteDeltaEntry> {
		let changes = sized_pair();
		let events: Vec<CacheEvent<'static>> = uuids.iter().copied().map(removed).collect();
		changes.note_remote_batch(&mut events.iter());
		changes.take().take_remote()
	}

	/// An interrupted pass hands BOTH lists back, and its announced changes lead the ones that
	/// arrived while it ran — the order the cache dispatched them in, which is the order the next
	/// pass applies them to its derived view.
	#[test]
	fn an_interrupted_passs_announced_changes_come_back_ahead_of_the_newer_ones() {
		let handed = minted_delta(&[100, 101]);
		let newer = [removed(102)];
		let changes = sized_pair();
		changes.note_owed(BTreeSet::from(["gone.txt".to_string()]));
		changes.note_owed_remote(handed.clone());
		changes.note_remote_batch(&mut newer.iter());

		let mut scope = changes.take();
		assert_eq!(
			scope.full_pass_reason(10),
			None,
			"a hand-back is something to look at, not a reason to read everything"
		);
		assert_eq!(
			scope.take_remote(),
			[handed, minted_delta(&[102])].concat(),
			"the replayed changes must lead the ones that arrived while the pass ran"
		);
		assert_eq!(
			scope.take_local(),
			BTreeSet::from(["gone.txt".to_string()]),
			"and the paths that pass's plan owed come with them"
		);
		// Taken with the lists: they belong to the pass that took them now.
		assert!(
			changes.take().take_remote().is_empty(),
			"a handed-back change was replayed to two passes"
		);
	}

	/// And the cap does not touch them, for the reason it does not touch the owed paths: the
	/// hand-back is the previous pass's own list, so collapsing on it would make an interrupted
	/// first sync — where `dirty_cap` sits at its floor — read everything for ever.
	#[test]
	fn the_handed_back_changes_join_the_next_pass_outside_the_cap() {
		let handed = minted_delta(&(200..250).collect::<Vec<u128>>());
		let changes = PairChanges::new();
		changes.cover_local();

		changes.note_owed_remote(handed.clone());
		let mut scope = changes.take();
		assert_eq!(
			scope.full_pass_reason(10),
			None,
			"the cap collapsed a pass onto its own hand-back"
		);
		assert_eq!(scope.take_remote().len(), handed.len());
	}

	/// A run of cut-short passes hands the same changes on with whatever arrived between them, so
	/// the hand-back is bounded like the lists are: past the cap it collapses to a whole read
	/// rather than growing a queue nothing drains.
	#[test]
	fn a_hand_back_past_the_cap_collapses_to_a_whole_read() {
		let one = minted_delta(&[300]).remove(0);
		let changes = sized_pair();

		changes.note_owed_remote(vec![one; DIRTY_CAP + 1]);
		let scope = changes.take();
		assert_eq!(
			scope.full_pass_reason(10),
			Some(R::RemoteOverflow),
			"a hand-back past the cap must collapse, not queue"
		);
		assert!(
			scope.remote.is_empty(),
			"a collapsed list hands the pass nothing to narrow itself with"
		);
	}

	/// What a wake has to look at, and what the pass may therefore skip entirely.
	#[test]
	fn a_scope_with_nothing_announced_is_idle() {
		let changes = sized_pair();
		assert!(
			changes.take().is_idle(),
			"a quiet watched pair has nothing to look at"
		);

		note(&changes, &create("a.txt"));
		assert!(
			!changes.take().is_idle(),
			"a changed path is something to look at"
		);

		changes.note_owed(BTreeSet::from(["owed.txt".to_string()]));
		assert!(
			!changes.take().is_idle(),
			"a path the last plan left owing is something to look at"
		);

		changes.force(FullPassReason::SafetyNet);
		assert!(!changes.take().is_idle(), "a forced whole read is not idle");

		let unwatched = PairChanges::new();
		assert!(
			!unwatched.take().is_idle(),
			"with no watcher an empty list is no evidence, so there is always something to read"
		);
	}

	/// What a change-scoped pass asks before it trusts an absence: has anything already made the
	/// next pass a whole-tree one, which would say its own narrowed read may be missing something.
	#[test]
	fn a_pending_whole_read_is_visible_while_a_pass_is_still_reading() {
		let changes = sized_pair();
		assert!(!changes.full_pending());

		changes.force(FullPassReason::ConflictResolved);
		assert!(changes.full_pending());
		// Taken with the lists, so it belongs to the pass that took it and not to the one after.
		let _ = changes.take();
		assert!(!changes.full_pending());

		changes.uncover_local();
		assert!(
			changes.full_pending(),
			"with no watcher recording them, every pass reads the local side whole"
		);
	}

	/// The local paths a pass would take, and the reason it cannot narrow down.
	fn taken(changes: &PairChanges) -> (Vec<String>, Option<FullPassReason>) {
		let scope = changes.take();
		(scope.local.iter().cloned().collect(), scope.full)
	}

	pub(crate) fn dt(ms: i64) -> DateTime<Utc> {
		DateTime::from_timestamp_millis(ms).expect("a valid timestamp")
	}

	pub(crate) fn file_key() -> FileKey {
		FileKey::from_str_with_version(&"a".repeat(64), FileEncryptionVersion::V3)
			.expect("a valid key")
	}

	pub(crate) fn cacheable_file(uuid: Uuid, parent: Uuid, name: &str) -> CacheableFile<'static> {
		CacheableFile {
			uuid,
			stable_uuid: StableUuid::new_for_test(uuid),
			parent,
			chunks_size: 1024,
			chunks: 1,
			favorited: false,
			region: Cow::Borrowed("de-1"),
			bucket: Cow::Borrowed("bucket-a"),
			timestamp: dt(0),
			name: Cow::Owned(name.to_string()),
			size: 7,
			mime: Cow::Borrowed("text/plain"),
			key: file_key(),
			last_modified: dt(1_234),
			created: None,
			hash: None,
		}
	}

	pub(crate) fn cacheable_dir(uuid: Uuid, parent: Uuid, name: &str) -> CacheableDir<'static> {
		CacheableDir {
			uuid,
			parent,
			color: DirColor::Default,
			favorited: false,
			timestamp: dt(0),
			name: Cow::Owned(name.to_string()),
			created: Some(dt(99)),
		}
	}

	pub(crate) fn cache_event(
		id: Option<u64>,
		event: CacheEventType<'static>,
	) -> CacheEvent<'static> {
		CacheEvent { id, event }
	}

	/// Every kind that names a changed path is collected, under the same keys the scan produces;
	/// non-mutating access is noise, except the close of a WRITE, which is how inotify announces a
	/// finished write.
	#[test]
	fn every_mutating_event_names_paths_and_reads_are_dropped() {
		let changes = sized_pair();
		let mutating = [
			("create", EventKind::Create(CreateKind::File)),
			(
				"data",
				EventKind::Modify(ModifyKind::Data(DataChange::Content)),
			),
			(
				"meta",
				EventKind::Modify(ModifyKind::Metadata(MetadataKind::WriteTime)),
			),
			(
				"rename",
				EventKind::Modify(ModifyKind::Name(RenameMode::Any)),
			),
			("modify-any", EventKind::Modify(ModifyKind::Any)),
			("remove", EventKind::Remove(RemoveKind::File)),
			("any", EventKind::Any),
			("other", EventKind::Other),
			(
				"close-write",
				EventKind::Access(AccessKind::Close(AccessMode::Write)),
			),
		];
		for (name, kind) in mutating {
			note(&changes, &fs_event(kind, &[&format!("{ROOT}/{name}")]));
		}
		for kind in [
			EventKind::Access(AccessKind::Read),
			EventKind::Access(AccessKind::Open(AccessMode::Read)),
			EventKind::Access(AccessKind::Close(AccessMode::Read)),
			EventKind::Access(AccessKind::Any),
		] {
			note(&changes, &fs_event(kind, &[&format!("{ROOT}/noise")]));
		}

		let (paths, full) = taken(&changes);
		assert_eq!(full, None, "nothing was lost, so nothing may collapse");
		let mut expected: Vec<String> = mutating.iter().map(|(name, _)| name.to_string()).collect();
		expected.sort();
		assert_eq!(paths, expected, "a read must not dirty a path");
	}

	/// A directory event names the directory, and the root itself is the empty key the pair's own
	/// row uses — both are paths to LOOK at, which is all this type can say.
	#[test]
	fn a_directory_and_the_root_itself_are_ordinary_keys() {
		let changes = sized_pair();
		note(
			&changes,
			&fs_event(
				EventKind::Create(CreateKind::Folder),
				&[&format!("{ROOT}/sub")],
			),
		);
		note(
			&changes,
			&fs_event(
				EventKind::Modify(ModifyKind::Metadata(MetadataKind::WriteTime)),
				&[ROOT],
			),
		);
		assert_eq!(
			taken(&changes),
			(vec![String::new(), "sub".to_string()], None)
		);
	}

	/// The collapse rows of the local table: a kernel drop, the root changing, and a path that
	/// cannot be keyed. Each is sticky until a pass takes it, and each drops the paths it can no
	/// longer narrow.
	#[test]
	fn a_lost_local_event_collapses_the_set_and_stays_collapsed() {
		for (label, lost, reason) in [
			("rescan", rescan(), R::KernelDropped),
			("root changed", root_changed(), R::RootChanged),
			(
				"outside the root",
				fs_event(EventKind::Create(CreateKind::File), &["/elsewhere/x.txt"]),
				R::UnmappedLocalPath,
			),
		] {
			let changes = sized_pair();
			note(&changes, &create("a.txt"));
			note(&changes, &lost);
			// A later ordinary event cannot un-collapse it.
			note(&changes, &create("b.txt"));

			let (paths, full) = taken(&changes);
			assert_eq!(full, Some(reason), "{label}");
			assert!(paths.is_empty(), "{label}: no path can narrow this pass");
			// Taken means taken: the next pass starts clean.
			assert_eq!(taken(&changes), (Vec::new(), None), "{label}");
		}
	}

	/// The cap scales with the tree the last full pass saw, is never zero, and never exceeds the
	/// absolute ceiling.
	#[test]
	fn the_dirty_cap_is_a_quarter_of_the_tree_within_bounds() {
		assert_eq!(
			dirty_cap(0),
			1,
			"an unknown size must still narrow one path"
		);
		assert_eq!(dirty_cap(4), 1);
		assert_eq!(dirty_cap(400), 100);
		assert_eq!(dirty_cap(usize::MAX), DIRTY_CAP);
	}

	/// Past the cap the local set collapses rather than growing without bound.
	#[test]
	fn too_many_local_paths_collapse_to_a_full_pass() {
		let changes = PairChanges::new();
		changes.cover_local();
		changes.note_tree_size(40); // cap: 10
		for i in 0..10 {
			note(&changes, &create(&format!("f{i}")));
		}
		let scope = changes.take();
		assert_eq!(scope.full, None, "the cap itself is not an overflow");
		assert_eq!(scope.sizes().0, 10);

		for i in 0..11 {
			note(&changes, &create(&format!("f{i}")));
		}
		assert_eq!(taken(&changes).1, Some(R::LocalOverflow));
	}

	/// The engine's own staging writes are dropped from the set, as they are from the wake-up.
	#[test]
	fn staging_writes_do_not_dirty_a_path() {
		let changes = sized_pair();
		let staged = format!("{ROOT}/sub/x.filendl");
		let real = format!("{ROOT}/sub/x.txt");
		changes.note_local_event(
			Path::new(ROOT),
			&fs_event(EventKind::Create(CreateKind::File), &[&staged, &real]),
			|path| path.extension() == Some(OsStr::new("filendl")),
		);
		assert_eq!(taken(&changes), (vec!["sub/x.txt".to_string()], None));
	}

	/// Each remote event shape becomes the change the remote view is built from — and a removal is
	/// the only shape that can say an item is gone.
	#[test]
	fn a_remote_batch_becomes_the_projection_the_view_reads() {
		let changes = sized_pair();
		let file = Uuid::from_u128(1);
		let dir = Uuid::from_u128(2);
		let parent = Uuid::from_u128(3);
		let successor = Uuid::from_u128(4);
		let events = [
			cache_event(
				Some(10),
				CacheEventType::File(FileEvent::New(cacheable_file(file, parent, "a.txt"))),
			),
			// A resync synthetic carries no drive message id.
			cache_event(
				None,
				CacheEventType::Dir(DirEvent::New(cacheable_dir(dir, parent, "sub"))),
			),
			cache_event(
				Some(11),
				CacheEventType::File(FileEvent::Trashed {
					uuid: file,
					stable_uuid: StableUuid::new_for_test(file),
					new_uuid: Some(successor),
				}),
			),
			cache_event(Some(12), CacheEventType::Dir(DirEvent::Removed(dir))),
			// Nothing the engine reads.
			cache_event(
				Some(13),
				CacheEventType::Dir(DirEvent::ColorChanged {
					uuid: dir,
					color: DirColor::Blue,
				}),
			),
			cache_event(Some(14), CacheEventType::NoOp),
			cache_event(Some(15), CacheEventType::Global(GlobalEvent::TrashEmpty)),
		];
		changes.note_remote_batch(&mut events.iter());

		let scope = changes.take();
		assert_eq!(scope.full, None);
		assert_eq!(
			scope.sizes().1,
			4,
			"a colour change, a no-op and an emptied trash are not changes to this tree"
		);

		let RemoteChange::Upsert(item) = &scope.remote[0].change else {
			panic!("a new file must upsert the view");
		};
		assert_eq!(scope.remote[0].id, Some(10));
		assert_eq!(
			(item.uuid, item.parent, item.name.as_str(), item.size),
			(file, parent, "a.txt", 7)
		);
		assert!(
			item.stable_uuid.is_some(),
			"a file carries its whole-life id"
		);
		assert_eq!(item.modified_millis, 1_234);

		let RemoteChange::Upsert(item) = &scope.remote[1].change else {
			panic!("a new directory must upsert the view");
		};
		assert_eq!(
			scope.remote[1].id, None,
			"a synthetic has no id to align by"
		);
		assert_eq!(item.stable_uuid, None, "a directory has no whole-life id");
		assert_eq!((item.size, item.modified_millis), (0, 99));

		let RemoteChange::Gone(gone) = &scope.remote[2].change else {
			panic!("a trash must be an absence");
		};
		assert_eq!(gone.uuid(), file);
		assert_eq!(
			gone.successor(),
			Some(successor),
			"a versioning edit re-mints the file rather than deleting it"
		);

		let RemoteChange::Gone(gone) = &scope.remote[3].change else {
			panic!("a removed directory must be an absence");
		};
		assert_eq!((gone.uuid(), gone.successor()), (dir, None));
	}

	/// A metadata patch is a rename, with the content stamps that ride along for a file and none
	/// for a directory — and it names no parent, because the item did not move.
	#[test]
	fn a_metadata_patch_renames_without_moving() {
		let changes = sized_pair();
		let file = Uuid::from_u128(1);
		let dir = Uuid::from_u128(2);
		let events = [
			cache_event(
				Some(1),
				CacheEventType::File(FileEvent::MetadataChanged {
					uuid: file,
					meta: DecryptedFileMeta {
						name: Cow::Borrowed("new.txt"),
						size: 12,
						mime: Cow::Borrowed("text/plain"),
						key: file_key(),
						last_modified: dt(500),
						created: None,
						hash: None,
					},
				}),
			),
			cache_event(
				Some(2),
				CacheEventType::Dir(DirEvent::MetadataChanged {
					uuid: dir,
					meta: DecryptedDirectoryMeta {
						name: Cow::Borrowed("renamed"),
						created: None,
					},
				}),
			),
		];
		changes.note_remote_batch(&mut events.iter());

		let scope = changes.take();
		assert_eq!(
			scope.remote[0].change,
			RemoteChange::Renamed {
				uuid: file,
				name: "new.txt".to_string(),
				content: Some(FileContent {
					size: 12,
					hash: None,
					modified_millis: 500,
				}),
			}
		);
		assert_eq!(
			scope.remote[1].change,
			RemoteChange::Renamed {
				uuid: dir,
				name: "renamed".to_string(),
				content: None,
			}
		);
	}

	/// An account-wide wipe cannot be described as a delta, and past the cap the delta collapses.
	#[test]
	fn a_wipe_or_an_overflow_collapses_the_delta() {
		let changes = sized_pair();
		let wipe = [cache_event(
			Some(1),
			CacheEventType::Global(GlobalEvent::DeleteAll),
		)];
		changes.note_remote_batch(&mut wipe.iter());
		assert_eq!(changes.take().full, Some(R::RemoteWiped));

		let changes = PairChanges::new();
		changes.cover_local();
		changes.note_tree_size(40); // cap: 10
		let batch: Vec<CacheEvent<'static>> = (0..11)
			.map(|i| {
				cache_event(
					Some(i),
					CacheEventType::Dir(DirEvent::Removed(Uuid::from_u128(u128::from(i) + 1))),
				)
			})
			.collect();
		changes.note_remote_batch(&mut batch.iter());
		assert_eq!(changes.take().full, Some(R::RemoteOverflow));
	}

	/// With no filesystem watcher attached, nothing is recording local changes, so an empty local
	/// list means "nobody looked" rather than "nothing changed" — every pass reads the local side
	/// whole. A watcher that starts clears it; one that ends brings it back.
	#[test]
	fn an_unwatched_pair_never_narrows_its_local_half() {
		let changes = PairChanges::new();
		changes.note_tree_size(4_000);
		assert_eq!(
			changes.take().full_pass_reason(10),
			Some(R::LocalEventsUnwatched),
			"a pair nothing is watching cannot narrow a pass down"
		);

		changes.cover_local();
		note(&changes, &create("a.txt"));
		assert_eq!(
			changes.take().full_pass_reason(10),
			None,
			"a watcher that covers the tree is what makes the list evidence"
		);

		changes.uncover_local();
		assert_eq!(
			changes.take().full_pass_reason(10),
			Some(R::LocalEventsUnwatched),
			"a watch that ended stops being evidence for what did NOT change"
		);
	}

	/// A rule file is not a path to look at: its contents decide what is hidden for its whole
	/// directory and below, on both sides, so a change to one on either side collapses that side
	/// to a whole-tree read. What the notification cannot show — a removal, which names a uuid and
	/// no name — is left to the consuming pass, which holds the baseline.
	#[test]
	fn a_rule_file_change_collapses_its_side() {
		// Local: at the root, and nested.
		for rel in [FILENIGNORE, "proj/sub/.filenignore"] {
			let changes = sized_pair();
			note(&changes, &create("a.txt"));
			note(
				&changes,
				&fs_event(
					EventKind::Modify(ModifyKind::Data(DataChange::Content)),
					&[&format!("{ROOT}/{rel}")],
				),
			);
			let (paths, full) = taken(&changes);
			assert_eq!(full, Some(R::RulesChanged), "{rel}");
			assert!(
				paths.is_empty(),
				"{rel}: no path set describes a rules change"
			);
		}

		// A file that merely LOOKS like one is an ordinary path.
		let changes = sized_pair();
		note(&changes, &create("sub/.filenignore.bak"));
		note(&changes, &create("sub/filenignore"));
		assert_eq!(taken(&changes).1, None);

		// Remote: a rule file arriving, and one being renamed into that name.
		let dir = Uuid::from_u128(9);
		let rule = Uuid::from_u128(10);
		let arriving = [cache_event(
			Some(1),
			CacheEventType::File(FileEvent::New(cacheable_file(rule, dir, FILENIGNORE))),
		)];
		let changes = sized_pair();
		changes.note_remote_batch(&mut arriving.iter());
		assert_eq!(changes.take().full, Some(R::RulesChanged));

		let renamed = [cache_event(
			Some(2),
			CacheEventType::File(FileEvent::MetadataChanged {
				uuid: rule,
				meta: DecryptedFileMeta {
					name: Cow::Borrowed(FILENIGNORE),
					size: 3,
					mime: Cow::Borrowed("text/plain"),
					key: file_key(),
					last_modified: dt(1),
					created: None,
					hash: None,
				},
			}),
		)];
		let changes = sized_pair();
		changes.note_remote_batch(&mut renamed.iter());
		assert_eq!(changes.take().full, Some(R::RulesChanged));

		// A removal names a uuid and no name, so the event cannot say it was a rule file: it is
		// recorded as the ordinary absence it looks like, and the consuming pass checks the
		// baseline. Pinned so that obligation cannot be forgotten silently.
		let removed = [cache_event(
			Some(3),
			CacheEventType::File(FileEvent::Removed(rule)),
		)];
		let changes = sized_pair();
		changes.note_remote_batch(&mut removed.iter());
		let scope = changes.take();
		assert_eq!(scope.full, None);
		assert_eq!(scope.sizes().1, 1);
	}

	/// A degraded trigger source forces every pass full for the life of the watch: a pass does not
	/// clear it, and it outranks every other reason.
	#[test]
	fn a_degraded_source_keeps_every_pass_full() {
		for reason in [R::LocalEventsDegraded, R::RemoteEventsDegraded] {
			let changes = sized_pair();
			changes.degrade(reason);
			changes.force(R::SafetyNet);
			assert_eq!(changes.take().full, Some(reason));
			changes.note_tree_size(4_000);
			assert_eq!(
				changes.take().full,
				Some(reason),
				"lost coverage does not come back"
			);
		}
	}

	/// A forced reason goes to exactly one pass — the next one to START — keeps the first reason
	/// recorded, and is outranked by evidence that events were actually lost.
	#[test]
	fn a_forced_reason_goes_to_the_next_pass_to_start() {
		let changes = sized_pair();
		changes.force(R::PreviousRefusal);
		changes.force(R::SafetyNet);
		assert_eq!(
			changes.take().full,
			Some(R::PreviousRefusal),
			"the first reason is the one that made the pass mandatory"
		);
		// That pass took it, so the pass after it is free again.
		changes.note_tree_size(4_000);
		assert_eq!(changes.take().full, None);

		let lossy = sized_pair();
		lossy.force(R::RulesChanged);
		note(&lossy, &rescan());
		assert_eq!(
			lossy.take().full,
			Some(R::KernelDropped),
			"lost events are the more specific reason"
		);
	}

	/// A reason recorded while a pass is already running belongs to the pass AFTER it: the running
	/// one took its lists before that demand existed and cannot have covered it. Clearing the
	/// reason when the pass ended instead dropped it — the lost-trigger hole, silent by
	/// construction.
	#[test]
	fn a_reason_forced_after_the_take_survives_that_pass() {
		let changes = sized_pair();
		note(&changes, &create("a.txt"));
		let scope = changes.take();
		assert_eq!(scope.full_pass_reason(10), None, "nothing forced it yet");

		// ... and now, mid-pass, the rules change under it.
		changes.force(R::RulesChanged);
		// The pass ends and records the tree it read.
		changes.note_tree_size(4_000);

		assert_eq!(
			changes.take().full,
			Some(R::RulesChanged),
			"a demand that arrived mid-pass must outlive that pass"
		);
	}

	/// The one row only the pass itself knows: an empty baseline is a first sync whatever the
	/// changelists say — and a collapsed list outranks even that.
	#[test]
	fn the_pass_itself_contributes_the_last_row() {
		let changes = sized_pair();
		note(&changes, &create("a.txt"));
		let scope = changes.take();
		assert_eq!(scope.full_pass_reason(10), None);
		assert_eq!(scope.full_pass_reason(0), Some(R::EmptyBaseline));

		let lossy = sized_pair();
		note(&lossy, &rescan());
		assert_eq!(lossy.take().full_pass_reason(0), Some(R::KernelDropped));
	}

	/// Every reason renders as a sentence a caller can show.
	#[test]
	fn every_reason_says_why() {
		for reason in [
			R::FirstPass,
			R::EmptyBaseline,
			R::KernelDropped,
			R::RootChanged,
			R::UnmappedLocalPath,
			R::LocalOverflow,
			R::RemoteOverflow,
			R::RemoteWiped,
			R::LocalEventsDegraded,
			R::RemoteEventsDegraded,
			R::SafetyNet,
			R::PreviousRefusal,
			R::DeletionHold,
			R::InterruptedPass,
			R::RulesChanged,
			R::UnappliedWork,
			R::ConflictResolved,
			R::LocalEventsUnwatched,
		] {
			assert!(!reason.to_string().is_empty(), "{reason:?}");
		}
	}
}

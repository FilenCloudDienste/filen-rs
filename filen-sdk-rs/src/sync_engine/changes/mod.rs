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
//! of half the tree costs a full pass, which is what it would cost anyway. What a pass hands the
//! next one is bounded by the absolute [`DIRTY_CAP`] instead, and past it the next pass reads both
//! sides whole too ([`FullPassReason::CarriedOver`], [`FullPassReason::RemoteOverflow`]).
//!
//! One entry can still name a whole directory, and a change-scoped pass holds a node or a
//! tombstone for every path under a directory it sees renamed, moved, trashed or deleted. So the
//! pass that finds one change spanning more than [`SUBTREE_CAP`] paths reads both sides whole
//! instead ([`FullPassReason::LocalOverflow`], [`FullPassReason::RemoteOverflow`]): what it
//! expands each change into is bounded by that cap rather than by the largest directory in the
//! tree. The cap is per change, so several large ones add up, and a directory renamed more than
//! once before a pass leaves a set of tombstones per name. The rows that stand in for neither
//! side are not a change's: every pass holds and observes them, and the subtree of each, whatever
//! changed (`derive::from_baseline`).

use std::{
	collections::BTreeSet,
	fmt, mem,
	path::Path,
	sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use filen_types::crypto::Blake3Hash;
use notify::{
	Event, EventKind,
	event::{AccessKind, AccessMode},
};
use tokio::sync::{Notify, futures::Notified};
use uuid::Uuid;

use super::{ignore::FILENIGNORE, scan::normalize_rel_path};
use crate::cache::{CacheEvent, CacheEventType, DirEvent, FileEvent, GlobalEvent, RemoteItem};

/// The most entries a changelist holds before it collapses to a full pass, whatever the tree size.
///
/// Sized by what a change-scoped pass holds with BOTH lists this full, the lists themselves
/// included: the most entries a list for which that pass stays within 16 MiB
/// (`derive`'s `a_scoped_pass_at_both_changelist_caps_peaks_under_16_mib`, where 7,700 peak at
/// 15.90 MiB and 7,800 at 16.02). A list also sits in memory for as long as the pair waits for its
/// next pass, which this bounds too.
pub(super) const DIRTY_CAP: usize = 7_700;

/// The most paths a change-scoped pass expands one change into: the baseline rows strictly under a
/// directory the change renamed, moved, trashed or deleted, or the entries one walk of a dirty
/// directory lists below it. A change past it makes the pass read both sides whole instead, so
/// what the pass holds for each change is bounded by this rather than by the largest directory in
/// the tree. The local observation does not count the rows that stand in for neither side, which
/// every pass holds whatever changed (`observe::cap_under`).
pub(super) const SUBTREE_CAP: usize = 16_384;

/// Whether `count` is past `cap`. A cap is the most a pass may hold, so it is exceeded only above
/// it; every cap the engine bounds a pass with is checked through this one comparison.
pub(super) fn past_cap(count: usize, cap: usize) -> bool {
	count > cap
}

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
	/// More local paths changed than a change-scoped pass narrows itself to: more than
	/// [`dirty_cap`] allows, or one directory among them renamed, moved or deleted with more than
	/// 16,384 paths under it.
	LocalOverflow,
	/// More remote changes arrived than a change-scoped pass narrows itself to: more than
	/// [`dirty_cap`] allows, or one of them a directory renamed, moved or trashed with more than
	/// 16,384 paths under it.
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
	/// A caller asked for a pass ([`request_pass`](super::SyncEngine::request_pass)), which reads
	/// both sides whole like a [`SafetyNet`](Self::SafetyNet) tick.
	Requested,
	/// The previous pass refused to run (a name collision on either side), which is whole-tree
	/// state.
	PreviousRefusal,
	/// The previous pass held deletions back, and something else has since given the pair a pass
	/// to run.
	///
	/// A hold alone does not make a pass due: re-reading two trees nothing has changed in only
	/// holds the identical batch again, so a watch wake that announced nothing new while a hold
	/// stands is skipped like any idle one (see `PairChanges::is_idle`). The pass that does run —
	/// for a change on either side, the safety net, or an
	/// [`approval`](super::SyncEngine::approve_deletions), which forces this reason itself and
	/// wakes the pair's watch — reads both sides whole, and this is its reason when nothing more
	/// specific is.
	///
	/// Whole, because three of the holds want evidence only a whole-tree read supplies: an
	/// incomplete scan, an unconverged remote, or a remote that listed nothing at all. The other
	/// two — a volume threshold, a first sync against a populated destination — want something
	/// else: the approval a caller gives names ONE exact batch by its token, so the pass that
	/// offers that batch again has to reproduce it exactly, and the absences it was built from were
	/// observations the holding pass consumed. Reading everything is the only way to reproduce
	/// them, which is also why a held batch's paths are not carried over as owed
	/// (`facts::carry_over`): the whole read already looks at them.
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
	/// The previous pass's plan named more paths than a pass carries over to the next one — as many
	/// as a changelist holds at most, whatever the tree size — so none of them was kept. Kept, they
	/// would hold memory for as long as the pair waits for its next pass (after a first sync, one
	/// path for every item in the tree), and a whole read finds every one of them again.
	///
	/// It says nothing about whether that plan was carried out: a first sync that applied every
	/// action reports it too.
	CarriedOver,
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
			Self::Requested => "a caller asked for a pass",
			Self::PreviousRefusal => "the previous pass refused to run",
			Self::DeletionHold => "the previous pass held deletions for whole-tree evidence",
			Self::InterruptedPass => "the previous pass was cancelled part-way through",
			Self::RulesChanged => "the ignore patterns or the pair's mode changed",
			Self::UnappliedWork => "the previous pass planned work it did not apply",
			Self::CarriedOver => {
				"the previous pass's plan named too many paths to carry over individually"
			}
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
/// Paths are root-relative, NFC, `/`-joined — the keys
/// [`scan_local_staged`](super::scan::scan_local_staged) produces — so a path recorded here names
/// the same node the scan and the baseline do. There is deliberately nothing here to express
/// "absent": a path is something to LOOK at.
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
				DirEvent::New(d)
				| DirEvent::Restored(d)
				| DirEvent::Move(d)
				| DirEvent::Changed(d) => {
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
pub(super) struct PairChanges {
	state: Mutex<ChangeState>,
	/// The pair's doorbell: rung by whatever gives its next pass something to do — the filesystem
	/// watcher, the cache, an approval of held deletions — and waited on by the pair's watch loop.
	/// Owned here, with the changelists, so the engine can ring it without knowing whether a watch
	/// is running: rung with nobody waiting, it keeps one wake for the next wait. `Arc`: the watch's
	/// trigger callbacks and its loop each hold it on their own threads.
	doorbell: Arc<Notify>,
	/// A caller's request for a pass ([`request_pass`](super::SyncEngine::request_pass)), kept
	/// apart from the doorbell because a watch backing off after a failed pass hears only this
	/// one: a change event must wait out the backoff, a "sync now" must not. Kept like the
	/// doorbell, and an `Arc` for the same reason.
	requested: Arc<Notify>,
	/// Rung to end a pass's wait for the cache to finish listing the pair's remote folder (see
	/// [`stop_waiting`](Self::stop_waiting)). Owned here for the same reason as the doorbell: the
	/// engine rings it without knowing whether a pass is waiting.
	stop_waiting: Notify,
}

#[derive(Debug, Default)]
struct ChangeState {
	local: LocalDirty,
	remote: RemoteDelta,
	/// Recorded by the engine for the NEXT pass — the previous pass's outcome, a rules change, the
	/// safety-net tick, a caller's request. Keeps the FIRST reason recorded (the one that made the
	/// pass mandatory) and is TAKEN by the next pass to start, in the same breath as the
	/// changelists ([`PairChanges::take`]).
	///
	/// Taken rather than cleared at the end, because a pass cannot honour a demand it never saw: a
	/// reason recorded while it was already reading (a `set_user_ignore` that commits mid-pass, a
	/// second pass forcing one while this one applies) belongs to the pass AFTER it. Clearing it at
	/// the end dropped exactly those. Nothing is lost by taking it early either — a pass that took
	/// a reason and then failed, or did not apply what it planned, forces one again on its way out
	/// (`next_pass_scope`).
	forced: Option<FullPassReason>,
	/// Set by a pass that ended holding deletions back: the next pass to RUN reads both sides
	/// whole ([`FullPassReason::DeletionHold`]). Taken by that pass like [`forced`](Self::forced),
	/// and kept apart from it for two reasons. It is not a reason to run a pass at all, so
	/// [`PairChanges::is_idle`] ignores it and a wake with nothing new leaves it in place. And
	/// `forced` keeps the FIRST reason recorded, so a hold parked there would swallow the reason
	/// that actually made the next pass due.
	held: bool,
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
	/// that pass's own plan. Bounded all the same, by the absolute cap — see
	/// [`note_owed`](PairChanges::note_owed).
	owed: BTreeSet<String>,
	/// The other half of [`owed`](Self::owed): the announced changes the previous pass consumed,
	/// handed back when it was cut short before it applied them (see the module docs).
	///
	/// Outside the tree-scaled cap for the same reason `owed` is, and REPLAYED IN FRONT of
	/// whatever arrived since, which is the order the cache dispatched them in. Bounded all the
	/// same — see [`note_owed_remote`](PairChanges::note_owed_remote).
	owed_remote: Vec<RemoteDeltaEntry>,
}

impl ChangeState {
	/// See [`PairChanges::full_pending`].
	fn full_pending(&self) -> bool {
		self.degraded.is_some()
			|| !self.local_covered
			|| self.local.full.is_some()
			|| self.remote.full.is_some()
			|| self.forced.is_some()
	}
}

impl PairChanges {
	pub(super) fn new() -> Self {
		Self::default()
	}

	/// Record what one filesystem event means. `is_staging` names the engine's own download temp
	/// files and quarantine bin, filen-sync's trash and, in a pair rooted at the drive, the
	/// account's data directory, whose paths are dropped rather than re-observed — the rename that
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
					if past_cap(state.local.paths.len(), cap) {
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
					if past_cap(state.remote.entries.len(), cap) {
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
	///
	/// Bounded by the absolute [`DIRTY_CAP`], like the remote half
	/// ([`note_owed_remote`](Self::note_owed_remote)) and for the same reason it is not the
	/// tree-scaled [`dirty_cap`]. Past it no set is kept and the next pass reads both sides whole
	/// ([`FullPassReason::CarriedOver`]): the set lives for as long as the pair waits for that pass,
	/// and after a first sync it names every item in the tree. Counted as it is built, so a plan
	/// of any size never costs more than one path past the cap here.
	pub(super) fn note_owed<'a>(&self, paths: impl IntoIterator<Item = &'a str>) {
		let mut owed = BTreeSet::new();
		for path in paths {
			owed.insert(path.to_owned());
			if past_cap(owed.len(), DIRTY_CAP) {
				self.state().owed = BTreeSet::new();
				self.force(FullPassReason::CarriedOver);
				return;
			}
		}
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
		if past_cap(delta.len(), DIRTY_CAP) {
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
		self.state().full_pending()
	}

	/// Whether the next pass would have nothing to look at: nothing announced on either side,
	/// nothing owed by the last plan, and no reason pending to read everything. Asked WITHOUT
	/// taking anything, so a wake that skips its pass leaves every list and reason to the pass that
	/// does run.
	///
	/// A pending deletion hold deliberately does not count (see [`FullPassReason::DeletionHold`]):
	/// re-reading two trees nothing has changed in only holds the identical batch again. It stays
	/// recorded, and makes the next pass that runs a whole one.
	///
	/// The one row of the trigger table it cannot see is the empty baseline, which is why a caller
	/// that acts on this is deciding whether to READ the pair at all — a pair with an empty
	/// baseline and nothing announced has nothing to do either.
	pub(super) fn is_idle(&self) -> bool {
		let state = self.state();
		!state.full_pending()
			&& state.local.paths.is_empty()
			&& state.remote.entries.is_empty()
			&& state.owed.is_empty()
			&& state.owed_remote.is_empty()
	}

	/// Record that the NEXT pass must read both sides whole. Keeps the first reason recorded.
	pub(super) fn force(&self, reason: FullPassReason) {
		let mut state = self.state();
		if state.forced.is_none() {
			state.forced = Some(reason);
		}
	}

	/// Record that the pass that just ended held deletions back, so the next pass to run reads both
	/// sides whole and can offer the same batch again. Unlike [`force`](Self::force) it does not
	/// make that pass due (see [`is_idle`](Self::is_idle)).
	pub(super) fn note_deletion_hold(&self) {
		self.state().held = true;
	}

	/// The pair's doorbell, for a watch to wait on and to hand its trigger sources.
	pub(super) fn doorbell(&self) -> Arc<Notify> {
		Arc::clone(&self.doorbell)
	}

	/// Wake the pair's watch, if one is running, for a pass at its debounce rather than its next
	/// trigger.
	pub(super) fn ring(&self) {
		self.doorbell.notify_one();
	}

	/// The pair's request signal, for a watch to wait on (see [`request`](Self::request)).
	pub(super) fn request_signal(&self) -> Arc<Notify> {
		Arc::clone(&self.requested)
	}

	/// Wake the pair's watch for a pass a caller asked for: at its debounce, or at once when it is
	/// backing off after a failed pass.
	pub(super) fn request(&self) {
		self.requested.notify_one();
	}

	/// End every wait in progress of a pass of this pair for the cache to finish listing its remote
	/// folder (see `SyncEngine::await_remote_view`): rung for a watch being stopped and an engine
	/// being closed, which leave nobody to wait for. A pass not waiting is left alone, and so is the
	/// next one to wait: nothing is kept for a wait that starts later.
	pub(super) fn stop_waiting(&self) {
		self.stop_waiting.notify_waiters();
	}

	/// Resolves at the next [`stop_waiting`](Self::stop_waiting) after this was called, whether or
	/// not it has been polled by then.
	pub(super) fn waiting_stopped(&self) -> Notified<'_> {
		self.stop_waiting.notified()
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
		let held = mem::take(&mut state.held).then_some(FullPassReason::DeletionHold);
		// Most specific first: a permanently degraded source, then no watcher at all, then evidence
		// that events were lost, then what the engine itself recorded about the previous pass, and
		// a deletion hold last: it only needs this pass to read whole, which any reason above
		// already makes it.
		let full = state
			.degraded
			.or((!state.local_covered).then_some(FullPassReason::LocalEventsUnwatched))
			.or(local.full)
			.or(remote.full)
			.or(forced)
			.or(held);
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
		self.state.lock().unwrap_or_else(PoisonError::into_inner)
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
pub(super) mod tests;

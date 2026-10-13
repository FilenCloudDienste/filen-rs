//! The records the sync engine's bindings hand JS in place of the engine's own types: those carry
//! `usize` counts, `PathBuf`s, `Duration`s, private fields or lists as long as the tree, none of
//! which crosses as is. Counts and bytes widen to `u64`, durations become milliseconds, paths
//! become (lossy UTF-8) strings, and a report's lists are capped at [`REPORT_LIST_LIMIT`].

use std::{path::PathBuf, time::Duration};

use filen_macros::js_type;
use filen_types::fs::Uuid;

use crate::{
	Error, ErrorKind,
	job::millis,
	sync_engine::{
		BaselineInUse, DeleteGuard, GuardReason, HaltReason, IgnoredPath, PairId, PairOverlap,
		PairRecord, PauseMode, PauseOptions, PlanOutcome, PlannedAction, PlannedActionKind,
		PlannedConflict, RefuseReason, SyncEvent, SyncMode, SyncReport, TransferDirection,
		UnsyncablePath, WatchConfig, WatchStatus,
	},
};

/// How many entries each list of a [`SyncPassReport`] carries at most; its `*_total` says how
/// many the pass had. A report's lists are as long as the tree on a first sync, and every one
/// crosses the bridge whole and stays in memory as the pair's last report.
pub(super) const REPORT_LIST_LIMIT: usize = 1000;

/// A pair id as JS sees it. The engine's are sqlite rowids, which are positive, so one that does
/// not fit is a broken invariant, not an input error.
pub(super) fn pair_id_out(pair: PairId) -> Result<u64, Error> {
	u64::try_from(pair).map_err(|error| {
		Error::custom_with_source(
			ErrorKind::Internal,
			error,
			Some(format!("sync pair id {pair} is negative")),
		)
	})
}

/// `list` cut down to [`REPORT_LIST_LIMIT`] entries, and how many it had.
fn capped<T>(mut list: Vec<T>) -> (Vec<T>, u64) {
	let total = list.len() as u64;
	list.truncate(REPORT_LIST_LIMIT);
	// The report is kept as the pair's last one: without this a capped list would keep the whole
	// list's allocation alive.
	list.shrink_to_fit();
	(list, total)
}

/// The entries of `list` from `offset` on, at most `limit` of them and never more than
/// [`REPORT_LIST_LIMIT`]; none past the end.
pub(super) fn page<T: Clone>(list: &[T], offset: u64, limit: u64) -> Vec<T> {
	let limit =
		usize::try_from(limit).map_or(REPORT_LIST_LIMIT, |limit| limit.min(REPORT_LIST_LIMIT));
	usize::try_from(offset)
		.ok()
		.and_then(|offset| list.get(offset..))
		.unwrap_or_default()
		.iter()
		.take(limit)
		.cloned()
		.collect()
}

/// `path` as JS gets it: lossy UTF-8.
pub(super) fn path_string(path: PathBuf) -> String {
	path.into_os_string()
		.into_string()
		.unwrap_or_else(|path| path.to_string_lossy().into_owned())
}

/// Mirror of [`GuardReason`], with its counts widened from `usize`.
#[js_type(export, no_deser, tagged)]
pub enum SyncGuardReason {
	/// The local scan did not complete, so every deletion is held.
	ScanIncomplete,
	/// The pair's first sync would delete what the destination already held.
	FirstSyncWithDeletions { deletions: u64 },
	/// The remote view has never converged, so its absences cannot be trusted yet.
	RemoteUnconverged { deletions: u64 },
	/// The remote listed nothing at all while the pair tracks remote items.
	RemoteEmptied { deletions: u64 },
	/// More deletions than the pair's [`DeleteGuardState`] allows.
	ExceededThreshold { deletions: u64, limit: u64 },
}

impl From<GuardReason> for SyncGuardReason {
	fn from(reason: GuardReason) -> Self {
		match reason {
			GuardReason::ScanIncomplete => Self::ScanIncomplete,
			GuardReason::FirstSyncWithDeletions { deletions } => Self::FirstSyncWithDeletions {
				deletions: deletions as u64,
			},
			GuardReason::RemoteUnconverged { deletions } => Self::RemoteUnconverged {
				deletions: deletions as u64,
			},
			GuardReason::RemoteEmptied { deletions } => Self::RemoteEmptied {
				deletions: deletions as u64,
			},
			GuardReason::ExceededThreshold { deletions, limit } => Self::ExceededThreshold {
				deletions: deletions as u64,
				limit: limit as u64,
			},
		}
	}
}

/// A pair's mass-delete guard (mirror of [`DeleteGuard`], whose fields are private).
// `ratio` is an `f64`, so no `Eq`, hence `no_default`.
#[derive(Debug, Clone, PartialEq)]
#[js_type(export, no_deser, no_default, tagged)]
pub enum DeleteGuardState {
	/// A pass's deletions beyond `max(floor, ratio × tracked items)` are held.
	Limited { floor: u64, ratio: f64 },
	/// No volume check (`DeleteGuard::unlimited()`); the holds about untrustworthy evidence still
	/// apply.
	Unlimited,
}

impl From<DeleteGuard> for DeleteGuardState {
	fn from(guard: DeleteGuard) -> Self {
		if guard == DeleteGuard::unlimited() {
			Self::Unlimited
		} else {
			Self::Limited {
				floor: guard.floor() as u64,
				ratio: guard.ratio(),
			}
		}
	}
}

/// The guard `setDeleteGuard` asks for. A floor this platform cannot count to is refused, as the
/// engine refuses a ratio outside `0.0..=1.0`.
pub(super) fn delete_guard(floor: u64, ratio: f64) -> Result<DeleteGuard, Error> {
	let floor = usize::try_from(floor).map_err(|error| {
		Error::custom_with_source(
			ErrorKind::InvalidState,
			error,
			Some(format!(
				"a delete-guard floor of {floor} does not fit this platform"
			)),
		)
	})?;
	DeleteGuard::new(floor, ratio)
}

/// What the binding is running for a pair.
#[js_type(export, no_deser)]
pub enum SyncPairActivity {
	/// Nothing: the pair is neither watched nor syncing once.
	Idle,
	/// A watch is being started.
	Starting,
	/// A watch is running.
	Watching,
	/// A watch is being stopped.
	Stopping,
	/// A one-shot pass is running.
	SyncingOnce,
}

/// Mirror of `pausePair`'s options ([`PauseOptions`]): what it takes, and what
/// [`SyncPairInfo::pause_options`] says it was given.
#[js_type(import, export)]
pub struct SyncPauseOptions {
	/// What happens to the pass in flight.
	pub mode: PauseMode,
	/// Milliseconds after the pause call a `Suspend` becomes a `Cancel`. `None` never escalates:
	/// the parked pass then holds the account-wide drive-write lock until the pair is resumed.
	pub cancel_after_ms: Option<u64>,
}

impl From<SyncPauseOptions> for PauseOptions {
	fn from(options: SyncPauseOptions) -> Self {
		Self {
			mode: options.mode,
			cancel_after: options.cancel_after_ms.map(Duration::from_millis),
		}
	}
}

impl From<PauseOptions> for SyncPauseOptions {
	fn from(options: PauseOptions) -> Self {
		let PauseOptions { mode, cancel_after } = options;
		Self {
			mode,
			cancel_after_ms: cancel_after.map(millis),
		}
	}
}

/// Mirror of [`WatchConfig`], in milliseconds. The engine refuses a zero debounce and a safety
/// net no longer than the debounce.
#[js_type(import, no_ser)]
pub struct SyncWatchConfig {
	/// Quiet window a burst of changes is coalesced over before a pass runs.
	pub debounce_ms: u64,
	/// The least time between two passes that read both sides whole.
	pub safety_net_ms: u64,
}

impl From<SyncWatchConfig> for WatchConfig {
	fn from(config: SyncWatchConfig) -> Self {
		Self {
			debounce: Duration::from_millis(config.debounce_ms),
			safety_net: Duration::from_millis(config.safety_net_ms),
		}
	}
}

/// Mirror of [`PairRecord`], plus what the binding knows live about the pair, so a screen mounted
/// mid-pass, or after a JS reload, learns what the listener would have told it.
#[derive(Debug, Clone, PartialEq)]
#[js_type(export, no_deser, no_default)]
pub struct SyncPairInfo {
	/// The pair's id, as every pair-taking call takes it.
	pub id: u64,
	/// The canonicalized local root.
	pub local_root: String,
	/// The remote folder the pair syncs against.
	pub remote_root: Uuid,
	pub mode: SyncMode,
	/// The pair's mass-delete guard.
	pub delete_guard: DeleteGuardState,
	/// Whether the pair is paused (persisted by the engine).
	pub paused: bool,
	pub activity: SyncPairActivity,
	/// The watch's latest status; set exactly while `activity` is `Watching` or `Stopping`.
	pub watch_status: Option<WatchStatus>,
	/// The `pass_id` of the pass running now, from its `PassStarted` to its `PassCompleted` or
	/// `PassFailed`.
	pub pass_in_flight: Option<u64>,
	/// When (Unix epoch milliseconds) the last pass this process saw complete did so.
	pub last_pass_completed_at_ms: Option<u64>,
	/// Why the last pass this process saw end failed: its `PassFailed` error, or the
	/// [`SyncPassReport::pass_error`] of its `PassCompleted`. `None` when it succeeded.
	pub last_pass_error: Option<String>,
	/// When (Unix epoch milliseconds) this process paused the pair; `None` while it runs, and for
	/// a pause from before this process started (the engine persists only the flag).
	pub paused_at_ms: Option<u64>,
	/// The options of that pause.
	pub pause_options: Option<SyncPauseOptions>,
}

/// A pair the binding runs nothing for and knows nothing live about.
impl TryFrom<PairRecord> for SyncPairInfo {
	type Error = Error;

	fn try_from(record: PairRecord) -> Result<Self, Error> {
		let PairRecord {
			id,
			local_root,
			remote_root,
			mode,
			delete_guard,
			paused,
		} = record;
		Ok(Self {
			id: pair_id_out(id)?,
			local_root,
			remote_root,
			mode,
			delete_guard: delete_guard.into(),
			paused,
			activity: SyncPairActivity::Idle,
			watch_status: None,
			pass_in_flight: None,
			last_pass_completed_at_ms: None,
			last_pass_error: None,
			paused_at_ms: None,
			pause_options: None,
		})
	}
}

/// Mirror of [`SyncReport`]. Every list holds at most [`REPORT_LIST_LIMIT`] entries, the first
/// ones in the engine's order; its `*_total` counts them all.
#[js_type(export, no_deser)]
pub struct SyncPassReport {
	pub downloaded: u64,
	pub uploaded: u64,
	pub local_dirs_created: u64,
	pub remote_dirs_created: u64,
	/// Items moved into the local quarantine bin; a directory counts once.
	pub locally_deleted: u64,
	/// Items moved to the Filen trash; a directory counts once.
	pub remotely_trashed: u64,
	pub moved_remote: u64,
	pub moved_local: u64,
	/// Paths held as two-way conflicts.
	pub conflicts: Vec<PlannedConflict>,
	pub conflicts_total: u64,
	/// What the mass-delete guard held back: the deletions, plus the create half of a held type
	/// flip.
	pub held: Vec<PlannedAction>,
	pub held_total: u64,
	/// How many of the held actions are deletions, counted over all of them.
	pub held_deletions: u64,
	/// Paths the engine will not act on, and why.
	pub unsyncable: Vec<UnsyncablePath>,
	pub unsyncable_total: u64,
	/// The top of each subtree the ignore rules hide.
	pub ignored: Vec<IgnoredPath>,
	pub ignored_total: u64,
	/// Set when the pass refused to run, so applied nothing.
	pub refused: Option<RefuseReason>,
	/// Paths the pass deliberately left alone.
	pub deferred_paths: u64,
	/// Why the guard held deletions, when it did.
	pub guard: Option<SyncGuardReason>,
	/// The token to approve the held deletions with, when the guard held some.
	pub deletion_token: Option<String>,
	/// The pass's failures, one line each.
	pub errors: Vec<String>,
	pub errors_total: u64,
	/// Paths whose local copy was moved into the quarantine bin to make way for a write.
	pub quarantined: Vec<String>,
	pub quarantined_total: u64,
	/// The pair was paused, so the pass did nothing.
	pub paused: bool,
	/// Planned actions the pass did not carry out (a cancel, or a drive lock it could not take).
	pub interrupted: u64,
	/// Set when a side ran out of room during the pass.
	pub halted: Option<HaltReason>,
	/// Why the pass read both sides whole; diagnostic text only.
	pub full_pass_reason: Option<String>,
	/// Why this pass FAILED although it returned a report, or `None` for a healthy one: the verdict
	/// a watch backs off on.
	pub pass_error: Option<String>,
}

impl From<SyncReport> for SyncPassReport {
	fn from(report: SyncReport) -> Self {
		// Both read lists the report gives up below.
		let pass_error = report.pass_error();
		let held_deletions = report.held_deletions() as u64;
		// Whole, so a field the engine adds does not silently stay behind.
		let SyncReport {
			downloaded,
			uploaded,
			local_dirs_created,
			remote_dirs_created,
			locally_deleted,
			remotely_trashed,
			moved_remote,
			moved_local,
			conflicts,
			held,
			unsyncable,
			ignored,
			refused,
			deferred_paths,
			guard,
			deletion_token,
			errors,
			quarantined,
			paused,
			interrupted,
			halted,
			full_pass,
			// What these say for JS is `pass_error`, read above.
			lock_failed: _,
			store_failed: _,
			// The engine's own bookkeeping: `errors` renders the failures for people.
			dropped_actions: _,
			read_cost: _,
			failed_paths: _,
		} = report;
		let (conflicts, conflicts_total) = capped(conflicts);
		let (held, held_total) = capped(held);
		let (unsyncable, unsyncable_total) = capped(unsyncable);
		let (ignored, ignored_total) = capped(ignored);
		let (errors, errors_total) = capped(errors);
		let (quarantined, quarantined_total) = capped(quarantined);
		Self {
			downloaded: downloaded as u64,
			uploaded: uploaded as u64,
			local_dirs_created: local_dirs_created as u64,
			remote_dirs_created: remote_dirs_created as u64,
			locally_deleted: locally_deleted as u64,
			remotely_trashed: remotely_trashed as u64,
			moved_remote: moved_remote as u64,
			moved_local: moved_local as u64,
			conflicts,
			conflicts_total,
			held,
			held_total,
			held_deletions,
			unsyncable,
			unsyncable_total,
			ignored,
			ignored_total,
			refused,
			deferred_paths: deferred_paths as u64,
			guard: guard.map(Into::into),
			deletion_token,
			errors,
			errors_total,
			quarantined,
			quarantined_total,
			paused,
			interrupted: interrupted as u64,
			halted,
			full_pass_reason: full_pass.map(|reason| reason.to_string()),
			pass_error,
		}
	}
}

/// Mirror of [`SyncEvent`]: the same variants in the same order, with the same meaning.
#[js_type(export, no_deser, tagged)]
pub enum SyncEventMessage {
	PassStarted {
		mode: SyncMode,
	},
	Refused {
		reason: String,
	},
	/// How many actions the pass will apply: the denominator of its progress.
	Planned {
		actions: u64,
	},
	Conflict {
		rel_path: String,
	},
	DeletionsHeld {
		count: u64,
		reason: String,
		pass_token: String,
	},
	/// Sent when the upload FINISHED, after its `Progress`.
	Uploading {
		rel_path: String,
	},
	/// Sent when the download FINISHED, after its `Progress`.
	Downloading {
		rel_path: String,
	},
	/// Bytes moved so far by a transfer; `bytes` can exceed `total`.
	Progress {
		rel_path: String,
		direction: TransferDirection,
		bytes: u64,
		total: u64,
	},
	CreatingRemoteDir {
		rel_path: String,
	},
	CreatingLocalDir {
		rel_path: String,
	},
	TrashingRemote {
		rel_path: String,
	},
	DeletingLocal {
		rel_path: String,
	},
	MovingRemote {
		from: String,
		to: String,
	},
	MovingLocal {
		from: String,
		to: String,
	},
	/// A local copy moved into the quarantine bin to make way for a write; `bin_path` is where it
	/// went (lossy UTF-8).
	Quarantined {
		rel_path: String,
		bin_path: String,
	},
	ActionFailed {
		rel_path: String,
		error: String,
	},
	Interrupted {
		actions: u64,
	},
	AdoptedBaseline {
		rel_path: String,
	},
	PassCompleted {
		report: SyncPassReport,
	},
	PassFailed {
		error: String,
	},
}

impl From<SyncEvent> for SyncEventMessage {
	fn from(event: SyncEvent) -> Self {
		match event {
			SyncEvent::PassStarted { mode } => Self::PassStarted { mode },
			SyncEvent::Refused { reason } => Self::Refused { reason },
			SyncEvent::Planned { actions } => Self::Planned {
				actions: actions as u64,
			},
			SyncEvent::Conflict { rel_path } => Self::Conflict { rel_path },
			SyncEvent::DeletionsHeld {
				count,
				reason,
				pass_token,
			} => Self::DeletionsHeld {
				count: count as u64,
				reason,
				pass_token,
			},
			SyncEvent::Uploading { rel_path } => Self::Uploading { rel_path },
			SyncEvent::Downloading { rel_path } => Self::Downloading { rel_path },
			SyncEvent::Progress {
				rel_path,
				direction,
				bytes,
				total,
			} => Self::Progress {
				rel_path,
				direction,
				bytes,
				total,
			},
			SyncEvent::CreatingRemoteDir { rel_path } => Self::CreatingRemoteDir { rel_path },
			SyncEvent::CreatingLocalDir { rel_path } => Self::CreatingLocalDir { rel_path },
			SyncEvent::TrashingRemote { rel_path } => Self::TrashingRemote { rel_path },
			SyncEvent::DeletingLocal { rel_path } => Self::DeletingLocal { rel_path },
			SyncEvent::MovingRemote { from, to } => Self::MovingRemote { from, to },
			SyncEvent::MovingLocal { from, to } => Self::MovingLocal { from, to },
			SyncEvent::Quarantined { rel_path, bin_path } => Self::Quarantined {
				rel_path,
				bin_path: path_string(bin_path),
			},
			SyncEvent::ActionFailed { rel_path, error } => Self::ActionFailed { rel_path, error },
			SyncEvent::Interrupted { actions } => Self::Interrupted {
				actions: actions as u64,
			},
			SyncEvent::AdoptedBaseline { rel_path } => Self::AdoptedBaseline { rel_path },
			SyncEvent::PassCompleted { report } => Self::PassCompleted {
				report: report.into(),
			},
			SyncEvent::PassFailed { error } => Self::PassFailed { error },
		}
	}
}

/// One event of one pass of one pair.
#[js_type(export, no_deser)]
pub struct SyncPassEvent {
	/// The pair whose pass this is.
	pub pair_id: u64,
	/// Minted by the binding, increasing across the engine: every event of one pass carries the
	/// same id, and every `PassStarted` mints a fresh one. A pass's events end with its
	/// `PassCompleted` or `PassFailed`.
	pub pass_id: u64,
	/// When (Unix epoch milliseconds) the binding saw the event.
	pub emitted_at_ms: u64,
	pub event: SyncEventMessage,
}

/// What a dry run would do; its lists are paged separately.
#[js_type(export, no_deser)]
pub struct SyncPlanSummary {
	pub actions_total: u64,
	/// Bytes the planned uploads would send (saturating; for display).
	pub upload_bytes: u64,
	/// Bytes the planned downloads would fetch (saturating; for display).
	pub download_bytes: u64,
	pub held_total: u64,
	/// How many of the held actions are deletions.
	pub held_deletions: u64,
	pub held_reason: Option<SyncGuardReason>,
	/// The token to approve the held deletions with.
	pub pass_token: Option<String>,
	pub conflicts_total: u64,
	pub unsyncable_total: u64,
	pub ignored_total: u64,
	pub refused: Option<RefuseReason>,
}

impl From<&PlanOutcome> for SyncPlanSummary {
	fn from(outcome: &PlanOutcome) -> Self {
		// Whole, so a list the engine adds does not silently go uncounted.
		let PlanOutcome {
			actions,
			held,
			held_reason,
			pass_token,
			conflicts,
			unsyncable,
			ignored,
			refused,
		} = outcome;
		let bytes = |kind: PlannedActionKind| {
			actions
				.iter()
				.filter(|action| action.kind == kind)
				.filter_map(|action| action.size)
				.fold(0, u64::saturating_add)
		};
		Self {
			actions_total: actions.len() as u64,
			upload_bytes: bytes(PlannedActionKind::UploadFile),
			download_bytes: bytes(PlannedActionKind::DownloadFile),
			held_total: held.len() as u64,
			held_deletions: held
				.iter()
				.filter(|action| action.kind.is_deletion())
				.count() as u64,
			// The plan keeps its own: it is paged after this.
			held_reason: held_reason.clone().map(Into::into),
			pass_token: pass_token.clone(),
			conflicts_total: conflicts.len() as u64,
			unsyncable_total: unsyncable.len() as u64,
			ignored_total: ignored.len() as u64,
			refused: *refused,
		}
	}
}

/// One page of a pair's held conflicts, in path order.
#[js_type(export, no_deser)]
pub struct SyncConflictPage {
	pub conflicts: Vec<PlannedConflict>,
	/// How many conflicts the pair holds in all.
	pub total: u64,
}

/// The refusals a sync call fails with that JS can tell apart without reading the message.
#[js_type(export, no_deser, tagged)]
pub enum SyncRefusal {
	/// Another sync engine, in this process or another, has this baseline DB open.
	BaselineInUse { path: String },
	/// Sync pair `pair` already syncs this local folder.
	LocalRootInUse { pair: u64, existing: String },
	/// The local folder is inside sync pair `pair`'s.
	LocalRootNested { pair: u64, existing: String },
	/// The local folder contains sync pair `pair`'s.
	LocalRootContains { pair: u64, existing: String },
	/// Sync pair `pair` already syncs this remote folder.
	RemoteRootInUse { pair: u64, existing: Uuid },
	/// The remote folder is inside sync pair `pair`'s.
	RemoteRootNested { pair: u64, existing: Uuid },
	/// The remote folder contains sync pair `pair`'s.
	RemoteRootContains { pair: u64, existing: Uuid },
}

impl From<&BaselineInUse> for SyncRefusal {
	fn from(in_use: &BaselineInUse) -> Self {
		Self::BaselineInUse {
			path: in_use.path.to_string_lossy().into_owned(),
		}
	}
}

impl TryFrom<&PairOverlap> for SyncRefusal {
	type Error = Error;

	fn try_from(overlap: &PairOverlap) -> Result<Self, Error> {
		Ok(match overlap {
			PairOverlap::LocalRootInUse { pair, existing } => Self::LocalRootInUse {
				pair: pair_id_out(*pair)?,
				existing: existing.clone(),
			},
			PairOverlap::LocalRootNested { pair, existing } => Self::LocalRootNested {
				pair: pair_id_out(*pair)?,
				existing: existing.clone(),
			},
			PairOverlap::LocalRootContains { pair, existing } => Self::LocalRootContains {
				pair: pair_id_out(*pair)?,
				existing: existing.clone(),
			},
			PairOverlap::RemoteRootInUse { pair, existing } => Self::RemoteRootInUse {
				pair: pair_id_out(*pair)?,
				existing: *existing,
			},
			PairOverlap::RemoteRootNested { pair, existing } => Self::RemoteRootNested {
				pair: pair_id_out(*pair)?,
				existing: *existing,
			},
			PairOverlap::RemoteRootContains { pair, existing } => Self::RemoteRootContains {
				pair: pair_id_out(*pair)?,
				existing: *existing,
			},
		})
	}
}

/// The typed refusal `error` carries, if it is one.
pub(super) fn sync_refusal(error: &Error) -> Option<SyncRefusal> {
	if let Some(in_use) = error.downcast_ref::<BaselineInUse>() {
		return Some(in_use.into());
	}
	let overlap = error.downcast_ref::<PairOverlap>()?;
	match SyncRefusal::try_from(overlap) {
		Ok(refusal) => Some(refusal),
		Err(broken) => {
			tracing::warn!("sync refusal {overlap} cannot cross to JS: {broken}");
			None
		}
	}
}

#[cfg(test)]
mod tests {
	use std::{path::PathBuf, sync::Arc};

	use super::*;
	use crate::{
		auth::test_support::offline_client,
		sync_engine::{
			IgnoreLevel, PlannedActionKind, PlannedNodeKind, SyncEngine, UnsyncableReason,
		},
	};

	fn conflict(index: usize) -> PlannedConflict {
		PlannedConflict {
			rel_path: format!("conflict-{index}"),
			local: Some(PlannedNodeKind::File),
			remote: None,
		}
	}

	fn trash(index: usize) -> PlannedAction {
		PlannedAction {
			kind: PlannedActionKind::TrashRemote,
			rel_path: format!("trash-{index}"),
			node: PlannedNodeKind::File,
			size: Some(1),
		}
	}

	fn report_with(entries: usize) -> SyncReport {
		SyncReport {
			conflicts: (0..entries).map(conflict).collect(),
			held: (0..entries).map(trash).collect(),
			unsyncable: (0..entries)
				.map(|index| UnsyncablePath {
					rel_path: format!("unsyncable-{index}"),
					reason: UnsyncableReason::RemoteUndecodable,
				})
				.collect(),
			ignored: (0..entries)
				.map(|index| IgnoredPath {
					rel_path: format!("ignored-{index}"),
					level: IgnoreLevel::User,
					pattern: "*.tmp".to_owned(),
					tracked: false,
				})
				.collect(),
			errors: (0..entries).map(|index| format!("error-{index}")).collect(),
			quarantined: (0..entries)
				.map(|index| format!("quarantined-{index}"))
				.collect(),
			..SyncReport::default()
		}
	}

	fn list_lengths(report: &SyncPassReport) -> [usize; 6] {
		[
			report.conflicts.len(),
			report.held.len(),
			report.unsyncable.len(),
			report.ignored.len(),
			report.errors.len(),
			report.quarantined.len(),
		]
	}

	fn totals(report: &SyncPassReport) -> [u64; 6] {
		[
			report.conflicts_total,
			report.held_total,
			report.unsyncable_total,
			report.ignored_total,
			report.errors_total,
			report.quarantined_total,
		]
	}

	#[test]
	fn a_report_at_the_limit_keeps_every_entry() {
		let report = SyncPassReport::from(report_with(1000));

		assert_eq!(list_lengths(&report), [1000; 6]);
		assert_eq!(totals(&report), [1000; 6]);
		assert_eq!(report.held_deletions, 1000);
	}

	#[test]
	fn a_report_past_the_limit_keeps_the_first_entries_and_counts_them_all() {
		let report = SyncPassReport::from(report_with(1001));

		assert_eq!(list_lengths(&report), [1000; 6]);
		assert_eq!(totals(&report), [1001; 6]);
		assert_eq!(report.held_deletions, 1001);
		assert_eq!(report.conflicts.first(), Some(&conflict(0)));
		assert_eq!(report.conflicts.last(), Some(&conflict(999)));
		assert_eq!(report.errors.last().map(String::as_str), Some("error-999"));
	}

	#[test]
	fn a_report_carries_its_counts_and_why_its_pass_failed() {
		let report = SyncReport {
			downloaded: 2,
			uploaded: 3,
			interrupted: 4,
			halted: Some(HaltReason::LocalStorageFull),
			guard: Some(GuardReason::RemoteEmptied { deletions: 5 }),
			deletion_token: Some("token".to_owned()),
			..SyncReport::default()
		};
		let mirrored = SyncPassReport::from(report);

		assert_eq!(mirrored.downloaded, 2);
		assert_eq!(mirrored.uploaded, 3);
		assert_eq!(mirrored.interrupted, 4);
		assert_eq!(mirrored.halted, Some(HaltReason::LocalStorageFull));
		assert_eq!(
			mirrored.guard,
			Some(SyncGuardReason::RemoteEmptied { deletions: 5 })
		);
		assert_eq!(mirrored.deletion_token.as_deref(), Some("token"));
		assert_eq!(
			mirrored.pass_error.as_deref(),
			Some("the local disk is full: no further downloads were started this pass")
		);
		assert_eq!(SyncPassReport::from(SyncReport::default()).pass_error, None);
	}

	#[test]
	fn guard_reasons_map_with_their_counts() {
		let cases = [
			(GuardReason::ScanIncomplete, SyncGuardReason::ScanIncomplete),
			(
				GuardReason::FirstSyncWithDeletions { deletions: 1 },
				SyncGuardReason::FirstSyncWithDeletions { deletions: 1 },
			),
			(
				GuardReason::RemoteUnconverged { deletions: 2 },
				SyncGuardReason::RemoteUnconverged { deletions: 2 },
			),
			(
				GuardReason::RemoteEmptied { deletions: 3 },
				SyncGuardReason::RemoteEmptied { deletions: 3 },
			),
			(
				GuardReason::ExceededThreshold {
					deletions: 40,
					limit: 10,
				},
				SyncGuardReason::ExceededThreshold {
					deletions: 40,
					limit: 10,
				},
			),
		];
		for (reason, expected) in cases {
			assert_eq!(SyncGuardReason::from(reason), expected);
		}
	}

	#[test]
	fn events_map_with_their_payloads() {
		let cases = [
			(
				SyncEvent::Planned { actions: 7 },
				SyncEventMessage::Planned { actions: 7 },
			),
			(
				SyncEvent::DeletionsHeld {
					count: 12,
					reason: "too many".to_owned(),
					pass_token: "token".to_owned(),
				},
				SyncEventMessage::DeletionsHeld {
					count: 12,
					reason: "too many".to_owned(),
					pass_token: "token".to_owned(),
				},
			),
			(
				SyncEvent::Progress {
					rel_path: "a/b.txt".to_owned(),
					direction: TransferDirection::Download,
					bytes: 10,
					total: 20,
				},
				SyncEventMessage::Progress {
					rel_path: "a/b.txt".to_owned(),
					direction: TransferDirection::Download,
					bytes: 10,
					total: 20,
				},
			),
			(
				SyncEvent::Quarantined {
					rel_path: "a/b.txt".to_owned(),
					bin_path: PathBuf::from("/root/.filen-sync-trash/b.txt"),
				},
				SyncEventMessage::Quarantined {
					rel_path: "a/b.txt".to_owned(),
					bin_path: "/root/.filen-sync-trash/b.txt".to_owned(),
				},
			),
			(
				SyncEvent::MovingLocal {
					from: "a".to_owned(),
					to: "b".to_owned(),
				},
				SyncEventMessage::MovingLocal {
					from: "a".to_owned(),
					to: "b".to_owned(),
				},
			),
			(
				SyncEvent::Interrupted { actions: 3 },
				SyncEventMessage::Interrupted { actions: 3 },
			),
			(
				SyncEvent::PassFailed {
					error: "gone".to_owned(),
				},
				SyncEventMessage::PassFailed {
					error: "gone".to_owned(),
				},
			),
		];
		for (event, expected) in cases {
			assert_eq!(SyncEventMessage::from(event), expected);
		}
	}

	#[test]
	fn a_completed_pass_carries_its_capped_report() {
		let message = SyncEventMessage::from(SyncEvent::PassCompleted {
			report: report_with(1001),
		});

		let SyncEventMessage::PassCompleted { report } = message else {
			panic!("a completed pass mapped to {message:?}");
		};
		assert_eq!(list_lengths(&report), [1000; 6]);
		assert_eq!(totals(&report), [1001; 6]);
	}

	#[test]
	fn delete_guards_map_to_their_state() {
		assert_eq!(
			DeleteGuardState::from(DeleteGuard::default()),
			DeleteGuardState::Limited {
				floor: 10,
				ratio: 0.5
			}
		);
		assert_eq!(
			DeleteGuardState::from(DeleteGuard::unlimited()),
			DeleteGuardState::Unlimited
		);
		assert_eq!(
			DeleteGuardState::from(DeleteGuard::new(3, 0.25).unwrap()),
			DeleteGuardState::Limited {
				floor: 3,
				ratio: 0.25
			}
		);
	}

	#[test]
	fn pause_options_map_both_ways() {
		let options = PauseOptions::from(SyncPauseOptions {
			mode: PauseMode::Suspend,
			cancel_after_ms: Some(1500),
		});
		assert_eq!(options.mode, PauseMode::Suspend);
		assert_eq!(options.cancel_after, Some(Duration::from_millis(1500)));

		let never = PauseOptions::from(SyncPauseOptions {
			mode: PauseMode::Cancel,
			cancel_after_ms: None,
		});
		assert_eq!(never.cancel_after, None);

		assert_eq!(
			SyncPauseOptions::from(PauseOptions::default()),
			SyncPauseOptions {
				mode: PauseMode::Suspend,
				cancel_after_ms: Some(300_000),
			}
		);
	}

	#[test]
	fn a_negative_pair_id_is_an_internal_error() {
		assert_eq!(pair_id_out(7).unwrap(), 7);
		assert_eq!(pair_id_out(-1).unwrap_err().kind(), ErrorKind::Internal);
	}

	/// The refusal a second engine on an open baseline fails with, read off the engine's own error.
	#[tokio::test]
	async fn a_baseline_in_use_reads_as_its_refusal() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("baseline.db");
		let _first = SyncEngine::open(Arc::new(offline_client()), path.clone())
			.await
			.unwrap();

		let error = match SyncEngine::open(Arc::new(offline_client()), path.clone()).await {
			Ok(_) => panic!("a second engine opened a baseline the first one has open"),
			Err(error) => error,
		};
		assert_eq!(
			sync_refusal(&error),
			Some(SyncRefusal::BaselineInUse {
				path: path.to_string_lossy().into_owned(),
			})
		);
	}

	/// The refusals `add_pair` fails with for overlapping roots, read off the engine's own errors,
	/// naming the conflicting pair by its JS id.
	#[tokio::test]
	async fn an_overlapping_pair_reads_as_its_refusal() {
		let dir = tempfile::tempdir().unwrap();
		let outer = dir.path().join("outer");
		let inner = outer.join("inner");
		let other = dir.path().join("other");
		std::fs::create_dir_all(&inner).unwrap();
		std::fs::create_dir_all(&other).unwrap();
		let engine = SyncEngine::open(Arc::new(offline_client()), dir.path().join("baseline.db"))
			.await
			.unwrap();
		let remote = Uuid::from_u128(1);
		let pair = engine
			.add_pair(outer.clone(), remote, SyncMode::TwoWay)
			.await
			.unwrap();
		let pair = u64::try_from(pair).unwrap();

		let nested = engine
			.add_pair(inner, Uuid::from_u128(2), SyncMode::TwoWay)
			.await
			.unwrap_err();
		assert_eq!(
			sync_refusal(&nested),
			Some(SyncRefusal::LocalRootNested {
				pair,
				existing: std::fs::canonicalize(&outer)
					.unwrap()
					.to_string_lossy()
					.into_owned(),
			})
		);

		let same_remote = engine
			.add_pair(other, remote, SyncMode::TwoWay)
			.await
			.unwrap_err();
		assert_eq!(
			sync_refusal(&same_remote),
			Some(SyncRefusal::RemoteRootInUse {
				pair,
				existing: remote,
			})
		);
	}

	#[test]
	fn an_error_without_a_refusal_has_none() {
		let error = Error::custom(ErrorKind::InvalidState, "unknown sync pair");
		assert_eq!(sync_refusal(&error), None);
	}
}

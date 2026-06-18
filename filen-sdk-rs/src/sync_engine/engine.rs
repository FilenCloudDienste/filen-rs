//! The engine orchestration: register pairs, plan a pass (read-only), and run one (plan + apply).
//!
//! `prepare` runs the read-only half — load the baseline, scan the local tree (fast-path),
//! enumerate the remote subtree from the cache, build the remote view — shared by `plan_pair` (a
//! dry run) and `sync_once` (plan + guard + apply + baseline advance).

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use tokio::sync::Mutex;
use uuid::Uuid;

use super::{
	SyncEvent, SyncMode,
	apply::{self, ApplyContext, SyncReport},
	baseline::{BaselineEntry, BaselineStore, PairId, PairRecord},
	guard::{self, DeleteGuard, GuardReason},
	plan::{self, RemoteView, SyncAction},
	scan::{self, LocalScan, ScanError},
};
use crate::{
	Error, ErrorKind, auth::Client, fs::dir::cache::CacheableDir, fs::file::cache::CacheableFile,
};

/// A configured sync engine: an `Arc<Client>` (whose cache supplies the remote view) plus the
/// per-pair baseline store.
pub struct SyncEngine {
	pub(super) client: Arc<Client>,
	pub(super) store: Mutex<BaselineStore>,
}

/// Why the engine refused to act on a pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RefuseReason {
	/// Two remote items resolve to the same case-insensitive path.
	RemoteCollision,
	/// Two local items normalize to the same path.
	LocalCollision,
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
	dirs: Vec<CacheableDir<'static>>,
	files: Vec<CacheableFile<'static>>,
}

/// The outcome of `plan_pair` (a dry run).
#[derive(Debug)]
pub(crate) enum PlanOutcome {
	Refused(RefuseReason),
	Planned {
		actions: Vec<SyncAction>,
		held_deletions: Vec<SyncAction>,
		conflicts: Vec<String>,
		guard_reason: Option<GuardReason>,
	},
}

impl SyncEngine {
	/// Open the engine, creating the baseline DB at `db_path` if needed. The `client`'s cache must
	/// be configured (it supplies the remote view).
	pub async fn open(client: Arc<Client>, db_path: PathBuf) -> Result<Self, Error> {
		let store = tokio::task::spawn_blocking(move || BaselineStore::open(&db_path))
			.await
			.map_err(|e| {
				Error::custom(ErrorKind::Internal, format!("baseline open panicked: {e}"))
			})?
			.map_err(|e| db_error(e, "opening the sync baseline DB"))?;
		Ok(Self {
			client,
			store: Mutex::new(store),
		})
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
		self.store
			.lock()
			.await
			.create_pair(&local, remote_root, mode)
			.map_err(|e| db_error(e, "registering a sync pair"))
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

		let snapshot = self
			.client
			.enumerate_sync_root_snapshot(record.remote_root)
			.await?;
		let remote_view =
			plan::build_remote_view(record.remote_root, &snapshot.dirs, &snapshot.files);

		Ok(Prepared {
			record,
			baseline,
			local_scan,
			remote_view,
			remote_converged: snapshot.watermark.is_some(),
			dirs: snapshot.dirs,
			files: snapshot.files,
		})
	}

	/// Reconcile + guard-screen a pass without applying it (a dry run).
	pub(crate) async fn plan_pair(&self, pair: PairId) -> Result<PlanOutcome, Error> {
		let prep = self.prepare(pair).await?;
		if let Some(refusal) = refusal(&prep) {
			return Ok(PlanOutcome::Refused(refusal));
		}
		let (conflicts, decision) = reconcile_and_screen(&prep, screen_state(&prep));
		Ok(PlanOutcome::Planned {
			actions: decision.safe,
			held_deletions: decision.held,
			conflicts,
			guard_reason: decision.reason,
		})
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

		if let Some(refusal) = refusal(&prep) {
			tracing::debug!("sync_once[pair {pair}]: refused — {refusal:?}");
			let reason = format!("name collision ({refusal:?})");
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
		let (conflicts, decision) = reconcile_and_screen(&prep, state);
		report.conflicts = conflicts;
		// `held` can also carry the create half of a held type flip; the report counts deletions.
		report.held_deletions = decision.held.iter().filter(|a| a.is_delete()).count();
		report.guard_message = decision.reason.map(|reason| format!("{reason:?}"));

		for rel_path in &report.conflicts {
			observer(SyncEvent::Conflict {
				rel_path: rel_path.clone(),
			});
		}
		if report.held_deletions > 0 {
			observer(SyncEvent::DeletionsHeld {
				count: report.held_deletions,
				reason: report.guard_message.clone().unwrap_or_default(),
			});
		}
		observer(SyncEvent::Planned {
			actions: decision.safe.len(),
		});

		if decision.safe.is_empty() {
			tracing::debug!(
				"sync_once[pair {pair}]: nothing to apply ({} deletion(s) held, {} conflict(s))",
				report.held_deletions,
				report.conflicts.len(),
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
			remote: &prep.remote_view.nodes,
			root_remote,
			absence_trusted: state.absence_trusted(),
			dirs: &prep.dirs,
			files: &prep.files,
		};
		apply::apply(ctx, decision.safe, &mut report, observer).await;
		tracing::debug!(
			"sync_once[pair {pair}]: done — {} uploaded, {} downloaded, {} remote dir(s), {} local dir(s), {} trashed, {} locally deleted, {} moved remote, {} moved local, {} conflict(s), {} held, {} error(s)",
			report.uploaded,
			report.downloaded,
			report.remote_dirs_created,
			report.local_dirs_created,
			report.remotely_trashed,
			report.locally_deleted,
			report.moved_remote,
			report.moved_local,
			report.conflicts.len(),
			report.held_deletions,
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

/// Reconcile the prepared inputs and screen deletions through the guard, splitting out conflicts.
fn reconcile_and_screen(
	prep: &Prepared,
	state: guard::ScreenState,
) -> (Vec<String>, guard::GuardDecision) {
	let all_actions = plan::reconcile(
		prep.record.mode,
		&prep.baseline,
		&prep.local_scan.nodes,
		&prep.remote_view.nodes,
	);
	let (conflict_actions, executable): (Vec<_>, Vec<_>) = all_actions
		.into_iter()
		.partition(|a| matches!(a, SyncAction::Conflict { .. }));
	let conflicts = conflict_actions
		.into_iter()
		.map(|a| a.rel_path().to_string())
		.collect();
	let decision = guard::screen(executable, state, DeleteGuard::default());
	(conflicts, decision)
}

/// How trustworthy this pass's inputs are, for the guard.
fn screen_state(prep: &Prepared) -> guard::ScreenState {
	guard::ScreenState {
		scan_complete: prep.local_scan.complete,
		remote_converged: prep.remote_converged,
		// A wholly empty remote view while the baseline still tracks remote items: what a
		// transient backend/cache fault looks like, and it would otherwise delete the whole pair.
		remote_emptied: prep.remote_view.nodes.is_empty()
			&& prep.baseline.values().any(|e| e.remote_uuid.is_some()),
		first_sync: prep.baseline.is_empty(),
		tracked: prep.baseline.len(),
	}
}

fn db_error(error: rusqlite::Error, context: &str) -> Error {
	Error::custom_with_source(ErrorKind::Internal, error, Some(context.to_string()))
}

//! The engine orchestration: register pairs and compute a guarded sync plan for one.
//!
//! `plan_pair` runs the whole read-only half of a sync pass — load the baseline, scan the local
//! tree (with the fast-path), enumerate the remote subtree from the cache, build the remote view,
//! reconcile, and screen the result through the mass-delete guard — and returns the ordered,
//! safe-to-apply actions plus anything held back (deletions over the guard, surfaced conflicts) or
//! a refusal (a name collision that makes a 1:1 mapping impossible). Executing the plan against the
//! network is the apply layer, layered on top of this.

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use tokio::sync::Mutex;
use uuid::Uuid;

use super::{
	SyncMode,
	baseline::{BaselineEntry, BaselineStore, PairId},
	guard::{self, DeleteGuard, GuardReason},
	plan::{self, SyncAction},
	scan::{self, ScanError},
};
use crate::{Error, ErrorKind, auth::Client};

/// A configured sync engine: an `Arc<Client>` (whose cache supplies the remote view) plus the
/// per-pair baseline store.
pub struct SyncEngine {
	client: Arc<Client>,
	store: Mutex<BaselineStore>,
}

/// Why the engine refused to plan a pass (rather than producing a partial plan).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RefuseReason {
	/// Two remote items resolve to the same case-insensitive path — a 1:1 local mapping is
	/// impossible; the user must rename one.
	RemoteCollision,
	/// Two local items normalize to the same path.
	LocalCollision,
}

/// The outcome of planning one pass.
#[derive(Debug)]
pub(crate) enum PlanOutcome {
	/// A name collision blocks reconciliation until the user resolves it.
	Refused(RefuseReason),
	/// The reconciled, guard-screened plan.
	Planned(SyncPlan),
}

/// The result of reconciling + screening one pass.
#[derive(Debug)]
pub(crate) struct SyncPlan {
	/// Ordered actions safe to apply now (creates/transfers, plus deletions within the guard).
	pub(crate) actions: Vec<SyncAction>,
	/// Deletions the guard held back (with [`guard_reason`](Self::guard_reason) set).
	pub(crate) held_deletions: Vec<SyncAction>,
	/// Relative paths surfaced as two-way conflicts (left untouched for the caller to resolve).
	pub(crate) conflicts: Vec<String>,
	/// Set when the guard held deletions.
	pub(crate) guard_reason: Option<GuardReason>,
	/// The remote watermark at the snapshot instant, for aligning a future live event stream.
	pub(crate) watermark: Option<u64>,
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
			.map_err(|e| {
				Error::custom_with_source(
					ErrorKind::Internal,
					e,
					Some("opening the sync baseline DB".to_string()),
				)
			})?;
		Ok(Self {
			client,
			store: Mutex::new(store),
		})
	}

	/// Register a sync pair (idempotent for the same `(local_root, remote_root)`), returning its id.
	/// The `remote_root` must be a sync root the cache covers (the engine reads its subtree from the
	/// cache).
	pub async fn add_pair(
		&self,
		local_root: PathBuf,
		remote_root: Uuid,
		mode: SyncMode,
	) -> Result<PairId, Error> {
		let local = local_root.to_string_lossy().into_owned();
		self.store
			.lock()
			.await
			.create_pair(&local, remote_root, mode)
			.map_err(|e| {
				Error::custom_with_source(
					ErrorKind::Internal,
					e,
					Some("registering a sync pair".to_string()),
				)
			})
	}

	/// Compute the guarded plan for one pass over `pair` — the read-only half of a sync.
	pub(crate) async fn plan_pair(&self, pair: PairId) -> Result<PlanOutcome, Error> {
		let store = self.store.lock().await;
		let record = store
			.pair(pair)
			.map_err(|e| db_error(e, "loading the sync pair"))?
			.ok_or_else(|| Error::custom(ErrorKind::InvalidState, "unknown sync pair"))?;
		let baseline_entries = store
			.entries(pair)
			.map_err(|e| db_error(e, "loading the baseline"))?;
		drop(store);

		let baseline: Arc<HashMap<String, BaselineEntry>> = Arc::new(
			baseline_entries
				.into_iter()
				.map(|entry| (entry.rel_path.clone(), entry))
				.collect(),
		);

		// Local scan (blocking FS + hashing) off the async runtime.
		let local_root = PathBuf::from(&record.local_root);
		let scan_baseline = Arc::clone(&baseline);
		let local_scan =
			tokio::task::spawn_blocking(move || scan::scan_local(&local_root, &scan_baseline))
				.await
				.map_err(|e| {
					Error::custom(ErrorKind::Internal, format!("local scan panicked: {e}"))
				})?;

		if local_scan
			.errors
			.iter()
			.any(|e| matches!(e, ScanError::DuplicateName { .. }))
		{
			return Ok(PlanOutcome::Refused(RefuseReason::LocalCollision));
		}

		// Remote view from the cache snapshot of this root's subtree.
		let snapshot = self
			.client
			.enumerate_sync_root_snapshot(record.remote_root)
			.await?;
		let remote_view =
			plan::build_remote_view(record.remote_root, &snapshot.dirs, &snapshot.files);
		if remote_view.has_collisions {
			return Ok(PlanOutcome::Refused(RefuseReason::RemoteCollision));
		}

		let all_actions = plan::reconcile(
			record.mode,
			&baseline,
			&local_scan.nodes,
			&remote_view.nodes,
		);

		// Conflicts are reported, not executed; everything else goes through the guard.
		let (conflict_actions, executable): (Vec<_>, Vec<_>) = all_actions
			.into_iter()
			.partition(|a| matches!(a, SyncAction::Conflict { .. }));
		let conflicts = conflict_actions
			.into_iter()
			.map(|a| a.rel_path().to_string())
			.collect();

		let decision = guard::screen(
			executable,
			local_scan.complete,
			baseline.len(),
			DeleteGuard::default(),
		);

		Ok(PlanOutcome::Planned(SyncPlan {
			actions: decision.safe,
			held_deletions: decision.held,
			conflicts,
			guard_reason: decision.reason,
			watermark: snapshot.watermark,
		}))
	}
}

fn db_error(error: rusqlite::Error, context: &str) -> Error {
	Error::custom_with_source(ErrorKind::Internal, error, Some(context.to_string()))
}

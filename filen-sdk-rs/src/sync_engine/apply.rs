//! The apply layer: execute a screened [`SyncAction`] plan against the remote (via `Client`) and
//! the local filesystem, advancing the baseline after each action so an interrupted pass simply
//! re-reconciles to the same plan.
//!
//! Each action is best-effort: a failure is recorded in the [`SyncReport`] and the pass continues,
//! since the actions are independent and the next pass re-plans from truth. Local deletions move
//! to the pair's quarantine dir (recoverable); remote deletions go to Filen trash. The local mtime
//! written to the baseline after a download is read back from disk (not the value we asked for) so
//! the scanner's fast-path stays stable.

use std::{
	collections::HashMap,
	path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use futures::StreamExt;
use uuid::Uuid;

use super::{
	baseline::{BaselineEntry, BaselineState, BaselineStore, NodeKind, PairId},
	events::SyncEvent,
	plan::{RemoteNode, SyncAction, create_target_paths},
	scan::{LocalNode, QUARANTINE_DIR},
};
use crate::{
	auth::Client,
	fs::{
		HasUUID,
		categories::{DirType, Normal},
		dir::cache::CacheableDir,
		file::{cache::CacheableFile, meta::FileMetaChanges},
	},
	io::{RemoteDirectory, RemoteFile, client_impl::IoSharedClientExt},
};
use tokio::sync::Mutex;

/// Outcome of one apply pass.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SyncReport {
	pub downloaded: usize,
	pub uploaded: usize,
	pub local_dirs_created: usize,
	pub remote_dirs_created: usize,
	pub locally_deleted: usize,
	pub remotely_trashed: usize,
	/// Files re-parented/renamed on the remote in place of a re-upload.
	pub moved_remote: usize,
	/// Files renamed locally in place of a re-download.
	pub moved_local: usize,
	/// Relative paths surfaced as two-way conflicts (left untouched).
	pub conflicts: Vec<String>,
	/// How many deletions the mass-delete guard held back this pass.
	pub held_deletions: usize,
	/// Set when the guard held deletions; a human-readable reason.
	pub guard_message: Option<String>,
	/// Per-action failures (the pass continues past them).
	pub errors: Vec<String>,
}

/// Everything the apply pass needs besides the actions: the resolved remote-side objects and the
/// scan/view maps.
pub(super) struct ApplyContext<'a> {
	pub(super) client: &'a Client,
	pub(super) local_root: &'a Path,
	pub(super) pair: PairId,
	pub(super) store: &'a Mutex<BaselineStore>,
	pub(super) local: &'a HashMap<String, LocalNode>,
	pub(super) remote: &'a HashMap<String, RemoteNode>,
	/// The sync root resolved to a remote directory (every top-level parent).
	pub(super) root_remote: RemoteDirectory,
	/// Whether this pass's absence evidence is trustworthy (see
	/// [`ScreenState::absence_trusted`](super::guard::ScreenState::absence_trusted)) — gates
	/// dropping a baseline row because both sides look gone.
	pub(super) absence_trusted: bool,
	pub(super) dirs: &'a [CacheableDir<'static>],
	pub(super) files: &'a [CacheableFile<'static>],
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
/// The drive-write lock is taken ONCE for the whole pass (when anything mutates the remote) so the
/// per-op `lock_drive` calls inside upload/create become free clones of the held lock instead of
/// each doing an acquire+release server round-trip — the dominant per-file cost at scale.
///
/// File transfers (uploads/downloads) — the bulk of the work and independent of one another — run
/// CONCURRENTLY, bounded by the client's configured concurrency ([`ClientConfig::with_concurrency`],
/// default 16) and the global request rate limiter. Directory creates/moves (which populate the
/// parent map and have parent-before-child ordering) and deletes (descendant-first for cascade
/// safety) stay serial.
///
/// Each action emits its [`SyncEvent`] to `observer` (and an [`ActionFailed`](SyncEvent::ActionFailed)
/// if it errors): serial actions when they start, concurrent transfers as each completes.
pub(super) async fn apply(
	ctx: ApplyContext<'_>,
	actions: Vec<SyncAction>,
	report: &mut SyncReport,
	observer: &mut (dyn FnMut(SyncEvent) + Send),
) {
	let file_by_uuid: HashMap<Uuid, &CacheableFile<'static>> =
		ctx.files.iter().map(|f| (f.uuid, f)).collect();
	let dir_by_uuid: HashMap<Uuid, &CacheableDir<'static>> =
		ctx.dirs.iter().map(|d| (d.uuid, d)).collect();

	// path -> remote directory, for resolving the parent of an item. Seeded with the root at "" and
	// every existing remote dir; new dirs are added as they are created.
	let mut dir_by_path: HashMap<String, RemoteDirectory> = HashMap::new();
	dir_by_path.insert(String::new(), ctx.root_remote.clone());
	for (path, node) in ctx.remote {
		if node.kind == NodeKind::Dir
			&& let Some(cacheable) = dir_by_uuid.get(&node.remote_uuid)
		{
			dir_by_path.insert(path.clone(), RemoteDirectory::from((*cacheable).clone()));
		}
	}

	// Hold the drive-write lock for the whole pass when it mutates the remote (a pure pull touches
	// only local files and needs no lock). Inner `lock_drive` calls then return a clone of this.
	let _drive_lock = if actions.iter().any(mutates_remote) {
		match ctx.client.lock_drive().await {
			Ok(lock) => Some(lock),
			Err(error) => {
				report
					.errors
					.push(format!("failed to acquire the drive lock: {error}"));
				return;
			}
		}
	} else {
		None
	};

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

	for action in &pre {
		apply_serial(
			&ctx,
			action,
			&file_by_uuid,
			&mut dir_by_path,
			report,
			observer,
		)
		.await;
	}

	if !transfers.is_empty() {
		let concurrency = ctx.client.unauthed().state().max_concurrency().max(1);
		let ctx_ref = &ctx;
		let dir_ref = &dir_by_path;
		let files_ref = &file_by_uuid;
		let mut stream = std::pin::pin!(
			futures::stream::iter(transfers.iter())
				.map(|action| async move {
					(
						action,
						apply_transfer(ctx_ref, action, files_ref, dir_ref).await,
					)
				})
				.buffer_unordered(concurrency)
		);
		while let Some((action, result)) = stream.next().await {
			match result {
				Ok(()) => {
					tracing::debug!("apply: {} done", action.describe());
					observer(action.to_event());
					match action {
						SyncAction::UploadFile { .. } => report.uploaded += 1,
						SyncAction::DownloadFile { .. } => report.downloaded += 1,
						_ => {}
					}
				}
				Err(error) => {
					tracing::debug!("apply: {} FAILED — {error}", action.describe());
					observer(SyncEvent::ActionFailed {
						rel_path: action.rel_path().to_string(),
						error: error.to_string(),
					});
					report
						.errors
						.push(format!("{}: {error}", action.rel_path()));
				}
			}
		}
	}

	for action in &post {
		apply_serial(
			&ctx,
			action,
			&file_by_uuid,
			&mut dir_by_path,
			report,
			observer,
		)
		.await;
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
async fn apply_serial(
	ctx: &ApplyContext<'_>,
	action: &SyncAction,
	file_by_uuid: &HashMap<Uuid, &CacheableFile<'static>>,
	dir_by_path: &mut HashMap<String, RemoteDirectory>,
	report: &mut SyncReport,
	observer: &mut (dyn FnMut(SyncEvent) + Send),
) {
	observer(action.to_event());
	tracing::debug!("apply: {}", action.describe());
	if let Err(error) = apply_one(ctx, action, file_by_uuid, dir_by_path, report).await {
		tracing::debug!("apply: {} FAILED — {error}", action.describe());
		observer(SyncEvent::ActionFailed {
			rel_path: action.rel_path().to_string(),
			error: error.to_string(),
		});
		report
			.errors
			.push(format!("{}: {error}", action.rel_path()));
	}
}

/// Perform a transfer (download or upload): the network op + baseline write only. It touches no
/// shared mutable state — it READS `dir_by_path` and the baseline store is internally locked — so
/// transfers run concurrently; the caller does the report/event accounting as each completes.
async fn apply_transfer(
	ctx: &ApplyContext<'_>,
	action: &SyncAction,
	file_by_uuid: &HashMap<Uuid, &CacheableFile<'static>>,
	dir_by_path: &HashMap<String, RemoteDirectory>,
) -> Result<(), crate::Error> {
	match action {
		SyncAction::DownloadFile {
			rel_path,
			remote_uuid,
		} => {
			let cacheable = file_by_uuid
				.get(remote_uuid)
				.ok_or_else(|| internal("download target missing from the snapshot"))?;
			let remote_file = RemoteFile::from((*cacheable).clone());
			let path = confined_local_target(ctx.local_root, rel_path)?;
			if let Some(parent) = path.parent() {
				std::fs::create_dir_all(parent).map_err(io_err)?;
			}
			ctx.client
				.download_file_to_path(&remote_file, &path, None)
				.await?;
			let remote = ctx.remote.get(rel_path);
			upsert_file_baseline(
				ctx,
				rel_path,
				Some(*remote_uuid),
				remote.and_then(|n| n.content_hash),
				remote.map(|n| n.size).unwrap_or(0),
				local_mtime_of(&path),
				remote.map(|n| n.modified_millis),
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
			let (uploaded, _file) = ctx
				.client
				.upload_file_from_path(&parent_type, path, None)
				.await?;
			let local = ctx.local.get(rel_path);
			let new_uuid: Uuid = uploaded.uuid();
			upsert_file_baseline(
				ctx,
				rel_path,
				Some(new_uuid),
				local.and_then(|n| n.content_hash),
				local.map(|n| n.size).unwrap_or(0),
				local.map(|n| n.mtime_millis),
				Some(uploaded.timestamp.timestamp_millis()),
			)
			.await?;
		}
		_ => return Err(internal("apply_transfer called with a non-transfer action")),
	}
	Ok(())
}

async fn apply_one(
	ctx: &ApplyContext<'_>,
	action: &SyncAction,
	file_by_uuid: &HashMap<Uuid, &CacheableFile<'static>>,
	dir_by_path: &mut HashMap<String, RemoteDirectory>,
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
			apply_transfer(ctx, action, file_by_uuid, dir_by_path).await?;
			report.downloaded += 1;
		}
		SyncAction::DeleteLocal { rel_path, .. } => {
			quarantine_local(ctx.local_root, rel_path)?;
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
			dir_by_path.insert(rel_path.clone(), new_dir);
			upsert_dir_baseline(ctx, rel_path, Some(new_uuid), None).await?;
			report.remote_dirs_created += 1;
		}
		SyncAction::UploadFile { .. } => {
			apply_transfer(ctx, action, file_by_uuid, dir_by_path).await?;
			report.uploaded += 1;
		}
		SyncAction::TrashRemote {
			rel_path,
			kind,
			remote_uuid,
		} => {
			match kind {
				NodeKind::File => {
					let cacheable = file_by_uuid
						.get(remote_uuid)
						.ok_or_else(|| internal("trash target file missing from the snapshot"))?;
					let mut remote_file = RemoteFile::from((*cacheable).clone());
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
			delete_baseline(ctx, rel_path).await?;
			report.remotely_trashed += 1;
		}
		SyncAction::MoveRemote {
			from_path,
			to_path,
			remote_uuid,
		} => {
			let cacheable = file_by_uuid
				.get(remote_uuid)
				.ok_or_else(|| internal("move-source file missing from the snapshot"))?;
			let mut remote_file = RemoteFile::from((*cacheable).clone());
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
			delete_baseline(ctx, from_path).await?;
			upsert_file_baseline(
				ctx,
				to_path,
				Some(*remote_uuid),
				local.and_then(|n| n.content_hash),
				local.map(|n| n.size).unwrap_or(0),
				local.map(|n| n.mtime_millis),
				Some(remote_file.timestamp.timestamp_millis()),
			)
			.await?;
			report.moved_remote += 1;
		}
		SyncAction::MoveLocal { from_path, to_path } => {
			let from = local_path(ctx.local_root, from_path);
			let to = confined_local_target(ctx.local_root, to_path)?;
			if let Some(parent) = to.parent() {
				std::fs::create_dir_all(parent).map_err(io_err)?;
			}
			std::fs::rename(&from, &to).map_err(io_err)?;
			let remote = ctx.remote.get(to_path);
			delete_baseline(ctx, from_path).await?;
			upsert_file_baseline(
				ctx,
				to_path,
				remote.map(|n| n.remote_uuid),
				remote.and_then(|n| n.content_hash),
				remote.map(|n| n.size).unwrap_or(0),
				local_mtime_of(&to),
				remote.map(|n| n.modified_millis),
			)
			.await?;
			report.moved_local += 1;
		}
		SyncAction::Conflict { .. } => {
			// Conflicts are reported by the engine, never applied here.
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

/// What an [`SyncAction::AdoptBaseline`] writes for one path. Decided purely (no I/O) so the
/// policy is unit-testable.
#[derive(Debug, PartialEq, Eq)]
enum AdoptOutcome {
	/// Both sides hold the item and agree — record the converged state.
	Record(BaselineEntry),
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
		(Some(local), Some(remote)) => AdoptOutcome::Record(BaselineEntry {
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
		}),
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

fn local_mtime_of(path: &Path) -> Option<i64> {
	use crate::io::FilenMetaExt;
	std::fs::metadata(path)
		.ok()
		.map(|m| FilenMetaExt::modified(&m).timestamp_millis())
}

async fn upsert_dir_baseline(
	ctx: &ApplyContext<'_>,
	rel_path: &str,
	remote_uuid: Option<Uuid>,
	local_mtime: Option<i64>,
) -> Result<(), crate::Error> {
	let entry = BaselineEntry {
		rel_path: rel_path.to_string(),
		kind: NodeKind::Dir,
		remote_uuid,
		content_hash: None,
		size: None,
		local_mtime,
		remote_modified: None,
		state: BaselineState::Synced,
	};
	upsert_baseline(ctx, &entry).await
}

#[allow(clippy::too_many_arguments)]
async fn upsert_file_baseline(
	ctx: &ApplyContext<'_>,
	rel_path: &str,
	remote_uuid: Option<Uuid>,
	content_hash: Option<filen_types::crypto::Blake3Hash>,
	size: u64,
	local_mtime: Option<i64>,
	remote_modified: Option<i64>,
) -> Result<(), crate::Error> {
	let entry = BaselineEntry {
		rel_path: rel_path.to_string(),
		kind: NodeKind::File,
		remote_uuid,
		content_hash,
		size: Some(size),
		local_mtime,
		remote_modified,
		state: BaselineState::Synced,
	};
	upsert_baseline(ctx, &entry).await
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
fn quarantine_local(root: &Path, rel_path: &str) -> Result<(), crate::Error> {
	let source = local_path(root, rel_path);
	// A missing source is a no-op — e.g. it already moved as part of an ancestor's quarantine.
	if source.symlink_metadata().is_err() {
		return Ok(());
	}
	let dest = unique_quarantine_dest(local_path(&root.join(QUARANTINE_DIR), rel_path));
	if let Some(parent) = dest.parent() {
		std::fs::create_dir_all(parent).map_err(io_err)?;
	}
	std::fs::rename(&source, &dest).map_err(io_err)
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
	use filen_types::crypto::Blake3Hash;
	use uuid::Uuid;

	use super::*;

	fn temp_dir() -> PathBuf {
		let dir = std::env::temp_dir().join(format!("filen_confine_test_{}", Uuid::new_v4()));
		std::fs::create_dir_all(&dir).unwrap();
		dir
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
		quarantine_local(&root, "x.txt").unwrap();
		assert_eq!(std::fs::read(trash.join("x.txt")).unwrap(), b"first");

		// A second deletion at the same rel path must NOT clobber the first quarantined copy.
		std::fs::write(root.join("x.txt"), b"second").unwrap();
		quarantine_local(&root, "x.txt").unwrap();
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
		RemoteNode {
			rel_path: rel.to_string(),
			kind: NodeKind::File,
			remote_uuid: Uuid::new_v4(),
			content_hash: hash,
			size: 3,
			modified_millis: 900,
		}
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

	#[test]
	fn quarantine_of_a_missing_source_is_a_noop() {
		let root = temp_dir();
		// e.g. the item already moved as part of an ancestor's quarantine.
		quarantine_local(&root, "already/gone.txt").expect("missing source is a no-op");
		std::fs::remove_dir_all(&root).ok();
	}
}

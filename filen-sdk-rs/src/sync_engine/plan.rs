//! The pure planning layer: build the remote-side view from a cache snapshot, then reconcile the
//! three inputs (baseline, local scan, remote view) into an ordered list of [`SyncAction`]s.
//!
//! `reconcile` is deliberately side-effect-free and synchronous so the whole decision matrix is
//! unit-testable without a filesystem or network. One-way modes converge the destination onto the
//! source (`make dst match src`, gated by whether deletions propagate); two-way uses the baseline
//! to tell which side changed and surfaces a genuine both-sides-changed divergence as a conflict.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use filen_types::{crypto::Blake3Hash, fs::StableUuid};
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

use super::{
	baseline::{BaselineEntry, BaselineState, NodeKind},
	events::SyncEvent,
	scan::{LocalNode, QUARANTINE_DIR, collision_key},
};
use crate::fs::{dir::cache::CacheableDir, file::cache::CacheableFile};

/// Guards against a malformed (cyclic) remote parent chain when resolving a path.
const MAX_REMOTE_DEPTH: usize = 256;

/// One item in the remote view, keyed (like [`LocalNode`]) by its NFC-normalized `/`-joined path
/// relative to the sync root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteNode {
	pub(crate) rel_path: String,
	pub(crate) kind: NodeKind,
	pub(crate) remote_uuid: Uuid,
	/// The server-minted whole-life id of a FILE: `remote_uuid` is re-minted by every content edit
	/// and version restore, this is not. `None` for a directory, which has no such id — a
	/// directory's own uuid already survives its renames.
	pub(crate) stable_uuid: Option<StableUuid>,
	/// BLAKE3 of the content (files); `None` for dirs and for older files the server stored
	/// without a hash.
	pub(crate) content_hash: Option<Blake3Hash>,
	pub(crate) size: u64,
	pub(crate) modified_millis: i64,
}

/// One action the apply layer should execute. Pull actions move remote -> local; push actions move
/// local -> remote; a conflict is left untouched for the caller to resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SyncAction {
	CreateLocalDir {
		rel_path: String,
	},
	DownloadFile {
		rel_path: String,
		remote_uuid: Uuid,
	},
	DeleteLocal {
		rel_path: String,
		kind: NodeKind,
	},
	CreateRemoteDir {
		rel_path: String,
	},
	UploadFile {
		rel_path: String,
	},
	TrashRemote {
		rel_path: String,
		kind: NodeKind,
		remote_uuid: Uuid,
	},
	Conflict {
		rel_path: String,
	},
	/// A path already identical on both sides (or already gone from both) but lacking a correct
	/// baseline row -> record the converged state into the baseline with NO network/FS transfer, so
	/// a later one-sided change at this path is classified correctly (a delete reads as a delete, an
	/// edit as an edit) rather than misread against an empty baseline. Emitted in every mode: a
	/// one-way pair opened on an already-synced tree transfers nothing, so this is the only thing
	/// that seeds its baseline — and until it has one, the guard treats every pass as a first sync.
	AdoptBaseline {
		rel_path: String,
	},
	/// An item moved/renamed on the local side -> re-parent + rename the remote item (uuid kept)
	/// instead of re-uploading its content or re-creating it.
	///
	/// A DIRECTORY move carries its whole subtree: it is planned by [`fold_dir_moves`], which
	/// re-keys the pass's inputs so every child is read at its new path and is a no-op unless it
	/// changed on its own. It runs before every other action of the pass.
	MoveRemote {
		from_path: String,
		to_path: String,
		kind: NodeKind,
		remote_uuid: Uuid,
	},
	/// An item moved/renamed on the remote side -> rename the local item instead of re-downloading
	/// it (a file) or quarantining and re-creating its subtree (a directory, see
	/// [`fold_dir_moves`]).
	MoveLocal {
		from_path: String,
		to_path: String,
		kind: NodeKind,
	},
}

impl SyncAction {
	/// The path that determines apply ORDER (the destination for a move). Parents sort before
	/// children, so a create of the destination's parent dir precedes a move into it.
	pub(super) fn rel_path(&self) -> &str {
		match self {
			Self::CreateLocalDir { rel_path }
			| Self::DownloadFile { rel_path, .. }
			| Self::DeleteLocal { rel_path, .. }
			| Self::CreateRemoteDir { rel_path }
			| Self::UploadFile { rel_path }
			| Self::TrashRemote { rel_path, .. }
			| Self::Conflict { rel_path }
			| Self::AdoptBaseline { rel_path } => rel_path,
			Self::MoveRemote { to_path, .. } | Self::MoveLocal { to_path, .. } => to_path,
		}
	}

	/// Both paths an action touches: `(from, to)` for a move or rename, the one path twice
	/// otherwise.
	pub(super) fn endpoints(&self) -> (&str, &str) {
		match self {
			Self::MoveRemote {
				from_path, to_path, ..
			}
			| Self::MoveLocal {
				from_path, to_path, ..
			} => (from_path, to_path),
			other => (other.rel_path(), other.rel_path()),
		}
	}

	pub(super) fn is_delete(&self) -> bool {
		matches!(self, Self::DeleteLocal { .. } | Self::TrashRemote { .. })
	}

	/// Whether this action MATERIALIZES an item at its path (a directory create or a content
	/// transfer). A delete whose path matches one of these in the SAME pass is a "replace" (a type
	/// flip — old item removed, new-kind item created at the same path), and the delete must run
	/// BEFORE the create or the server rejects it (the old same-name item still exists). See
	/// [`order_actions`] and the apply layer.
	pub(super) fn is_create(&self) -> bool {
		matches!(
			self,
			Self::CreateLocalDir { .. }
				| Self::CreateRemoteDir { .. }
				| Self::UploadFile { .. }
				| Self::DownloadFile { .. }
		)
	}

	/// A short human-readable description of the action, for `tracing::debug!` tracing of a pass.
	pub(super) fn describe(&self) -> String {
		match self {
			Self::CreateLocalDir { rel_path } => format!("create local dir {rel_path:?}"),
			Self::DownloadFile { rel_path, .. } => format!("download file {rel_path:?}"),
			Self::DeleteLocal { rel_path, kind } => {
				format!("delete local {kind:?} {rel_path:?} (to quarantine)")
			}
			Self::CreateRemoteDir { rel_path } => format!("create remote dir {rel_path:?}"),
			Self::UploadFile { rel_path } => format!("upload file {rel_path:?}"),
			Self::TrashRemote { rel_path, kind, .. } => {
				format!("trash remote {kind:?} {rel_path:?}")
			}
			Self::Conflict { rel_path } => format!("conflict {rel_path:?}"),
			Self::AdoptBaseline { rel_path } => format!("adopt baseline {rel_path:?}"),
			Self::MoveRemote {
				from_path,
				to_path,
				kind,
				..
			} => format!("move remote {kind:?} {from_path:?} -> {to_path:?}"),
			Self::MoveLocal {
				from_path,
				to_path,
				kind,
			} => format!("move local {kind:?} {from_path:?} -> {to_path:?}"),
		}
	}

	/// The "in progress" [`SyncEvent`] for this action, emitted by the apply layer just before it
	/// executes the action.
	pub(super) fn to_event(&self) -> SyncEvent {
		match self {
			Self::UploadFile { rel_path } => SyncEvent::Uploading {
				rel_path: rel_path.clone(),
			},
			Self::DownloadFile { rel_path, .. } => SyncEvent::Downloading {
				rel_path: rel_path.clone(),
			},
			Self::CreateRemoteDir { rel_path } => SyncEvent::CreatingRemoteDir {
				rel_path: rel_path.clone(),
			},
			Self::CreateLocalDir { rel_path } => SyncEvent::CreatingLocalDir {
				rel_path: rel_path.clone(),
			},
			Self::TrashRemote { rel_path, .. } => SyncEvent::TrashingRemote {
				rel_path: rel_path.clone(),
			},
			Self::DeleteLocal { rel_path, .. } => SyncEvent::DeletingLocal {
				rel_path: rel_path.clone(),
			},
			Self::MoveRemote {
				from_path, to_path, ..
			} => SyncEvent::MovingRemote {
				from: from_path.clone(),
				to: to_path.clone(),
			},
			Self::MoveLocal {
				from_path, to_path, ..
			} => SyncEvent::MovingLocal {
				from: from_path.clone(),
				to: to_path.clone(),
			},
			Self::Conflict { rel_path } => SyncEvent::Conflict {
				rel_path: rel_path.clone(),
			},
			Self::AdoptBaseline { rel_path } => SyncEvent::AdoptedBaseline {
				rel_path: rel_path.clone(),
			},
		}
	}
}

/// Whether a remote item name is safe to use as a single local path component. Rejects empty,
/// `.`/`..`, and names containing a path separator or NUL — a remote name is untrusted input (a
/// non-conforming client or, in future, a shared-folder peer could set one), and any of these
/// would let a pull escape the sync root (path traversal) or corrupt the `/`-joined key.
fn is_safe_name(name: &str) -> bool {
	!name.is_empty()
		&& name != "."
		&& name != ".."
		&& !name.contains('/')
		&& !name.contains('\\')
		&& !name.contains('\0')
}

/// Resolve the `/`-joined, NFC-normalized path of an item from its name + parent by walking the
/// dir index up to `root`. `None` for an orphan (a parent not present in the snapshot), a chain
/// that exceeds [`MAX_REMOTE_DEPTH`] (a malformed cycle), or any name on the chain that is not a
/// safe path component (so a traversal name like `..` never enters the plan).
fn resolve_path(
	name: &str,
	parent: Uuid,
	root: Uuid,
	dir_index: &HashMap<Uuid, (String, Uuid)>,
) -> Option<String> {
	if !is_safe_name(name) {
		return None;
	}
	let mut parts = vec![name.to_string()];
	let mut current = parent;
	let mut steps = 0;
	while current != root {
		let (parent_name, grandparent) = dir_index.get(&current)?;
		if !is_safe_name(parent_name) {
			return None;
		}
		parts.push(parent_name.clone());
		current = *grandparent;
		steps += 1;
		if steps > MAX_REMOTE_DEPTH {
			return None;
		}
	}
	parts.reverse();
	Some(parts.join("/"))
}

/// The remote view of a sync root's subtree, plus whether it is safe to reconcile against.
#[derive(Debug)]
pub(crate) struct RemoteView {
	pub(crate) nodes: HashMap<String, RemoteNode>,
	/// `true` if two distinct remote items resolved to the same case-insensitive path but NOT the
	/// same byte-identical one. The engine refuses to reconcile such a pair (a 1:1 local mapping is
	/// impossible) until the user cleans it up.
	pub(crate) has_collisions: bool,
	/// Paths two byte-identically named remote items resolved to. The server never allows that —
	/// its dedup is on the lowercased name hash — so it can only be a cache mid-transition: a
	/// re-upload whose successor has been applied while the predecessor's trash has not. Only that
	/// one path is held back; the pass runs.
	pub(crate) held_paths: HashSet<String>,
}

/// Build the remote view from a cache subtree snapshot rooted at `root`. Names are NFC-normalized
/// so they key 1:1 against the local scan; orphaned items (broken parent chain) are skipped.
pub(crate) fn build_remote_view(
	root: Uuid,
	dirs: &[CacheableDir<'_>],
	files: &[CacheableFile<'_>],
) -> RemoteView {
	let dir_index: HashMap<Uuid, (String, Uuid)> = dirs
		.iter()
		.map(|d| (d.uuid, (d.name.nfc().collect::<String>(), d.parent)))
		.collect();

	let mut nodes = HashMap::new();
	// collision key -> the raw path that claimed it, so a byte-identical duplicate is told apart
	// from a case-only one.
	let mut claimed: HashMap<String, String> = HashMap::new();
	let mut has_collisions = false;
	let mut held_paths: HashSet<String> = HashSet::new();

	let mut insert = |rel_path: String, node: RemoteNode| {
		// The local quarantine dir is excluded from the local scan; exclude it from the remote view
		// too, so a remote folder that happens to be named `.filen-sync-trash` is never mistaken for
		// (or synced into) the quarantine area.
		if rel_path == QUARANTINE_DIR || rel_path.starts_with(&format!("{QUARANTINE_DIR}/")) {
			return;
		}
		match claimed.get(&collision_key(&rel_path)) {
			None => {
				claimed.insert(collision_key(&rel_path), rel_path.clone());
				nodes.insert(rel_path, node);
			}
			// Byte-identical names under one parent cannot exist on the server, so this is the
			// cache showing both halves of a re-upload at once. Withhold the path for this pass
			// rather than refusing the whole one; the next snapshot has one of them.
			Some(previous) if *previous == rel_path => {
				tracing::debug!(
					"remote view: holding {rel_path:?} — the cache is mid-transition, listing two items under that exact name"
				);
				nodes.remove(&rel_path);
				held_paths.insert(rel_path);
			}
			// A genuine case-only collision: no 1:1 local mapping exists, so the pass is refused.
			Some(_) => has_collisions = true,
		}
	};

	for dir in dirs {
		let name = dir.name.nfc().collect::<String>();
		let Some(rel_path) = resolve_path(&name, dir.parent, root, &dir_index) else {
			continue;
		};
		insert(
			rel_path.clone(),
			RemoteNode {
				rel_path,
				kind: NodeKind::Dir,
				remote_uuid: dir.uuid,
				stable_uuid: None,
				content_hash: None,
				size: 0,
				modified_millis: dir.created.map(|c| c.timestamp_millis()).unwrap_or(0),
			},
		);
	}

	for file in files {
		let name = file.name.nfc().collect::<String>();
		let Some(rel_path) = resolve_path(&name, file.parent, root, &dir_index) else {
			continue;
		};
		insert(
			rel_path.clone(),
			RemoteNode {
				rel_path,
				kind: NodeKind::File,
				remote_uuid: file.uuid,
				stable_uuid: Some(file.stable_uuid),
				content_hash: file.hash,
				size: file.size,
				modified_millis: file.last_modified.timestamp_millis(),
			},
		);
	}

	RemoteView {
		nodes,
		has_collisions,
		held_paths,
	}
}

/// How one side compares to the baseline at a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
	/// Neither the side nor the baseline has the path.
	Absent,
	/// Present and matching the baseline.
	Unchanged,
	/// Present with no baseline (newly appeared).
	Created,
	/// Present but diverged from the baseline.
	Modified,
	/// Gone, but the baseline had it.
	Deleted,
}

impl Side {
	fn changed(self) -> bool {
		matches!(self, Self::Created | Self::Modified | Self::Deleted)
	}
}

fn classify_local(node: Option<&LocalNode>, base: Option<&BaselineEntry>) -> Side {
	match (node, base) {
		(None, None) => Side::Absent,
		(Some(_), None) => Side::Created,
		(None, Some(_)) => Side::Deleted,
		(Some(node), Some(base)) => {
			if node.kind != base.kind {
				Side::Modified
			} else {
				match node.kind {
					NodeKind::Dir => Side::Unchanged,
					NodeKind::File => {
						if node.content_hash.is_some() && node.content_hash == base.content_hash {
							Side::Unchanged
						} else {
							Side::Modified
						}
					}
				}
			}
		}
	}
}

/// Whether the remote item at a path is the SAME file the baseline row recorded there — the same
/// server-minted lineage, whatever version id it currently wears. `false` whenever either side has
/// no lineage id: an unknown lineage is not evidence of sameness.
pub(super) fn same_file_lineage(base: &BaselineEntry, node: &RemoteNode) -> bool {
	matches!(
		(base.remote_stable_uuid, node.stable_uuid),
		(Some(recorded), Some(current)) if recorded == current
	)
}

fn classify_remote(node: Option<&RemoteNode>, base: Option<&BaselineEntry>) -> Side {
	match (node, base) {
		(None, None) => Side::Absent,
		(Some(_), None) => Side::Created,
		(None, Some(_)) => Side::Deleted,
		(Some(node), Some(base)) => {
			if node.kind != base.kind {
				Side::Modified
			} else {
				match node.kind {
					NodeKind::File => classify_remote_file(node, base),
					NodeKind::Dir => Side::Unchanged,
				}
			}
		}
	}
}

/// How a remote FILE compares to its baseline row. Three cases, told apart by the two ids the
/// server gives a file:
///
/// - the SAME version uuid: nothing has happened to it;
/// - a new version uuid of the SAME lineage: an EDIT — but only where the content actually moved. A
///   re-upload of identical bytes (another client pushing what we already have, a restore of the
///   version we are already on) re-mints the uuid without changing anything, and reading that as a
///   remote edit turns a one-sided LOCAL edit into a conflict that has no second side;
/// - a DIFFERENT lineage, or one neither side can name: a REPLACEMENT — another file has taken the
///   path over. It reconciles exactly like an edit, since the destination has to converge on
///   whatever holds the path either way. Where the distinction is load-bearing is
///   [`PendingWrites::settle`](super::engine::PendingWrites): another client's edit of the file we
///   just pushed is a two-way conflict, its replacement of it is not.
fn classify_remote_file(node: &RemoteNode, base: &BaselineEntry) -> Side {
	if base.remote_uuid == Some(node.remote_uuid) {
		return Side::Unchanged;
	}
	let identical_content = matches!(
		(base.content_hash, node.content_hash),
		(Some(recorded), Some(current)) if recorded == current
	);
	if same_file_lineage(base, node) && identical_content {
		Side::Unchanged
	} else {
		Side::Modified
	}
}

/// Whether the local file matches the remote file by content. Prefers a direct hash comparison;
/// when the remote has no stored hash, falls back to the baseline (same local hash + same remote
/// version) so an old hashless remote file is not re-transferred every pass.
fn remote_matches_local(
	local: &LocalNode,
	remote: &RemoteNode,
	base: Option<&BaselineEntry>,
) -> bool {
	if local.kind != remote.kind {
		return false;
	}
	match local.kind {
		NodeKind::Dir => true,
		NodeKind::File => match (local.content_hash, remote.content_hash) {
			(Some(l), Some(r)) => l == r,
			// Remote hash missing: trust the baseline if it records this exact local content
			// already synced to the remote's current version.
			_ => base.is_some_and(|b| {
				b.content_hash == local.content_hash && b.remote_uuid == Some(remote.remote_uuid)
			}),
		},
	}
}

/// Do the local and remote nodes represent the same content right now (the two-way convergence
/// check)? Both-absent counts as agreement (both deleted).
fn nodes_agree(local: Option<&LocalNode>, remote: Option<&RemoteNode>) -> bool {
	match (local, remote) {
		(None, None) => true,
		(Some(l), Some(r)) => {
			l.kind == r.kind
				&& match l.kind {
					NodeKind::Dir => true,
					NodeKind::File => l.content_hash.is_some() && l.content_hash == r.content_hash,
				}
		}
		_ => false,
	}
}

/// Whether the baseline already records exactly what both sides currently hold at a path, so there
/// is nothing to adopt. `(None, None)` agrees only when there is no row at all.
fn baseline_is_current(
	local: Option<&LocalNode>,
	remote: Option<&RemoteNode>,
	base: Option<&BaselineEntry>,
) -> bool {
	match (local, remote, base) {
		(None, None, base) => base.is_none(),
		(Some(local), Some(remote), Some(base)) => {
			base.kind == local.kind
				&& base.remote_uuid == Some(remote.remote_uuid)
				&& match local.kind {
					NodeKind::Dir => true,
					NodeKind::File => base.content_hash == local.content_hash,
				}
		}
		_ => false,
	}
}

/// Record an already-converged path into the baseline when the baseline does not (yet) say so.
///
/// The one-way paths transfer nothing when the two sides already agree, so without this a pair
/// opened on an already-synced tree would never seed a baseline at all: every later pass would
/// still see an empty baseline, read as a first sync, and have all its deletions held by the guard.
/// It is also what retires a stale row for a path both sides have since dropped.
fn adopt_if_baseline_stale(
	rel_path: &str,
	local: Option<&LocalNode>,
	remote: Option<&RemoteNode>,
	base: Option<&BaselineEntry>,
	actions: &mut Vec<SyncAction>,
) {
	if baseline_is_current(local, remote, base) {
		return;
	}
	let action = SyncAction::AdoptBaseline {
		rel_path: rel_path.to_string(),
	};
	tracing::debug!(
		"plan: {} — the two sides already agree; recording the baseline",
		action.describe()
	);
	actions.push(action);
}

/// Make the remote match the local at one path (push). `delete_ok` gates deletions (off for backup
/// modes, which never delete on their destination).
fn push_to_remote(
	rel_path: &str,
	local: Option<&LocalNode>,
	remote: Option<&RemoteNode>,
	base: Option<&BaselineEntry>,
	delete_ok: bool,
	actions: &mut Vec<SyncAction>,
) {
	match (local, remote) {
		(Some(local), None) => {
			let action = create_remote(rel_path, local.kind);
			tracing::debug!(
				"plan: {} — new locally, absent on remote",
				action.describe()
			);
			actions.push(action);
		}
		(Some(local), Some(remote)) => {
			if !remote_matches_local(local, remote, base) {
				if local.kind != remote.kind {
					// Type flip: trash the stale remote node, then create the new kind.
					let trash = SyncAction::TrashRemote {
						rel_path: rel_path.to_string(),
						kind: remote.kind,
						remote_uuid: remote.remote_uuid,
					};
					tracing::debug!("plan: {} — local/remote kind differs", trash.describe());
					actions.push(trash);
				}
				let action = create_remote(rel_path, local.kind);
				tracing::debug!(
					"plan: {} — local content differs from remote",
					action.describe()
				);
				actions.push(action);
			} else {
				adopt_if_baseline_stale(rel_path, Some(local), Some(remote), base, actions);
			}
		}
		(None, Some(remote)) => {
			if delete_ok {
				let trash = SyncAction::TrashRemote {
					rel_path: rel_path.to_string(),
					kind: remote.kind,
					remote_uuid: remote.remote_uuid,
				};
				tracing::debug!("plan: {} — gone locally (deletion)", trash.describe());
				actions.push(trash);
			}
		}
		(None, None) => adopt_if_baseline_stale(rel_path, None, None, base, actions),
	}
}

fn create_remote(rel_path: &str, kind: NodeKind) -> SyncAction {
	match kind {
		// Files: a same-name upload versions the existing remote file, so create and update are
		// the same action.
		NodeKind::File => SyncAction::UploadFile {
			rel_path: rel_path.to_string(),
		},
		NodeKind::Dir => SyncAction::CreateRemoteDir {
			rel_path: rel_path.to_string(),
		},
	}
}

/// Make the local match the remote at one path (pull). `delete_ok` gates deletions.
fn pull_to_local(
	rel_path: &str,
	local: Option<&LocalNode>,
	remote: Option<&RemoteNode>,
	base: Option<&BaselineEntry>,
	delete_ok: bool,
	actions: &mut Vec<SyncAction>,
) {
	match (remote, local) {
		(Some(remote), None) => {
			let action = create_local(rel_path, remote);
			tracing::debug!(
				"plan: {} — new on remote, absent locally",
				action.describe()
			);
			actions.push(action);
		}
		(Some(remote), Some(local)) => {
			if !remote_matches_local(local, remote, base) {
				if local.kind != remote.kind {
					let del = SyncAction::DeleteLocal {
						rel_path: rel_path.to_string(),
						kind: local.kind,
					};
					tracing::debug!("plan: {} — local/remote kind differs", del.describe());
					actions.push(del);
				}
				let action = create_local(rel_path, remote);
				tracing::debug!(
					"plan: {} — remote content differs from local",
					action.describe()
				);
				actions.push(action);
			} else {
				adopt_if_baseline_stale(rel_path, Some(local), Some(remote), base, actions);
			}
		}
		(None, Some(local)) => {
			if delete_ok {
				let del = SyncAction::DeleteLocal {
					rel_path: rel_path.to_string(),
					kind: local.kind,
				};
				tracing::debug!("plan: {} — gone on remote (deletion)", del.describe());
				actions.push(del);
			}
		}
		(None, None) => adopt_if_baseline_stale(rel_path, None, None, base, actions),
	}
}

fn create_local(rel_path: &str, remote: &RemoteNode) -> SyncAction {
	match remote.kind {
		NodeKind::File => SyncAction::DownloadFile {
			rel_path: rel_path.to_string(),
			remote_uuid: remote.remote_uuid,
		},
		NodeKind::Dir => SyncAction::CreateLocalDir {
			rel_path: rel_path.to_string(),
		},
	}
}

/// Advance every baseline row's agreed-content marker that this pass's RAW cache snapshot
/// confirms, and return the rows that moved so the caller can persist them.
///
/// The confirmation is one specific observation: the snapshot lists the row's own version uuid at
/// the row's path. That is the remote saying it holds exactly what the row records, which is what
/// makes the row's content agreed by both sides. A push cannot claim this for itself — the upload
/// only proves the server took the bytes — so this is the step that retires the gap a push leaves.
///
/// It MUST be given the raw snapshot, never the view
/// [`PendingWrites::fold_into`](super::engine::PendingWrites::fold_into) has corrected: the fold
/// synthesises our own just-written version at the path from the very row being confirmed, so a
/// folded view would confirm every push against itself and the marker would mean nothing.
pub(super) fn confirm_agreed_content(
	baseline: &mut HashMap<String, BaselineEntry>,
	raw_remote: &HashMap<String, RemoteNode>,
) -> Vec<BaselineEntry> {
	let mut advanced = Vec::new();
	for (rel_path, entry) in baseline.iter_mut() {
		if !awaits_confirmation(entry) {
			continue;
		}
		if raw_remote.get(rel_path).map(|node| node.remote_uuid) != entry.remote_uuid {
			continue;
		}
		entry.agreed_hash = entry.content_hash;
		tracing::debug!(
			"plan: the remote confirms {rel_path:?} — both sides hold what the baseline records"
		);
		advanced.push(entry.clone());
	}
	advanced
}

/// Whether a row is a push still waiting for something to confirm it: this side's content is on
/// record, and it is not what the two sides last agreed on.
pub(super) fn awaits_confirmation(entry: &BaselineEntry) -> bool {
	entry.kind == NodeKind::File
		&& entry.state == BaselineState::Synced
		&& entry.content_hash.is_some()
		&& entry.agreed_hash != entry.content_hash
}

/// Advance the agreed-content marker of every unconfirmed row whose version uuid is in `confirmed`
/// — the pushes some other evidence than a snapshot has vouched for (a tenure as the remote head,
/// or the server's own version chain) — and return the rows that moved so the caller can persist
/// them.
///
/// Split from [`confirm_agreed_content`] because the evidence comes from outside the plan (the
/// cache's announcement timestamps, a version listing) while the row rule is the same; keeping the
/// rule here keeps the pass's confirmation policy in one place and unit-testable.
pub(super) fn confirm_agreed_pushes(
	baseline: &mut HashMap<String, BaselineEntry>,
	confirmed: &HashSet<Uuid>,
) -> Vec<BaselineEntry> {
	let mut advanced = Vec::new();
	for (rel_path, entry) in baseline.iter_mut() {
		if !awaits_confirmation(entry)
			|| !entry
				.remote_uuid
				.is_some_and(|uuid| confirmed.contains(&uuid))
		{
			continue;
		}
		entry.agreed_hash = entry.content_hash;
		tracing::debug!(
			"plan: {rel_path:?} — this engine's push stood as the remote head long enough to count as agreed"
		);
		advanced.push(entry.clone());
	}
	advanced
}

/// What the server's version chain says about a push of ours: how long it stood before anything
/// landed on top of it.
///
/// `versions` is the lineage as [`Client::list_file_versions`](crate::auth::Client::list_file_versions)
/// returns it and `superseded_by` is the version the pass's snapshot found at the row's path.
/// `Some(true)` when ours stood for at least `tenure`.
///
/// The tenure is measured against the EARLIEST version that is not ours and is not older than ours
/// — never against `superseded_by` itself, which is only the version the path carries now. A client
/// that edited beside us and then edited again minutes later leaves both on the chain, and dating
/// ours against the later one would read a concurrent edit as consent and pull over our bytes.
/// `superseded_by` is required to be on the chain (a chain that does not carry it is not answering
/// about this path) and is a candidate like any other.
///
/// Deliberately not read from the chain's ORDER: the server stamps versions to the SECOND, so the
/// two versions of one race carry the same stamp and their relative order in the listing is
/// arbitrary. A tie therefore measures as no tenure at all — the safe direction, and the right
/// answer for a race. The version our upload replaced ties with ours the same way if the two landed
/// in one second, which costs a confirmation the row could have had; a conflict surfaced is the
/// direction to be wrong in.
///
/// `None` when the chain does not carry ours or `superseded_by` — a versioning-disabled account
/// keeps only the head — and when nothing on it is newer than ours at all, which a lineage somebody
/// has restored an old version into can look like. No evidence either way.
///
/// This is the evidence of last resort — it needs no observation of ours, so it answers for a row a
/// restarted engine knows nothing about.
pub(super) fn version_chain_verdict(
	versions: &[(Uuid, chrono::DateTime<chrono::Utc>)],
	ours: Uuid,
	superseded_by: Uuid,
	tenure: std::time::Duration,
) -> Option<bool> {
	let at = |wanted: Uuid| {
		versions
			.iter()
			.find(|(uuid, _)| *uuid == wanted)
			.map(|(_, at)| *at)
	};
	let ours_at = at(ours)?;
	// The version the snapshot shows has to be on the chain, or it is not this file's chain.
	at(superseded_by)?;
	let first_after = versions
		.iter()
		.filter(|(uuid, at)| *uuid != ours && *at >= ours_at)
		.map(|(_, at)| *at)
		.min()?;
	let stood_for = first_after.signed_duration_since(ours_at);
	Some(stood_for.to_std().is_ok_and(|stood| stood >= tenure))
}

/// The version our own upload buried, if it buried one.
///
/// `versions` is the lineage as the server lists it — with the versions this engine minted itself
/// already filtered out, bar `replaced` — `ours` the version the upload minted and `replaced` the
/// version the pass's snapshot said the path held. A version that is neither, and that is later
/// than `replaced`, landed between the pass reading the remote and our upload: another client's
/// edit that a same-name upload versioned rather than refused — our bytes are on top of it now, and
/// its author has no way of knowing. The earliest such version is the one our upload buried.
///
/// Not read from the chain's ORDER, which cannot answer it: the server records a version's time to
/// the second, so two versions of one second sort arbitrarily, and our own previous upload is
/// routinely listed above the version it was later replaced by. The comparison is strict for the
/// same reason — inside one second the chain cannot say which of two versions came first, and
/// guessing there costs a conflict on a path nobody but us touched. What that gives up is an
/// interleave that landed in the very second of the version it displaced; the client that made it
/// still surfaces its own side of the divergence. The caller strips our own recent uploads first,
/// which is what the stamps cannot do at all.
///
/// `None` when nothing on the chain is later, and when the chain does not carry `replaced` at
/// all — a versioning-disabled account keeps only the head, which is no evidence either way.
///
/// A version that landed on top of OURS between the upload and this listing is later than
/// `replaced` too, and is reported the same way. It is a concurrent edit whichever of the two is
/// the remote head, and the resolution reads that from the server.
pub(super) fn interleaved_version(
	versions: &[(Uuid, chrono::DateTime<chrono::Utc>)],
	ours: Uuid,
	replaced: Uuid,
) -> Option<Uuid> {
	let replaced_at = versions
		.iter()
		.find(|(uuid, _)| *uuid == replaced)
		.map(|(_, at)| *at)?;
	versions
		.iter()
		.filter(|(uuid, at)| *uuid != ours && *uuid != replaced && *at > replaced_at)
		.min_by_key(|(_, at)| *at)
		.map(|(uuid, _)| *uuid)
}

/// The baseline rows a [`Backlog::AdoptDestination`](super::mode::Backlog::AdoptDestination) mode
/// switch writes: one per path the NEW mode's source side has nothing at while the destination
/// holds a copy — the pair's standing backlog, plus whatever else the destination has accumulated.
///
/// Each row records what the destination holds RIGHT NOW (from the same snapshot and scan a pass
/// reads), marked [`BaselineState::Adopted`]: the copy is intended, not a deletion waiting to be
/// propagated. What that means per mode is [`reconcile`]'s business, not this function's.
///
/// Two sources, both destination-only:
/// - a TRACKED path (a `Synced` row) the source has lost: the standing backlog a backup mode
///   accumulated.
/// - an UNTRACKED path — no baseline row at all — that only the destination holds: a file another
///   client created straight on the destination. It has never been the pair's to delete, and
///   without a row a one-way mirror trashes it on the very next pass, so the switch adopts it too.
///   Only in the ONE-WAY modes: two-way has no destination to spare, an untracked copy already
///   flows back to the other side under the ordinary rules, and a row here would only take the path
///   out of move detection (see [`detect_moves`]).
///
/// A held conflict is excluded from both: it is the caller's to
/// [`resolve_conflict`](super::SyncEngine::resolve_conflict), and overwriting the row with an
/// adoption would drop the divergence it is holding.
pub(crate) fn adopt_destination_rows(
	mode: super::SyncMode,
	baseline: &HashMap<String, BaselineEntry>,
	local: &HashMap<String, LocalNode>,
	remote: &HashMap<String, RemoteNode>,
) -> Vec<BaselineEntry> {
	let mut rows = Vec::new();
	for (rel_path, base) in baseline {
		if base.state != BaselineState::Synced {
			continue;
		}
		let (local_node, remote_node) = (local.get(rel_path), remote.get(rel_path));
		let row = match mode {
			super::SyncMode::LocalToRemote | super::SyncMode::LocalBackup => remote_node
				.filter(|_| local_node.is_none())
				.map(|node| adopted_from_remote(rel_path, node)),
			super::SyncMode::RemoteToLocal | super::SyncMode::RemoteBackup => local_node
				.filter(|_| remote_node.is_none())
				.map(|node| adopted_from_local(rel_path, node)),
			// Either side may be the one that kept the item; a path both sides still hold is not a
			// standing deletion at all.
			super::SyncMode::TwoWay => match (local_node, remote_node) {
				(None, Some(node)) => Some(adopted_from_remote(rel_path, node)),
				(Some(node), None) => Some(adopted_from_local(rel_path, node)),
				_ => None,
			},
		};
		if let Some(row) = row {
			tracing::debug!(
				"reconfigure: adopting the destination's copy of {rel_path:?} — the source no longer has it"
			);
			rows.push(row);
		}
	}
	let untracked = |rel_path: &String| !baseline.contains_key(rel_path);
	match mode {
		super::SyncMode::LocalToRemote | super::SyncMode::LocalBackup => {
			for (rel_path, node) in remote.iter().filter(|(p, _)| untracked(p)) {
				if local.contains_key(rel_path) {
					continue;
				}
				tracing::debug!(
					"reconfigure: adopting the destination-only item {rel_path:?} — the pair never tracked it"
				);
				rows.push(adopted_from_remote(rel_path, node));
			}
		}
		super::SyncMode::RemoteToLocal | super::SyncMode::RemoteBackup => {
			for (rel_path, node) in local.iter().filter(|(p, _)| untracked(p)) {
				if remote.contains_key(rel_path) {
					continue;
				}
				tracing::debug!(
					"reconfigure: adopting the destination-only item {rel_path:?} — the pair never tracked it"
				);
				rows.push(adopted_from_local(rel_path, node));
			}
		}
		super::SyncMode::TwoWay => {}
	}
	rows
}

/// An [`Adopted`](BaselineState::Adopted) row anchored to what the REMOTE holds at a path.
fn adopted_from_remote(rel_path: &str, node: &RemoteNode) -> BaselineEntry {
	let is_file = node.kind == NodeKind::File;
	BaselineEntry {
		rel_path: rel_path.to_string(),
		kind: node.kind,
		remote_uuid: Some(node.remote_uuid),
		content_hash: is_file.then_some(node.content_hash).flatten(),
		size: is_file.then_some(node.size),
		local_mtime: None,
		remote_modified: is_file.then_some(node.modified_millis),
		state: BaselineState::Adopted,
		local_kind: None,
		remote_kind: None,
		remote_hash: None,
		remote_size: None,
		remote_stable_uuid: node.stable_uuid,
		// Nothing is agreed: only one side holds this path.
		agreed_hash: None,
	}
}

/// An [`Adopted`](BaselineState::Adopted) row anchored to what the LOCAL tree holds at a path.
fn adopted_from_local(rel_path: &str, node: &LocalNode) -> BaselineEntry {
	let is_file = node.kind == NodeKind::File;
	BaselineEntry {
		rel_path: rel_path.to_string(),
		kind: node.kind,
		remote_uuid: None,
		content_hash: is_file.then_some(node.content_hash).flatten(),
		size: is_file.then_some(node.size),
		local_mtime: Some(node.mtime_millis),
		remote_modified: None,
		state: BaselineState::Adopted,
		local_kind: None,
		remote_kind: None,
		remote_hash: None,
		remote_size: None,
		remote_stable_uuid: None,
		agreed_hash: None,
	}
}

/// Whether the remote change at a path is another client's edit made CONCURRENTLY with a push of
/// ours that no snapshot ever confirmed — the one remote change a two-way pass must NOT just pull.
///
/// The row holds our own content and the version uuid our push minted; `agreed_hash` holds the last
/// content both sides were known to hold. Equal, and the push was confirmed (a pass saw our version
/// as the remote head), so a foreign version stacked on top of it is an edit made AFTER ours and
/// pulling it is right — that is the ordinary sequential case. Different, and our push was never
/// observed to land while a foreign version of the SAME file now carries content that is neither
/// ours nor the agreed one: both sides moved off the agreed content, which is a conflict.
///
/// Deliberately narrow — a file on both sides, the same server-minted lineage (a DIFFERENT lineage
/// is a replacement and converges the ordinary way), a different version uuid, and hashes on both
/// sides that actually differ.
///
/// `agreed_hash: None` — a file this side created and pushed, nothing having confirmed it since —
/// is an UNCONFIRMED push like any other, not an absence of evidence: the two clients that create
/// the same name concurrently both hold such a row, and treating the marker's absence as consent
/// resolves that silently in whichever client's favour happened to look second. Only a snapshot
/// listing our own version at the path (`agreed_hash == content_hash`) makes a foreign version on
/// top of it an edit made AFTER ours.
///
/// A snapshot listing our version is not the only thing that can confirm it: a push that stood as
/// the remote head for [`CONFIRM_TENURE`](super::engine::CONFIRM_TENURE) counts too, dated either
/// from the cache's own announcements or from the server's version chain (see
/// `SyncEngine::confirm_pushes`). That is what keeps a pair that was paused for an hour — no pass,
/// so nothing a snapshot could confirm — from reading every foreign edit made since as a conflict.
/// What stays a conflict is the case the rule is for: a foreign version that landed on ours INSIDE
/// that window, which is what a genuinely concurrent edit looks like.
///
/// TWO-WAY ONLY. A one-way mode has an authoritative side and no divergence to surface:
/// [`SyncMode::LocalToRemote`](super::SyncMode::LocalToRemote) re-pushes the local copy and
/// [`SyncMode::RemoteToLocal`](super::SyncMode::RemoteToLocal) pulls the remote one, exactly as
/// before.
fn is_unconfirmed_concurrent_edit(
	local: Option<&LocalNode>,
	remote: Option<&RemoteNode>,
	base: Option<&BaselineEntry>,
) -> bool {
	let (Some(local), Some(remote), Some(base)) = (local, remote, base) else {
		return false;
	};
	local.kind == NodeKind::File
		&& remote.kind == NodeKind::File
		&& base.kind == NodeKind::File
		// Our push is still unconfirmed: the row's content is not what both sides last agreed on.
		// `None` is not agreement — nothing has ever confirmed this row's content.
		&& base.content_hash != base.agreed_hash
		// A foreign VERSION of the same file — not our own, and not a different file taking over.
		&& base.remote_uuid != Some(remote.remote_uuid)
		&& same_file_lineage(base, remote)
		// ... carrying content that is genuinely not ours. An identical re-upload is no divergence,
		// and a remote with no stored hash is no evidence of one.
		&& matches!(
			(base.content_hash, remote.content_hash),
			(Some(ours), Some(theirs)) if ours != theirs
		)
}

fn reconcile_two_way(
	rel_path: &str,
	local: Option<&LocalNode>,
	remote: Option<&RemoteNode>,
	base: Option<&BaselineEntry>,
	actions: &mut Vec<SyncAction>,
) {
	let local_side = classify_local(local, base);
	let remote_side = classify_remote(remote, base);

	match (local_side.changed(), remote_side.changed()) {
		// Already in sync (relative to the baseline).
		(false, false) => {}
		// Only one side moved — propagate it (deletes always propagate in two-way).
		(true, false) => push_to_remote(rel_path, local, remote, base, true, actions),
		// The remote moved and the local side did not — a pull, UNLESS the row's own content is a
		// push we never saw land and the remote now carries somebody else's edit of the same file:
		// then both sides moved off the last agreed content and pulling would bury ours.
		(false, true) => {
			if is_unconfirmed_concurrent_edit(local, remote, base) {
				let action = SyncAction::Conflict {
					rel_path: rel_path.to_string(),
				};
				tracing::debug!(
					"plan: {} — a foreign version of the same file landed on a push this engine never saw confirmed",
					action.describe()
				);
				actions.push(action);
			} else {
				pull_to_local(rel_path, local, remote, base, true, actions);
			}
		}
		// Both moved: either they converged on the same content (adopt, no transfer) or they
		// genuinely diverged (a conflict the caller resolves; modify-vs-delete is refined later).
		(true, true) => {
			if !nodes_agree(local, remote) {
				let action = SyncAction::Conflict {
					rel_path: rel_path.to_string(),
				};
				tracing::debug!(
					"plan: {} — both sides changed and diverged (local {local_side:?}, remote {remote_side:?})",
					action.describe()
				);
				actions.push(action);
			} else {
				// Both sides changed but hold identical content — there is nothing to transfer, but
				// the baseline is missing or stale here (otherwise this would be the (false,false)
				// branch). Record the converged state so a LATER one-sided change at this path is
				// classified correctly instead of being misread against an absent baseline.
				let action = SyncAction::AdoptBaseline {
					rel_path: rel_path.to_string(),
				};
				tracing::debug!(
					"plan: {} — both sides converged on identical content; recording the baseline",
					action.describe()
				);
				actions.push(action);
			}
		}
	}
}

/// Detect file moves/renames so they apply as a single metadata op instead of a re-transfer, and
/// return the set of paths they consume (excluded from the per-path reconcile). Files only.
///
/// - Remote moves are matched by UUID (a server-side move keeps the file's uuid): a baseline file
///   whose uuid now sits at a different remote path, still unchanged locally, becomes a `MoveLocal`.
///   Failing that, by the file's server-minted LINEAGE id, which survives the content edit that
///   re-mints the uuid — so a move and an edit landing inside one window stay one item rather than
///   splitting into a local deletion and an unrelated download. Such a move is paired with a
///   download of the new version onto the renamed copy.
///
///   "Still unchanged locally" is a REQUIREMENT of both matches, not a description of the usual
///   case. A local edit at the move's source is a second change to the same item, and renaming the
///   local copy would carry that edit to the destination for the pull to write over — silently,
///   since a same-size edit satisfies the scanner's `(size, mtime)` fast path afterwards and is
///   never looked at again. Such a path falls through to the per-path reconcile, which resolves it
///   by mode: TwoWay surfaces the source as a conflict and downloads the moved version under its
///   new name; RemoteToLocal lets the remote win, quarantining the local edit and downloading at
///   the new name; RemoteBackup never deletes on its destination, so the edited file stays where it
///   is and the moved version arrives beside it. The push-only modes never reach this block at all:
///   `LocalToRemote` re-pushes the edit at the source path and trashes the remote's moved copy,
///   `LocalBackup` re-pushes and leaves it.
///
///   A lineage match whose new version is a foreign EDIT over a push nothing confirmed
///   ([`is_unconfirmed_concurrent_edit`]) is not carried across either, for the same reason at one
///   remove: the rename would put our unconfirmed copy under the download. In two-way it surfaces
///   the source as a conflict right here — the reconcile could not, since with nothing left at the
///   source path it reads a plain remote-side deletion — and the moved version downloads at its new
///   name as an untracked remote item.
/// - Local moves are matched by content hash: a baseline file gone locally whose content reappears
///   at a new local path (uniquely — ambiguous content is left to delete+create), with the remote
///   still holding the original, becomes a `MoveRemote`.
///
/// Only a `Synced` row is a move endpoint. A path held in conflict must not be one: consuming it
/// here would skip the per-path conflict hold, rewrite its baseline as `Synced` against one side's
/// content and lose the divergence for good. Nor is a row
/// [`Adopted`](BaselineState::Adopted) from a destination at a mode switch — it records one side's
/// standing copy, not a synced pair, so matching it by uuid or by content would move an item the
/// source never had. Both fall through to the reconcile loop, which knows what to do with them.
fn detect_moves(
	mode: super::SyncMode,
	baseline: &HashMap<String, BaselineEntry>,
	local: &HashMap<String, LocalNode>,
	remote: &HashMap<String, RemoteNode>,
	actions: &mut Vec<SyncAction>,
	consumed: &mut HashSet<String>,
) {
	if mode.pulls() {
		let remote_path_of_uuid: HashMap<Uuid, &str> = remote
			.iter()
			.filter(|(_, node)| node.kind == NodeKind::File)
			.map(|(path, node)| (node.remote_uuid, path.as_str()))
			.collect();
		// A file's uuid is re-minted by every content edit, so a move that CARRIED an edit inside
		// one window is invisible to the index above — the uuid the baseline recorded no longer
		// names anything live. The server-minted lineage id is not re-minted, so it still finds the
		// file at its new path and the pass carries the item across instead of quarantining the old
		// path and downloading the new one from scratch.
		let remote_path_of_lineage: HashMap<StableUuid, &str> = remote
			.iter()
			.filter(|(_, node)| node.kind == NodeKind::File)
			.filter_map(|(path, node)| Some((node.stable_uuid?, path.as_str())))
			.collect();
		for (from, base) in baseline {
			if base.kind != NodeKind::File
				|| base.state != BaselineState::Synced
				|| consumed.contains(from)
			{
				continue;
			}
			let Some(uuid) = base.remote_uuid else {
				continue;
			};
			// Only where the local side still matches the baseline — for EITHER match. With a local
			// edit at `from` this is a genuine both-sides divergence, and consuming it as a move
			// would rename the local copy and then write over it, laundering a conflict into a
			// silent overwrite. The mode decides what it becomes instead; see the doc comment.
			//
			// A row with NO local content on record is not such an edit: it is a row waiting for a
			// push (a `KeepLocal` resolution anchors the baseline to the remote and clears the
			// local half on purpose). Comparing a hash against `None` reports "changed" for every
			// one of them, which would turn a remote rename landing before that push into a second
			// conflict at the source plus a download at the new name. No local evidence is not
			// evidence of a local change: carry the move across and let the pending push follow at
			// the new name. The waiver is for the RENAME only — a move that also carried an EDIT is
			// refused below, where the kept copy would end up under the download.
			if base.content_hash.is_some()
				&& classify_local(local.get(from), Some(base)) != Side::Unchanged
			{
				continue;
			}
			// `carries_edit`: the lineage moved AND changed version, so the local copy this move
			// renames is the pre-edit one and still has to be refreshed.
			let (to, carries_edit) = match remote_path_of_uuid.get(&uuid) {
				Some(&to) => (to, false),
				None => match base
					.remote_stable_uuid
					.and_then(|lineage| remote_path_of_lineage.get(&lineage))
				{
					Some(&to) => (to, true),
					None => continue,
				},
			};
			// The edit a move carried is measured by the same rule as one that stayed put: over a
			// push no snapshot ever confirmed it is a divergence, not an item to carry across.
			// Consuming it would rename our unconfirmed copy to the destination for the paired
			// download to write over — the loss the rule exists to stop, one rename away — and
			// leaving the pair to the reconcile is no better, since the source would then read as a
			// remote-side deletion. So the source is surfaced here and its path consumed; the moved
			// version is untracked at its new name and downloads there through the ordinary
			// reconcile, which is the same outcome the uuid branch produces for a two-sided change.
			//
			// A row with no local content on record is measured the same way, and has to be spelled
			// out because [`is_unconfirmed_concurrent_edit`] has no hash of ours to compare: being
			// excused from the local-change guard above is not agreement. The copy a `KeepLocal`
			// resolution kept is diverged from what the remote holds by construction, so a remote
			// change that renamed AND edited the file is the same divergence one rename away.
			if carries_edit
				&& mode.pushes()
				&& (base.content_hash.is_none()
					|| is_unconfirmed_concurrent_edit(local.get(from), remote.get(to), Some(base)))
			{
				let action = SyncAction::Conflict {
					rel_path: from.clone(),
				};
				tracing::debug!(
					"plan: {} — the moved version is a foreign edit over a push this engine never saw confirmed",
					action.describe()
				);
				actions.push(action);
				consumed.insert(from.clone());
				continue;
			}
			if to != from
				&& !baseline.contains_key(to)
				&& local.contains_key(from)
				&& !local.contains_key(to)
				&& !consumed.contains(to)
			{
				let action = SyncAction::MoveLocal {
					from_path: from.clone(),
					to_path: to.to_string(),
					kind: NodeKind::File,
				};
				tracing::debug!(
					"plan: {} — remote item moved (matched by {}); renaming locally instead of re-downloading",
					action.describe(),
					if carries_edit { "lineage" } else { "uuid" }
				);
				actions.push(action);
				if carries_edit {
					// The move brought a new version with it: pull it onto the renamed copy in this
					// same pass. Without this the pass would leave the pre-edit bytes at the new
					// path with a baseline row that has to describe them as stale, and the next
					// pass would have to re-pull anyway.
					let download = SyncAction::DownloadFile {
						rel_path: to.to_string(),
						remote_uuid: remote[to].remote_uuid,
					};
					tracing::debug!(
						"plan: {} — the move carried a content edit",
						download.describe()
					);
					actions.push(download);
				}
				consumed.insert(from.clone());
				consumed.insert(to.to_string());
			}
		}
	}

	if mode.pushes() {
		// Content hash -> the new local paths carrying it (not in baseline, not on the remote).
		// Keyed by the raw bytes since `Blake3Hash` is not `std::hash::Hash`.
		let mut created_by_hash: HashMap<[u8; 32], Vec<&str>> = HashMap::new();
		for (path, node) in local {
			if node.kind == NodeKind::File
				&& !baseline.contains_key(path)
				&& !remote.contains_key(path)
				&& let Some(hash) = node.content_hash
			{
				created_by_hash
					.entry(*hash.as_ref())
					.or_default()
					.push(path.as_str());
			}
		}
		for (from, base) in baseline {
			if base.kind != NodeKind::File
				|| base.state != BaselineState::Synced
				|| consumed.contains(from)
				|| local.contains_key(from)
			{
				continue;
			}
			let (Some(hash), Some(uuid)) = (base.content_hash, base.remote_uuid) else {
				continue;
			};
			// The remote must still hold the original file at `from` for there to be one to move.
			if remote.get(from).map(|n| n.remote_uuid) != Some(uuid) {
				continue;
			}
			let Some(candidates) = created_by_hash.get(hash.as_ref()) else {
				continue;
			};
			let fresh: Vec<&str> = candidates
				.iter()
				.copied()
				.filter(|to| !consumed.contains(*to) && !remote.contains_key(*to))
				.collect();
			// Only an UNAMBIGUOUS match is a move; otherwise fall back to delete + create.
			if let [to] = fresh[..] {
				let action = SyncAction::MoveRemote {
					from_path: from.clone(),
					to_path: to.to_string(),
					kind: NodeKind::File,
					remote_uuid: uuid,
				};
				tracing::debug!(
					"plan: {} — local file moved (matched by content hash); re-parenting/renaming on the remote instead of re-uploading",
					action.describe()
				);
				actions.push(action);
				consumed.insert(from.clone());
				consumed.insert(to.to_string());
			}
		}
	}
}

/// Whether `key` is one of `held` or lives under one. A path the view cannot resolve makes
/// everything the snapshot would otherwise resolve beneath it unresolvable too.
fn is_under_held_path(key: &str, held: &HashSet<String>) -> bool {
	held.contains(key) || held.iter().any(|p| is_under(key, p))
}

/// Whether `path` is a STRICT descendant of `prefix` (`prefix/...`).
pub(super) fn is_under(path: &str, prefix: &str) -> bool {
	path.len() > prefix.len()
		&& path.as_bytes()[prefix.len()] == b'/'
		&& path.as_bytes()[..prefix.len()] == *prefix.as_bytes()
}

/// Drop every action that falls under a path this pass reports as a conflict — freshly surfaced or
/// still held from an earlier pass. The conflicting path itself cannot be materialized until the
/// caller resolves it, so an action on its subtree would fail (e.g. an upload into a remote dir
/// that cannot be created while the conflicting file holds the name). The conflicts themselves are
/// kept.
fn suppress_conflicted_subtrees(actions: &mut Vec<SyncAction>) {
	let conflicted: Vec<String> = actions
		.iter()
		.filter_map(|a| match a {
			SyncAction::Conflict { rel_path } => Some(rel_path.clone()),
			_ => None,
		})
		.collect();
	if conflicted.is_empty() {
		return;
	}
	actions.retain(|action| {
		if matches!(action, SyncAction::Conflict { .. }) {
			return true;
		}
		let (from, to) = action.endpoints();
		let held = conflicted
			.iter()
			.any(|c| is_under(from, c) || is_under(to, c));
		if held {
			tracing::debug!(
				"plan: dropping {} — it sits under a path held in conflict this pass",
				action.describe()
			);
		}
		!held
	});
}

/// Carry every directory move the pass can identify across as ONE move of the directory, and re-key
/// the three inputs so the rest of the pass reads that subtree where it ends up. Returns the moves
/// in the order they have to run: each one is found in inputs the earlier ones already re-keyed, so
/// a directory's move precedes any move inside it.
///
/// Planned per path, a directory move is a create at the new path, a move per file and a trash of
/// the old directory: a round trip per child instead of one, and a new directory uuid, which loses
/// everything keyed by it (a public link is). A move of the directory keeps its uuid, and with the
/// inputs re-keyed every child is a no-op unless it changed on its own — an edit inside a moved
/// directory is planned at its new path and applied after the move.
///
/// Two shapes are found, a case-only rename first:
///
/// - A directory the two sides spell differently only by case. Planned any other way it destroys
///   the directory: the per-path reconcile sees a create at the new spelling and a delete at the
///   old one, the server's name dedup is case-insensitive, so the create is handed back the very
///   directory the delete then trashes, subtree and all; the pull side quarantines the whole local
///   subtree on a case-insensitive filesystem the same way. Only a directory under the SAME parent
///   path on both sides is a candidate; a case change deeper down is found on a later iteration,
///   once its parent has been re-keyed. Which side is renamed:
///   - a one-way mode renames its destination to the source's spelling;
///   - two-way renames the REMOTE when the baseline recorded the remote's spelling for that same
///     directory and has no row at the local one — the local side is what changed — and the LOCAL
///     side otherwise: the remote renamed it, or nothing says which side moved and the spelling
///     every other device of the account already sees is the one to keep.
///
///   A path held in conflict at either spelling is left to the ordinary reconcile.
/// - A directory moved to a different path on one side; see [`next_dir_move`].
///
/// Each move re-scans the inputs, so the cost is the tree size times the number of directory moves
/// in one pass; an index by collision key and uuid is the upgrade if mass moves show up.
pub(crate) fn fold_dir_moves(
	mode: super::SyncMode,
	baseline: &mut HashMap<String, BaselineEntry>,
	local: &mut HashMap<String, LocalNode>,
	remote: &mut HashMap<String, RemoteNode>,
	held: &HashSet<String>,
) -> Vec<SyncAction> {
	let mut renames = Vec::new();
	while let Some(action) = next_case_only_dir_rename(mode, baseline, local, remote)
		.or_else(|| next_dir_move(mode, baseline, local, remote, held))
	{
		let (from, to) = action.endpoints();
		if matches!(action, SyncAction::MoveRemote { .. }) {
			rekey_subtree(remote, from, to, |node, path| {
				node.rel_path = path.to_string()
			});
		} else {
			rekey_subtree(local, from, to, |node, path| {
				node.rel_path = path.to_string()
			});
		}
		rekey_subtree(baseline, from, to, |entry, path| {
			entry.rel_path = path.to_string()
		});
		tracing::debug!(
			"plan: {} — the two sides spell the directory differently only by case",
			action.describe()
		);
		renames.push(action);
	}
	renames
}

/// The shallowest case-only directory rename left in the inputs (see [`fold_dir_moves`]).
fn next_case_only_dir_rename(
	mode: super::SyncMode,
	baseline: &HashMap<String, BaselineEntry>,
	local: &HashMap<String, LocalNode>,
	remote: &HashMap<String, RemoteNode>,
) -> Option<SyncAction> {
	// Collision key -> the local directory no remote item holds under that exact spelling. The scan
	// refuses two local entries with one key, so the map loses nothing.
	let local_only: HashMap<String, &str> = local
		.iter()
		.filter(|(path, node)| node.kind == NodeKind::Dir && !remote.contains_key(*path))
		.map(|(path, _)| (collision_key(path), path.as_str()))
		.collect();
	if local_only.is_empty() {
		return None;
	}
	let mut candidates: Vec<(&str, &str, Uuid)> = remote
		.iter()
		.filter(|(path, node)| node.kind == NodeKind::Dir && !local.contains_key(*path))
		.filter_map(|(remote_path, node)| {
			let local_path = *local_only.get(&collision_key(remote_path))?;
			(parent_path(local_path) == parent_path(remote_path)).then_some((
				local_path,
				remote_path.as_str(),
				node.remote_uuid,
			))
		})
		.collect();
	candidates
		.sort_unstable_by_key(|(local_path, ..)| (local_path.matches('/').count(), *local_path));
	candidates
		.into_iter()
		.find_map(|(local_path, remote_path, remote_uuid)| {
			let at_local = baseline.get(local_path);
			let at_remote = baseline.get(remote_path);
			if at_local
				.into_iter()
				.chain(at_remote)
				.any(|row| row.state.is_conflict())
			{
				return None;
			}
			let rename_remote = match mode {
				super::SyncMode::LocalToRemote | super::SyncMode::LocalBackup => true,
				super::SyncMode::RemoteToLocal | super::SyncMode::RemoteBackup => false,
				super::SyncMode::TwoWay => {
					at_local.is_none()
						&& at_remote.is_some_and(|row| {
							row.kind == NodeKind::Dir
								&& row.state == BaselineState::Synced
								&& row.remote_uuid == Some(remote_uuid)
						})
				}
			};
			Some(if rename_remote {
				SyncAction::MoveRemote {
					from_path: remote_path.to_string(),
					to_path: local_path.to_string(),
					kind: NodeKind::Dir,
					remote_uuid,
				}
			} else {
				SyncAction::MoveLocal {
					from_path: local_path.to_string(),
					to_path: remote_path.to_string(),
					kind: NodeKind::Dir,
				}
			})
		})
}

/// The shallowest directory move to a DIFFERENT path left in the inputs (see [`fold_dir_moves`]).
/// Two ways to find one, each tied to the side that moved:
///
/// - Pulled (a mode that pulls): a `Synced` baseline directory whose remote uuid the view now shows
///   at another path, while the local directory still sits at the recorded path. The uuid is the
///   server's own identity for the directory, so the match is certain. It becomes a `MoveLocal`.
/// - Pushed (a mode that pushes): a `Synced` baseline directory gone locally, still on the remote
///   under the recorded uuid at the recorded path, whose subtree reappears under exactly one new
///   local directory ([`dir_signature`]: the same relative paths and kinds, and every file with the
///   same hash and size). The subtree has to hold at least one file — a subtree of empty
///   directories says nothing about identity — and a second match on either end is ambiguous. It
///   becomes a `MoveRemote`. A move that also changed something inside does not match; it falls
///   back to the per-path plan, which is correct and only slower.
///
/// Either is left to the per-path plan when any of these holds:
/// - a baseline row at or under the source is not `Synced` (a held conflict, an adopted row);
/// - the destination is taken, or has something under it, in the baseline or on the side being
///   moved — compared by collision key, which is how the server dedups names;
/// - one end lies under the other, or under a path the cache is holding (`held`);
/// - the destination's parent directory does not exist yet on the side being moved: the moves run
///   before the creates of the pass;
/// - on the remote, the path a directory passes through between its re-parent and its rename is
///   taken.
fn next_dir_move(
	mode: super::SyncMode,
	baseline: &HashMap<String, BaselineEntry>,
	local: &HashMap<String, LocalNode>,
	remote: &HashMap<String, RemoteNode>,
	held: &HashSet<String>,
) -> Option<SyncAction> {
	let mut sources: Vec<(&str, Uuid)> = baseline
		.iter()
		.filter(|(_, row)| row.kind == NodeKind::Dir && row.state == BaselineState::Synced)
		.filter_map(|(path, row)| Some((path.as_str(), row.remote_uuid?)))
		.collect();
	if sources.is_empty() {
		return None;
	}
	sources.sort_unstable_by_key(|(path, _)| (path.matches('/').count(), *path));
	let remote_dir_at: HashMap<Uuid, &str> = remote
		.iter()
		.filter(|_| mode.pulls())
		.filter(|(_, node)| node.kind == NodeKind::Dir)
		.map(|(path, node)| (node.remote_uuid, path.as_str()))
		.collect();
	// The new local directories and what each holds, built only once a pushed move is possible.
	let mut new_local_dirs: Option<Vec<(&str, Signature<'_>)>> = None;
	let is_dir = |node: Option<&RemoteNode>| node.is_some_and(|n| n.kind == NodeKind::Dir);

	sources.iter().find_map(|&(from, uuid)| {
		let action = if local.get(from).is_some_and(|n| n.kind == NodeKind::Dir) {
			let to = *remote_dir_at.get(&uuid)?;
			let parent = parent_path(to);
			let parent_exists =
				parent.is_empty() || local.get(parent).is_some_and(|n| n.kind == NodeKind::Dir);
			(parent_exists && !occupied(local, to)).then(|| SyncAction::MoveLocal {
				from_path: from.to_string(),
				to_path: to.to_string(),
				kind: NodeKind::Dir,
			})?
		} else if mode.pushes()
			&& remote
				.get(from)
				.is_some_and(|n| n.kind == NodeKind::Dir && n.remote_uuid == uuid)
			&& !occupied(local, from)
		{
			let signature = baseline_dir_signature(baseline, from)?;
			let new_dirs = new_local_dirs.get_or_insert_with(|| {
				local
					.iter()
					.filter(|(path, node)| {
						node.kind == NodeKind::Dir && !baseline.contains_key(path.as_str())
					})
					.filter_map(|(path, _)| Some((path.as_str(), dir_signature(local, path)?)))
					.collect()
			});
			let mut matches = new_dirs.iter().filter(|(_, s)| *s == signature);
			let (to, _) = matches.next()?;
			if matches.next().is_some() {
				return None;
			}
			// The same subtree vanished from somewhere else too: which of the two moved is a guess.
			let twin = sources.iter().any(|&(other, _)| {
				other != from
					&& !local.contains_key(other)
					&& baseline_dir_signature(baseline, other).as_ref() == Some(&signature)
			});
			let (to_parent, to_name) = (parent_path(to), leaf(to));
			let via = match to_parent.is_empty() {
				true => leaf(from).to_string(),
				false => format!("{to_parent}/{}", leaf(from)),
			};
			let passes_free =
				parent_path(from) == to_parent || leaf(from) == to_name || !occupied(remote, &via);
			let parent_exists = to_parent.is_empty() || is_dir(remote.get(to_parent));
			(!twin && parent_exists && passes_free && !occupied(remote, to)).then(|| {
				SyncAction::MoveRemote {
					from_path: from.to_string(),
					to_path: to.to_string(),
					kind: NodeKind::Dir,
					remote_uuid: uuid,
				}
			})?
		} else {
			return None;
		};
		let (from_key, to_key) = (collision_key(from), collision_key(action.rel_path()));
		let to = action.rel_path();
		let clear = from_key != to_key
			&& !is_under(&to_key, &from_key)
			&& !is_under(&from_key, &to_key)
			&& !occupied(baseline, to)
			&& ![from, to].into_iter().any(|end| {
				is_under_held_path(end, held) || held.iter().any(|path| is_under(path, end))
			}) && baseline
			.iter()
			.all(|(path, row)| !is_under(path, from) || row.state == BaselineState::Synced);
		clear.then_some(action)
	})
}

/// What a directory holds, relative to it: every path under it with its kind and, for a file, its
/// content hash and size. Two directories with equal signatures hold the same tree.
type Signature<'a> = BTreeMap<&'a str, (NodeKind, Option<Blake3Hash>, u64)>;

/// The signature of the local directory at `root`; `None` when a file under it has no hash.
fn dir_signature<'a>(local: &'a HashMap<String, LocalNode>, root: &str) -> Option<Signature<'a>> {
	local
		.iter()
		.filter(|(path, _)| is_under(path, root))
		.map(|(path, node)| {
			let (hash, size) = match node.kind {
				NodeKind::File => (Some(node.content_hash?), node.size),
				NodeKind::Dir => (None, 0),
			};
			Some((&path[root.len()..], (node.kind, hash, size)))
		})
		.collect()
}

/// The signature the baseline recorded for the directory at `root` — `None` when it cannot vouch
/// for the subtree: a row under it that is not `Synced`, a file row without a hash or a size, or no
/// file at all.
fn baseline_dir_signature<'a>(
	baseline: &'a HashMap<String, BaselineEntry>,
	root: &str,
) -> Option<Signature<'a>> {
	let signature: Signature<'a> = baseline
		.iter()
		.filter(|(path, _)| is_under(path, root))
		.map(|(path, row)| {
			if row.state != BaselineState::Synced {
				return None;
			}
			let (hash, size) = match row.kind {
				NodeKind::File => (Some(row.content_hash?), row.size?),
				NodeKind::Dir => (None, 0),
			};
			Some((&path[root.len()..], (row.kind, hash, size)))
		})
		.collect::<Option<_>>()?;
	signature
		.values()
		.any(|(kind, ..)| *kind == NodeKind::File)
		.then_some(signature)
}

/// Whether `map` holds `rel_path`, under any spelling, or anything under it.
fn occupied<T>(map: &HashMap<String, T>, rel_path: &str) -> bool {
	let key = collision_key(rel_path);
	map.keys().any(|path| {
		let path = collision_key(path);
		path == key || is_under(&path, &key)
	})
}

/// The last component of a `/`-joined relative path.
fn leaf(rel_path: &str) -> &str {
	rel_path.rsplit_once('/').map_or(rel_path, |(_, name)| name)
}

/// The parent part of a `/`-joined relative path (`""` at the top level).
fn parent_path(rel_path: &str) -> &str {
	rel_path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

/// Move every entry at `from` or under it to the same place under `to`, rewriting the path the
/// entry carries with `set_path`.
fn rekey_subtree<T>(
	map: &mut HashMap<String, T>,
	from: &str,
	to: &str,
	set_path: impl Fn(&mut T, &str),
) {
	let moving: Vec<String> = map
		.keys()
		.filter(|key| key.as_str() == from || is_under(key, from))
		.cloned()
		.collect();
	for old in moving {
		let Some(mut value) = map.remove(&old) else {
			continue;
		};
		let new = format!("{to}{}", &old[from.len()..]);
		set_path(&mut value, &new);
		map.insert(new, value);
	}
}

/// What a pass must NOT act on, beyond what the three inputs themselves say.
#[derive(Debug, Default)]
pub(crate) struct PassHolds {
	/// Remote uuids this engine already sent to the trash, whose removal the cache has not applied
	/// yet. Only the DELETION of those uuids is suppressed, not their paths: a
	/// delete-then-recreate uploads a new file at the very path that was just trashed, and
	/// freezing it would stall that.
	pub(crate) trashed: HashSet<Uuid>,
	/// Remote paths the cache is showing mid-transition (see [`RemoteView::held_paths`]).
	pub(crate) held_remote: HashSet<String>,
}

/// One reconciled pass.
pub(crate) struct Plan {
	pub(crate) actions: Vec<SyncAction>,
	/// How many paths the pass deliberately left alone (an unresolvable name, or a deletion this
	/// engine has already made). A pass that skips what it would otherwise have done is not the
	/// same as a pass with nothing to do, and the report has to be able to tell them apart.
	pub(crate) deferred_paths: usize,
}

/// Reconcile a pair's three inputs into an ordered action plan. `baseline`/`local`/`remote` are all
/// keyed by the same NFC-normalized relative path; `holds` is what the pass must leave alone.
pub(crate) fn reconcile(
	mode: super::SyncMode,
	baseline: &HashMap<String, BaselineEntry>,
	local: &HashMap<String, LocalNode>,
	remote: &HashMap<String, RemoteNode>,
	holds: &PassHolds,
) -> Plan {
	let mut actions = Vec::new();
	let mut consumed = HashSet::new();
	tracing::debug!(
		"reconcile: mode {mode:?} — {} baseline / {} local / {} remote entries",
		baseline.len(),
		local.len(),
		remote.len()
	);
	let keys: BTreeSet<&str> = baseline
		.keys()
		.chain(local.keys())
		.chain(remote.keys())
		.map(String::as_str)
		.collect();

	// Consume the paths the view could not resolve before anything else looks at them, so neither
	// move detection nor the per-path reconcile acts on a name the cache is showing twice. (A path
	// this engine has just written to needs no such hold: the engine folds its own unacknowledged
	// writes into the view instead — see `PendingWrites::fold_into`.)
	//
	// A held path with nothing on either side is skipped by nobody below, but it IS being withheld
	// and the report has to say so.
	let mut deferred_paths = holds
		.held_remote
		.iter()
		.filter(|path| !keys.contains(path.as_str()))
		.count();
	if !holds.held_remote.is_empty() {
		for key in &keys {
			if is_under_held_path(key, &holds.held_remote) {
				tracing::debug!(
					"reconcile: skipping {key:?} — the cache is listing that name twice, so the view cannot resolve it"
				);
				consumed.insert((*key).to_string());
				deferred_paths += 1;
			}
		}
	}

	// Resolve moves next; their endpoints are then excluded from the per-path reconcile so a move
	// is never also emitted as a delete + create. This runs in EVERY mode, backup modes included: a
	// rename on the source side is one item that changed name, and mirroring it as a metadata-only
	// re-parent/rename keeps the backup a faithful copy instead of accumulating the old name beside
	// the new one forever. It does not weaken the backup guarantee — a move deletes nothing on the
	// destination, and a real deletion (content that reappears nowhere) is still suppressed by the
	// `delete_ok` gate below.
	detect_moves(mode, baseline, local, remote, &mut actions, &mut consumed);

	for key in keys {
		if consumed.contains(key) {
			continue;
		}
		let base = baseline.get(key);
		// A held conflict is never acted on until the caller resolves it — but it IS re-reported
		// every pass, so a caller watching the reports keeps seeing what is outstanding. Its
		// subtree is suppressed below, along with any conflict surfaced by this pass.
		if base.is_some_and(|b| b.state.is_conflict()) {
			actions.push(SyncAction::Conflict {
				rel_path: key.to_string(),
			});
			continue;
		}
		let local_node = local.get(key);
		let remote_node = remote.get(key);

		// A path adopted from the destination at a mode switch (see
		// [`adopt_destination_rows`]). The row is never a baseline for classification — whatever
		// still holds the path reads as newly created — and in a one-way mode it holds off the
		// destination-side deletion for as long as the source has nothing there.
		let mut base = base;
		if base.is_some_and(|b| b.state == BaselineState::Adopted) {
			let source_holds = match mode {
				super::SyncMode::LocalToRemote | super::SyncMode::LocalBackup => {
					local_node.is_some()
				}
				super::SyncMode::RemoteToLocal | super::SyncMode::RemoteBackup => {
					remote_node.is_some()
				}
				// Both sides are sources; the surviving copy flows back to the other one.
				super::SyncMode::TwoWay => local_node.is_some() || remote_node.is_some(),
			};
			if local_node.is_some() || remote_node.is_some() {
				if !source_holds {
					tracing::debug!(
						"reconcile: leaving {key:?} alone — the destination's copy was adopted at a mode switch and the source is still empty here"
					);
					continue;
				}
				base = None;
			}
			// Neither side holds the path any more: the real row falls through below, where the
			// both-absent arm retires it.
		}

		// A backup mode is its mirror mode minus deletions on the destination — the ONE difference,
		// so the two share an arm and the deletion policy is read off the mode itself.
		let delete_ok = mode.propagates_deletes();
		match mode {
			super::SyncMode::LocalToRemote | super::SyncMode::LocalBackup => {
				push_to_remote(key, local_node, remote_node, base, delete_ok, &mut actions)
			}
			super::SyncMode::RemoteToLocal | super::SyncMode::RemoteBackup => {
				pull_to_local(key, local_node, remote_node, base, delete_ok, &mut actions)
			}
			super::SyncMode::TwoWay => {
				reconcile_two_way(key, local_node, remote_node, base, &mut actions)
			}
		}
	}

	if !holds.trashed.is_empty() {
		actions.retain(|action| {
			let SyncAction::TrashRemote {
				rel_path,
				remote_uuid,
				..
			} = action
			else {
				return true;
			};
			if !holds.trashed.contains(remote_uuid) {
				return true;
			}
			tracing::debug!(
				"reconcile: skipping the remote deletion of {rel_path:?} — this engine already trashed it and the cache has not caught up"
			);
			deferred_paths += 1;
			false
		});
	}

	suppress_conflicted_subtrees(&mut actions);
	order_actions(&mut actions);
	tracing::debug!(
		"reconcile: planned {} action(s), {deferred_paths} path(s) deferred",
		actions.len()
	);
	Plan {
		actions,
		deferred_paths,
	}
}

/// Apply phases, lowest first. A "replace-delete" (a delete whose path is also created this pass —
/// a file<->dir type flip) has to precede the create at that path, but a DIRECTORY delete is
/// recursive (the server trashes a dir's whole subtree; a local delete quarantines it), so it must
/// still wait until everything moving OUT of that directory has moved.
///
/// A directory move runs before all of them: the rest of the plan already names its subtree by the
/// path it moves to (see [`fold_dir_moves`]).
const PHASE_DIR_MOVE: u8 = 0;
const PHASE_FILE_REPLACE_DELETE: u8 = 1;
const PHASE_CREATE: u8 = 2;
const PHASE_MOVE: u8 = 3;
const PHASE_DIR_REPLACE_DELETE: u8 = 4;
const PHASE_TRANSFER: u8 = 5;
const PHASE_DELETE: u8 = 6;
const PHASE_CONFLICT: u8 = 7;

/// The apply phase of an action given the set of paths the pass materializes (`create_targets`).
/// See the `PHASE_*` constants; other deletes run LAST (child-before-parent) so a directory delete
/// cannot strand an item an earlier move/transfer still needs.
fn action_phase(action: &SyncAction, create_targets: &std::collections::HashSet<String>) -> u8 {
	match action {
		SyncAction::MoveRemote {
			kind: NodeKind::Dir,
			..
		}
		| SyncAction::MoveLocal {
			kind: NodeKind::Dir,
			..
		} => PHASE_DIR_MOVE,
		// Replace-delete of a FILE: removing it strands nothing, so it goes first — the create at
		// that path would otherwise hit the still-present old item.
		SyncAction::DeleteLocal {
			kind: NodeKind::File,
			..
		}
		| SyncAction::TrashRemote {
			kind: NodeKind::File,
			..
		} if create_targets.contains(action.rel_path()) => PHASE_FILE_REPLACE_DELETE,
		// Baseline bookkeeping, no transfer/dependency — order is irrelevant; runs in `pre`.
		SyncAction::AdoptBaseline { .. } => PHASE_CREATE,
		// Directories that may host a move/transfer destination — created early, parent-before-child.
		SyncAction::CreateLocalDir { .. } | SyncAction::CreateRemoteDir { .. } => PHASE_CREATE,
		// Re-parent/rename in place: sources still exist (deletes run later), destinations now exist.
		SyncAction::MoveLocal { .. } | SyncAction::MoveRemote { .. } => PHASE_MOVE,
		// Replace-delete of a DIRECTORY: recursive, so it runs only after the moves that carry
		// items out of it — but still before the transfer that recreates the path as a file.
		SyncAction::DeleteLocal { .. } | SyncAction::TrashRemote { .. }
			if create_targets.contains(action.rel_path()) =>
		{
			PHASE_DIR_REPLACE_DELETE
		}
		// Content transfers into already-created parents.
		SyncAction::UploadFile { .. } | SyncAction::DownloadFile { .. } => PHASE_TRANSFER,
		// Destructive last, child-before-parent (see `order_actions`).
		SyncAction::DeleteLocal { .. } | SyncAction::TrashRemote { .. } => PHASE_DELETE,
		// Conflicts are never executed (the engine splits them out); order is irrelevant.
		SyncAction::Conflict { .. } => PHASE_CONFLICT,
	}
}

/// Paths that a create/transfer materializes this pass (see [`SyncAction::is_create`]).
pub(super) fn create_target_paths(actions: &[SyncAction]) -> std::collections::HashSet<String> {
	actions
		.iter()
		.filter(|a| a.is_create())
		.map(|a| a.rel_path().to_string())
		.collect()
}

/// Order the plan so it applies safely: file replace-deletes → creates (parent-before-child) →
/// moves → directory replace-deletes → transfers → the remaining deletions (child-before-parent).
/// Deletions run last so a cascading directory delete never removes a path an earlier move/transfer
/// depends on; within a delete phase, child-before-parent keeps a server-side cascade or a local
/// subtree quarantine from racing its own children.
///
/// A file<->directory type flip at one path resolves in a SINGLE pass: the replace-delete phases
/// put the old item's removal ahead of the create at that path, with the directory case held back
/// until after the moves because trashing a directory takes its whole subtree with it.
fn order_actions(actions: &mut [SyncAction]) {
	let create_targets = create_target_paths(actions);
	actions.sort_by(|a, b| {
		let (pa, pb) = (
			action_phase(a, &create_targets),
			action_phase(b, &create_targets),
		);
		if pa != pb {
			pa.cmp(&pb)
		} else if pa == PHASE_DIR_REPLACE_DELETE || pa == PHASE_DELETE {
			// Deletes that cascade: child-before-parent (descending path).
			b.rel_path().cmp(a.rel_path())
		} else {
			// Everything else: parent-before-child (ascending path).
			a.rel_path().cmp(b.rel_path())
		}
	});
}

#[cfg(test)]
mod tests {
	use std::borrow::Cow;

	use chrono::{DateTime, Utc};

	use super::{
		super::engine::{Observations, PendingKind, PendingWrites},
		*,
	};
	use crate::sync_engine::SyncMode;

	/// Reconcile with no pending writes — the ordinary case; the cache-lag window has its own tests.
	fn plan(
		mode: SyncMode,
		baseline: &HashMap<String, BaselineEntry>,
		local: &HashMap<String, LocalNode>,
		remote: &HashMap<String, RemoteNode>,
	) -> Vec<SyncAction> {
		reconcile(mode, baseline, local, remote, &PassHolds::default()).actions
	}

	/// The pair the fold tests write as.
	const PAIR: super::super::baseline::PairId = 1;

	/// What a pass does: correct the snapshot with the engine's own unacknowledged writes, then
	/// reconcile against the corrected view.
	fn plan_folded(
		mode: SyncMode,
		baseline: &HashMap<String, BaselineEntry>,
		local: &HashMap<String, LocalNode>,
		remote: &HashMap<String, RemoteNode>,
		writes: &PendingWrites,
	) -> Vec<SyncAction> {
		let mut remote = remote.clone();
		writes.fold_into(PAIR, baseline, &mut remote);
		reconcile(mode, baseline, local, &remote, &PassHolds::default()).actions
	}

	/// What a pass plans once the case-only directory renames are folded into its inputs: the
	/// renames first, then the reconciled rest.
	fn plan_with_renames(
		mode: SyncMode,
		baseline: &HashMap<String, BaselineEntry>,
		local: &HashMap<String, LocalNode>,
		remote: &HashMap<String, RemoteNode>,
	) -> Vec<SyncAction> {
		let (mut baseline, mut local, mut remote) =
			(baseline.clone(), local.clone(), remote.clone());
		let mut actions = fold_dir_moves(
			mode,
			&mut baseline,
			&mut local,
			&mut remote,
			&HashSet::new(),
		);
		actions.extend(plan(mode, &baseline, &local, &remote));
		actions
	}

	/// A synced directory holding `a.txt`, spelled `base_name` in the baseline, `local_name` on disk
	/// and `remote_name` on the remote.
	fn case_tree(
		base_name: &str,
		local_name: &str,
		remote_name: &str,
		dir: Uuid,
		file: Uuid,
	) -> (
		HashMap<String, BaselineEntry>,
		HashMap<String, LocalNode>,
		HashMap<String, RemoteNode>,
	) {
		let child = |name: &str| format!("{name}/a.txt");
		let baseline = HashMap::from([
			(base_name.to_string(), base_dir(base_name, dir)),
			(
				child(base_name),
				base_file(&child(base_name), file, [1; 32]),
			),
		]);
		let local = HashMap::from([
			(local_name.to_string(), local_dir(local_name)),
			(child(local_name), local_file(&child(local_name), [1; 32])),
		]);
		let remote = HashMap::from([
			(remote_name.to_string(), remote_dir_node(remote_name, dir)),
			(
				child(remote_name),
				remote_file(&child(remote_name), file, [1; 32]),
			),
		]);
		(baseline, local, remote)
	}

	/// A directory renamed only by case on the LOCAL side is renamed in place on the remote, and
	/// nothing else happens: no create at the new spelling (the dedup would hand it the same
	/// directory) and no trash at the old one (which would take that directory down with its
	/// subtree).
	#[test]
	fn a_local_case_only_dir_rename_renames_the_remote_dir_in_place() {
		for mode in [
			SyncMode::TwoWay,
			SyncMode::LocalToRemote,
			SyncMode::LocalBackup,
		] {
			let (dir, file) = (Uuid::new_v4(), Uuid::new_v4());
			let (baseline, local, remote) = case_tree("Docs", "docs", "Docs", dir, file);
			assert_eq!(
				plan_with_renames(mode, &baseline, &local, &remote),
				vec![SyncAction::MoveRemote {
					from_path: "Docs".to_string(),
					to_path: "docs".to_string(),
					kind: NodeKind::Dir,
					remote_uuid: dir,
				}],
				"{mode:?}"
			);
		}
	}

	/// The mirror: a directory renamed only by case on the REMOTE side is renamed in place locally,
	/// instead of quarantining the local subtree and re-creating it.
	#[test]
	fn a_remote_case_only_dir_rename_renames_the_local_dir_in_place() {
		for mode in [
			SyncMode::TwoWay,
			SyncMode::RemoteToLocal,
			SyncMode::RemoteBackup,
		] {
			let (dir, file) = (Uuid::new_v4(), Uuid::new_v4());
			let (baseline, local, remote) = case_tree("Docs", "Docs", "docs", dir, file);
			assert_eq!(
				plan_with_renames(mode, &baseline, &local, &remote),
				vec![SyncAction::MoveLocal {
					from_path: "Docs".to_string(),
					to_path: "docs".to_string(),
					kind: NodeKind::Dir,
				}],
				"{mode:?}"
			);
		}
	}

	/// A one-way mode puts its SOURCE's spelling back on a destination whose case drifted, whatever
	/// the baseline says.
	#[test]
	fn a_one_way_mode_renames_its_destination_to_the_source_spelling() {
		let (dir, file) = (Uuid::new_v4(), Uuid::new_v4());
		let (baseline, local, remote) = case_tree("Docs", "Docs", "docs", dir, file);
		assert_eq!(
			plan_with_renames(SyncMode::LocalToRemote, &baseline, &local, &remote),
			vec![SyncAction::MoveRemote {
				from_path: "docs".to_string(),
				to_path: "Docs".to_string(),
				kind: NodeKind::Dir,
				remote_uuid: dir,
			}]
		);
	}

	/// Without the fold the same inputs plan the destructive shape this exists to prevent — a create
	/// at one spelling and a directory trash at the other. Pins why the fold has to run first.
	#[test]
	fn a_case_only_dir_rename_planned_per_path_trashes_the_directory() {
		let (dir, file) = (Uuid::new_v4(), Uuid::new_v4());
		let (baseline, local, remote) = case_tree("Docs", "docs", "Docs", dir, file);
		let actions = plan(SyncMode::TwoWay, &baseline, &local, &remote);
		assert!(
			actions.contains(&SyncAction::TrashRemote {
				rel_path: "Docs".to_string(),
				kind: NodeKind::Dir,
				remote_uuid: dir,
			}),
			"{actions:?}"
		);
	}

	/// A child that changed in the same pass is still planned — at the new spelling, after the
	/// rename.
	#[test]
	fn a_child_edited_under_a_case_renamed_dir_is_pushed_at_the_new_spelling() {
		let (dir, file) = (Uuid::new_v4(), Uuid::new_v4());
		let (baseline, mut local, remote) = case_tree("Docs", "docs", "Docs", dir, file);
		local.insert("docs/a.txt".to_string(), local_file("docs/a.txt", [2; 32]));
		let rename = SyncAction::MoveRemote {
			from_path: "Docs".to_string(),
			to_path: "docs".to_string(),
			kind: NodeKind::Dir,
			remote_uuid: dir,
		};
		let mut ordered = vec![
			SyncAction::UploadFile {
				rel_path: "docs/a.txt".to_string(),
			},
			rename.clone(),
		];
		order_actions(&mut ordered);
		assert_eq!(
			ordered[0], rename,
			"the rename runs before anything under it"
		);
		assert_eq!(
			plan_with_renames(SyncMode::TwoWay, &baseline, &local, &remote),
			vec![
				rename,
				SyncAction::UploadFile {
					rel_path: "docs/a.txt".to_string(),
				},
			]
		);
	}

	/// A case change at two levels is two renames, the parent first, and nothing more.
	#[test]
	fn nested_case_only_dir_renames_fold_parent_first() {
		let (outer, inner, file) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
		let baseline = HashMap::from([
			("Docs".to_string(), base_dir("Docs", outer)),
			("Docs/Sub".to_string(), base_dir("Docs/Sub", inner)),
			(
				"Docs/Sub/a.txt".to_string(),
				base_file("Docs/Sub/a.txt", file, [1; 32]),
			),
		]);
		let local = HashMap::from([
			("docs".to_string(), local_dir("docs")),
			("docs/sub".to_string(), local_dir("docs/sub")),
			(
				"docs/sub/a.txt".to_string(),
				local_file("docs/sub/a.txt", [1; 32]),
			),
		]);
		let remote = HashMap::from([
			("Docs".to_string(), remote_dir_node("Docs", outer)),
			("Docs/Sub".to_string(), remote_dir_node("Docs/Sub", inner)),
			(
				"Docs/Sub/a.txt".to_string(),
				remote_file("Docs/Sub/a.txt", file, [1; 32]),
			),
		]);
		assert_eq!(
			plan_with_renames(SyncMode::TwoWay, &baseline, &local, &remote),
			vec![
				SyncAction::MoveRemote {
					from_path: "Docs".to_string(),
					to_path: "docs".to_string(),
					kind: NodeKind::Dir,
					remote_uuid: outer,
				},
				SyncAction::MoveRemote {
					from_path: "docs/Sub".to_string(),
					to_path: "docs/sub".to_string(),
					kind: NodeKind::Dir,
					remote_uuid: inner,
				},
			]
		);
	}

	/// A path held in conflict is not renamed: the conflict hold decides it.
	#[test]
	fn a_conflicted_dir_is_not_case_renamed() {
		let (dir, file) = (Uuid::new_v4(), Uuid::new_v4());
		let (mut baseline, mut local, mut remote) = case_tree("Docs", "docs", "Docs", dir, file);
		baseline.get_mut("Docs").unwrap().state = BaselineState::Conflicted;
		assert!(
			fold_dir_moves(
				SyncMode::TwoWay,
				&mut baseline,
				&mut local,
				&mut remote,
				&HashSet::new()
			)
			.is_empty()
		);
	}

	/// The uuids of a synced directory tree `<dir>/a.txt`, `<dir>/sub/`, `<dir>/sub/b.txt`.
	#[derive(Clone, Copy)]
	struct TreeIds {
		dir: Uuid,
		a: Uuid,
		sub: Uuid,
		b: Uuid,
	}

	impl TreeIds {
		fn new() -> Self {
			Self {
				dir: Uuid::new_v4(),
				a: Uuid::new_v4(),
				sub: Uuid::new_v4(),
				b: Uuid::new_v4(),
			}
		}

		fn paths(dir: &str) -> [String; 4] {
			[
				dir.to_string(),
				format!("{dir}/a.txt"),
				format!("{dir}/sub"),
				format!("{dir}/sub/b.txt"),
			]
		}

		fn baseline(self, dir: &str) -> Vec<(String, BaselineEntry)> {
			let [d, a, s, b] = Self::paths(dir);
			vec![
				(d.clone(), base_dir(&d, self.dir)),
				(a.clone(), base_file(&a, self.a, [1; 32])),
				(s.clone(), base_dir(&s, self.sub)),
				(b.clone(), base_file(&b, self.b, [2; 32])),
			]
		}

		fn local(dir: &str) -> Vec<(String, LocalNode)> {
			let [d, a, s, b] = Self::paths(dir);
			vec![
				(d.clone(), local_dir(&d)),
				(a.clone(), local_file(&a, [1; 32])),
				(s.clone(), local_dir(&s)),
				(b.clone(), local_file(&b, [2; 32])),
			]
		}

		fn remote(self, dir: &str) -> Vec<(String, RemoteNode)> {
			let [d, a, s, b] = Self::paths(dir);
			vec![
				(d.clone(), remote_dir_node(&d, self.dir)),
				(a.clone(), remote_file(&a, self.a, [1; 32])),
				(s.clone(), remote_dir_node(&s, self.sub)),
				(b.clone(), remote_file(&b, self.b, [2; 32])),
			]
		}
	}

	/// A pair holding the synced tree under `docs/` plus an empty synced `archive/`, with the local
	/// side holding it under `local_dir` and the remote under `remote_dir`.
	fn moved_tree(
		ids: TreeIds,
		local_at: &str,
		remote_at: &str,
	) -> (
		HashMap<String, BaselineEntry>,
		HashMap<String, LocalNode>,
		HashMap<String, RemoteNode>,
	) {
		let archive = Uuid::new_v4();
		let mut baseline: HashMap<_, _> = ids.baseline("docs").into_iter().collect();
		baseline.insert("archive".to_string(), base_dir("archive", archive));
		let mut local: HashMap<_, _> = TreeIds::local(local_at).into_iter().collect();
		local.insert("archive".to_string(), local_dir("archive"));
		let mut remote: HashMap<_, _> = ids.remote(remote_at).into_iter().collect();
		remote.insert("archive".to_string(), remote_dir_node("archive", archive));
		(baseline, local, remote)
	}

	fn dir_moves_in(actions: &[SyncAction]) -> Vec<&SyncAction> {
		actions
			.iter()
			.filter(|action| {
				matches!(
					action,
					SyncAction::MoveRemote {
						kind: NodeKind::Dir,
						..
					} | SyncAction::MoveLocal {
						kind: NodeKind::Dir,
						..
					}
				)
			})
			.collect()
	}

	/// A directory the remote moved and renamed is ONE local move — children carried with it,
	/// nothing downloaded, quarantined or re-created — in every mode that pulls.
	#[test]
	fn a_remote_dir_move_is_one_local_move() {
		for mode in [
			SyncMode::TwoWay,
			SyncMode::RemoteToLocal,
			SyncMode::RemoteBackup,
		] {
			let (baseline, local, remote) = moved_tree(TreeIds::new(), "docs", "archive/documents");
			assert_eq!(
				plan_with_renames(mode, &baseline, &local, &remote),
				vec![SyncAction::MoveLocal {
					from_path: "docs".to_string(),
					to_path: "archive/documents".to_string(),
					kind: NodeKind::Dir,
				}],
				"{mode:?}"
			);
		}
	}

	/// A directory the local side moved and renamed, with its subtree intact, is ONE remote move of
	/// the same uuid in every mode that pushes.
	#[test]
	fn a_local_dir_move_is_one_remote_move_of_the_same_uuid() {
		for mode in [
			SyncMode::TwoWay,
			SyncMode::LocalToRemote,
			SyncMode::LocalBackup,
		] {
			let ids = TreeIds::new();
			let (baseline, local, remote) = moved_tree(ids, "archive/documents", "docs");
			assert_eq!(
				plan_with_renames(mode, &baseline, &local, &remote),
				vec![SyncAction::MoveRemote {
					from_path: "docs".to_string(),
					to_path: "archive/documents".to_string(),
					kind: NodeKind::Dir,
					remote_uuid: ids.dir,
				}],
				"{mode:?}"
			);
		}
	}

	/// Without the fold a local directory rename is the per-path plan: a new directory, a move per
	/// file, and the old directory trashed. Pins what the fold replaces.
	#[test]
	fn a_local_dir_rename_planned_per_path_re_creates_the_directory() {
		let ids = TreeIds::new();
		let (baseline, local, remote) = moved_tree(ids, "documents", "docs");
		let actions = plan(SyncMode::LocalToRemote, &baseline, &local, &remote);
		assert!(
			actions.contains(&SyncAction::TrashRemote {
				rel_path: "docs".to_string(),
				kind: NodeKind::Dir,
				remote_uuid: ids.dir,
			}),
			"{actions:?}"
		);
		assert!(
			actions.contains(&SyncAction::CreateRemoteDir {
				rel_path: "documents".to_string(),
			}),
			"{actions:?}"
		);
	}

	/// A local rename whose subtree also changed is not matched: an edited child, a child added and
	/// a child left behind each break the signature, and the pass falls back to the per-path plan.
	#[test]
	fn a_local_dir_rename_with_a_changed_subtree_is_not_matched() {
		type Change = fn(&mut HashMap<String, LocalNode>);
		let changes: [(&str, Change); 4] = [
			("a child edited", |local| {
				local.insert(
					"documents/a.txt".to_string(),
					local_file("documents/a.txt", [9; 32]),
				);
			}),
			("a child added", |local| {
				local.insert(
					"documents/new.txt".to_string(),
					local_file("documents/new.txt", [9; 32]),
				);
			}),
			("a child left behind", |local| {
				local.remove("documents/sub/b.txt");
				local.insert("b.txt".to_string(), local_file("b.txt", [2; 32]));
			}),
			("a child renamed", |local| {
				let node = local.remove("documents/a.txt").unwrap();
				local.insert(
					"documents/renamed.txt".to_string(),
					LocalNode {
						rel_path: "documents/renamed.txt".to_string(),
						..node
					},
				);
			}),
		];
		for (label, change) in changes {
			let (baseline, mut local, remote) = moved_tree(TreeIds::new(), "documents", "docs");
			change(&mut local);
			let actions = plan_with_renames(SyncMode::TwoWay, &baseline, &local, &remote);
			assert!(dir_moves_in(&actions).is_empty(), "{label}: {actions:?}");
		}
	}

	/// A subtree that holds no file at all says nothing about identity: two empty directories are
	/// not a move.
	#[test]
	fn a_local_rename_of_a_subtree_without_files_is_not_matched() {
		let (dir, sub) = (Uuid::new_v4(), Uuid::new_v4());
		let baseline = map(vec![
			("old", base_dir("old", dir)),
			("old/sub", base_dir("old/sub", sub)),
		]);
		let remote = map(vec![
			("old", remote_dir_node("old", dir)),
			("old/sub", remote_dir_node("old/sub", sub)),
		]);
		let local = map(vec![
			("new", local_dir("new")),
			("new/sub", local_dir("new/sub")),
		]);
		let actions = plan_with_renames(SyncMode::LocalToRemote, &baseline, &local, &remote);
		assert!(dir_moves_in(&actions).is_empty(), "{actions:?}");
	}

	/// Two candidates on either end make the match a guess, so neither is taken.
	#[test]
	fn an_ambiguous_local_dir_rename_is_not_matched() {
		// One vanished tree, two new copies of it.
		let (baseline, mut local, remote) = moved_tree(TreeIds::new(), "documents", "docs");
		local.extend(TreeIds::local("copy"));
		let actions = plan_with_renames(SyncMode::LocalToRemote, &baseline, &local, &remote);
		assert!(dir_moves_in(&actions).is_empty(), "two new: {actions:?}");

		// Two vanished trees with the same content, one new copy.
		let (mut baseline, local, mut remote) = moved_tree(TreeIds::new(), "documents", "docs");
		let twin = TreeIds::new();
		baseline.extend(twin.baseline("twin"));
		remote.extend(twin.remote("twin"));
		let actions = plan_with_renames(SyncMode::LocalToRemote, &baseline, &local, &remote);
		assert!(
			dir_moves_in(&actions).is_empty(),
			"two vanished: {actions:?}"
		);
	}

	/// A destination already taken — on the side being moved, or in the baseline — is not moved
	/// onto.
	#[test]
	fn a_dir_move_onto_an_existing_path_is_not_planned() {
		// Push: the remote already holds another directory under the new name. The tree is not
		// moved onto it; what lands inside it is the per-path plan's business (its `sub/` may still
		// move in as a directory of its own).
		let (baseline, local, mut remote) = moved_tree(TreeIds::new(), "documents", "docs");
		remote.insert(
			"documents".to_string(),
			remote_dir_node("documents", Uuid::new_v4()),
		);
		let actions = plan_with_renames(SyncMode::LocalToRemote, &baseline, &local, &remote);
		assert!(
			!dir_moves_in(&actions)
				.iter()
				.any(|action| action.rel_path() == "documents"),
			"push: {actions:?}"
		);

		// Pull: the local side already holds a directory where the remote moved it.
		let (baseline, mut local, remote) = moved_tree(TreeIds::new(), "docs", "documents");
		local.insert("documents".to_string(), local_dir("documents"));
		let actions = plan_with_renames(SyncMode::RemoteToLocal, &baseline, &local, &remote);
		assert!(
			moves_onto(&actions, "documents").is_none(),
			"pull: {actions:?}"
		);

		// Pull: the baseline still tracks something at the destination.
		let (mut baseline, local, remote) = moved_tree(TreeIds::new(), "docs", "documents");
		baseline.insert(
			"documents/x.txt".to_string(),
			base_file("documents/x.txt", Uuid::new_v4(), [3; 32]),
		);
		let actions = plan_with_renames(SyncMode::TwoWay, &baseline, &local, &remote);
		assert!(
			moves_onto(&actions, "documents").is_none(),
			"baseline: {actions:?}"
		);
	}

	/// The directory move in `actions` whose destination is `to`, if any.
	fn moves_onto<'a>(actions: &'a [SyncAction], to: &str) -> Option<&'a SyncAction> {
		dir_moves_in(actions)
			.into_iter()
			.find(|action| action.rel_path() == to)
	}

	/// A move into a directory that does not exist on the moved side yet is left to the per-path
	/// plan: the directory moves run before the creates.
	#[test]
	fn a_dir_move_into_a_new_parent_is_not_planned() {
		let (baseline, local, remote) = moved_tree(TreeIds::new(), "fresh/documents", "docs");
		let actions = plan_with_renames(SyncMode::LocalToRemote, &baseline, &local, &remote);
		assert!(dir_moves_in(&actions).is_empty(), "push: {actions:?}");

		let (baseline, local, remote) = moved_tree(TreeIds::new(), "docs", "fresh/documents");
		let actions = plan_with_renames(SyncMode::RemoteToLocal, &baseline, &local, &remote);
		assert!(dir_moves_in(&actions).is_empty(), "pull: {actions:?}");
	}

	/// A conflict held anywhere in the subtree keeps the directory where it is.
	#[test]
	fn a_dir_holding_a_conflict_is_not_moved() {
		let (mut baseline, local, remote) = moved_tree(TreeIds::new(), "docs", "documents");
		baseline.get_mut("docs/sub/b.txt").unwrap().state = BaselineState::Conflicted;
		let actions = plan_with_renames(SyncMode::TwoWay, &baseline, &local, &remote);
		assert!(dir_moves_in(&actions).is_empty(), "{actions:?}");
	}

	/// A path the cache is holding at either end keeps the directory where it is.
	#[test]
	fn a_dir_under_a_held_remote_path_is_not_moved() {
		let (mut baseline, mut local, mut remote) = moved_tree(TreeIds::new(), "docs", "documents");
		let held = HashSet::from(["documents/sub/b.txt".to_string()]);
		assert!(
			fold_dir_moves(
				SyncMode::TwoWay,
				&mut baseline,
				&mut local,
				&mut remote,
				&held
			)
			.is_empty()
		);
	}

	/// A remote move whose local subtree was edited meanwhile is still one move: the edit is
	/// planned at the new path, after the move.
	#[test]
	fn a_child_edited_under_a_remotely_moved_dir_is_pushed_at_its_new_path() {
		let (baseline, mut local, remote) = moved_tree(TreeIds::new(), "docs", "documents");
		local.insert("docs/a.txt".to_string(), local_file("docs/a.txt", [9; 32]));
		assert_eq!(
			plan_with_renames(SyncMode::TwoWay, &baseline, &local, &remote),
			vec![
				SyncAction::MoveLocal {
					from_path: "docs".to_string(),
					to_path: "documents".to_string(),
					kind: NodeKind::Dir,
				},
				SyncAction::UploadFile {
					rel_path: "documents/a.txt".to_string(),
				},
			]
		);
	}

	/// A move inside a moved directory — the remote moved `docs/` and renamed `sub/` in it — is the
	/// outer move first, then the inner one at the outer's new path, and nothing for the files.
	#[test]
	fn nested_dir_moves_fold_the_outer_one_first() {
		let ids = TreeIds::new();
		let (baseline, local, mut remote) = moved_tree(ids, "docs", "documents");
		let node = |path: &str| RemoteNode {
			rel_path: path.to_string(),
			..remote["documents/sub/b.txt"].clone()
		};
		let b = node("documents/renamed/b.txt");
		remote.remove("documents/sub/b.txt");
		remote.remove("documents/sub");
		remote.insert("documents/renamed/b.txt".to_string(), b);
		remote.insert(
			"documents/renamed".to_string(),
			remote_dir_node("documents/renamed", ids.sub),
		);
		assert_eq!(
			plan_with_renames(SyncMode::TwoWay, &baseline, &local, &remote),
			vec![
				SyncAction::MoveLocal {
					from_path: "docs".to_string(),
					to_path: "documents".to_string(),
					kind: NodeKind::Dir,
				},
				SyncAction::MoveLocal {
					from_path: "documents/sub".to_string(),
					to_path: "documents/renamed".to_string(),
					kind: NodeKind::Dir,
				},
			]
		);
	}

	/// A pushed move that re-parented the directory and then failed to rename it leaves the remote at
	/// the new parent under the old name. With the rows recorded there, the next pass plans the rename
	/// alone; with the rows still at the source, it finds the directory at neither end and plans no
	/// move at all — which is why apply records the half-done step.
	#[test]
	fn a_half_done_local_dir_move_is_finished_by_the_next_pass() {
		let ids = TreeIds::new();
		let archive = Uuid::new_v4();
		let tree = |baseline_at: &str| {
			let mut baseline: HashMap<_, _> = ids.baseline(baseline_at).into_iter().collect();
			baseline.insert("archive".to_string(), base_dir("archive", archive));
			let mut local: HashMap<_, _> =
				TreeIds::local("archive/documents").into_iter().collect();
			local.insert("archive".to_string(), local_dir("archive"));
			let mut remote: HashMap<_, _> = ids.remote("archive/docs").into_iter().collect();
			remote.insert("archive".to_string(), remote_dir_node("archive", archive));
			(baseline, local, remote)
		};

		let (baseline, local, remote) = tree("archive/docs");
		assert_eq!(
			plan_with_renames(SyncMode::TwoWay, &baseline, &local, &remote),
			vec![SyncAction::MoveRemote {
				from_path: "archive/docs".to_string(),
				to_path: "archive/documents".to_string(),
				kind: NodeKind::Dir,
				remote_uuid: ids.dir,
			}]
		);

		let (baseline, local, remote) = tree("docs");
		let actions = plan_with_renames(SyncMode::TwoWay, &baseline, &local, &remote);
		assert!(dir_moves_in(&actions).is_empty(), "{actions:?}");
	}

	/// A moved directory is re-parented before it is renamed, so the name it has in between must be
	/// free on the remote too.
	#[test]
	fn a_local_dir_move_whose_intermediate_name_is_taken_is_not_planned() {
		let (baseline, local, mut remote) = moved_tree(TreeIds::new(), "archive/documents", "docs");
		remote.insert(
			"archive/docs".to_string(),
			remote_dir_node("archive/docs", Uuid::new_v4()),
		);
		let actions = plan_with_renames(SyncMode::LocalToRemote, &baseline, &local, &remote);
		assert!(dir_moves_in(&actions).is_empty(), "{actions:?}");
	}

	fn ms(millis: i64) -> DateTime<Utc> {
		DateTime::from_timestamp_millis(millis).unwrap()
	}

	/// A cacheable file with a fresh uuid, named `name` under `parent`.
	fn cacheable_file(parent: Uuid, name: &'static str) -> CacheableFile<'static> {
		let uuid = Uuid::new_v4();
		CacheableFile {
			uuid,
			stable_uuid: filen_types::fs::StableUuid::new_for_test(uuid),
			parent,
			chunks_size: 1,
			chunks: 1,
			favorited: false,
			region: Cow::Borrowed("r"),
			bucket: Cow::Borrowed("b"),
			timestamp: ms(1),
			name: Cow::Borrowed(name),
			size: 5,
			mime: Cow::Borrowed("text/plain"),
			key: crate::crypto::file::FileKey::from_str_with_version(
				&"a".repeat(64),
				filen_types::auth::FileEncryptionVersion::V3,
			)
			.unwrap(),
			last_modified: ms(7),
			created: Some(ms(1)),
			hash: Some(Blake3Hash::from([5; 32])),
		}
	}

	fn local_file(rel: &str, hash: [u8; 32]) -> LocalNode {
		LocalNode {
			rel_path: rel.to_string(),
			kind: NodeKind::File,
			size: 10,
			mtime_millis: 1,
			content_hash: Some(Blake3Hash::from(hash)),
		}
	}

	fn local_dir(rel: &str) -> LocalNode {
		LocalNode {
			rel_path: rel.to_string(),
			kind: NodeKind::Dir,
			size: 0,
			mtime_millis: 1,
			content_hash: None,
		}
	}

	/// A remote file whose lineage id is its own uuid — the shape a file that has never been
	/// re-uploaded has. [`remote_version`] models a later version of the same lineage.
	fn remote_file(rel: &str, uuid: Uuid, hash: [u8; 32]) -> RemoteNode {
		RemoteNode {
			rel_path: rel.to_string(),
			kind: NodeKind::File,
			remote_uuid: uuid,
			stable_uuid: Some(StableUuid::new_for_test(uuid)),
			content_hash: Some(Blake3Hash::from(hash)),
			size: 10,
			modified_millis: 1,
		}
	}

	/// A new VERSION of the file whose lineage is `lineage`: a fresh uuid, the same whole-life id.
	fn remote_version(rel: &str, lineage: Uuid, uuid: Uuid, hash: [u8; 32]) -> RemoteNode {
		RemoteNode {
			stable_uuid: Some(StableUuid::new_for_test(lineage)),
			..remote_file(rel, uuid, hash)
		}
	}

	fn remote_dir_node(rel: &str, uuid: Uuid) -> RemoteNode {
		RemoteNode {
			rel_path: rel.to_string(),
			kind: NodeKind::Dir,
			remote_uuid: uuid,
			stable_uuid: None,
			content_hash: None,
			size: 0,
			modified_millis: 0,
		}
	}

	fn base_dir(rel: &str, uuid: Uuid) -> BaselineEntry {
		BaselineEntry {
			rel_path: rel.to_string(),
			kind: NodeKind::Dir,
			remote_uuid: Some(uuid),
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

	fn base_file(rel: &str, uuid: Uuid, hash: [u8; 32]) -> BaselineEntry {
		BaselineEntry {
			rel_path: rel.to_string(),
			kind: NodeKind::File,
			remote_uuid: Some(uuid),
			content_hash: Some(Blake3Hash::from(hash)),
			size: Some(10),
			local_mtime: Some(1),
			remote_modified: Some(1),
			state: BaselineState::Synced,
			local_kind: None,
			remote_kind: None,
			remote_hash: None,
			remote_size: None,
			remote_stable_uuid: Some(StableUuid::new_for_test(uuid)),
			// The ordinary shape: what the row records is what both sides were last known to hold.
			agreed_hash: Some(Blake3Hash::from(hash)),
		}
	}

	/// The row a push left behind: our content on top of a DIFFERENT agreed one, and no snapshot
	/// has confirmed the push since.
	fn base_file_pushed(rel: &str, uuid: Uuid, hash: [u8; 32], agreed: [u8; 32]) -> BaselineEntry {
		BaselineEntry {
			agreed_hash: Some(Blake3Hash::from(agreed)),
			..base_file(rel, uuid, hash)
		}
	}

	/// Rule (d), the case it exists for: our push is unconfirmed and a foreign version of the same
	/// file now holds the path with different bytes. Both sides moved off the agreed content, so
	/// this is a two-way conflict rather than a pull that would bury our copy.
	#[test]
	fn a_foreign_edit_over_an_unconfirmed_push_is_a_conflict() {
		let lineage = Uuid::new_v4();
		// The row: we pushed [1;32] on top of the agreed [0;32]; nothing has confirmed it.
		let baseline = map(vec![(
			"a.txt",
			base_file_pushed("a.txt", lineage, [1; 32], [0; 32]),
		)]);
		let local = map(vec![("a.txt", local_file("a.txt", [1; 32]))]);
		let remote = map(vec![(
			"a.txt",
			remote_version("a.txt", lineage, Uuid::new_v4(), [2; 32]),
		)]);
		assert_eq!(
			plan(SyncMode::TwoWay, &baseline, &local, &remote),
			vec![SyncAction::Conflict {
				rel_path: "a.txt".to_string(),
			}],
			"a concurrent edit of a file we just pushed must be surfaced, not pulled over"
		);
	}

	/// The negative: once a pass has confirmed our push (agreed == what the row records), a foreign
	/// version on top of it is an edit made AFTER ours and pulls exactly as it always did.
	#[test]
	fn a_foreign_edit_over_a_confirmed_push_still_pulls() {
		let lineage = Uuid::new_v4();
		let baseline = map(vec![("a.txt", base_file("a.txt", lineage, [1; 32]))]);
		let local = map(vec![("a.txt", local_file("a.txt", [1; 32]))]);
		let remote = map(vec![(
			"a.txt",
			remote_version("a.txt", lineage, Uuid::new_v4(), [2; 32]),
		)]);
		let new_uuid = remote["a.txt"].remote_uuid;
		assert_eq!(
			plan(SyncMode::TwoWay, &baseline, &local, &remote),
			vec![SyncAction::DownloadFile {
				rel_path: "a.txt".to_string(),
				remote_uuid: new_uuid,
			}]
		);
	}

	/// No agreed content on record (a file this side created and pushed, nothing confirmed since)
	/// is an UNCONFIRMED push, not consent: a foreign version on top of it is surfaced, not pulled
	/// over. This is what makes two clients creating the same name concurrently non-silent — both
	/// hold exactly this row, so with `None` read as agreement the loser's copy would be buried.
	#[test]
	fn a_foreign_edit_with_no_agreed_content_recorded_is_a_conflict() {
		let lineage = Uuid::new_v4();
		let baseline = map(vec![(
			"a.txt",
			BaselineEntry {
				agreed_hash: None,
				..base_file("a.txt", lineage, [1; 32])
			},
		)]);
		let local = map(vec![("a.txt", local_file("a.txt", [1; 32]))]);
		let remote = map(vec![(
			"a.txt",
			remote_version("a.txt", lineage, Uuid::new_v4(), [2; 32]),
		)]);
		assert_eq!(
			plan(SyncMode::TwoWay, &baseline, &local, &remote),
			vec![SyncAction::Conflict {
				rel_path: "a.txt".to_string(),
			}]
		);
	}

	/// A DIFFERENT file taking the path over is a replacement, not a concurrent edit of ours: the
	/// destination has to converge on whatever holds the path, so it pulls.
	#[test]
	fn a_replacement_over_an_unconfirmed_push_is_not_a_concurrent_edit() {
		let lineage = Uuid::new_v4();
		let baseline = map(vec![(
			"a.txt",
			base_file_pushed("a.txt", lineage, [1; 32], [0; 32]),
		)]);
		let local = map(vec![("a.txt", local_file("a.txt", [1; 32]))]);
		let remote = map(vec![(
			"a.txt",
			remote_file("a.txt", Uuid::new_v4(), [2; 32]),
		)]);
		let other_uuid = remote["a.txt"].remote_uuid;
		assert_eq!(
			plan(SyncMode::TwoWay, &baseline, &local, &remote),
			vec![SyncAction::DownloadFile {
				rel_path: "a.txt".to_string(),
				remote_uuid: other_uuid,
			}]
		);
	}

	/// A foreign version carrying the SAME bytes we pushed is nobody diverging — the other client
	/// wrote what we already hold. Nothing happens at all.
	#[test]
	fn a_content_equal_foreign_version_over_an_unconfirmed_push_is_no_conflict() {
		let lineage = Uuid::new_v4();
		let baseline = map(vec![(
			"a.txt",
			base_file_pushed("a.txt", lineage, [1; 32], [0; 32]),
		)]);
		let local = map(vec![("a.txt", local_file("a.txt", [1; 32]))]);
		let remote = map(vec![(
			"a.txt",
			remote_version("a.txt", lineage, Uuid::new_v4(), [1; 32]),
		)]);
		assert!(
			plan(SyncMode::TwoWay, &baseline, &local, &remote).is_empty(),
			"identical bytes under a new version id are not a divergence"
		);
	}

	/// The one-way modes are untouched: each has an authoritative side and no divergence to
	/// surface, so the same inputs re-push (local-authoritative) or pull (remote-authoritative).
	#[test]
	fn one_way_modes_ignore_the_agreed_content_marker() {
		let lineage = Uuid::new_v4();
		let baseline = map(vec![(
			"a.txt",
			base_file_pushed("a.txt", lineage, [1; 32], [0; 32]),
		)]);
		let local = map(vec![("a.txt", local_file("a.txt", [1; 32]))]);
		let remote = map(vec![(
			"a.txt",
			remote_version("a.txt", lineage, Uuid::new_v4(), [2; 32]),
		)]);
		let new_uuid = remote["a.txt"].remote_uuid;
		for mode in [SyncMode::LocalToRemote, SyncMode::LocalBackup] {
			assert_eq!(
				plan(mode, &baseline, &local, &remote),
				vec![SyncAction::UploadFile {
					rel_path: "a.txt".to_string(),
				}],
				"{mode:?} must re-push the local copy"
			);
		}
		for mode in [SyncMode::RemoteToLocal, SyncMode::RemoteBackup] {
			assert_eq!(
				plan(mode, &baseline, &local, &remote),
				vec![SyncAction::DownloadFile {
					rel_path: "a.txt".to_string(),
					remote_uuid: new_uuid,
				}],
				"{mode:?} must pull the remote copy"
			);
		}
	}

	/// Rule (c): the snapshot listing our own version at the path is what makes the row's content
	/// agreed. Only the rows that actually moved come back, so only those are persisted.
	#[test]
	fn the_raw_snapshot_showing_our_version_advances_the_agreed_content() {
		let uuid = Uuid::new_v4();
		let mut baseline = map(vec![(
			"a.txt",
			base_file_pushed("a.txt", uuid, [1; 32], [0; 32]),
		)]);
		let remote = map(vec![("a.txt", remote_file("a.txt", uuid, [1; 32]))]);

		let advanced = confirm_agreed_content(&mut baseline, &remote);
		assert_eq!(
			advanced
				.iter()
				.map(|e| e.rel_path.as_str())
				.collect::<Vec<_>>(),
			vec!["a.txt"],
			"the confirmed row is handed back to be persisted"
		);
		assert_eq!(
			baseline["a.txt"].agreed_hash,
			Some(Blake3Hash::from([1; 32])),
			"the marker moved onto what the row records"
		);
		// Idempotent: a second pass over an already-agreed row has nothing to persist.
		assert!(confirm_agreed_content(&mut baseline, &remote).is_empty());
	}

	/// A foreign version at the path proves nothing about our push, and neither does an empty
	/// snapshot: the marker stays where it was.
	#[test]
	fn a_path_the_snapshot_does_not_confirm_leaves_the_agreed_content_alone() {
		let ours = Uuid::new_v4();
		let row = base_file_pushed("a.txt", ours, [1; 32], [0; 32]);
		for remote in [
			map(vec![(
				"a.txt",
				remote_version("a.txt", ours, Uuid::new_v4(), [2; 32]),
			)]),
			HashMap::new(),
		] {
			let mut baseline = map(vec![("a.txt", row.clone())]);
			assert!(confirm_agreed_content(&mut baseline, &remote).is_empty());
			assert_eq!(
				baseline["a.txt"].agreed_hash,
				Some(Blake3Hash::from([0; 32])),
				"an unconfirmed push must not advance its own marker"
			);
		}
	}

	/// A conflicted row is a holding cell for a divergence, not a converged state — confirming it
	/// would claim both sides agree on the loser's content.
	#[test]
	fn a_conflicted_row_is_never_confirmed() {
		let uuid = Uuid::new_v4();
		let mut baseline = map(vec![(
			"a.txt",
			BaselineEntry {
				state: BaselineState::Conflicted,
				..base_file_pushed("a.txt", uuid, [1; 32], [0; 32])
			},
		)]);
		let remote = map(vec![("a.txt", remote_file("a.txt", uuid, [1; 32]))]);
		assert!(confirm_agreed_content(&mut baseline, &remote).is_empty());
	}

	/// The tenure rule's other half: evidence from OUTSIDE the snapshot (the cache's announcement
	/// timestamps, or the server's version chain) names the version uuids it vouches for, and the
	/// same row rule applies to them.
	#[test]
	fn a_push_some_other_evidence_vouches_for_advances_the_agreed_content() {
		let uuid = Uuid::new_v4();
		let mut baseline = map(vec![
			("a.txt", base_file_pushed("a.txt", uuid, [1; 32], [0; 32])),
			(
				"b.txt",
				base_file_pushed("b.txt", Uuid::new_v4(), [2; 32], [0; 32]),
			),
		]);
		let advanced = confirm_agreed_pushes(&mut baseline, &HashSet::from([uuid]));

		assert_eq!(
			advanced
				.iter()
				.map(|e| e.rel_path.as_str())
				.collect::<Vec<_>>(),
			vec!["a.txt"],
			"only the vouched-for version's row moves"
		);
		assert_eq!(
			baseline["a.txt"].agreed_hash,
			baseline["a.txt"].content_hash
		);
		assert_eq!(
			baseline["b.txt"].agreed_hash,
			Some(Blake3Hash::from([0; 32])),
			"a row nothing vouched for is left alone"
		);
	}

	#[test]
	fn a_row_that_is_not_an_unconfirmed_push_is_never_advanced_by_tenure() {
		let uuid = Uuid::new_v4();
		for row in [
			// Already agreed.
			base_file_pushed("a.txt", uuid, [1; 32], [1; 32]),
			// Held in conflict.
			BaselineEntry {
				state: BaselineState::Conflicted,
				..base_file_pushed("a.txt", uuid, [1; 32], [0; 32])
			},
			// A directory has no content for the two sides to agree on.
			base_dir("a.txt", uuid),
		] {
			let agreed = row.agreed_hash;
			let mut baseline = map(vec![("a.txt", row)]);
			assert!(confirm_agreed_pushes(&mut baseline, &HashSet::from([uuid])).is_empty());
			assert_eq!(baseline["a.txt"].agreed_hash, agreed);
		}
	}

	/// The server-side evidence: how long our version stood before the one that superseded it.
	#[test]
	fn the_version_chain_dates_a_push_by_what_landed_on_top_of_it() {
		let tenure = std::time::Duration::from_secs(30);
		let (ours, foreign, older) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());

		// Superseded well after the window: an edit made against our version, so pulling it is right.
		let chain = vec![(foreign, ms(100_000)), (ours, ms(60_000)), (older, ms(0))];
		assert_eq!(
			version_chain_verdict(&chain, ours, foreign, tenure),
			Some(true)
		);

		// Superseded INSIDE the window: the other client was editing at the same time.
		let chain = vec![(foreign, ms(65_000)), (ours, ms(60_000)), (older, ms(0))];
		assert_eq!(
			version_chain_verdict(&chain, ours, foreign, tenure),
			Some(false)
		);

		// A second foreign edit later on does not undo the first. What buried our version is
		// whatever landed on it FIRST, so the row stays unconfirmed even though the version the
		// snapshot now shows is minutes newer.
		let latest = Uuid::new_v4();
		let chain = vec![
			(latest, ms(100_000)),
			(foreign, ms(65_000)),
			(ours, ms(60_000)),
			(older, ms(0)),
		];
		assert_eq!(
			version_chain_verdict(&chain, ours, latest, tenure),
			Some(false),
			"the edit that buried ours landed inside the window; a later one on top of THAT says \
			 nothing about ours"
		);

		// The same stamp on both — a race the server could not separate — is no tenure at all,
		// whichever way the listing happens to order them.
		let chain = vec![(ours, ms(60_000)), (foreign, ms(60_000))];
		assert_eq!(
			version_chain_verdict(&chain, ours, foreign, tenure),
			Some(false)
		);

		// A chain missing either version (versioning disabled) is no evidence at all.
		assert_eq!(
			version_chain_verdict(&[(foreign, ms(65_000))], ours, foreign, tenure),
			None
		);
	}

	/// The pusher's side of a concurrent edit: what our own upload landed on top of.
	#[test]
	fn an_upload_that_landed_on_a_version_this_pass_never_saw_reports_it() {
		let (ours, replaced, stranger) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());

		// The ordinary push: ours is the only version as recent as the one the snapshot showed.
		assert_eq!(
			interleaved_version(
				&[(ours, ms(60_000)), (replaced, ms(30_000))],
				ours,
				replaced
			),
			None
		);

		// Somebody else's edit landed on it too — our bytes are on top of it and nothing else this
		// pass can see says so.
		let raced = [
			(ours, ms(60_000)),
			(stranger, ms(45_000)),
			(replaced, ms(30_000)),
		];
		assert_eq!(
			interleaved_version(&raced, ours, replaced),
			Some(stranger),
			"the version that landed on the one this pass saw is the one we buried"
		);

		// The same, with the racing pair stamped identically so the listing orders them the other
		// way round: the answer comes from the stamps, so it does not move.
		let tied = [
			(stranger, ms(60_000)),
			(ours, ms(60_000)),
			(replaced, ms(30_000)),
		];
		assert_eq!(interleaved_version(&tied, ours, replaced), Some(stranger));

		// A version stamped in the very second of the one we replaced is NOT reported: inside one
		// second the chain cannot say which came first, and our own previous upload lands there
		// often enough that guessing would hold a conflict on a path nobody else touched. The cost
		// is a race that tight going unreported on this side — its author still surfaces it.
		let tied_with_replaced = [
			(ours, ms(60_000)),
			(stranger, ms(45_000)),
			(replaced, ms(45_000)),
		];
		assert_eq!(
			interleaved_version(&tied_with_replaced, ours, replaced),
			None
		);

		// Versions older than the one the snapshot showed are the file's history, not an interleave.
		let older = [
			(ours, ms(60_000)),
			(replaced, ms(45_000)),
			(stranger, ms(10_000)),
		];
		assert_eq!(interleaved_version(&older, ours, replaced), None);

		// A chain that does not carry the version we replaced is no evidence about it.
		assert_eq!(
			interleaved_version(&[(ours, ms(60_000))], ours, replaced),
			None
		);
		assert_eq!(
			interleaved_version(
				&[(stranger, ms(45_000)), (replaced, ms(30_000))],
				ours,
				replaced
			),
			Some(stranger),
			"a version on top of the one we replaced counts even when ours is no longer listed"
		);
	}

	#[test]
	fn a_local_rename_becomes_a_remote_move_not_a_re_upload() {
		let uuid = Uuid::new_v4();
		// Baseline + remote still have the file at a.txt; locally it now lives at b.txt (same hash).
		let baseline = map(vec![("a.txt", base_file("a.txt", uuid, [5; 32]))]);
		let remote = map(vec![("a.txt", remote_file("a.txt", uuid, [5; 32]))]);
		let local = map(vec![("b.txt", local_file("b.txt", [5; 32]))]);
		assert_eq!(
			plan(SyncMode::LocalToRemote, &baseline, &local, &remote),
			vec![SyncAction::MoveRemote {
				from_path: "a.txt".to_string(),
				to_path: "b.txt".to_string(),
				kind: NodeKind::File,
				remote_uuid: uuid,
			}],
			"a local move re-parents the remote item, not trash + re-upload"
		);
	}

	#[test]
	fn a_remote_rename_becomes_a_local_move_not_a_re_download() {
		let uuid = Uuid::new_v4();
		// Baseline + local have it at a.txt; the remote now carries the SAME uuid at b.txt.
		let baseline = map(vec![("a.txt", base_file("a.txt", uuid, [5; 32]))]);
		let local = map(vec![("a.txt", local_file("a.txt", [5; 32]))]);
		let remote = map(vec![("b.txt", remote_file("b.txt", uuid, [5; 32]))]);
		assert_eq!(
			plan(SyncMode::RemoteToLocal, &baseline, &local, &remote),
			vec![SyncAction::MoveLocal {
				from_path: "a.txt".to_string(),
				to_path: "b.txt".to_string(),
				kind: NodeKind::File,
			}],
			"a remote move renames the local file, not delete + re-download"
		);
	}

	#[test]
	fn a_re_upload_of_identical_bytes_is_not_read_as_a_remote_edit() {
		// Another client (or a version restore) put the SAME content back under a new version uuid,
		// while the local side genuinely edited the file. Reading the re-mint as a remote edit would
		// invent a conflict with no second side; only the local edit actually happened, so it pushes.
		let lineage = Uuid::new_v4();
		let baseline = map(vec![("a.txt", base_file("a.txt", lineage, [0; 32]))]);
		let local = map(vec![("a.txt", local_file("a.txt", [1; 32]))]);
		let remote = map(vec![(
			"a.txt",
			remote_version("a.txt", lineage, Uuid::new_v4(), [0; 32]),
		)]);
		assert_eq!(
			plan(SyncMode::TwoWay, &baseline, &local, &remote),
			vec![SyncAction::UploadFile {
				rel_path: "a.txt".to_string(),
			}],
			"a re-mint that changed no bytes is not a remote change"
		);
	}

	#[test]
	fn a_new_version_of_the_same_file_is_still_an_edit_and_a_different_file_is_still_a_change() {
		let lineage = Uuid::new_v4();
		let baseline = map(vec![("a.txt", base_file("a.txt", lineage, [0; 32]))]);
		let local = map(vec![("a.txt", local_file("a.txt", [0; 32]))]);

		// Same lineage, new version, DIFFERENT bytes: an edit — pulled.
		let edited = map(vec![(
			"a.txt",
			remote_version("a.txt", lineage, Uuid::new_v4(), [2; 32]),
		)]);
		let new_uuid = edited["a.txt"].remote_uuid;
		assert_eq!(
			plan(SyncMode::TwoWay, &baseline, &local, &edited),
			vec![SyncAction::DownloadFile {
				rel_path: "a.txt".to_string(),
				remote_uuid: new_uuid,
			}]
		);

		// A DIFFERENT lineage at the same path: a replacement — converged on the same way.
		let replaced = map(vec![(
			"a.txt",
			remote_file("a.txt", Uuid::new_v4(), [2; 32]),
		)]);
		let other_uuid = replaced["a.txt"].remote_uuid;
		assert_eq!(
			plan(SyncMode::TwoWay, &baseline, &local, &replaced),
			vec![SyncAction::DownloadFile {
				rel_path: "a.txt".to_string(),
				remote_uuid: other_uuid,
			}]
		);
	}

	#[test]
	fn a_remote_move_that_carried_an_edit_stays_one_item_and_pulls_the_new_version() {
		// The other side renamed a.txt -> b.txt AND edited it before this pass ran, so the uuid the
		// baseline recorded names nothing live. Matched on the lineage id, this is one file that
		// moved and changed — a local rename plus a pull — not a local deletion and an unrelated
		// download (which the mass-delete guard would hold, and a backup mode would refuse).
		let lineage = Uuid::new_v4();
		let new_uuid = Uuid::new_v4();
		let baseline = map(vec![("a.txt", base_file("a.txt", lineage, [5; 32]))]);
		let local = map(vec![("a.txt", local_file("a.txt", [5; 32]))]);
		let remote = map(vec![(
			"b.txt",
			remote_version("b.txt", lineage, new_uuid, [6; 32]),
		)]);
		assert_eq!(
			plan(SyncMode::TwoWay, &baseline, &local, &remote),
			vec![
				SyncAction::MoveLocal {
					from_path: "a.txt".to_string(),
					to_path: "b.txt".to_string(),
					kind: NodeKind::File,
				},
				SyncAction::DownloadFile {
					rel_path: "b.txt".to_string(),
					remote_uuid: new_uuid,
				},
			],
			"the move and the edit it carried are applied together"
		);
	}

	/// The guard is about a local CHANGE, not about a row that happens to record no local content.
	/// A `KeepLocal` resolution anchors the row to the remote and clears the local half on purpose,
	/// so the row sits at `content_hash: None` until the re-push. Comparing a hash against `None`
	/// reports "changed" for every such row, and a remote rename landing in that window would then
	/// be refused as a move: a second conflict at the source plus a download at the new name,
	/// instead of the rename the remote actually made.
	#[test]
	fn a_remote_rename_before_a_kept_local_re_push_is_still_a_move() {
		let uuid = Uuid::new_v4();
		// The row a KeepLocal resolution leaves: the remote's anchor, no local evidence at all.
		let kept = BaselineEntry {
			content_hash: None,
			size: None,
			local_mtime: None,
			agreed_hash: None,
			..base_file("a.txt", uuid, [5; 32])
		};
		let baseline = map(vec![("a.txt", kept)]);
		// The local copy still holds the bytes the caller kept; the remote renamed its own (still
		// diverged) copy a.txt -> b.txt, same uuid.
		let local = map(vec![("a.txt", local_file("a.txt", [1; 32]))]);
		let remote = map(vec![("b.txt", remote_file("b.txt", uuid, [2; 32]))]);

		assert_eq!(
			plan(SyncMode::TwoWay, &baseline, &local, &remote),
			vec![SyncAction::MoveLocal {
				from_path: "a.txt".to_string(),
				to_path: "b.txt".to_string(),
				kind: NodeKind::File,
			}],
			"the rename must be carried across, with nothing else planned at either path"
		);

		// The pending push follows on the NEXT pass, at the new name. Planned against the row the
		// apply step actually writes for this move — not a hand-made one: the row's local half is
		// what decides whether the scanner even looks at the file again, so a test that invented it
		// could pass over an engine that never pushes the kept copy at all.
		let moved = map(vec![(
			"b.txt",
			super::super::apply::moved_file_row(
				"b.txt",
				baseline.get("a.txt"),
				remote.get("b.txt"),
				Some(1),
			),
		)]);
		assert_eq!(
			moved["b.txt"].content_hash, None,
			"the moved row records no local content, so the scanner must re-hash at the new path"
		);
		let local_moved = map(vec![("b.txt", local_file("b.txt", [1; 32]))]);
		assert_eq!(
			plan(SyncMode::TwoWay, &moved, &local_moved, &remote),
			vec![SyncAction::UploadFile {
				rel_path: "b.txt".to_string(),
			}],
			"the kept local copy must be pushed at its new name"
		);
	}

	/// The other half of that guard. "No local content on record" excuses the row from being read as
	/// a local EDIT, but it is not agreement either: the copy a `KeepLocal` resolution kept is
	/// diverged from what the remote holds, by construction. So a remote change that renamed AND
	/// edited the file is not carried across it — the rename would put the kept bytes under the
	/// paired download, which is the loss the unconfirmed-push rule exists to stop, one rename away.
	/// Same outcome as that rule's: the source surfaces, and the moved version arrives untracked
	/// under its new name.
	#[test]
	fn a_remote_rename_that_carried_an_edit_is_not_moved_onto_a_kept_local_copy() {
		let lineage = Uuid::new_v4();
		let new_uuid = Uuid::new_v4();
		let kept = BaselineEntry {
			content_hash: None,
			size: None,
			local_mtime: None,
			agreed_hash: None,
			..base_file("a.txt", lineage, [5; 32])
		};
		let baseline = map(vec![("a.txt", kept)]);
		let local = map(vec![("a.txt", local_file("a.txt", [1; 32]))]);
		let remote = map(vec![(
			"b.txt",
			remote_version("b.txt", lineage, new_uuid, [2; 32]),
		)]);

		let actions = plan(SyncMode::TwoWay, &baseline, &local, &remote);
		assert!(
			!actions
				.iter()
				.any(|a| matches!(a, SyncAction::MoveLocal { .. })),
			"the kept copy must not be renamed under the download: {actions:?}"
		);
		assert!(
			actions.contains(&SyncAction::Conflict {
				rel_path: "a.txt".to_string(),
			}),
			"the divergence must be surfaced: {actions:?}"
		);
		assert!(
			actions.iter().any(|a| matches!(
				a,
				SyncAction::DownloadFile { rel_path, .. } if rel_path == "b.txt"
			)),
			"the moved version still arrives under its new name: {actions:?}"
		);
	}

	/// The same guard on the UUID-matched branch: a remote move whose content did NOT change is
	/// still not a move when the local copy at its source was edited. Consuming it would rename the
	/// edited file to the destination and pull the remote's bytes over it — and a same-size edit
	/// satisfies the scanner's (size, mtime) fast path afterwards, so the loss is never noticed.
	#[test]
	fn a_local_edit_at_a_uuid_matched_remote_moves_source_is_not_a_move() {
		let uuid = Uuid::new_v4();
		// The remote renamed a.txt -> b.txt (same uuid, same bytes); a.txt was edited locally.
		let baseline = map(vec![("a.txt", base_file("a.txt", uuid, [5; 32]))]);
		let local = map(vec![("a.txt", local_file("a.txt", [9; 32]))]);
		let remote = map(vec![("b.txt", remote_file("b.txt", uuid, [5; 32]))]);

		// TwoWay: both sides changed the same item, so the source is held for the caller and the
		// moved version still arrives under its new name.
		let actions = plan(SyncMode::TwoWay, &baseline, &local, &remote);
		assert!(
			!actions
				.iter()
				.any(|a| matches!(a, SyncAction::MoveLocal { .. })),
			"an edited local copy must not be renamed out from under the edit: {actions:?}"
		);
		assert!(
			actions.contains(&SyncAction::Conflict {
				rel_path: "a.txt".to_string(),
			}),
			"the divergence must be surfaced: {actions:?}"
		);
		assert!(
			actions.iter().any(|a| matches!(
				a,
				SyncAction::DownloadFile { rel_path, .. } if rel_path == "b.txt"
			)),
			"the moved version still arrives at its new name: {actions:?}"
		);

		// RemoteToLocal: the remote is authoritative, so the local edit is quarantined by the
		// deletion of the source rather than carried to the destination and overwritten there.
		assert_eq!(
			plan(SyncMode::RemoteToLocal, &baseline, &local, &remote),
			vec![
				SyncAction::DownloadFile {
					rel_path: "b.txt".to_string(),
					remote_uuid: uuid,
				},
				SyncAction::DeleteLocal {
					rel_path: "a.txt".to_string(),
					kind: NodeKind::File,
				},
			]
		);

		// RemoteBackup never deletes on its destination, so the edited file simply stays put.
		assert_eq!(
			plan(SyncMode::RemoteBackup, &baseline, &local, &remote),
			vec![SyncAction::DownloadFile {
				rel_path: "b.txt".to_string(),
				remote_uuid: uuid,
			}]
		);
	}

	#[test]
	fn a_local_edit_stops_a_lineage_move_from_laundering_the_divergence() {
		// Same remote move+edit, but the local copy diverged too. Consuming that as a move would
		// rename the local file and pull straight over it — a both-sides conflict silently resolved
		// in the remote's favour. It must fall through to the ordinary reconcile instead.
		let lineage = Uuid::new_v4();
		let baseline = map(vec![("a.txt", base_file("a.txt", lineage, [5; 32]))]);
		let local = map(vec![("a.txt", local_file("a.txt", [9; 32]))]);
		let remote = map(vec![(
			"b.txt",
			remote_version("b.txt", lineage, Uuid::new_v4(), [6; 32]),
		)]);
		let actions = plan(SyncMode::TwoWay, &baseline, &local, &remote);
		assert!(
			!actions
				.iter()
				.any(|a| matches!(a, SyncAction::MoveLocal { .. })),
			"a diverged local copy must not be consumed as a move: {actions:?}"
		);
	}

	/// The move branch reads the same rule as the per-path reconcile. A foreign version of our own
	/// lineage that moved AND edited over a push nothing confirmed is a divergence, and consuming
	/// it as a move would rename our unconfirmed copy to the destination for the paired download to
	/// write over — the same silent loss the rule exists to stop, just one rename away. Leaving the
	/// pair alone is not enough either: the source would then read as a remote-side deletion and be
	/// deleted locally. The source is surfaced, and the moved version still arrives at its new name.
	#[test]
	fn a_lineage_move_over_an_unconfirmed_push_surfaces_instead_of_moving() {
		let lineage = Uuid::new_v4();
		let new_uuid = Uuid::new_v4();
		// We pushed [1;32] over the agreed [0;32]; nothing confirmed it. The other client edited
		// the same file to [2;32] and renamed it in the same window.
		let baseline = map(vec![(
			"a.txt",
			base_file_pushed("a.txt", lineage, [1; 32], [0; 32]),
		)]);
		let local = map(vec![("a.txt", local_file("a.txt", [1; 32]))]);
		let remote = map(vec![(
			"b.txt",
			remote_version("b.txt", lineage, new_uuid, [2; 32]),
		)]);

		let actions = plan(SyncMode::TwoWay, &baseline, &local, &remote);
		assert!(
			!actions
				.iter()
				.any(|a| matches!(a, SyncAction::MoveLocal { .. })),
			"our unconfirmed copy must not be renamed under the download: {actions:?}"
		);
		assert!(
			!actions.iter().any(|a| matches!(
				a,
				SyncAction::DeleteLocal { rel_path, .. } if rel_path == "a.txt"
			)),
			"nor deleted as though the remote had dropped it: {actions:?}"
		);
		assert!(
			actions.contains(&SyncAction::Conflict {
				rel_path: "a.txt".to_string(),
			}),
			"the divergence must be surfaced: {actions:?}"
		);
		assert!(
			actions.iter().any(|a| matches!(
				a,
				SyncAction::DownloadFile { rel_path, .. } if rel_path == "b.txt"
			)),
			"the moved version still arrives under its new name: {actions:?}"
		);
	}

	/// The negative, so the guard cannot swallow the ordinary move+edit: with the push confirmed
	/// (or nothing pushed at all) the pair is still carried across as one item.
	#[test]
	fn a_lineage_move_over_a_confirmed_push_is_still_a_move() {
		let lineage = Uuid::new_v4();
		let new_uuid = Uuid::new_v4();
		let baseline = map(vec![("a.txt", base_file("a.txt", lineage, [1; 32]))]);
		let local = map(vec![("a.txt", local_file("a.txt", [1; 32]))]);
		let remote = map(vec![(
			"b.txt",
			remote_version("b.txt", lineage, new_uuid, [2; 32]),
		)]);
		assert_eq!(
			plan(SyncMode::TwoWay, &baseline, &local, &remote),
			vec![
				SyncAction::MoveLocal {
					from_path: "a.txt".to_string(),
					to_path: "b.txt".to_string(),
					kind: NodeKind::File,
				},
				SyncAction::DownloadFile {
					rel_path: "b.txt".to_string(),
					remote_uuid: new_uuid,
				},
			]
		);
	}

	#[test]
	fn a_rename_in_a_backup_mode_moves_the_destination_instead_of_copying_it() {
		let uuid = Uuid::new_v4();
		// LocalBackup: the source (local) renamed a.txt -> b.txt. The remote backup follows with a
		// metadata-only re-parent/rename, not an upload of b.txt with a.txt left behind.
		let baseline = map(vec![("a.txt", base_file("a.txt", uuid, [5; 32]))]);
		let remote = map(vec![("a.txt", remote_file("a.txt", uuid, [5; 32]))]);
		let local = map(vec![("b.txt", local_file("b.txt", [5; 32]))]);
		assert_eq!(
			plan(SyncMode::LocalBackup, &baseline, &local, &remote),
			vec![SyncAction::MoveRemote {
				from_path: "a.txt".to_string(),
				to_path: "b.txt".to_string(),
				kind: NodeKind::File,
				remote_uuid: uuid,
			}],
			"a backup destination follows a source rename instead of accumulating both names"
		);

		// RemoteBackup: the mirror image — the source (remote) renamed, the local backup follows.
		let baseline = map(vec![("a.txt", base_file("a.txt", uuid, [5; 32]))]);
		let local = map(vec![("a.txt", local_file("a.txt", [5; 32]))]);
		let remote = map(vec![("b.txt", remote_file("b.txt", uuid, [5; 32]))]);
		assert_eq!(
			plan(SyncMode::RemoteBackup, &baseline, &local, &remote),
			vec![SyncAction::MoveLocal {
				from_path: "a.txt".to_string(),
				to_path: "b.txt".to_string(),
				kind: NodeKind::File,
			}]
		);
	}

	#[test]
	fn a_backup_mode_still_never_propagates_a_real_deletion() {
		// The distinction move detection must preserve: a rename is a move, but a deletion with no
		// new home for the content stays suppressed on a backup destination.
		let uuid = Uuid::new_v4();
		let baseline = map(vec![("a.txt", base_file("a.txt", uuid, [5; 32]))]);
		let remote = map(vec![("a.txt", remote_file("a.txt", uuid, [5; 32]))]);
		assert!(
			plan(SyncMode::LocalBackup, &baseline, &HashMap::new(), &remote).is_empty(),
			"a local deletion must not reach a remote backup"
		);
		let local = map(vec![("a.txt", local_file("a.txt", [5; 32]))]);
		assert!(
			plan(SyncMode::RemoteBackup, &baseline, &local, &HashMap::new()).is_empty(),
			"a remote deletion must not reach a local backup"
		);
	}

	#[test]
	fn ambiguous_content_falls_back_to_delete_plus_create() {
		let uuid = Uuid::new_v4();
		// a.txt vanished locally, but TWO new local files share its content — no unambiguous move.
		let baseline = map(vec![("a.txt", base_file("a.txt", uuid, [5; 32]))]);
		let remote = map(vec![("a.txt", remote_file("a.txt", uuid, [5; 32]))]);
		let local = map(vec![
			("b.txt", local_file("b.txt", [5; 32])),
			("c.txt", local_file("c.txt", [5; 32])),
		]);
		let actions = plan(SyncMode::LocalToRemote, &baseline, &local, &remote);
		assert!(
			!actions
				.iter()
				.any(|a| matches!(a, SyncAction::MoveRemote { .. })),
			"ambiguous content must NOT be guessed as a move: {actions:?}"
		);
		assert!(
			actions
				.iter()
				.any(|a| matches!(a, SyncAction::TrashRemote { .. })),
			"a.txt is instead trashed + the new files uploaded"
		);
	}

	fn map<T>(items: Vec<(&str, T)>) -> HashMap<String, T> {
		items.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
	}

	// ------------------------------------------------------------------------
	// Backlog::AdoptDestination — the mode-switch re-seed and what it plans
	// ------------------------------------------------------------------------

	/// The re-seed picks exactly the paths whose SOURCE side is gone under the NEW mode — tracked or
	/// not — and anchors each row to what the destination holds now.
	#[test]
	fn the_reseed_adopts_only_the_paths_the_source_no_longer_has() {
		let gone = Uuid::new_v4();
		let both = Uuid::new_v4();
		let baseline = map(vec![
			("gone.txt", base_file("gone.txt", gone, [1; 32])),
			("both.txt", base_file("both.txt", both, [2; 32])),
		]);
		// gone.txt survives only on the remote; both.txt is still on both sides. untracked.txt is a
		// remote item the pair never synced.
		let local = map(vec![("both.txt", local_file("both.txt", [2; 32]))]);
		let remote = map(vec![
			("gone.txt", remote_file("gone.txt", gone, [1; 32])),
			("both.txt", remote_file("both.txt", both, [2; 32])),
			(
				"untracked.txt",
				remote_file("untracked.txt", Uuid::new_v4(), [3; 32]),
			),
		]);

		let rows = adopt_destination_rows(SyncMode::LocalToRemote, &baseline, &local, &remote);
		let mut adopted: Vec<&str> = rows.iter().map(|r| r.rel_path.as_str()).collect();
		adopted.sort_unstable();
		assert_eq!(
			adopted,
			vec!["gone.txt", "untracked.txt"],
			"the standing source deletion AND the destination-only item the pair never tracked; \
			 both.txt is on both sides, so it is neither"
		);
		let row = rows.iter().find(|r| r.rel_path == "gone.txt").unwrap();
		assert_eq!(row.state, BaselineState::Adopted);
		assert_eq!(row.remote_uuid, Some(gone), "anchored to the remote copy");
		assert_eq!(row.content_hash, Some(Blake3Hash::from([1; 32])));
		assert_eq!(row.agreed_hash, None, "one side only: nothing is agreed");

		// The mirror image: with the REMOTE as the source, the local-only copy is the one adopted.
		let rows = adopt_destination_rows(
			SyncMode::RemoteToLocal,
			&baseline,
			&remote_as_local(),
			&map(vec![]),
		);
		assert!(
			rows.iter().all(|r| r.remote_uuid.is_none()),
			"a local-side adoption records no remote anchor: {rows:?}"
		);
	}

	/// A file another client created straight on the destination has no baseline row, so before the
	/// switch adopted it a one-way mirror read it as an item to remove and trashed it on the very
	/// next pass. The adoption is what makes the destination's contents what the caller asked to
	/// keep, rather than only the pair's own standing backlog.
	#[test]
	fn the_reseed_adopts_a_destination_only_item_the_pair_never_tracked() {
		let theirs = Uuid::new_v4();
		let remote = map(vec![(
			"theirs.txt",
			remote_file("theirs.txt", theirs, [7; 32]),
		)]);
		let (baseline, local) = (map(vec![]), map(vec![]));

		// Without a row the mirror removes it.
		assert_eq!(
			plan(SyncMode::LocalToRemote, &baseline, &local, &remote),
			vec![SyncAction::TrashRemote {
				rel_path: "theirs.txt".to_string(),
				kind: NodeKind::File,
				remote_uuid: theirs,
			}]
		);

		let rows = adopt_destination_rows(SyncMode::LocalToRemote, &baseline, &local, &remote);
		assert_eq!(
			rows.iter().map(|r| r.rel_path.as_str()).collect::<Vec<_>>(),
			vec!["theirs.txt"]
		);
		assert_eq!(rows[0].state, BaselineState::Adopted);
		assert_eq!(rows[0].remote_uuid, Some(theirs));

		let adopted = map(vec![("theirs.txt", rows.into_iter().next().unwrap())]);
		assert!(
			plan(SyncMode::LocalToRemote, &adopted, &local, &remote).is_empty(),
			"the adopted copy must survive the switch"
		);

		// Two-way needs no row: an untracked destination-only item already flows back on its own,
		// and a row here would only take the path out of move detection.
		assert!(adopt_destination_rows(SyncMode::TwoWay, &baseline, &local, &remote).is_empty());
		assert_eq!(
			plan(SyncMode::TwoWay, &baseline, &local, &remote),
			vec![SyncAction::DownloadFile {
				rel_path: "theirs.txt".to_string(),
				remote_uuid: theirs,
			}]
		);
	}

	/// The mirror image, with the LOCAL tree as the destination: a file someone dropped into the
	/// local root is adopted rather than deleted by the first pull-only pass after the switch.
	#[test]
	fn the_reseed_adopts_a_local_only_item_when_the_local_side_is_the_destination() {
		let local = map(vec![("dropped.txt", local_file("dropped.txt", [7; 32]))]);
		let (baseline, remote) = (map(vec![]), map(vec![]));

		assert_eq!(
			plan(SyncMode::RemoteToLocal, &baseline, &local, &remote),
			vec![SyncAction::DeleteLocal {
				rel_path: "dropped.txt".to_string(),
				kind: NodeKind::File,
			}]
		);

		let rows = adopt_destination_rows(SyncMode::RemoteToLocal, &baseline, &local, &remote);
		assert_eq!(
			rows.iter().map(|r| r.rel_path.as_str()).collect::<Vec<_>>(),
			vec!["dropped.txt"]
		);
		assert!(rows[0].remote_uuid.is_none(), "anchored to the local copy");

		let adopted = map(vec![("dropped.txt", rows.into_iter().next().unwrap())]);
		assert!(plan(SyncMode::RemoteToLocal, &adopted, &local, &remote).is_empty());
	}

	/// A local tree standing in for "the local side kept both files, the remote lost them".
	fn remote_as_local() -> HashMap<String, LocalNode> {
		map(vec![
			("gone.txt", local_file("gone.txt", [1; 32])),
			("both.txt", local_file("both.txt", [2; 32])),
		])
	}

	/// What the adoption buys: a mirror mode leaves the adopted copy alone instead of trashing it,
	/// where the same state with an ordinary synced row is a deletion (which is what
	/// `Backlog::Propagate` keeps).
	#[test]
	fn a_mirror_leaves_an_adopted_destination_copy_alone() {
		let uuid = Uuid::new_v4();
		let remote = map(vec![("gone.txt", remote_file("gone.txt", uuid, [1; 32]))]);
		let local = map(vec![]);

		// Propagate: the standing row makes this a deletion under the new mode.
		let synced = map(vec![("gone.txt", base_file("gone.txt", uuid, [1; 32]))]);
		assert_eq!(
			plan(SyncMode::LocalToRemote, &synced, &local, &remote),
			vec![SyncAction::TrashRemote {
				rel_path: "gone.txt".to_string(),
				kind: NodeKind::File,
				remote_uuid: uuid,
			}]
		);

		// AdoptDestination: the same two sides, with the re-seeded row, plan nothing at all.
		let adopted = map(vec![(
			"gone.txt",
			adopted_from_remote("gone.txt", &remote["gone.txt"]),
		)]);
		assert!(
			plan(SyncMode::LocalToRemote, &adopted, &local, &remote).is_empty(),
			"an adopted destination copy must not be deleted, or re-created"
		);
		// And a backup mode never deleted it either way.
		assert!(plan(SyncMode::LocalBackup, &adopted, &local, &remote).is_empty());
	}

	/// TwoWay has no destination to spare: the adopted copy reads as newly created on the side that
	/// still holds it and flows back to the other one.
	#[test]
	fn two_way_pulls_an_adopted_destination_copy_back() {
		let uuid = Uuid::new_v4();
		let remote = map(vec![("gone.txt", remote_file("gone.txt", uuid, [1; 32]))]);
		let adopted = map(vec![(
			"gone.txt",
			adopted_from_remote("gone.txt", &remote["gone.txt"]),
		)]);
		assert_eq!(
			plan(SyncMode::TwoWay, &adopted, &map(vec![]), &remote),
			vec![SyncAction::DownloadFile {
				rel_path: "gone.txt".to_string(),
				remote_uuid: uuid,
			}]
		);
	}

	/// The row retires on its own: as soon as the source has something at the path again the two
	/// sides reconcile normally (here: the same bytes on both sides, so the pass records the
	/// converged state), and once neither side holds the path the row is dropped.
	#[test]
	fn an_adopted_row_retires_as_soon_as_either_side_moves() {
		let uuid = Uuid::new_v4();
		let remote = map(vec![("gone.txt", remote_file("gone.txt", uuid, [1; 32]))]);
		let adopted = map(vec![(
			"gone.txt",
			adopted_from_remote("gone.txt", &remote["gone.txt"]),
		)]);

		// The source is back with the SAME content: nothing to transfer, but the row must stop
		// being an adoption.
		let local = map(vec![("gone.txt", local_file("gone.txt", [1; 32]))]);
		assert_eq!(
			plan(SyncMode::LocalToRemote, &adopted, &local, &remote),
			vec![SyncAction::AdoptBaseline {
				rel_path: "gone.txt".to_string(),
			}]
		);
		// Back with DIFFERENT content: the source is authoritative again.
		let edited = map(vec![("gone.txt", local_file("gone.txt", [9; 32]))]);
		assert_eq!(
			plan(SyncMode::LocalToRemote, &adopted, &edited, &remote),
			vec![SyncAction::UploadFile {
				rel_path: "gone.txt".to_string(),
			}]
		);
		// The destination lost it too (someone deleted it there): the stale row is retired.
		assert_eq!(
			plan(
				SyncMode::LocalToRemote,
				&adopted,
				&map(vec![]),
				&map(vec![])
			),
			vec![SyncAction::AdoptBaseline {
				rel_path: "gone.txt".to_string(),
			}]
		);
	}

	/// An adopted row is one side's standing copy, not a synced pair, so move detection must not
	/// match it: the "moved" item was never on the source side to begin with.
	#[test]
	fn an_adopted_row_is_never_a_move_endpoint() {
		let uuid = Uuid::new_v4();
		let remote = map(vec![("moved.txt", remote_file("moved.txt", uuid, [1; 32]))]);
		let adopted = map(vec![(
			"gone.txt",
			adopted_from_remote("gone.txt", &remote_file("gone.txt", uuid, [1; 32])),
		)]);
		let actions = plan(SyncMode::TwoWay, &adopted, &map(vec![]), &remote);
		assert!(
			!actions
				.iter()
				.any(|a| matches!(a, SyncAction::MoveLocal { .. })),
			"the adopted row was matched as a move source: {actions:?}"
		);
	}

	#[test]
	fn local_to_remote_uploads_new_and_trashes_remote_only() {
		let local = map(vec![("a.txt", local_file("a.txt", [1; 32]))]);
		let stale = Uuid::new_v4();
		let remote = map(vec![("old.txt", remote_file("old.txt", stale, [2; 32]))]);
		let actions = plan(SyncMode::LocalToRemote, &HashMap::new(), &local, &remote);
		assert_eq!(
			actions,
			vec![
				SyncAction::UploadFile {
					rel_path: "a.txt".to_string(),
				},
				SyncAction::TrashRemote {
					rel_path: "old.txt".to_string(),
					kind: NodeKind::File,
					remote_uuid: stale,
				},
			],
			"transfers ordered before deletes (deletes run last)"
		);
	}

	#[test]
	fn local_backup_uploads_but_never_trashes() {
		let local = map(vec![("a.txt", local_file("a.txt", [1; 32]))]);
		let remote = map(vec![(
			"old.txt",
			remote_file("old.txt", Uuid::new_v4(), [2; 32]),
		)]);
		let actions = plan(SyncMode::LocalBackup, &HashMap::new(), &local, &remote);
		assert_eq!(
			actions,
			vec![SyncAction::UploadFile {
				rel_path: "a.txt".to_string(),
			}],
			"backup pushes the new file but leaves the remote-only file untouched"
		);
	}

	#[test]
	fn remote_to_local_downloads_and_deletes_local_only() {
		let uuid = Uuid::new_v4();
		let remote = map(vec![("r.txt", remote_file("r.txt", uuid, [3; 32]))]);
		let local = map(vec![("extra.txt", local_file("extra.txt", [4; 32]))]);
		let actions = plan(SyncMode::RemoteToLocal, &HashMap::new(), &local, &remote);
		assert_eq!(
			actions,
			vec![
				SyncAction::DownloadFile {
					rel_path: "r.txt".to_string(),
					remote_uuid: uuid,
				},
				SyncAction::DeleteLocal {
					rel_path: "extra.txt".to_string(),
					kind: NodeKind::File,
				},
			],
			"transfers ordered before deletes (deletes run last)"
		);
	}

	const ALL_MODES: [SyncMode; 5] = [
		SyncMode::LocalToRemote,
		SyncMode::RemoteToLocal,
		SyncMode::LocalBackup,
		SyncMode::RemoteBackup,
		SyncMode::TwoWay,
	];

	#[test]
	fn identical_content_adopts_the_baseline_in_every_mode() {
		let uuid = Uuid::new_v4();
		let local = map(vec![("a.txt", local_file("a.txt", [7; 32]))]);
		let remote = map(vec![("a.txt", remote_file("a.txt", uuid, [7; 32]))]);
		// An already-converged path with no baseline row is adopted (no transfer) in EVERY mode:
		// without it a pair opened on an already-synced tree never seeds a baseline, so every later
		// pass still looks like a first sync and the guard holds all deletions forever.
		for mode in ALL_MODES {
			assert_eq!(
				plan(mode, &HashMap::new(), &local, &remote),
				vec![SyncAction::AdoptBaseline {
					rel_path: "a.txt".to_string()
				}],
				"{mode:?} must adopt the converged path"
			);
		}
		// Once the baseline records it, the pass is a clean no-op — adopting must not re-trigger.
		let baseline = map(vec![("a.txt", base_file("a.txt", uuid, [7; 32]))]);
		for mode in ALL_MODES {
			assert!(
				plan(mode, &baseline, &local, &remote).is_empty(),
				"{mode:?} must be a no-op once the baseline is current"
			);
		}
	}

	#[test]
	fn a_baseline_row_for_a_path_gone_from_both_sides_is_adopted_away() {
		let uuid = Uuid::new_v4();
		let baseline = map(vec![("a.txt", base_file("a.txt", uuid, [7; 32]))]);
		for mode in ALL_MODES {
			assert_eq!(
				plan(mode, &baseline, &HashMap::new(), &HashMap::new()),
				vec![SyncAction::AdoptBaseline {
					rel_path: "a.txt".to_string()
				}],
				"{mode:?} must drop the stale baseline row for a convergently-deleted path"
			);
		}
	}

	#[test]
	fn one_way_modes_adopt_a_matching_dir_so_the_pair_stops_looking_like_a_first_sync() {
		let uuid = Uuid::new_v4();
		let remote_dir = RemoteNode {
			rel_path: "d".to_string(),
			kind: NodeKind::Dir,
			remote_uuid: uuid,
			stable_uuid: None,
			content_hash: None,
			size: 0,
			modified_millis: 0,
		};
		let local = map(vec![("d", local_dir("d"))]);
		let remote = map(vec![("d", remote_dir)]);
		assert_eq!(
			plan(SyncMode::LocalToRemote, &HashMap::new(), &local, &remote),
			vec![SyncAction::AdoptBaseline {
				rel_path: "d".to_string()
			}]
		);
	}

	#[test]
	fn create_actions_order_parents_before_children() {
		let local = map(vec![
			("dir/sub/c.txt", local_file("dir/sub/c.txt", [1; 32])),
			("dir", local_dir("dir")),
			("dir/sub", local_dir("dir/sub")),
		]);
		let actions = plan(
			SyncMode::LocalToRemote,
			&HashMap::new(),
			&local,
			&HashMap::new(),
		);
		let paths: Vec<_> = actions.iter().map(|a| a.rel_path()).collect();
		assert_eq!(paths, vec!["dir", "dir/sub", "dir/sub/c.txt"]);
	}

	#[test]
	fn dir_rename_moves_children_before_trashing_the_old_dir() {
		// Local rename of `old/` -> `new/` (with child `x.txt`). The child is a hash-matched move;
		// the old dir is trashed. The move MUST be ordered before the old-dir trash, otherwise the
		// cascading dir delete destroys the move source (the headline ordering bug).
		let dir_uuid = Uuid::new_v4();
		let file_uuid = Uuid::new_v4();
		let base_dir = BaselineEntry {
			rel_path: "old".to_string(),
			kind: NodeKind::Dir,
			remote_uuid: Some(dir_uuid),
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
		};
		let baseline = map(vec![
			("old", base_dir),
			("old/x.txt", base_file("old/x.txt", file_uuid, [7; 32])),
		]);
		// Remote still holds the pre-rename tree.
		let remote_dir = RemoteNode {
			rel_path: "old".to_string(),
			kind: NodeKind::Dir,
			remote_uuid: dir_uuid,
			stable_uuid: None,
			content_hash: None,
			size: 0,
			modified_millis: 0,
		};
		let remote = map(vec![
			("old", remote_dir),
			("old/x.txt", remote_file("old/x.txt", file_uuid, [7; 32])),
		]);
		// Locally the dir and its child have been renamed to `new/`.
		let local = map(vec![
			("new", local_dir("new")),
			("new/x.txt", local_file("new/x.txt", [7; 32])),
		]);

		let actions = plan(SyncMode::LocalToRemote, &baseline, &local, &remote);
		assert_eq!(
			actions,
			vec![
				SyncAction::CreateRemoteDir {
					rel_path: "new".to_string(),
				},
				SyncAction::MoveRemote {
					from_path: "old/x.txt".to_string(),
					to_path: "new/x.txt".to_string(),
					kind: NodeKind::File,
					remote_uuid: file_uuid,
				},
				SyncAction::TrashRemote {
					rel_path: "old".to_string(),
					kind: NodeKind::Dir,
					remote_uuid: dir_uuid,
				},
			],
			"create new dir, MOVE the child out, THEN trash the old dir — never trash first"
		);
	}

	#[test]
	fn a_directory_replace_delete_runs_after_the_moves_out_of_that_directory() {
		// The file `item` and the directory `box/` (holding inner.txt) swap names: `item` becomes a
		// directory holding inner.txt, `box` becomes the file. Trashing a directory is RECURSIVE, so
		// the trash of `box` must not run before the move that carries `box/inner.txt` out of it —
		// the server would then reject the move with `cannot_move_this_file`.
		let item_uuid = Uuid::new_v4();
		let box_uuid = Uuid::new_v4();
		let inner_uuid = Uuid::new_v4();
		let baseline = map(vec![
			("item", base_file("item", item_uuid, [1; 32])),
			("box", base_dir("box", box_uuid)),
			(
				"box/inner.txt",
				base_file("box/inner.txt", inner_uuid, [2; 32]),
			),
		]);
		let remote = map(vec![
			("item", remote_file("item", item_uuid, [1; 32])),
			("box", remote_dir_node("box", box_uuid)),
			(
				"box/inner.txt",
				remote_file("box/inner.txt", inner_uuid, [2; 32]),
			),
		]);
		let local = map(vec![
			("item", local_dir("item")),
			("item/inner.txt", local_file("item/inner.txt", [2; 32])),
			("box", local_file("box", [1; 32])),
		]);

		let actions = plan(SyncMode::LocalToRemote, &baseline, &local, &remote);
		assert_eq!(
			actions,
			vec![
				// The old FILE at `item` goes first — nothing hangs off it and the dir create needs
				// the name free.
				SyncAction::TrashRemote {
					rel_path: "item".to_string(),
					kind: NodeKind::File,
					remote_uuid: item_uuid,
				},
				SyncAction::CreateRemoteDir {
					rel_path: "item".to_string(),
				},
				SyncAction::MoveRemote {
					from_path: "box/inner.txt".to_string(),
					to_path: "item/inner.txt".to_string(),
					kind: NodeKind::File,
					remote_uuid: inner_uuid,
				},
				// Only now may the old DIRECTORY be trashed; the upload of the new file `box`
				// follows it.
				SyncAction::TrashRemote {
					rel_path: "box".to_string(),
					kind: NodeKind::Dir,
					remote_uuid: box_uuid,
				},
				SyncAction::UploadFile {
					rel_path: "box".to_string(),
				},
			],
			"a recursive dir trash must follow the moves out of it, and precede the create at its path"
		);
	}

	#[test]
	fn a_just_written_remote_item_the_cache_has_not_caught_up_to_is_folded_in_not_acted_on() {
		let uuid = Uuid::new_v4();
		let baseline = map(vec![("a.txt", base_file("a.txt", uuid, [5; 32]))]);
		let local = map(vec![("a.txt", local_file("a.txt", [5; 32]))]);
		// The snapshot is behind: the uuid the pass just recorded is not in it yet.
		let remote = HashMap::new();
		let observations = Observations::default();
		let writes = PendingWrites::default();
		writes.record(
			&observations,
			PAIR,
			uuid,
			PendingKind::Created {
				path: "a.txt".to_string(),
				replaced: None,
			},
		);
		for mode in ALL_MODES {
			assert!(
				plan_folded(mode, &baseline, &local, &remote, &writes).is_empty(),
				"{mode:?} must neither re-transfer nor delete a write the cache has not seen"
			);
		}
		// Once the uuid is no longer pending (the grace window elapsed), the snapshot is believed
		// again: the file reads as a remote-side deletion and is re-pushed.
		assert_eq!(
			plan(SyncMode::LocalToRemote, &baseline, &local, &remote),
			vec![SyncAction::UploadFile {
				rel_path: "a.txt".to_string(),
			}],
			"past the grace window the old behaviour returns"
		);
	}

	/// Trashing an item drops its baseline row, so a snapshot that has not applied the trash yet
	/// reads it as an untracked remote file with nothing local — a deletion to make all over
	/// again. Only the deletion of that uuid is suppressed.
	#[test]
	fn a_remote_deletion_this_engine_already_made_is_not_repeated() {
		let uuid = Uuid::new_v4();
		let remote = map(vec![("note.txt", remote_file("note.txt", uuid, [5; 32]))]);
		let (baseline, local) = (HashMap::new(), HashMap::new());

		assert_eq!(
			plan(SyncMode::LocalToRemote, &baseline, &local, &remote),
			vec![SyncAction::TrashRemote {
				rel_path: "note.txt".to_string(),
				kind: NodeKind::File,
				remote_uuid: uuid,
			}],
			"without the hold this is exactly the duplicate trash"
		);
		assert!(
			reconcile(
				SyncMode::LocalToRemote,
				&baseline,
				&local,
				&remote,
				&PassHolds {
					trashed: HashSet::from([uuid]),
					..Default::default()
				},
			)
			.actions
			.is_empty(),
			"a trash this engine already applied must not be applied again"
		);
	}

	/// The ACTION is suppressed, not the path: a delete-then-recreate uploads a new file at the
	/// very path that was just trashed, and freezing the path would stall that upload.
	#[test]
	fn a_pending_trash_does_not_freeze_its_path_against_a_new_file() {
		let uuid = Uuid::new_v4();
		// The cache still lists the trashed file; a fresh local file now holds the same name.
		let remote = map(vec![("note.txt", remote_file("note.txt", uuid, [5; 32]))]);
		let local = map(vec![("note.txt", local_file("note.txt", [9; 32]))]);

		assert_eq!(
			reconcile(
				SyncMode::LocalToRemote,
				&HashMap::new(),
				&local,
				&remote,
				&PassHolds {
					trashed: HashSet::from([uuid]),
					..Default::default()
				},
			)
			.actions,
			vec![SyncAction::UploadFile {
				rel_path: "note.txt".to_string(),
			}],
			"the re-created file still uploads"
		);
	}

	#[test]
	fn a_directory_this_pass_created_is_not_created_a_second_time() {
		let dir_uuid = Uuid::new_v4();
		let file_uuid = Uuid::new_v4();
		let baseline = map(vec![
			("d", base_dir("d", dir_uuid)),
			("d/x.txt", base_file("d/x.txt", file_uuid, [1; 32])),
		]);
		let local = map(vec![
			("d", local_dir("d")),
			("d/x.txt", local_file("d/x.txt", [1; 32])),
		]);
		// Neither the new dir nor its child has reached the cache yet.
		let observations = Observations::default();
		let writes = PendingWrites::default();
		writes.record(
			&observations,
			PAIR,
			dir_uuid,
			PendingKind::Created {
				path: "d".to_string(),
				replaced: None,
			},
		);
		writes.record(
			&observations,
			PAIR,
			file_uuid,
			PendingKind::Created {
				path: "d/x.txt".to_string(),
				replaced: None,
			},
		);
		assert!(
			plan_folded(
				SyncMode::LocalToRemote,
				&baseline,
				&local,
				&HashMap::new(),
				&writes
			)
			.is_empty(),
			"a dir this pass created must not be re-created, and its unchanged child must not be \
			 re-uploaded"
		);
	}

	/// The same pass, one step later: the cache has announced the CHILD's upload but not the
	/// directory's creation. The announcement retires the child's record — it is evidence the cache
	/// has caught up, and a snapshot that still lists nothing there is evidence the file is gone
	/// (trashed, or superseded by another client), so the pass must put it back. Folding the
	/// directory in is what gives that upload a parent to land in; the directory itself is still not
	/// created a second time.
	#[test]
	fn a_child_the_cache_has_announced_is_re_uploaded_into_the_folded_parent() {
		let dir_uuid = Uuid::new_v4();
		let file_uuid = Uuid::new_v4();
		let baseline = map(vec![
			("d", base_dir("d", dir_uuid)),
			("d/x.txt", base_file("d/x.txt", file_uuid, [1; 32])),
		]);
		let local = map(vec![
			("d", local_dir("d")),
			("d/x.txt", local_file("d/x.txt", [1; 32])),
		]);
		let observations = Observations::default();
		let writes = PendingWrites::default();
		writes.record(
			&observations,
			PAIR,
			dir_uuid,
			PendingKind::Created {
				path: "d".to_string(),
				replaced: None,
			},
		);
		assert_eq!(
			plan_folded(
				SyncMode::LocalToRemote,
				&baseline,
				&local,
				&HashMap::new(),
				&writes
			),
			vec![SyncAction::UploadFile {
				rel_path: "d/x.txt".to_string(),
			}]
		);
	}

	#[test]
	fn an_unapplied_move_is_not_undone_by_a_snapshot_still_showing_the_old_path() {
		let uuid = Uuid::new_v4();
		// The pass moved a.txt -> b.txt and advanced its baseline; the cache still lists the uuid at
		// a.txt. Acting on that would trash a.txt (the very item that was moved) and re-upload
		// b.txt — in two-way, even quarantine the local file first.
		let baseline = map(vec![("b.txt", base_file("b.txt", uuid, [5; 32]))]);
		let local = map(vec![("b.txt", local_file("b.txt", [5; 32]))]);
		let remote = map(vec![("a.txt", remote_file("a.txt", uuid, [5; 32]))]);
		let observations = Observations::default();
		let writes = PendingWrites::default();
		writes.record(
			&observations,
			PAIR,
			uuid,
			PendingKind::Moved {
				from: "a.txt".to_string(),
				to: "b.txt".to_string(),
			},
		);
		for mode in ALL_MODES {
			assert!(
				plan_folded(mode, &baseline, &local, &remote, &writes).is_empty(),
				"{mode:?} must leave both ends of an unapplied move alone"
			);
		}
		// The engine only marks the uuid pending while the snapshot still shows the PRE-move state;
		// once it does not, the path is acted on again (another client moving the item after us
		// must reconverge immediately, not wait out the grace window).
		assert_eq!(
			plan(SyncMode::TwoWay, &baseline, &local, &remote),
			vec![SyncAction::MoveLocal {
				from_path: "b.txt".to_string(),
				to_path: "a.txt".to_string(),
				kind: NodeKind::File,
			}]
		);
	}

	#[test]
	fn is_safe_name_rejects_traversal_and_separators() {
		assert!(is_safe_name("normal.txt"));
		assert!(is_safe_name("a file with spaces"));
		assert!(!is_safe_name(""));
		assert!(!is_safe_name("."));
		assert!(!is_safe_name(".."));
		assert!(!is_safe_name("a/b"), "embedded separator");
		assert!(!is_safe_name("a\\b"), "embedded backslash");
	}

	#[test]
	fn resolve_path_skips_traversal_names_so_they_never_enter_the_plan() {
		let root = Uuid::new_v4();
		let empty = HashMap::new();
		// A direct child literally named ".." is rejected (would escape the root on pull).
		assert_eq!(resolve_path("..", root, root, &empty), None);
		// A normal child resolves.
		assert_eq!(
			resolve_path("ok.txt", root, root, &empty),
			Some("ok.txt".to_string())
		);
		// A child under an ancestor dir whose NAME is a traversal is rejected too.
		let evil_parent = Uuid::new_v4();
		let idx = HashMap::from([(evil_parent, ("..".to_string(), root))]);
		assert_eq!(resolve_path("x.txt", evil_parent, root, &idx), None);
	}

	#[test]
	fn build_remote_view_excludes_the_quarantine_dir_name() {
		let root = Uuid::new_v4();
		let trash = CacheableDir {
			uuid: Uuid::new_v4(),
			parent: root,
			color: Default::default(),
			favorited: false,
			timestamp: ms(1),
			name: Cow::Borrowed(QUARANTINE_DIR),
			created: Some(ms(1)),
		};
		let view = build_remote_view(root, std::slice::from_ref(&trash), &[]);
		assert!(
			view.nodes.is_empty(),
			"a remote folder named like the quarantine dir must be excluded from the view"
		);
	}

	#[test]
	fn to_event_maps_each_action_to_its_in_progress_event() {
		assert_eq!(
			SyncAction::UploadFile {
				rel_path: "a.txt".into()
			}
			.to_event(),
			SyncEvent::Uploading {
				rel_path: "a.txt".into()
			}
		);
		assert_eq!(
			SyncAction::DownloadFile {
				rel_path: "b.txt".into(),
				remote_uuid: Uuid::nil(),
			}
			.to_event(),
			SyncEvent::Downloading {
				rel_path: "b.txt".into()
			}
		);
		assert_eq!(
			SyncAction::TrashRemote {
				rel_path: "gone".into(),
				kind: NodeKind::File,
				remote_uuid: Uuid::nil(),
			}
			.to_event(),
			SyncEvent::TrashingRemote {
				rel_path: "gone".into()
			}
		);
		assert_eq!(
			SyncAction::MoveRemote {
				from_path: "x".into(),
				to_path: "y".into(),
				kind: NodeKind::File,
				remote_uuid: Uuid::nil(),
			}
			.to_event(),
			SyncEvent::MovingRemote {
				from: "x".into(),
				to: "y".into(),
			}
		);
	}

	#[test]
	fn two_way_pushes_local_edit_and_pulls_remote_edit() {
		let uuid = Uuid::new_v4();
		// Local edited (hash diverged from baseline), remote unchanged -> push.
		let baseline = map(vec![("a.txt", base_file("a.txt", uuid, [0; 32]))]);
		let local = map(vec![("a.txt", local_file("a.txt", [1; 32]))]);
		let remote = map(vec![("a.txt", remote_file("a.txt", uuid, [0; 32]))]);
		assert_eq!(
			plan(SyncMode::TwoWay, &baseline, &local, &remote),
			vec![SyncAction::UploadFile {
				rel_path: "a.txt".to_string(),
			}]
		);

		// Remote edited (new uuid), local unchanged -> pull.
		let new_uuid = Uuid::new_v4();
		let baseline = map(vec![("a.txt", base_file("a.txt", uuid, [0; 32]))]);
		let local = map(vec![("a.txt", local_file("a.txt", [0; 32]))]);
		let remote = map(vec![("a.txt", remote_file("a.txt", new_uuid, [1; 32]))]);
		assert_eq!(
			plan(SyncMode::TwoWay, &baseline, &local, &remote),
			vec![SyncAction::DownloadFile {
				rel_path: "a.txt".to_string(),
				remote_uuid: new_uuid,
			}]
		);
	}

	#[test]
	fn two_way_both_sides_diverge_is_a_conflict_but_same_edit_converges() {
		let uuid = Uuid::new_v4();
		let new_uuid = Uuid::new_v4();
		let baseline = map(vec![("a.txt", base_file("a.txt", uuid, [0; 32]))]);

		// Both edited to DIFFERENT content -> conflict.
		let local = map(vec![("a.txt", local_file("a.txt", [1; 32]))]);
		let remote = map(vec![("a.txt", remote_file("a.txt", new_uuid, [2; 32]))]);
		assert_eq!(
			plan(SyncMode::TwoWay, &baseline, &local, &remote),
			vec![SyncAction::Conflict {
				rel_path: "a.txt".to_string(),
			}]
		);

		// Both edited to the SAME content -> converged (no conflict, no transfer); the now-stale
		// baseline is refreshed to the converged state via AdoptBaseline.
		let local = map(vec![("a.txt", local_file("a.txt", [9; 32]))]);
		let remote = map(vec![("a.txt", remote_file("a.txt", new_uuid, [9; 32]))]);
		assert_eq!(
			plan(SyncMode::TwoWay, &baseline, &local, &remote),
			vec![SyncAction::AdoptBaseline {
				rel_path: "a.txt".to_string(),
			}],
			"identical concurrent edits converge without a conflict and re-baseline"
		);
	}

	#[test]
	fn two_way_propagates_a_local_delete_to_the_remote() {
		let uuid = Uuid::new_v4();
		let baseline = map(vec![("a.txt", base_file("a.txt", uuid, [0; 32]))]);
		let remote = map(vec![("a.txt", remote_file("a.txt", uuid, [0; 32]))]);
		// Local deleted (absent), remote unchanged -> trash remote.
		let actions = plan(SyncMode::TwoWay, &baseline, &HashMap::new(), &remote);
		assert_eq!(
			actions,
			vec![SyncAction::TrashRemote {
				rel_path: "a.txt".to_string(),
				kind: NodeKind::File,
				remote_uuid: uuid,
			}]
		);
	}

	#[test]
	fn conflicted_baseline_rows_are_re_reported_but_never_acted_on() {
		let uuid = Uuid::new_v4();
		let mut entry = base_file("a.txt", uuid, [0; 32]);
		entry.state = BaselineState::Conflicted;
		let baseline = map(vec![("a.txt", entry)]);
		let local = map(vec![("a.txt", local_file("a.txt", [1; 32]))]);
		let remote = map(vec![("a.txt", remote_file("a.txt", uuid, [0; 32]))]);
		assert_eq!(
			plan(SyncMode::TwoWay, &baseline, &local, &remote),
			vec![SyncAction::Conflict {
				rel_path: "a.txt".to_string()
			}],
			"a held conflict is re-reported, never re-acted on"
		);
	}

	#[test]
	fn a_conflicted_baseline_row_holds_its_whole_subtree() {
		// `thing` is held in conflict as a FILE; locally it is now a directory with a child. The
		// child must not be planned — the remote dir `thing` cannot exist while the held file does,
		// so the upload would fail with a missing remote parent.
		let uuid = Uuid::new_v4();
		let mut held = base_file("thing", uuid, [0; 32]);
		held.state = BaselineState::Conflicted;
		let baseline = map(vec![("thing", held)]);
		let local = map(vec![
			("thing", local_dir("thing")),
			("thing/child.txt", local_file("thing/child.txt", [1; 32])),
		]);
		let remote = map(vec![("thing", remote_file("thing", uuid, [0; 32]))]);
		assert_eq!(
			plan(SyncMode::TwoWay, &baseline, &local, &remote),
			vec![SyncAction::Conflict {
				rel_path: "thing".to_string()
			}],
			"nothing under a held conflict may be planned"
		);
	}

	#[test]
	fn a_held_conflict_is_never_laundered_into_a_remote_move() {
		// The conflicting remote item is renamed server-side while the conflict is still held. Move
		// detection matches by uuid and must NOT consume the held path: doing so would rename the
		// local copy, drop the Conflicted row and re-anchor the baseline to the remote's content,
		// after which the next pass reads the untouched local bytes as a one-sided edit and pushes
		// them over the remote's — silently destroying one side of an unresolved conflict.
		let uuid = Uuid::new_v4();
		let mut held = base_file("thing", uuid, [1; 32]);
		held.state = BaselineState::Conflicted;
		let baseline = map(vec![("thing", held)]);
		let local = map(vec![("thing", local_file("thing", [1; 32]))]);
		let remote = map(vec![(
			"thing_renamed",
			remote_file("thing_renamed", uuid, [2; 32]),
		)]);
		let actions = plan(SyncMode::TwoWay, &baseline, &local, &remote);
		assert!(
			actions.contains(&SyncAction::Conflict {
				rel_path: "thing".to_string()
			}),
			"the held conflict must still be reported: {actions:?}"
		);
		assert!(
			!actions.iter().any(|a| matches!(
				a,
				SyncAction::MoveLocal { .. } | SyncAction::MoveRemote { .. }
			)),
			"a held conflict must never be consumed as a move: {actions:?}"
		);
	}

	#[test]
	fn a_held_conflict_is_never_laundered_into_a_local_move() {
		// Mirror direction: the local half of a held conflict is deleted and its bytes reappear at a
		// new local path. Content-hash move detection must not re-parent the still-divergent remote
		// file onto that name and mark the row Synced, which would mask the conflict for good.
		let uuid = Uuid::new_v4();
		let mut held = base_file("thing", uuid, [1; 32]);
		held.state = BaselineState::Conflicted;
		let baseline = map(vec![("thing", held)]);
		let local = map(vec![("moved.txt", local_file("moved.txt", [1; 32]))]);
		let remote = map(vec![("thing", remote_file("thing", uuid, [2; 32]))]);
		let actions = plan(SyncMode::TwoWay, &baseline, &local, &remote);
		assert!(
			actions.contains(&SyncAction::Conflict {
				rel_path: "thing".to_string()
			}),
			"the held conflict must still be reported: {actions:?}"
		);
		assert!(
			!actions.iter().any(|a| matches!(
				a,
				SyncAction::MoveLocal { .. } | SyncAction::MoveRemote { .. }
			)),
			"a held conflict must never be consumed as a move: {actions:?}"
		);
	}

	#[test]
	fn a_conflict_surfaced_this_pass_suppresses_its_descendants() {
		// Same shape, but with NO persisted Conflicted row yet: the conflict on `thing` is surfaced
		// by THIS pass, so its descendants must already be suppressed (they would otherwise error
		// in apply before the row is ever written).
		let uuid = Uuid::new_v4();
		let baseline = map(vec![("thing", base_file("thing", uuid, [0; 32]))]);
		let local = map(vec![
			("thing", local_dir("thing")),
			("thing/child.txt", local_file("thing/child.txt", [1; 32])),
		]);
		// Remote edited `thing` (new uuid) while local flipped it to a dir -> both sides changed.
		let remote = map(vec![(
			"thing",
			remote_file("thing", Uuid::new_v4(), [2; 32]),
		)]);
		assert_eq!(
			plan(SyncMode::TwoWay, &baseline, &local, &remote),
			vec![SyncAction::Conflict {
				rel_path: "thing".to_string()
			}],
			"the conflict is reported, its subtree is not acted on"
		);
	}

	#[test]
	fn a_conflict_does_not_suppress_an_unrelated_sibling_prefix() {
		// `thing` is held; `thing2/x.txt` merely shares a name PREFIX and must still be planned.
		let uuid = Uuid::new_v4();
		let mut held = base_file("thing", uuid, [0; 32]);
		held.state = BaselineState::Conflicted;
		let baseline = map(vec![("thing", held)]);
		let local = map(vec![
			("thing", local_file("thing", [1; 32])),
			("thing2", local_dir("thing2")),
			("thing2/x.txt", local_file("thing2/x.txt", [3; 32])),
		]);
		let remote = map(vec![("thing", remote_file("thing", uuid, [0; 32]))]);
		let actions = plan(SyncMode::TwoWay, &baseline, &local, &remote);
		assert_eq!(
			actions,
			vec![
				SyncAction::CreateRemoteDir {
					rel_path: "thing2".to_string()
				},
				SyncAction::UploadFile {
					rel_path: "thing2/x.txt".to_string()
				},
				SyncAction::Conflict {
					rel_path: "thing".to_string()
				},
			],
			"only the held path is withheld; a mere name-prefix sibling still syncs"
		);
	}

	/// The cache can list a re-upload's successor and its not-yet-trashed predecessor at once, under
	/// the exact same name. Refusing the whole pass for that made every pass a no-op until the
	/// cache caught up; only the one path is withheld.
	#[test]
	fn a_byte_identical_duplicate_remote_name_holds_only_that_path() {
		let root = Uuid::new_v4();
		let predecessor = cacheable_file(root, "note.txt");
		let successor = CacheableFile {
			uuid: Uuid::new_v4(),
			..predecessor.clone()
		};
		let sibling = cacheable_file(root, "other.txt");

		let view = build_remote_view(root, &[], &[predecessor, successor, sibling.clone()]);

		assert!(
			!view.has_collisions,
			"a cache in transition must not refuse the pass"
		);
		assert_eq!(view.held_paths, HashSet::from(["note.txt".to_string()]));
		assert!(
			!view.nodes.contains_key("note.txt"),
			"neither half of the transition may be reconciled against"
		);
		assert_eq!(
			view.nodes["other.txt"].remote_uuid, sibling.uuid,
			"every other path still syncs"
		);
	}

	/// A case-only collision is a real one — the server does allow both names, and no 1:1 local
	/// mapping exists — so the whole-pass refusal stays.
	#[test]
	fn a_case_only_remote_collision_still_refuses_the_pass() {
		let root = Uuid::new_v4();
		let lower = cacheable_file(root, "note.txt");
		let upper = CacheableFile {
			uuid: Uuid::new_v4(),
			name: Cow::Borrowed("Note.txt"),
			..lower.clone()
		};

		let view = build_remote_view(root, &[], &[lower, upper]);

		assert!(
			view.has_collisions,
			"a case-only collision is not a transition"
		);
		assert!(view.held_paths.is_empty());
	}

	#[test]
	fn build_remote_view_resolves_paths_and_flags_collisions() {
		let root = Uuid::new_v4();
		let sub = CacheableDir {
			uuid: Uuid::new_v4(),
			parent: root,
			color: Default::default(),
			favorited: false,
			timestamp: ms(1),
			name: Cow::Borrowed("sub"),
			created: Some(ms(1)),
		};
		let file_uuid = Uuid::new_v4();
		let file = CacheableFile {
			uuid: file_uuid,
			stable_uuid: filen_types::fs::StableUuid::new_for_test(file_uuid),
			parent: sub.uuid,
			chunks_size: 1,
			chunks: 1,
			favorited: false,
			region: Cow::Borrowed("r"),
			bucket: Cow::Borrowed("b"),
			timestamp: ms(1),
			name: Cow::Borrowed("f.txt"),
			size: 5,
			mime: Cow::Borrowed("text/plain"),
			key: crate::crypto::file::FileKey::from_str_with_version(
				&"a".repeat(64),
				filen_types::auth::FileEncryptionVersion::V3,
			)
			.unwrap(),
			last_modified: ms(7),
			created: Some(ms(1)),
			hash: Some(Blake3Hash::from([5; 32])),
		};
		let orphan = CacheableFile {
			parent: Uuid::new_v4(), // a parent not in the snapshot -> orphan, skipped
			..file.clone()
		};

		let view = build_remote_view(root, std::slice::from_ref(&sub), &[file.clone(), orphan]);
		assert!(!view.has_collisions);
		let mut paths: Vec<_> = view.nodes.keys().cloned().collect();
		paths.sort();
		assert_eq!(
			paths,
			vec!["sub", "sub/f.txt"],
			"orphan skipped, path resolved"
		);
		assert_eq!(view.nodes["sub/f.txt"].remote_uuid, file.uuid);
		assert_eq!(
			view.nodes["sub/f.txt"].content_hash,
			Some(Blake3Hash::from([5; 32]))
		);
	}
}

//! The pure planning layer: build the remote-side view from a cache snapshot, then reconcile the
//! three inputs (baseline, local scan, remote view) into an ordered list of [`SyncAction`]s.
//!
//! `reconcile` is deliberately side-effect-free and synchronous so the whole decision matrix is
//! unit-testable without a filesystem or network. One-way modes converge the destination onto the
//! source (`make dst match src`, gated by whether deletions propagate); two-way uses the baseline
//! to tell which side changed and surfaces a genuine both-sides-changed divergence as a conflict.

use std::collections::{BTreeSet, HashMap, HashSet};

use filen_types::crypto::Blake3Hash;
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

use super::{
	baseline::{BaselineEntry, BaselineState, NodeKind},
	events::SyncEvent,
	scan::{LocalNode, QUARANTINE_DIR},
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
	/// A file moved/renamed on the local side -> re-parent + rename the remote item (uuid kept)
	/// instead of re-uploading its content.
	MoveRemote {
		from_path: String,
		to_path: String,
		remote_uuid: Uuid,
	},
	/// A file moved/renamed on the remote side -> rename the local file instead of re-downloading.
	MoveLocal {
		from_path: String,
		to_path: String,
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
				from_path, to_path, ..
			} => format!("move remote {from_path:?} -> {to_path:?}"),
			Self::MoveLocal { from_path, to_path } => {
				format!("move local {from_path:?} -> {to_path:?}")
			}
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
			Self::MoveLocal { from_path, to_path } => SyncEvent::MovingLocal {
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

fn collision_key(rel_path: &str) -> String {
	rel_path.chars().flat_map(char::to_lowercase).collect()
}

/// The remote view of a sync root's subtree, plus whether it is safe to reconcile against.
#[derive(Debug)]
pub(crate) struct RemoteView {
	pub(crate) nodes: HashMap<String, RemoteNode>,
	/// `true` if two distinct remote items resolved to the same case-insensitive path. The engine
	/// refuses to reconcile such a pair (a 1:1 local mapping is impossible) until the user cleans
	/// it up.
	pub(crate) has_collisions: bool,
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
	let mut claimed: HashMap<String, ()> = HashMap::new();
	let mut has_collisions = false;

	let mut insert = |rel_path: String, node: RemoteNode| {
		// The local quarantine dir is excluded from the local scan; exclude it from the remote view
		// too, so a remote folder that happens to be named `.filen-sync-trash` is never mistaken for
		// (or synced into) the quarantine area.
		if rel_path == QUARANTINE_DIR || rel_path.starts_with(&format!("{QUARANTINE_DIR}/")) {
			return;
		}
		if claimed.insert(collision_key(&rel_path), ()).is_some() {
			has_collisions = true;
			return;
		}
		nodes.insert(rel_path, node);
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
				content_hash: file.hash,
				size: file.size,
				modified_millis: file.last_modified.timestamp_millis(),
			},
		);
	}

	RemoteView {
		nodes,
		has_collisions,
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
					// A file's current remote version IS its uuid: a content change mints a new
					// uuid (the old becomes a prior version), so an unchanged uuid means unchanged
					// content regardless of whether the server stored a hash.
					NodeKind::File => {
						if base.remote_uuid == Some(node.remote_uuid) {
							Side::Unchanged
						} else {
							Side::Modified
						}
					}
					NodeKind::Dir => Side::Unchanged,
				}
			}
		}
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
		(false, true) => pull_to_local(rel_path, local, remote, base, true, actions),
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
/// - Local moves are matched by content hash: a baseline file gone locally whose content reappears
///   at a new local path (uniquely — ambiguous content is left to delete+create), with the remote
///   still holding the original, becomes a `MoveRemote`.
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
		for (from, base) in baseline {
			if base.kind != NodeKind::File || consumed.contains(from) {
				continue;
			}
			let Some(uuid) = base.remote_uuid else {
				continue;
			};
			if let Some(&to) = remote_path_of_uuid.get(&uuid)
				&& to != from
				&& !baseline.contains_key(to)
				&& local.contains_key(from)
				&& !local.contains_key(to)
				&& !consumed.contains(to)
			{
				let action = SyncAction::MoveLocal {
					from_path: from.clone(),
					to_path: to.to_string(),
				};
				tracing::debug!(
					"plan: {} — remote item moved (matched by uuid); renaming locally instead of re-downloading",
					action.describe()
				);
				actions.push(action);
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
			if base.kind != NodeKind::File || consumed.contains(from) || local.contains_key(from) {
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

/// The paths this pass must not act on, given the uuids whose place in the snapshot the engine
/// knows to be stale (`pending`): the baseline path the uuid is recorded at, and — for a move the
/// cache has not applied yet — the path the snapshot still shows it at. Acting on either would
/// re-transfer, duplicate, or delete the engine's own write.
fn stale_pending_paths<'a>(
	baseline: &'a HashMap<String, BaselineEntry>,
	remote: &'a HashMap<String, RemoteNode>,
	pending: &HashSet<Uuid>,
) -> HashSet<&'a str> {
	let mut stale = HashSet::new();
	if pending.is_empty() {
		return stale;
	}
	let snapshot_path: HashMap<Uuid, &str> = remote
		.iter()
		.map(|(path, node)| (node.remote_uuid, path.as_str()))
		.collect();
	for (path, entry) in baseline {
		let Some(uuid) = entry.remote_uuid else {
			continue;
		};
		if !pending.contains(&uuid) {
			continue;
		}
		stale.insert(path.as_str());
		if let Some(seen) = snapshot_path.get(&uuid) {
			stale.insert(seen);
		}
	}
	stale
}

/// Whether `key` is one of `stale` or lives under one. A directory the cache has not caught up to
/// is invisible, and so is everything the snapshot would otherwise resolve beneath it.
fn is_under_stale_path(key: &str, stale: &HashSet<&str>) -> bool {
	stale.contains(key)
		|| stale
			.iter()
			.any(|p| key.len() > p.len() && key.starts_with(p) && key.as_bytes()[p.len()] == b'/')
}

/// Reconcile a pair's three inputs into an ordered action plan. `baseline`/`local`/`remote` are all
/// keyed by the same NFC-normalized relative path.
///
/// `pending` holds remote uuids this engine wrote so recently that the cache — which learns of our
/// own writes only through socket events and resyncs — demonstrably still shows the pre-write
/// state. Their paths are left completely alone this pass, so a just-uploaded file is neither
/// re-uploaded nor read as a remote-side deletion, and a just-moved one is not moved back. The
/// engine decides which uuids qualify (see `PendingWrites`) and how long the window lasts (see
/// `PENDING_CREATE_GRACE`).
pub(crate) fn reconcile(
	mode: super::SyncMode,
	baseline: &HashMap<String, BaselineEntry>,
	local: &HashMap<String, LocalNode>,
	remote: &HashMap<String, RemoteNode>,
	pending: &HashSet<Uuid>,
) -> Vec<SyncAction> {
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

	// Consume the paths the snapshot is behind on before anything else looks at them, so neither
	// move detection nor the per-path reconcile can act on a cache that has not caught up.
	let stale = stale_pending_paths(baseline, remote, pending);
	if !stale.is_empty() {
		for key in &keys {
			if is_under_stale_path(key, &stale) {
				tracing::debug!(
					"reconcile: skipping {key:?} — a just-written remote item there is not yet visible in the cache"
				);
				consumed.insert((*key).to_string());
			}
		}
	}

	// Resolve moves next; their endpoints are then excluded from the per-path reconcile so a move
	// is never also emitted as a delete + create. Skip this in additive backup modes: a move's
	// other half is a deletion of the old name, and a backup mode must NEVER delete on its
	// destination — so a moved item is handled as an additive create of the new name with the old
	// name retained, not laundered into a move (which would remove the old name).
	if mode.propagates_deletes() {
		detect_moves(mode, baseline, local, remote, &mut actions, &mut consumed);
	}

	for key in keys {
		if consumed.contains(key) {
			continue;
		}
		let base = baseline.get(key);
		// A surfaced conflict is held until the caller resolves it — never re-acted on.
		if base.is_some_and(|b| b.state == BaselineState::Conflicted) {
			continue;
		}
		let local_node = local.get(key);
		let remote_node = remote.get(key);

		match mode {
			super::SyncMode::LocalToRemote => {
				push_to_remote(key, local_node, remote_node, base, true, &mut actions)
			}
			super::SyncMode::LocalBackup => {
				push_to_remote(key, local_node, remote_node, base, false, &mut actions)
			}
			super::SyncMode::RemoteToLocal => {
				pull_to_local(key, local_node, remote_node, base, true, &mut actions)
			}
			super::SyncMode::RemoteBackup => {
				pull_to_local(key, local_node, remote_node, base, false, &mut actions)
			}
			super::SyncMode::TwoWay => {
				reconcile_two_way(key, local_node, remote_node, base, &mut actions)
			}
		}
	}

	order_actions(&mut actions);
	tracing::debug!("reconcile: planned {} action(s)", actions.len());
	actions
}

/// Apply phases, lowest first. A "replace-delete" (a delete whose path is also created this pass —
/// a file<->dir type flip) has to precede the create at that path, but a DIRECTORY delete is
/// recursive (the server trashes a dir's whole subtree; a local delete quarantines it), so it must
/// still wait until everything moving OUT of that directory has moved.
const PHASE_FILE_REPLACE_DELETE: u8 = 0;
const PHASE_CREATE: u8 = 1;
const PHASE_MOVE: u8 = 2;
const PHASE_DIR_REPLACE_DELETE: u8 = 3;
const PHASE_TRANSFER: u8 = 4;
const PHASE_DELETE: u8 = 5;
const PHASE_CONFLICT: u8 = 6;

/// The apply phase of an action given the set of paths the pass materializes (`create_targets`).
/// See the `PHASE_*` constants; other deletes run LAST (child-before-parent) so a directory delete
/// cannot strand an item an earlier move/transfer still needs.
fn action_phase(action: &SyncAction, create_targets: &std::collections::HashSet<String>) -> u8 {
	match action {
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

	use super::*;
	use crate::sync_engine::SyncMode;

	/// Reconcile with no pending writes — the ordinary case; the cache-lag window has its own tests.
	fn plan(
		mode: SyncMode,
		baseline: &HashMap<String, BaselineEntry>,
		local: &HashMap<String, LocalNode>,
		remote: &HashMap<String, RemoteNode>,
	) -> Vec<SyncAction> {
		reconcile(mode, baseline, local, remote, &HashSet::new())
	}

	fn ms(millis: i64) -> DateTime<Utc> {
		DateTime::from_timestamp_millis(millis).unwrap()
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

	fn remote_file(rel: &str, uuid: Uuid, hash: [u8; 32]) -> RemoteNode {
		RemoteNode {
			rel_path: rel.to_string(),
			kind: NodeKind::File,
			remote_uuid: uuid,
			content_hash: Some(Blake3Hash::from(hash)),
			size: 10,
			modified_millis: 1,
		}
	}

	fn remote_dir_node(rel: &str, uuid: Uuid) -> RemoteNode {
		RemoteNode {
			rel_path: rel.to_string(),
			kind: NodeKind::Dir,
			remote_uuid: uuid,
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
		}
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
			}],
			"a remote move renames the local file, not delete + re-download"
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
	fn a_just_written_remote_item_the_cache_has_not_caught_up_to_is_left_alone() {
		let uuid = Uuid::new_v4();
		let baseline = map(vec![("a.txt", base_file("a.txt", uuid, [5; 32]))]);
		let local = map(vec![("a.txt", local_file("a.txt", [5; 32]))]);
		// The snapshot is behind: the uuid the pass just recorded is not in it yet.
		let remote = HashMap::new();
		let pending = HashSet::from([uuid]);
		for mode in ALL_MODES {
			assert!(
				reconcile(mode, &baseline, &local, &remote, &pending).is_empty(),
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

	#[test]
	fn a_pending_directory_shields_its_children_from_a_duplicate_create() {
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
		let pending = HashSet::from([dir_uuid, file_uuid]);
		assert!(
			reconcile(
				SyncMode::LocalToRemote,
				&baseline,
				&local,
				&HashMap::new(),
				&pending
			)
			.is_empty(),
			"a pending dir must not be re-created, and its children must not be uploaded into a \
			 parent that is not in the snapshot"
		);
		// A child whose OWN uuid has settled is still shielded by the invisible parent.
		let pending = HashSet::from([dir_uuid]);
		assert!(
			reconcile(
				SyncMode::LocalToRemote,
				&baseline,
				&local,
				&HashMap::new(),
				&pending
			)
			.is_empty()
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
		for mode in ALL_MODES {
			assert!(
				reconcile(mode, &baseline, &local, &remote, &HashSet::from([uuid])).is_empty(),
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
	fn conflicted_baseline_rows_are_skipped() {
		let uuid = Uuid::new_v4();
		let mut entry = base_file("a.txt", uuid, [0; 32]);
		entry.state = BaselineState::Conflicted;
		let baseline = map(vec![("a.txt", entry)]);
		let local = map(vec![("a.txt", local_file("a.txt", [1; 32]))]);
		let remote = map(vec![("a.txt", remote_file("a.txt", uuid, [0; 32]))]);
		assert!(
			plan(SyncMode::TwoWay, &baseline, &local, &remote).is_empty(),
			"a conflicted path is held, not re-acted on"
		);
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

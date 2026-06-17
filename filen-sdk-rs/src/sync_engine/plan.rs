//! The pure planning layer: build the remote-side view from a cache snapshot, then reconcile the
//! three inputs (baseline, local scan, remote view) into an ordered list of [`SyncAction`]s.
//!
//! `reconcile` is deliberately side-effect-free and synchronous so the whole decision matrix is
//! unit-testable without a filesystem or network. One-way modes converge the destination onto the
//! source (`make dst match src`, gated by whether deletions propagate); two-way uses the baseline
//! to tell which side changed and surfaces a genuine both-sides-changed divergence as a conflict.

use std::collections::{BTreeSet, HashMap};

use filen_types::crypto::Blake3Hash;
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

use super::{
	baseline::{BaselineEntry, BaselineState, NodeKind},
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
}

impl SyncAction {
	fn rel_path(&self) -> &str {
		match self {
			Self::CreateLocalDir { rel_path }
			| Self::DownloadFile { rel_path, .. }
			| Self::DeleteLocal { rel_path, .. }
			| Self::CreateRemoteDir { rel_path }
			| Self::UploadFile { rel_path }
			| Self::TrashRemote { rel_path, .. }
			| Self::Conflict { rel_path } => rel_path,
		}
	}

	fn is_delete(&self) -> bool {
		matches!(self, Self::DeleteLocal { .. } | Self::TrashRemote { .. })
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
		(Some(local), None) => actions.push(create_remote(rel_path, local.kind)),
		(Some(local), Some(remote)) => {
			if !remote_matches_local(local, remote, base) {
				if local.kind != remote.kind {
					// Type flip: trash the stale remote node, then create the new kind.
					actions.push(SyncAction::TrashRemote {
						rel_path: rel_path.to_string(),
						kind: remote.kind,
						remote_uuid: remote.remote_uuid,
					});
				}
				actions.push(create_remote(rel_path, local.kind));
			}
		}
		(None, Some(remote)) => {
			if delete_ok {
				actions.push(SyncAction::TrashRemote {
					rel_path: rel_path.to_string(),
					kind: remote.kind,
					remote_uuid: remote.remote_uuid,
				});
			}
		}
		(None, None) => {}
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
		(Some(remote), None) => actions.push(create_local(rel_path, remote)),
		(Some(remote), Some(local)) => {
			if !remote_matches_local(local, remote, base) {
				if local.kind != remote.kind {
					actions.push(SyncAction::DeleteLocal {
						rel_path: rel_path.to_string(),
						kind: local.kind,
					});
				}
				actions.push(create_local(rel_path, remote));
			}
		}
		(None, Some(local)) => {
			if delete_ok {
				actions.push(SyncAction::DeleteLocal {
					rel_path: rel_path.to_string(),
					kind: local.kind,
				});
			}
		}
		(None, None) => {}
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
				actions.push(SyncAction::Conflict {
					rel_path: rel_path.to_string(),
				});
			}
		}
	}
}

/// Reconcile a pair's three inputs into an ordered action plan. `baseline`/`local`/`remote` are all
/// keyed by the same NFC-normalized relative path.
pub(crate) fn reconcile(
	mode: super::SyncMode,
	baseline: &HashMap<String, BaselineEntry>,
	local: &HashMap<String, LocalNode>,
	remote: &HashMap<String, RemoteNode>,
) -> Vec<SyncAction> {
	let mut actions = Vec::new();

	let keys: BTreeSet<&str> = baseline
		.keys()
		.chain(local.keys())
		.chain(remote.keys())
		.map(String::as_str)
		.collect();

	for key in keys {
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
	actions
}

/// Order the plan so it applies safely: deletions first, child-before-parent (descending path),
/// then creates/transfers, parent-before-child (ascending path). A type-flip at one path emits a
/// trash (delete phase) before its create (create phase), which this ordering preserves.
fn order_actions(actions: &mut [SyncAction]) {
	actions.sort_by(|a, b| match (a.is_delete(), b.is_delete()) {
		(true, false) => std::cmp::Ordering::Less,
		(false, true) => std::cmp::Ordering::Greater,
		(true, true) => b.rel_path().cmp(a.rel_path()),
		(false, false) => a.rel_path().cmp(b.rel_path()),
	});
}

#[cfg(test)]
mod tests {
	use std::borrow::Cow;

	use chrono::{DateTime, Utc};

	use super::*;
	use crate::sync_engine::SyncMode;

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

	fn map<T>(items: Vec<(&str, T)>) -> HashMap<String, T> {
		items.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
	}

	#[test]
	fn local_to_remote_uploads_new_and_trashes_remote_only() {
		let local = map(vec![("a.txt", local_file("a.txt", [1; 32]))]);
		let stale = Uuid::new_v4();
		let remote = map(vec![("old.txt", remote_file("old.txt", stale, [2; 32]))]);
		let actions = reconcile(SyncMode::LocalToRemote, &HashMap::new(), &local, &remote);
		assert_eq!(
			actions,
			vec![
				SyncAction::TrashRemote {
					rel_path: "old.txt".to_string(),
					kind: NodeKind::File,
					remote_uuid: stale,
				},
				SyncAction::UploadFile {
					rel_path: "a.txt".to_string(),
				},
			],
			"deletes (trash) ordered before creates (upload)"
		);
	}

	#[test]
	fn local_backup_uploads_but_never_trashes() {
		let local = map(vec![("a.txt", local_file("a.txt", [1; 32]))]);
		let remote = map(vec![(
			"old.txt",
			remote_file("old.txt", Uuid::new_v4(), [2; 32]),
		)]);
		let actions = reconcile(SyncMode::LocalBackup, &HashMap::new(), &local, &remote);
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
		let actions = reconcile(SyncMode::RemoteToLocal, &HashMap::new(), &local, &remote);
		assert_eq!(
			actions,
			vec![
				SyncAction::DeleteLocal {
					rel_path: "extra.txt".to_string(),
					kind: NodeKind::File,
				},
				SyncAction::DownloadFile {
					rel_path: "r.txt".to_string(),
					remote_uuid: uuid,
				},
			]
		);
	}

	#[test]
	fn identical_content_produces_no_action() {
		let uuid = Uuid::new_v4();
		let local = map(vec![("a.txt", local_file("a.txt", [7; 32]))]);
		let remote = map(vec![("a.txt", remote_file("a.txt", uuid, [7; 32]))]);
		// Same hash on both sides, every mode: nothing to do.
		for mode in [
			SyncMode::LocalToRemote,
			SyncMode::RemoteToLocal,
			SyncMode::TwoWay,
			SyncMode::LocalBackup,
		] {
			assert!(
				reconcile(mode, &HashMap::new(), &local, &remote).is_empty(),
				"{mode:?} should be a no-op for identical content"
			);
		}
	}

	#[test]
	fn create_actions_order_parents_before_children() {
		let local = map(vec![
			("dir/sub/c.txt", local_file("dir/sub/c.txt", [1; 32])),
			("dir", local_dir("dir")),
			("dir/sub", local_dir("dir/sub")),
		]);
		let actions = reconcile(
			SyncMode::LocalToRemote,
			&HashMap::new(),
			&local,
			&HashMap::new(),
		);
		let paths: Vec<_> = actions.iter().map(|a| a.rel_path()).collect();
		assert_eq!(paths, vec!["dir", "dir/sub", "dir/sub/c.txt"]);
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
	fn two_way_pushes_local_edit_and_pulls_remote_edit() {
		let uuid = Uuid::new_v4();
		// Local edited (hash diverged from baseline), remote unchanged -> push.
		let baseline = map(vec![("a.txt", base_file("a.txt", uuid, [0; 32]))]);
		let local = map(vec![("a.txt", local_file("a.txt", [1; 32]))]);
		let remote = map(vec![("a.txt", remote_file("a.txt", uuid, [0; 32]))]);
		assert_eq!(
			reconcile(SyncMode::TwoWay, &baseline, &local, &remote),
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
			reconcile(SyncMode::TwoWay, &baseline, &local, &remote),
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
			reconcile(SyncMode::TwoWay, &baseline, &local, &remote),
			vec![SyncAction::Conflict {
				rel_path: "a.txt".to_string(),
			}]
		);

		// Both edited to the SAME content -> converged, no action.
		let local = map(vec![("a.txt", local_file("a.txt", [9; 32]))]);
		let remote = map(vec![("a.txt", remote_file("a.txt", new_uuid, [9; 32]))]);
		assert!(
			reconcile(SyncMode::TwoWay, &baseline, &local, &remote).is_empty(),
			"identical concurrent edits converge without a conflict"
		);
	}

	#[test]
	fn two_way_propagates_a_local_delete_to_the_remote() {
		let uuid = Uuid::new_v4();
		let baseline = map(vec![("a.txt", base_file("a.txt", uuid, [0; 32]))]);
		let remote = map(vec![("a.txt", remote_file("a.txt", uuid, [0; 32]))]);
		// Local deleted (absent), remote unchanged -> trash remote.
		let actions = reconcile(SyncMode::TwoWay, &baseline, &HashMap::new(), &remote);
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
			reconcile(SyncMode::TwoWay, &baseline, &local, &remote).is_empty(),
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

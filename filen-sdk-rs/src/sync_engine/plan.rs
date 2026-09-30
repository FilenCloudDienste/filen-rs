//! The pure planning layer: build the remote-side view from a cache snapshot, then reconcile the
//! three inputs (baseline, local scan, remote view) into an ordered list of [`SyncAction`]s.
//!
//! `reconcile` is deliberately side-effect-free and synchronous so the whole decision matrix is
//! unit-testable without a filesystem or network. One-way modes converge the destination onto the
//! source (`make dst match src`, gated by whether deletions propagate); two-way uses the baseline
//! to tell which side changed and surfaces a genuine both-sides-changed divergence as a conflict.

use std::{
	borrow::Cow,
	cmp::Reverse,
	collections::{BTreeMap, BTreeSet, HashMap, HashSet, btree_map},
};

use filen_types::{crypto::Blake3Hash, fs::StableUuid};
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

use super::{
	baseline::{BaselineEntry, BaselineState, NodeKind, SyncedPaths},
	events::SyncEvent,
	ignore::{IgnoreDecision, IgnoreLevel, IgnoreRules},
	outcome::{UnsyncablePath, UnsyncableReason},
	rows::Baseline,
	scan::{LocalNode, QUARANTINE_DIR, collision_hash, collision_key},
	side::{Nodes, NodesAt, Side},
};
use crate::cache::{RemoteItem, UndecodableItem};

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
pub(super) fn is_safe_name(name: &str) -> bool {
	!name.is_empty()
		&& name != "."
		&& name != ".."
		&& !name.contains('/')
		&& !name.contains('\\')
		&& !name.contains('\0')
}

/// Why [`resolve_parent`] could not place a directory under the sync root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unresolved {
	/// An ancestor — the directory with this uuid — is itself left out of the view (an unsafe name,
	/// an undecodable directory, or a broken chain further up). The item is recorded under that
	/// ancestor's path and reason.
	UnderSkipped(Uuid),
	/// The immediate parent is nowhere in the snapshot, or the chain runs past
	/// [`MAX_REMOTE_DEPTH`] (a malformed cycle).
	BrokenParent,
}

/// Resolve the `/`-joined, NFC-normalized path of the directory `parent` by walking the dir index
/// up to `root` (`""` for the root itself). Fails when the chain passes a directory the view leaves
/// out — one whose name is not a safe path component, so a traversal name like `..` never enters
/// the plan, or one listed in `undecodable_dirs` — or never reaches `root`.
fn resolve_parent(
	parent: Uuid,
	root: Uuid,
	dir_index: &HashMap<Uuid, (String, Uuid)>,
	undecodable_dirs: &HashSet<Uuid>,
) -> Result<String, Unresolved> {
	let mut parts: Vec<&str> = Vec::new();
	// The directory the walk stepped up from, whose own parent `current` is.
	let mut below: Option<Uuid> = None;
	let mut current = parent;
	while current != root {
		if parts.len() >= MAX_REMOTE_DEPTH {
			return Err(Unresolved::BrokenParent);
		}
		if undecodable_dirs.contains(&current) {
			return Err(Unresolved::UnderSkipped(current));
		}
		let Some((name, grandparent)) = dir_index.get(&current) else {
			return Err(match below {
				// That directory's parent is missing: it is the one recorded as broken.
				Some(orphan) => Unresolved::UnderSkipped(orphan),
				None => Unresolved::BrokenParent,
			});
		};
		if !is_safe_name(name) {
			return Err(Unresolved::UnderSkipped(current));
		}
		parts.push(name);
		below = Some(current);
		current = *grandparent;
	}
	parts.reverse();
	Ok(parts.join("/"))
}

pub(super) fn join_path(parent_path: &str, name: &str) -> String {
	if parent_path.is_empty() {
		name.to_string()
	} else {
		format!("{parent_path}/{name}")
	}
}

/// Whether `rel_path` is the local quarantine dir or inside it. The local scan never lists it, so
/// the remote view leaves out a remote folder that happens to carry the name too — it is never
/// mistaken for (or synced into) the quarantine area.
pub(super) fn in_quarantine(rel_path: &str) -> bool {
	rel_path == QUARANTINE_DIR || rel_path.starts_with(&format!("{QUARANTINE_DIR}/"))
}

/// The remote view of a sync root's subtree, plus whether it is safe to reconcile against.
#[derive(Debug)]
pub(crate) struct RemoteView {
	pub(crate) nodes: Side<RemoteNode>,
	/// `true` if two distinct remote items resolved to the same case-insensitive path but NOT the
	/// same byte-identical one. The engine refuses to reconcile such a pair (a 1:1 local mapping is
	/// impossible) until the user cleans it up.
	pub(crate) has_collisions: bool,
	/// Paths two byte-identically named remote items resolved to. The server never allows that —
	/// its dedup is on the lowercased name hash — so it can only be a cache mid-transition: a
	/// re-upload whose successor has been applied while the predecessor's trash has not. Only that
	/// one path is held back; the pass runs.
	pub(crate) held_paths: BTreeSet<String>,
	/// Remote items that exist but are NOT in `nodes`, and why. Their absence from `nodes` is no
	/// evidence of a deletion (see [`unknown_remote_paths`]).
	pub(crate) skipped: Vec<SkippedRemote>,
	/// The top-most items the ignore rules hide, with the deciding rule. Neither they nor anything
	/// under them is in `nodes` or `skipped`, or checked for collisions.
	pub(crate) ignored: BTreeMap<String, IgnoreDecision>,
	/// How many items only the BUILT-IN defaults hid with no baseline row at or under them — left
	/// out of `ignored` for the reason [`LocalScan::ignored_default_untracked`](super::scan::LocalScan::ignored_default_untracked)
	/// gives. Counted per item rather than per root, since it is logged and never reported.
	///
	/// A LOG LINE, not a fact about the tree, and the only field here that a change-scoped pass
	/// under-reports on purpose: [`filter_changed`](RemoteView::filter_changed) asks the rules about
	/// the decided paths alone, so it counts hits among those and not the whole view. Anything that
	/// ever needs the real number has to ask the rules about every node, which is the scan that
	/// narrowing removed.
	pub(crate) ignored_default_untracked: usize,
}

/// The rules a view is filtered with, and the baseline that says which of their hits are roots.
///
/// The two travel together because neither answers alone: an ignored path is only a ROOT the pass
/// records and untracks when a row sits at or under it, unless a level above the built-in defaults
/// hid it (see [`Baseline::tracked`]). A view built with no filter has no ignored roots and needs
/// no baseline.
#[derive(Clone, Copy)]
pub(crate) struct ViewFilter<'a> {
	pub(crate) rules: &'a IgnoreRules,
	pub(crate) baseline: &'a Baseline,
}

/// A remote item the snapshot holds that the view could not place at a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkippedRemote {
	pub(crate) remote_uuid: Uuid,
	/// The file's whole-life id; `None` for a directory.
	pub(crate) stable_uuid: Option<StableUuid>,
	/// Where the item would sit: its parent's path joined with its name. For an undecodable item,
	/// whose name is unknown, the parent's path (`""` for the sync root); for a broken parent chain,
	/// which has no path at all, the bare name. An item under a skipped directory carries that
	/// directory's path and reason.
	pub(crate) rel_path: String,
	/// Whether [`rel_path`](Self::rel_path) names a DIRECTORY — a fact about the path this record
	/// reports, not about the item that owns the record, and the two differ. An undecodable item is
	/// recorded at the directory that holds it, and an item under a skipped directory takes that
	/// directory's path, so both are directory paths whatever the item itself is; only an item
	/// recorded at its own would-be path carries its own kind.
	///
	/// [`RemoteView::filter`] asks the ignore rules about this path, and a rule written for
	/// directories only (`build/`) answers differently depending on which it is asked about.
	pub(crate) path_is_dir: bool,
	pub(crate) reason: UnsyncableReason,
}

/// Builds a [`RemoteView`] out of a cache subtree snapshot's items AS THEY ARRIVE, so the whole
/// subtree never exists as a `Vec` beside the view built from it. Names are NFC-normalized so
/// they key 1:1 against the local scan.
///
/// An item that cannot be placed — an unsafe name, a broken parent chain, or metadata the cache
/// could not decode (the undecodable records, which may hold other roots' too: only those whose
/// parent resolves under `root` are this view's) — is left out of `nodes` and recorded in
/// [`RemoteView::skipped`]. A descendant of a skipped directory is left out with it and recorded
/// under that directory's path and reason: its own uuid is still what a synced item moved beneath
/// the directory is found by (see [`unknown_remote_paths`]).
///
/// This is the view a pass reads the remote AS IT IS from: what the snapshot confirms, which
/// remote rule files there are, which of this engine's own writes the cache has caught up to, and
/// whether the remote came back emptied are facts about the remote whatever the rules hide. Those
/// reads come first of necessity — the rules are not known until the local scan has read the
/// `.filenignore` files on disk — and then [`RemoteView::filter`] hides what they hide IN PLACE,
/// leaving the one map reconcile, the folds and apply act on.
///
/// The rows may arrive in any order (the read's statement promises none). An item whose ancestry
/// has not arrived yet is set aside and placed by [`finish`](Self::finish), when the index is
/// complete and every answer is final — so the view does not depend on the order, and a read that
/// happens to hand over parents first sets nothing aside at all.
pub(crate) struct ViewBuilder {
	root: Uuid,
	/// uuid -> (NFC name, parent) for every directory seen so far, which is what resolves a path.
	dir_index: HashMap<Uuid, (String, Uuid)>,
	/// Held until [`finish`](Self::finish): resolving an undecodable record's path needs the
	/// COMPLETE index, since the record is placed at the directory that holds it.
	undecodable: Vec<UndecodableItem>,
	/// The undecodable DIRECTORIES, which placement needs from the first item on: nothing under
	/// one can be placed, and the read hands them over before any item.
	undecodable_dirs: HashSet<Uuid>,
	/// A plain map, not a [`Side`]: a builder is what a WHOLE read produces, so there is no
	/// baseline to derive from and every placement has to know what it displaced.
	nodes: HashMap<String, RemoteNode>,
	held_paths: BTreeSet<String>,
	skipped: Skipped,
	/// Items whose ancestry the index could not answer for when they arrived (see the type doc).
	deferred: Vec<(bool, RemoteItem)>,
}

impl ViewBuilder {
	/// A builder for `root`'s subtree, sized for a tree of `items`. A pass knows its baseline's
	/// row count, which is the size of a converged pair's view — and a map that never grows is
	/// one that never holds its old table and its new one at once.
	pub(crate) fn with_capacity(root: Uuid, items: usize) -> Self {
		Self {
			root,
			dir_index: HashMap::new(),
			undecodable: Vec::new(),
			undecodable_dirs: HashSet::new(),
			nodes: HashMap::with_capacity(items),
			held_paths: BTreeSet::new(),
			skipped: Skipped::default(),
			deferred: Vec::new(),
		}
	}

	/// The records the cache could not decode, taken before the first item (see
	/// [`SnapshotSink`](crate::cache::SnapshotSink)).
	fn take_undecodable(&mut self, items: Vec<UndecodableItem>) {
		self.undecodable_dirs = items
			.iter()
			.filter(|item| item.stable_uuid.is_none())
			.map(|item| item.uuid)
			.collect();
		self.undecodable = items;
	}

	/// Index `item` if it is a directory, then place it at its path or record why it has none.
	///
	/// `defer` is set while the read is still running, where an unresolved chain means one of two
	/// things the arriving item cannot tell apart: an ancestor the read has not reached, or an
	/// ancestor that is genuinely missing, unsafely named or undecodable. So it is set aside, and
	/// [`finish`](Self::finish) asks again with the index complete, where the answer is final. An
	/// item the index CAN answer for is answered once: a later row adds entries to the index, and
	/// adding entries cannot change a walk that already found every step it needed.
	fn place(&mut self, is_dir: bool, item: &RemoteItem, defer: bool) {
		let name = item.name.nfc().collect::<String>();
		if is_dir {
			self.dir_index
				.insert(item.uuid, (name.clone(), item.parent));
		}
		// A whole-life id is a file's; a directory's own uuid already survives its renames. Taken
		// from the KIND rather than from the field, which a directory is merely expected to leave
		// empty.
		let stable_uuid = if is_dir { None } else { item.stable_uuid };
		let skip = match resolve_parent(
			item.parent,
			self.root,
			&self.dir_index,
			&self.undecodable_dirs,
		) {
			Ok(parent_path) if is_safe_name(&name) => {
				self.insert(join_path(&parent_path, &name), is_dir, item, stable_uuid);
				return;
			}
			Ok(parent_path) => SkippedRemote {
				remote_uuid: item.uuid,
				stable_uuid,
				rel_path: join_path(&parent_path, &name),
				path_is_dir: is_dir,
				reason: UnsyncableReason::RemoteInvalidName { name },
			},
			Err(_) if defer => {
				self.deferred.push((is_dir, item.clone()));
				return;
			}
			Err(Unresolved::UnderSkipped(ancestor)) => {
				self.skipped.under(item.uuid, stable_uuid, ancestor);
				return;
			}
			Err(Unresolved::BrokenParent) => SkippedRemote {
				remote_uuid: item.uuid,
				stable_uuid,
				rel_path: name,
				path_is_dir: is_dir,
				reason: UnsyncableReason::RemoteBrokenParent,
			},
		};
		self.skipped.record(skip);
	}

	/// The node at `rel_path`, unless the path is the engine's own quarantine directory or a
	/// second item has already claimed it.
	fn insert(
		&mut self,
		rel_path: String,
		is_dir: bool,
		item: &RemoteItem,
		stable_uuid: Option<StableUuid>,
	) {
		// The engine's own directory comes before every rule, and is never reported as ignored.
		if in_quarantine(&rel_path) {
			return;
		}
		// Byte-identical names under one parent cannot exist on the server, so a second item at
		// this exact path can only be the cache showing both halves of a re-upload at once: the
		// successor has been applied and the predecessor's trash has not. Withhold that one path
		// for this pass rather than refusing the whole one; the next snapshot has one of them.
		if self.held_paths.contains(&rel_path) {
			return;
		}
		let node = RemoteNode {
			rel_path: rel_path.clone(),
			kind: if is_dir {
				NodeKind::Dir
			} else {
				NodeKind::File
			},
			remote_uuid: item.uuid,
			stable_uuid,
			content_hash: if is_dir { None } else { item.hash },
			size: if is_dir { 0 } else { item.size },
			modified_millis: item.modified_millis,
		};
		if self.nodes.insert(rel_path.clone(), node).is_some() {
			tracing::debug!(
				"remote view: holding {rel_path:?} — the cache is mid-transition, listing two items under that exact name"
			);
			self.nodes.remove(&rel_path);
			self.held_paths.insert(rel_path);
		}
	}

	/// The view, once everything that needed the WHOLE subtree has been answered: the undecodable
	/// records, whose path is the directory that holds them, the items that arrived before their
	/// ancestry, and the descendants of a skipped directory, which take that directory's record.
	pub(crate) fn finish(mut self) -> RemoteView {
		for item in std::mem::take(&mut self.undecodable) {
			match resolve_parent(
				item.parent,
				self.root,
				&self.dir_index,
				&self.undecodable_dirs,
			) {
				Ok(rel_path) => self.skipped.record_first(SkippedRemote {
					remote_uuid: item.uuid,
					stable_uuid: item.stable_uuid,
					rel_path,
					// The holding directory's path: an undecodable item's own name is what the
					// cache could not read.
					path_is_dir: true,
					reason: UnsyncableReason::RemoteUndecodable,
				}),
				Err(Unresolved::UnderSkipped(ancestor)) => {
					self.skipped.under(item.uuid, item.stable_uuid, ancestor);
				}
				// Not under this root at all: another root's record.
				Err(Unresolved::BrokenParent) => {}
			}
		}
		for (is_dir, item) in std::mem::take(&mut self.deferred) {
			self.place(is_dir, &item, false);
		}
		RemoteView {
			nodes: self.nodes.into(),
			has_collisions: false,
			held_paths: self.held_paths,
			skipped: self.skipped.finish(),
			ignored: BTreeMap::new(),
			ignored_default_untracked: 0,
		}
	}
}

impl crate::cache::SnapshotSink for ViewBuilder {
	fn undecodable(&mut self, items: Vec<UndecodableItem>) {
		self.take_undecodable(items);
	}

	fn item(&mut self, is_dir: bool, item: RemoteItem) {
		self.place(is_dir, &item, true);
	}
}

/// [`ViewBuilder`] over a snapshot that is already materialized: the shape the tests and the
/// probe read, and the one a pass read before it learned to place rows as they arrive. A pass
/// itself no longer has the two slices to hand — it never builds them — so this is gated with
/// the read that does.
#[cfg(any(test, feature = "bench-internals"))]
pub(crate) fn place_remote_items(
	root: Uuid,
	dirs: &[RemoteItem],
	files: &[RemoteItem],
	undecodable: &[UndecodableItem],
) -> RemoteView {
	let mut builder = ViewBuilder::with_capacity(root, dirs.len() + files.len());
	builder.take_undecodable(undecodable.to_vec());
	// Set aside and answered at `finish` exactly as a streamed row is: these slices are in no
	// more of an order than the read's rows are, and a directory that follows its own children
	// must not read as one with no parent.
	for dir in dirs {
		builder.place(true, dir, true);
	}
	for file in files {
		builder.place(false, file, true);
	}
	builder.finish()
}

impl RemoteView {
	/// Hide what `filter`'s rules hide, then resolve the name collisions among what is left — the
	/// two steps that turn a placed view ([`place_remote_items`]) into the set a pass reconciles,
	/// folds and applies against.
	///
	/// The rules come off first, so ignored case-twins never refuse a pass. With no filter nothing
	/// is hidden and the view stays the remote as it is, collisions resolved.
	///
	/// The two halves are `pub(super)` so the probe can time them apart — they are the largest
	/// per-node cost a change-scoped pass has, and one figure for both cannot say which of them a
	/// narrowing moved. THIS is the order; a caller that runs them itself mirrors it.
	pub(crate) fn filter(&mut self, filter: Option<ViewFilter<'_>>) {
		// ONE tree for both halves. The ignore half reads the side through `filter.baseline` and
		// the collision half walks the rows of the tree it is handed; taking them from two places
		// would let a caller derive the ignore decisions from one tree and the collision keys from
		// another, and the view would be one neither tree describes. `filter_changed` has always
		// read both off the filter, and this now does too.
		match filter {
			Some(filter) => {
				self.hide(filter, PassPaths::Whole);
				self.resolve_collisions(filter.baseline);
			}
			// No rules, so nothing to hide and no carried side to read: the collision check has
			// only the placed nodes to fold, and an empty tree is the whole of what it needs.
			None => self.resolve_collisions(&Baseline::default()),
		}
	}

	/// [`filter`](Self::filter) for a view a change-scoped pass DERIVED, which asks the rules only
	/// about the paths that pass decided.
	///
	/// The narrowing rests on the rules standing still. Every level of them forces a whole read
	/// when it changes — a `.filenignore` on either side, the user patterns, the pair's mode, all
	/// [`FullPassReason::RulesChanged`](super::changes::FullPassReason::RulesChanged) — so a
	/// scoped pass runs under exactly the rules its predecessor ran under, and a node it CARRIED
	/// from a baseline row is a node those same rules already let through. What is left to ask
	/// about is what moved: `decided` is the exact set of keys this pass's producers put somewhere
	/// other than the node its row carries, which is the same set the reconcile is driven from.
	///
	/// The rows that are the exception — rows left at hidden paths by an `untrack_ignored` whose
	/// subtree delete failed — stay in the view here where the whole form would drop them. That is
	/// the safe direction, and deliberately so: their LOCAL half is carried from the same row, so
	/// the pair reads as converged and the narrowed reconcile, which visits only `decided`, plans
	/// nothing at them. Dropping the remote half alone is what would read as a local deletion.
	///
	/// [`ignored`](Self::ignored) therefore comes back holding only the roots among `decided`. A
	/// root an earlier pass found goes on being reported and untracked without this pass re-deriving
	/// it, because `PairFacts::merge_remote_view` EXTENDS the carried ignored roots with this set
	/// rather than replacing them. Its PRUNE is the half that needs the care, and the reason that
	/// merge takes a baseline: it drops the whole subtree under each touched path, so a carried root
	/// strictly under one would be dropped on the strength of a view that never looked there. It
	/// re-supplies such a root where the baseline still names a row at or under it, and drops one
	/// with no rows left under it exactly as it did before this narrowing.
	pub(crate) fn filter_changed(&mut self, filter: ViewFilter<'_>, decided: &BTreeSet<String>) {
		let kept = self.hide(filter, PassPaths::Changed(decided));
		self.resolve_collisions_changed(filter.baseline, kept);
	}

	/// The ignore half of [`filter`](Self::filter): an item the rules hide at or above its path
	/// leaves `nodes` — everything under it with it, since nothing under an ignored directory can
	/// be re-included — and its top-most ignored path is recorded in
	/// [`ignored`](Self::ignored). An item the view could not place under an ignored path leaves
	/// [`skipped`](Self::skipped) too: it is out of sync, not unsyncable.
	///
	/// A [held](Self::held_paths) path stays held whatever the rules say. Both halves are
	/// byte-identical, so a rule hides them together, and a path the rules hide is blocked from
	/// every action anyway — holding it costs the pass nothing and it is untracked, with its rule
	/// reported, by the pass that finds the cache no longer mid-transition there.
	///
	/// `paths` says which nodes to ASK the rules about: every one of them
	/// ([`PassPaths::Whole`], what a pass that read the remote whole owes), or only the keys a
	/// change-scoped pass decided (see [`filter_changed`](Self::filter_changed)).
	///
	/// A change-scoped pass gets back the decided keys the view still holds, in the set's order:
	/// what [`resolve_collisions_changed`](Self::resolve_collisions_changed) folds. Asking the
	/// rules about a key needs its node, and on a carried side that is a read of its row, which
	/// already says whether the view holds the key — the collision check asking again read every
	/// decided row twice. A whole pass has no decided set, and gets nothing back.
	pub(super) fn hide<'d>(
		&mut self,
		filter: ViewFilter<'_>,
		paths: PassPaths<'d>,
	) -> Vec<&'d String> {
		let mut memo = HashMap::new();
		let mut ignored = BTreeMap::new();
		let mut untracked = 0usize;
		// Whether the rules hide `rel_path`, recording the root of what they hide as the pass
		// reports and untracks one (see [`Baseline::tracked`]).
		let mut hidden = |rel_path: &str, is_dir: bool| {
			let Some((ignored_root, decision)) =
				filter.rules.ignored_root(rel_path, is_dir, &mut memo)
			else {
				return false;
			};
			// A root the rules hid ABOVE this item is a directory; the item's own hit is a root of
			// whatever kind the item is.
			let root_is_dir = ignored_root != rel_path || is_dir;
			// Hidden either way — but only a tracked default hit is a ROOT (see `Baseline::tracked`).
			if decision.level == IgnoreLevel::Default
				&& !filter.baseline.tracked(ignored_root, root_is_dir)
			{
				untracked += 1;
			} else if !ignored.contains_key(ignored_root) {
				ignored.insert(ignored_root.to_owned(), decision);
			}
			true
		};
		let kept = match paths {
			PassPaths::Whole => {
				self.nodes.retain(filter.baseline, |rel_path, node| {
					!hidden(rel_path, node.kind == NodeKind::Dir)
				});
				Vec::new()
			}
			PassPaths::Changed(decided) => {
				let mut kept = Vec::new();
				let mut roots = Vec::new();
				for rel_path in decided {
					let Some(is_dir) = self
						.nodes
						.of(filter.baseline)
						.at(rel_path)
						.map(|node| node.kind == NodeKind::Dir)
					else {
						continue;
					};
					if hidden(rel_path, is_dir) {
						roots.push(rel_path);
					} else {
						kept.push(rel_path);
					}
				}
				for root in &roots {
					// Everything under a hidden directory goes with it, exactly as the whole form
					// drops it: nothing under an ignored directory can be re-included.
					//
					// That costs a subtree walk of the side per hit, where the whole form pays one
					// walk for the pass. Only a decided path that the rules hide pays it, which is
					// a remote create or move INTO an already-ignored directory and nothing else —
					// a rule that newly hides a directory forces a whole read instead. If that ever
					// stops being rare, the subtree is also named key-by-key in `decided` (every
					// re-key records both ends), so the walk can be dropped for a scan of the set.
					for under in self.nodes.subtree_paths(filter.baseline, root) {
						self.nodes.remove(filter.baseline, &under);
					}
					self.nodes.remove(filter.baseline, root);
				}
				// A kept key under a hidden directory left with it. Asked again only when the rules
				// hid something, which is the rare case the walk above already pays for.
				if !roots.is_empty() {
					kept.retain(|rel_path| self.nodes.of(filter.baseline).holds(rel_path));
				}
				kept
			}
		};
		self.ignored = ignored;
		self.ignored_default_untracked = untracked;
		// An unplaceable item's own record leaves with whatever hides it: the holding directory for
		// an undecodable one, whose name is unknown, and the would-be path for a name the remote
		// spells unsafely. A broken parent chain has no path to ask about. ASKED, not recorded —
		// the root of what hides such an item is the directory the rules hid, which is a node and
		// recorded as one; the item itself was never a root of its own.
		//
		// Asked about the kind the record's PATH is
		// ([`path_is_dir`](SkippedRemote::path_is_dir)), not the kind the item is: a file recorded
		// under a skipped directory carries that directory's path, and a rule written for
		// directories only (`build/`) hides the path it names or nothing at all. Asked as a file,
		// such a rule missed, the record outlived the directory it describes, and the pass reported
		// the very path the rule was written to silence.
		let mut is_hidden = |rel_path: &str, is_dir: bool| {
			filter
				.rules
				.ignored_root(rel_path, is_dir, &mut memo)
				.is_some()
		};
		self.skipped.retain(|skip| match &skip.reason {
			UnsyncableReason::RemoteUndecodable | UnsyncableReason::RemoteInvalidName { .. } => {
				!is_hidden(&skip.rel_path, skip.path_is_dir)
			}
			_ => true,
		});
		kept
	}

	/// The collision half of [`filter`](Self::filter): two remote items whose paths fold together
	/// case-insensitively have no 1:1 local mapping, so the loser leaves the view and the pass is
	/// refused ([`has_collisions`](Self::has_collisions)).
	pub(super) fn resolve_collisions(&mut self, baseline: &Baseline) {
		// A digest of every collision key taken so far. The keys themselves are not kept: a hit is
		// rare and is resolved against the paths already placed, which is what tells a real
		// case-twin from two keys that merely share a digest — a pass is never refused over that.
		let mut claimed: HashSet<u128> =
			HashSet::with_capacity(self.nodes.of(baseline).len() + self.held_paths.len());
		// A held path took its key when the view held it, so a case-variant of one still collides.
		claimed.extend(
			self.held_paths
				.iter()
				.map(|path| collision_hash(&collision_key(path))),
		);
		let mut clashes: Vec<String> = Vec::new();
		for rel_path in self.nodes.of(baseline).paths() {
			if !claimed.insert(collision_hash(&collision_key(&rel_path))) {
				clashes.push(rel_path.into_owned());
			}
		}
		for rel_path in clashes {
			let key = collision_key(&rel_path);
			// Which item already folded that way — asked on the error path only. A held path
			// counts: both halves of such a name were taken back out of `nodes`.
			let folds_like = |taken: &str| taken != rel_path && collision_key(taken) == key;
			let folds_onto = self
				.nodes
				.of(baseline)
				.paths()
				.any(|taken| folds_like(&taken))
				|| self.held_paths.iter().any(|taken| folds_like(taken));
			if folds_onto {
				tracing::debug!(
					"remote view: {rel_path:?} folds onto another remote item's name, so no 1:1 local mapping exists"
				);
				self.has_collisions = true;
				self.nodes.remove(baseline, &rel_path);
			}
		}
	}

	/// [`resolve_collisions`](Self::resolve_collisions) for a view a change-scoped pass DERIVED,
	/// which folds only the keys that pass decided.
	///
	/// The pass's CARRIED keys are not folded against each other, and that is not a gap. A pass
	/// that finds a collision REFUSES, and a refusal forces the next pass to read both sides whole
	/// (`next_pass_scope`'s `FullPassReason::PreviousRefusal`). So every pass since the last whole
	/// read folded exactly the keys it could itself have introduced — its decided set, which is
	/// what this does — and by induction no two carried keys fold together undetected. The whole
	/// read that starts the chain folded every one of them.
	///
	/// Each decided key is folded against three things, which together are every other key the view
	/// can hold: the other decided keys, the held paths (a held path took its key when the view
	/// held it), and the carried keys — asked of the BASELINE, whose rows are what a carried node
	/// is, so the question costs the path's own depth instead of a scan
	/// ([`Baseline::folded_row_paths`]).
	///
	/// A row the baseline names is only a collision where the view still HOLDS it: the delta may
	/// have detached that node, and refusing a pass over a row nothing is reconciling against
	/// would stall the pair for as long as the row lasted. Real keys are compared rather than
	/// digests of them, which the whole form uses to keep one `u128` per node instead of a second
	/// copy of every path — the decided set is a handful of paths, so there is nothing to save.
	///
	/// `kept` is the decided keys the view holds, in path order: what [`hide`](Self::hide) hands
	/// back, having read each one's node for the rules already. A decided key the view does not
	/// hold claims no name, and the order is the order the kept ones claim theirs in.
	/// `pub(super)` for the same reason [`hide`](Self::hide) is: the probe times the two halves of
	/// the scoped filter apart.
	pub(super) fn resolve_collisions_changed<'d>(
		&mut self,
		baseline: &Baseline,
		kept: impl IntoIterator<Item = &'d String>,
	) {
		let mut claimed: HashMap<String, String> = HashMap::new();
		let mut clashes: Vec<String> = Vec::new();
		for rel_path in kept {
			let key = collision_key(rel_path);
			let folds_like = |taken: &str| taken != rel_path && collision_key(taken) == key;
			let onto = claimed
				.get(&key)
				.filter(|taken| folds_like(taken))
				.cloned()
				.or_else(|| {
					self.held_paths
						.iter()
						.find(|held| folds_like(held))
						.cloned()
				})
				.or_else(|| {
					baseline
						.folded_row_paths(rel_path)
						.into_iter()
						.find(|row| folds_like(row) && self.nodes.of(baseline).holds(row))
				});
			if let Some(onto) = onto {
				tracing::debug!(
					"remote view: {rel_path:?} folds onto {onto:?}, so no 1:1 local mapping exists"
				);
				clashes.push(rel_path.clone());
			} else {
				claimed.insert(key, rel_path.clone());
			}
		}
		for rel_path in clashes {
			self.has_collisions = true;
			self.nodes.remove(baseline, &rel_path);
		}
	}
}

/// [`place_remote_items`] + [`RemoteView::filter`] in one call, for a caller that already has the
/// rules to hand. A pass builds the two halves apart, so the reads it owes the unfiltered view can
/// happen in between.
#[cfg(test)]
pub(crate) fn build_remote_view(
	root: Uuid,
	dirs: &[RemoteItem],
	files: &[RemoteItem],
	undecodable: &[UndecodableItem],
	filter: Option<ViewFilter<'_>>,
) -> RemoteView {
	let mut view = place_remote_items(root, dirs, files, undecodable);
	view.filter(filter);
	view
}

/// The skipped items of a view being built: the ones recorded with a reason of their own, and the
/// ones under a skipped directory, which take that directory's record once every item is placed.
#[derive(Default)]
struct Skipped {
	/// The records that belong at the HEAD of the answer: the undecodable ones, which a
	/// materialized build resolved before it placed anything and a streamed build can only
	/// resolve once every directory has arrived. Keeping them apart is what makes the two builds
	/// answer identically, in the same order, however the rows came in.
	first: Vec<SkippedRemote>,
	recorded: Vec<SkippedRemote>,
	/// Every item under a skipped directory: its uuid, whole-life id, and the nearest skipped
	/// ancestor `resolve_parent` found.
	under: Vec<(Uuid, Option<StableUuid>, Uuid)>,
}

impl Skipped {
	/// Record an item with its own reason — unless it sits in the quarantine dir, which the view
	/// leaves out without a word (and its descendants with it).
	fn record(&mut self, skip: SkippedRemote) {
		if in_quarantine(&skip.rel_path) {
			return;
		}
		self.recorded.push(skip);
	}

	/// [`record`](Self::record) for a record that belongs at the head of the answer (see
	/// [`first`](Self::first)).
	fn record_first(&mut self, skip: SkippedRemote) {
		if in_quarantine(&skip.rel_path) {
			return;
		}
		self.first.push(skip);
	}

	fn under(&mut self, remote_uuid: Uuid, stable_uuid: Option<StableUuid>, ancestor: Uuid) {
		self.under.push((remote_uuid, stable_uuid, ancestor));
	}

	/// Resolve every item under a skipped directory to the record of the directory that has one: the
	/// nearest skipped ancestor may itself sit under another, so the walk climbs, one skipped
	/// directory per step. An item whose chain ends in nothing recorded (the quarantine dir) stays
	/// out unrecorded.
	fn finish(mut self) -> Vec<SkippedRemote> {
		let mut recorded = std::mem::take(&mut self.first);
		recorded.append(&mut self.recorded);
		// uuid -> its index in `recorded`, built once the two halves are in their final order.
		// The records this loop appends are not in it, exactly as they were not before: an item
		// under a skipped directory is not itself a directory anything sits under.
		let by_uuid: HashMap<Uuid, usize> = recorded
			.iter()
			.enumerate()
			.map(|(index, record)| (record.remote_uuid, index))
			.collect();
		let parent_of: HashMap<Uuid, Uuid> = self
			.under
			.iter()
			.map(|&(uuid, _, ancestor)| (uuid, ancestor))
			.collect();
		for &(remote_uuid, stable_uuid, ancestor) in &self.under {
			let mut at = ancestor;
			for _ in 0..=MAX_REMOTE_DEPTH {
				if let Some(&index) = by_uuid.get(&at) {
					let record = &recorded[index];
					let (rel_path, reason) = (record.rel_path.clone(), record.reason.clone());
					recorded.push(SkippedRemote {
						remote_uuid,
						stable_uuid,
						rel_path,
						// `under` is only reached through `Unresolved::UnderSkipped`, whose ancestor
						// is always a DIRECTORY — an unsafe-named one, an undecodable one, or one
						// whose own parent is missing — and a directory's record is either its own
						// would-be path or the directory that holds it. So the path this record
						// takes names a directory whatever the item under it is, and the rules have
						// to be asked about it as one.
						path_is_dir: true,
						reason,
					});
					break;
				}
				match parent_of.get(&at) {
					Some(&up) => at = up,
					None => break,
				}
			}
		}
		recorded
	}
}

/// The remote ids [`unknown_remote_paths`] will ask about, for the caller to resolve against the
/// store first (see [`SyncedPaths`]).
///
/// The ONE place that enumeration lives. A caller that resolved a smaller set would hand
/// `unknown_remote_paths` an id it cannot answer for, and an unanswerable id reads as "never
/// synced" — which drops the path's protection and lets the pass plan a local delete over a remote
/// item that is still there.
pub(crate) fn skipped_ids(skipped: &[SkippedRemote]) -> (Vec<Uuid>, Vec<StableUuid>) {
	(
		skipped.iter().map(|item| item.remote_uuid).collect(),
		skipped.iter().filter_map(|item| item.stable_uuid).collect(),
	)
}

/// Split the view's skipped items into the SYNCED paths they make unknown and the reports for items
/// never synced.
///
/// A baseline row whose remote item — by uuid, or for a file by its whole-life id, which survives
/// the new version a foreign same-name upload makes — is among the skipped was not deleted: the
/// item is still there, only the view cannot say where it is or what it holds. Planning such a path
/// from its absence would quarantine the local copy (or re-upload it over the item), and one such
/// path is far below what the mass-delete guard holds. So it is returned keyed by its baseline path
/// with its reason, for the pass to leave that path and its subtree alone and report it. A skipped
/// item no baseline row names is only reported, under its would-be path.
pub(crate) fn unknown_remote_paths(
	synced: &SyncedPaths,
	skipped: &[SkippedRemote],
) -> (BTreeMap<String, UnsyncableReason>, Vec<UnsyncablePath>) {
	// Nothing was skipped, so nothing is looked up at all.
	if skipped.is_empty() {
		return (BTreeMap::new(), Vec::new());
	}
	let mut unknown = BTreeMap::new();
	let mut never_synced: Vec<UnsyncablePath> = Vec::new();
	for item in skipped {
		// The baseline's own uuid and lineage indexes, rather than two maps built over every row
		// of the pair to answer one question per skipped item.
		let synced_at = synced.path_by_uuid(item.remote_uuid).or_else(|| {
			item.stable_uuid
				.and_then(|lineage| synced.path_by_lineage(lineage))
		});
		if let Some(path) = synced_at {
			unknown.insert(path, item.reason.clone());
			continue;
		}
		let report = UnsyncablePath::new(item.rel_path.clone(), item.reason.clone());
		// Two undecodable items in one directory, or the items under one skipped directory, report
		// the same line; once is enough.
		if !never_synced.contains(&report) {
			never_synced.push(report);
		}
	}
	// A synced directory's subtree is already left alone with it: its synced descendants add no
	// line of their own.
	let unknown_dirs: Vec<String> = unknown.keys().cloned().collect();
	unknown.retain(|path, _| !unknown_dirs.iter().any(|dir| is_under(path, dir)));
	(unknown, never_synced)
}

/// How one side compares to the baseline at a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SideState {
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

impl SideState {
	fn changed(self) -> bool {
		matches!(self, Self::Created | Self::Modified | Self::Deleted)
	}
}

fn classify_local(node: Option<&LocalNode>, base: Option<&BaselineEntry>) -> SideState {
	match (node, base) {
		(None, None) => SideState::Absent,
		(Some(_), None) => SideState::Created,
		(None, Some(_)) => SideState::Deleted,
		(Some(node), Some(base)) => {
			if node.kind != base.kind {
				SideState::Modified
			} else {
				match node.kind {
					NodeKind::Dir => SideState::Unchanged,
					NodeKind::File => {
						if node.content_hash.is_some() && node.content_hash == base.content_hash {
							SideState::Unchanged
						} else {
							SideState::Modified
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

fn classify_remote(node: Option<&RemoteNode>, base: Option<&BaselineEntry>) -> SideState {
	match (node, base) {
		(None, None) => SideState::Absent,
		(Some(_), None) => SideState::Created,
		(None, Some(_)) => SideState::Deleted,
		(Some(node), Some(base)) => {
			if node.kind != base.kind {
				SideState::Modified
			} else {
				match node.kind {
					NodeKind::File => classify_remote_file(node, base),
					NodeKind::Dir => SideState::Unchanged,
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
fn classify_remote_file(node: &RemoteNode, base: &BaselineEntry) -> SideState {
	if base.remote_uuid == Some(node.remote_uuid) {
		return SideState::Unchanged;
	}
	let identical_content = matches!(
		(base.content_hash, node.content_hash),
		(Some(recorded), Some(current)) if recorded == current
	);
	if same_file_lineage(base, node) && identical_content {
		SideState::Unchanged
	} else {
		SideState::Modified
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
	baseline: &mut Baseline,
	raw_remote: &impl NodesAt<Node = RemoteNode>,
) -> Vec<BaselineEntry> {
	// Only the rows that await confirmation are candidates, and the baseline indexes those, so a
	// converged pair reads no row at all.
	baseline.confirm_where(|entry| {
		let confirmed =
			raw_remote.at(&entry.rel_path).map(|node| node.remote_uuid) == entry.remote_uuid;
		if confirmed {
			tracing::debug!(
				"plan: the remote confirms {:?} — both sides hold what the baseline records",
				entry.rel_path
			);
		}
		confirmed
	})
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
	baseline: &mut Baseline,
	confirmed: &HashSet<Uuid>,
) -> Vec<BaselineEntry> {
	baseline.confirm_where(|entry| {
		let stood = entry
			.remote_uuid
			.is_some_and(|uuid| confirmed.contains(&uuid));
		if stood {
			tracing::debug!(
				"plan: {:?} — this engine's push stood as the remote head long enough to count as agreed",
				entry.rel_path
			);
		}
		stood
	})
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
	baseline: &Baseline,
	local: &impl Nodes<Node = LocalNode>,
	remote: &impl Nodes<Node = RemoteNode>,
) -> Vec<BaselineEntry> {
	let mut rows = Vec::new();
	for base in baseline.iter() {
		if base.state != BaselineState::Synced {
			continue;
		}
		let rel_path = &base.rel_path;
		let (local_at, remote_at) = (local.at(rel_path), remote.at(rel_path));
		let (local_node, remote_node) = (local_at.as_deref(), remote_at.as_deref());
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
	let untracked = |rel_path: &str| !baseline.contains_key(rel_path);
	match mode {
		super::SyncMode::LocalToRemote | super::SyncMode::LocalBackup => {
			for (rel_path, node) in remote.iter().filter(|(path, _)| untracked(path)) {
				if local.holds(&rel_path) {
					continue;
				}
				tracing::debug!(
					"reconfigure: adopting the destination-only item {rel_path:?} — the pair never tracked it"
				);
				rows.push(adopted_from_remote(&rel_path, &node));
			}
		}
		super::SyncMode::RemoteToLocal | super::SyncMode::RemoteBackup => {
			for (rel_path, node) in local.iter().filter(|(path, _)| untracked(path)) {
				if remote.holds(&rel_path) {
					continue;
				}
				tracing::debug!(
					"reconfigure: adopting the destination-only item {rel_path:?} — the pair never tracked it"
				);
				rows.push(adopted_from_local(&rel_path, &node));
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

/// Hand `visit` every baseline row that could be the SOURCE of a move this pass detects.
///
/// On a change-scoped pass that is the rows at the paths it decides, and no others. A row it does
/// not decide still carries BOTH its nodes, so the file sits at its recorded path on both sides,
/// and the only endpoint such a row could match is its own path — which [`detect_moves`] refuses
/// (`to != from`). The one shape that would break the equivalence is two `Synced` rows recording
/// one remote uuid or one lineage id, and no write path produces one: a move commits the delete of
/// its source and the insert of its destination in a single transaction.
fn visit_move_sources(
	paths: PassPaths<'_>,
	baseline: &Baseline,
	mut visit: impl FnMut(&BaselineEntry),
) {
	match paths {
		PassPaths::Whole => baseline.visit_rows(visit),
		PassPaths::Changed(changed) => {
			let mut rows = baseline.cursor();
			for path in changed {
				if let Some(row) = rows.get(path) {
					visit(&row);
				}
			}
		}
	}
}

/// Hand `visit` every node of `map` a move this pass detects could END at, by the same argument as
/// [`visit_move_sources`]: a node the pass did not re-read is the one its own row records, so it
/// names no path but its own. A node with no row at all was necessarily observed, so it is in the
/// set by construction (see [`PassPaths::Changed`]).
fn visit_move_targets<'m, N: Nodes>(
	paths: PassPaths<'m>,
	map: &'m N,
	mut visit: impl FnMut(Cow<'m, str>, Cow<'m, N::Node>),
) {
	match paths {
		PassPaths::Whole => {
			for (path, node) in map.iter() {
				visit(path, node);
			}
		}
		PassPaths::Changed(changed) => {
			for path in changed {
				if let Some(node) = map.at(path) {
					visit(Cow::Borrowed(path.as_str()), node);
				}
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
fn detect_moves<'m>(
	mode: super::SyncMode,
	baseline: &Baseline,
	local: &'m impl Nodes<Node = LocalNode>,
	remote: &'m impl Nodes<Node = RemoteNode>,
	paths: PassPaths<'m>,
	actions: &mut Vec<SyncAction>,
	consumed: &mut HashSet<String>,
) {
	// Content hash -> the new local paths carrying it (not in baseline, not on the remote). Keyed by
	// the raw bytes since `Blake3Hash` is not `std::hash::Hash`. Built before either half runs: it
	// depends on neither half's decisions, and it is what bounds the rows the push half keeps below.
	let mut created_by_hash: HashMap<[u8; 32], Vec<Cow<'m, str>>> = HashMap::new();
	if mode.pushes() {
		visit_move_targets(paths, local, |path, node| {
			// The side map first: on a whole pass it is a hash lookup and the baseline is a read of
			// the store, and almost every local file is on the remote too.
			if node.kind == NodeKind::File
				&& !remote.holds(&path)
				&& !baseline.contains_key(&path)
				&& let Some(hash) = node.content_hash
			{
				created_by_hash
					.entry(*hash.as_ref())
					.or_default()
					.push(path);
			}
		});
	}
	// The rows a LOCAL move can start from: synced files the local side no longer holds whose content
	// reappears at a new local path. Collected on the pull half's walk of the rows where there is one,
	// so a two-way pass walks them once — in the order a walk of their own would hand them out, and
	// judged against `consumed` only once the pull half is done with it, exactly as that walk would
	// judge them. Only a row with somewhere to move to is kept: a mass deletion keeps none.
	let local_source = |base: &BaselineEntry| {
		base.content_hash
			.is_some_and(|hash| created_by_hash.contains_key(hash.as_ref()))
			&& base.kind == NodeKind::File
			&& base.state == BaselineState::Synced
			&& !local.holds(&base.rel_path)
	};
	let mut local_sources: Vec<BaselineEntry> = Vec::new();
	if mode.pulls() {
		// One walk for both indexes. The second is there because a file's uuid is re-minted by
		// every content edit, so a move that CARRIED an edit inside one window is invisible to the
		// first — the uuid the baseline recorded no longer names anything live. The server-minted
		// lineage id is not re-minted, so it still finds the file at its new path and the pass
		// carries the item across instead of quarantining the old path and downloading the new one
		// from scratch.
		let mut remote_path_of_uuid: HashMap<Uuid, Cow<'m, str>> = HashMap::new();
		let mut remote_path_of_lineage: HashMap<StableUuid, Cow<'m, str>> = HashMap::new();
		visit_move_targets(paths, remote, |path, node| {
			if node.kind != NodeKind::File {
				return;
			}
			if let Some(lineage) = node.stable_uuid {
				remote_path_of_lineage.insert(lineage, path.clone());
			}
			remote_path_of_uuid.insert(node.remote_uuid, path);
		});
		visit_move_sources(paths, baseline, |base| {
			if mode.pushes() && local_source(base) {
				local_sources.push(base.clone());
			}
			let from = &base.rel_path;
			if base.kind != NodeKind::File
				|| base.state != BaselineState::Synced
				|| consumed.contains(from)
			{
				return;
			}
			let Some(uuid) = base.remote_uuid else {
				return;
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
			let local_at_from = local.at(from);
			if base.content_hash.is_some()
				&& classify_local(local_at_from.as_deref(), Some(base)) != SideState::Unchanged
			{
				return;
			}
			// `carries_edit`: the lineage moved AND changed version, so the local copy this move
			// renames is the pre-edit one and still has to be refreshed.
			let (to, carries_edit) = match remote_path_of_uuid.get(&uuid) {
				Some(to) => (to.as_ref(), false),
				None => match base
					.remote_stable_uuid
					.and_then(|lineage| remote_path_of_lineage.get(&lineage))
				{
					Some(to) => (to.as_ref(), true),
					None => return,
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
			let remote_at_to = remote.at(to);
			if carries_edit
				&& mode.pushes()
				&& (base.content_hash.is_none()
					|| is_unconfirmed_concurrent_edit(
						local_at_from.as_deref(),
						remote_at_to.as_deref(),
						Some(base),
					)) {
				let action = SyncAction::Conflict {
					rel_path: from.clone(),
				};
				tracing::debug!(
					"plan: {} — the moved version is a foreign edit over a push this engine never saw confirmed",
					action.describe()
				);
				actions.push(action);
				consumed.insert(from.clone());
				return;
			}
			if to != from
				&& !baseline.contains_key(to)
				&& local_at_from.is_some()
				&& !local.holds(to)
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
						// The lineage index named this path, so the node is there.
						remote_uuid: remote_at_to
							.as_deref()
							.expect("the moved item's new path is in the remote side")
							.remote_uuid,
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
		});
	} else if mode.pushes() && !created_by_hash.is_empty() {
		visit_move_sources(paths, baseline, |base| {
			if local_source(base) {
				local_sources.push(base.clone());
			}
		});
	}

	if mode.pushes() {
		for base in &local_sources {
			let from = &base.rel_path;
			if consumed.contains(from) {
				continue;
			}
			let (Some(hash), Some(uuid)) = (base.content_hash, base.remote_uuid) else {
				continue;
			};
			// The remote must still hold the original file at `from` for there to be one to move.
			if remote.at(from).map(|n| n.remote_uuid) != Some(uuid) {
				continue;
			}
			let Some(candidates) = created_by_hash.get(hash.as_ref()) else {
				continue;
			};
			let fresh: Vec<&str> = candidates
				.iter()
				.map(Cow::as_ref)
				.filter(|to| !consumed.contains(*to) && !remote.holds(to))
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
fn is_under_held_path(key: &str, held: &BTreeSet<String>) -> bool {
	at_or_under_root(held, key)
}

/// Whether `path` is a STRICT descendant of `prefix` (`prefix/...`).
pub(super) fn is_under(path: &str, prefix: &str) -> bool {
	path.len() > prefix.len()
		&& path.as_bytes()[prefix.len()] == b'/'
		&& path.as_bytes()[..prefix.len()] == *prefix.as_bytes()
}

/// Whether `path` is one of `roots` or lives under one, found by walking the path's OWN ancestors
/// and asking the sorted set for each: O(depth · log n), where asking [`is_under`] per root is a
/// scan of every root for every path.
///
/// The walk is what makes it an ancestor test. Seeking to the nearest key at or before `path`
/// answers something else: with `a` and `a/b` recorded, the key before `a/c` is `a/b`, which is not
/// an ancestor of it, while the ancestor `a` lies further back still.
///
/// An ancestor walk never yields `""`, so a root of `""` covers only the pair root itself here. The
/// one caller for which it means "everything is under it" asks for that separately (`drop_blocked`).
pub(super) fn at_or_under_root(roots: &BTreeSet<String>, path: &str) -> bool {
	roots.contains(path)
		|| path
			.match_indices('/')
			.any(|(i, _)| roots.contains(&path[..i]))
}

/// The key range of everything strictly under `dir`: `dir/` up to `dir0`. `/` is 0x2F and `0` is
/// 0x30 and paths compare bytewise, so the half-open range is exactly that subtree, and a sorted map
/// or set answers "is anything under here?" by seeking to it instead of testing every key. `""`
/// holds nothing by this rule, as `is_under(path, "")` holds for nothing.
pub(super) fn subtree_bounds(dir: &str) -> std::ops::Range<String> {
	format!("{dir}/")..format!("{dir}0")
}

/// The entries of `map` strictly under `dir`, in key order (see [`subtree_bounds`]).
pub(super) fn under_dir<'m, V>(
	map: &'m BTreeMap<String, V>,
	dir: &str,
) -> btree_map::Range<'m, String, V> {
	map.range(subtree_bounds(dir))
}

/// Whether a directory move with `end` as one of its ends touches a path the cache is holding: at
/// or under it, or above it.
fn touches_held(end: &str, held: &BTreeSet<String>) -> bool {
	is_under_held_path(end, held) || held.range(subtree_bounds(end)).next().is_some()
}

/// Where `path` lands when the directory at `from` moves to `to`; `None` when it is neither `from`
/// nor under it.
pub(super) fn moved_path(path: &str, from: &str, to: &str) -> Option<String> {
	(path == from || is_under(path, from)).then(|| format!("{to}{}", &path[from.len()..]))
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
///   A path held in conflict at either spelling, or one touching a path the cache is holding, is
///   left to the ordinary reconcile.
/// - A directory moved to a different path on one side; see [`next_dir_move`].
///
/// The engine re-keys the paths it blocks the same way (see `Prepared::fold_dir_moves`), so a
/// block follows its item into the moved directory.
///
/// Only the side that moved is re-keyed. The other one keeps what it holds where it holds it,
/// which a whole map does by itself and a carried side has to be told: the rows it derives its
/// nodes from move with the fold (see `Side::stay_put`).
///
/// `paths` narrows what the two detectors ENUMERATE, exactly as it narrows what [`reconcile`]
/// decides, and for the same reason: a move both of whose endpoints the pass carried from their own
/// baseline rows is not a move. Both endpoints of a real one are always in the set —
///
/// - the side that moved observed an absence at the source and a node at the destination, and
///   [`PassPaths::Changed`]'s contract puts every such path in the set;
/// - a case-only rename is that same pair of observations under one folded name;
/// - the twin a pushed move is refused for is a second directory GONE locally, which is an
///   observed absence too.
///
/// — so scoping loses no move the whole read finds, and a move it does leave out is one the
/// narrowed reconcile would not have decided either. Each iteration re-keys the set with the move
/// it just folded, so a move nested in that subtree is named by where the outer one put it, and
/// reads the set's rows once for both detectors (see [`FoldDirs`]).
///
/// The paths the side a move did not carry had to record to stay put are decided too: they join
/// the set, and come back beside the moves keyed where the last move left them, for the caller to
/// add to its own copy once it has replayed the moves over it. A whole pass gets none back.
///
/// What still costs the whole map either way is the destination check (`occupied`) and the re-key
/// itself, both of which walk a side map that has no order to bisect. Those go when the side maps
/// stop being path-keyed whole-tree maps.
pub(crate) fn fold_dir_moves(
	mode: super::SyncMode,
	baseline: &mut Baseline,
	local: &mut Side<LocalNode>,
	remote: &mut Side<RemoteNode>,
	held: &BTreeSet<String>,
	paths: PassPaths<'_>,
) -> (Vec<SyncAction>, Vec<String>) {
	let mut renames = Vec::new();
	// Owned only once a move has actually landed: a fold that finds nothing — which is almost every
	// pass — reads the caller's set where it lies.
	let mut changed: Option<Cow<'_, BTreeSet<String>>> = match paths {
		PassPaths::Whole => None,
		PassPaths::Changed(set) => Some(Cow::Borrowed(set)),
	};
	// What the side a move did not carry recorded to stay put, keyed like `changed`: the part of it
	// the caller's own replay of the moves cannot produce.
	let mut stayed_put: Vec<String> = Vec::new();
	loop {
		let scope = changed
			.as_deref()
			.map_or(PassPaths::Whole, PassPaths::Changed);
		// An immutable reborrow, scoped so it is dead before `move_subtree` below wants the
		// mutable one back. A carried side reads its nodes off these very rows, which is why the
		// re-key and the row move that follows it cannot be reordered (see `Side::rekey_subtree`).
		let action = {
			let bl: &Baseline = baseline;
			let (local_ref, remote_ref) = (local.of(bl), remote.of(bl));
			let dirs = FoldDirs::read(scope, bl, &local_ref, &remote_ref);
			next_case_only_dir_rename(mode, bl, &local_ref, &remote_ref, held, &dirs)
				.map(|action| (action, Matched::Identity))
				.or_else(|| next_dir_move(mode, bl, &local_ref, &remote_ref, held, &dirs))
		};
		let Some((action, matched)) = action else {
			break;
		};
		let (from, to) = action.endpoints();
		// The side the move carries is re-keyed, and the other one told to stay where it is: both
		// read the rows about to move (see `Side::stay_put`). A push matched by its signature asks
		// for no walk of them, because the match already proved what staying put would record — no
		// local node at the source, and one at every path a row lands on.
		let stayed = {
			let bl: &Baseline = baseline;
			if matches!(action, SyncAction::MoveRemote { .. }) {
				remote.rekey_subtree(bl, from, to, |node, path| node.rel_path = path.to_string());
				match matched {
					Matched::Identity => local.stay_put(bl, from, to),
					Matched::Signature => Vec::new(),
				}
			} else {
				local.rekey_subtree(bl, from, to, |node, path| node.rel_path = path.to_string());
				remote.stay_put(bl, from, to)
			}
		};
		// Written only once a move is actually being folded: the rows the pass reads are shared
		// with the store, and a pass that folds no directory move must not copy them.
		baseline.move_subtree(from, to);
		// The next iteration's scope, keyed like the maps and the rows this move just re-keyed.
		// `Prepared::fold_dir_moves` does the same to the caller's copy once this returns; without
		// it here, a move nested under this one would be looked for at a path nothing holds. What
		// the other side had to record to stay put is decided too, keyed where it now stands, and
		// goes into the scope and the hand-back both, hence the copies of it.
		if let Some(set) = &mut changed {
			let rekeyed: BTreeSet<String> = set
				.iter()
				.map(|path| moved_path(path, from, to).unwrap_or_else(|| path.clone()))
				.chain(stayed.iter().cloned())
				.collect();
			*set = Cow::Owned(rekeyed);
			stayed_put = stayed_put
				.into_iter()
				.map(|path| moved_path(&path, from, to).unwrap_or(path))
				.chain(stayed)
				.collect();
		}
		tracing::debug!(
			"plan: {} — the two sides spell the directory differently only by case",
			action.describe()
		);
		renames.push(action);
	}
	(renames, stayed_put)
}

/// How a directory move was matched, which is what [`fold_dir_moves`] may take as known about the
/// subtree it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Matched {
	/// By name (a case-only rename) or by the server's uuid for the directory (a pull): nothing
	/// about what either side holds under it.
	Identity,
	/// A push, by the signature of the local subtree ([`next_dir_move`]): nothing local is left at
	/// or under the source, and every row under it has a local node where it lands.
	Signature,
}

/// What one iteration of [`fold_dir_moves`] enumerates for its two detectors: the directories each
/// side holds, and the directory rows a move can start from.
///
/// A whole pass enumerates each side's map and the directory rows, and a detector walks what it
/// asks about only once the one before it has left something to find. A change-scoped pass
/// enumerates its own paths for all of them, and asked one detector at a time it read the rows at
/// those paths once per walk: after a directory rename, the remote's directories twice and the move
/// sources once, three reads of the renamed rows an iteration. So a scoped iteration reads them in
/// ONE cursor walk, asks both sides about each path while the page that answers for it is the one
/// the cursor just read, and keeps only what the detectors ask about: the directories, a few among
/// the paths.
enum FoldDirs<'m> {
	/// A whole pass: each detector walks the maps and the directory rows itself.
	Whole,
	/// A change-scoped pass: its own paths, read once.
	Changed(ChangedDirs<'m>),
}

/// The directories at a change-scoped pass's own paths, each list in path order — the order the
/// detectors walked those paths in — and already narrowed by the question its detector asks of
/// each, asked in the walk while that path's row was in hand.
#[derive(Default)]
struct ChangedDirs<'m> {
	/// The rows a directory move can start from, with their remote uuids.
	sources: Vec<(&'m str, Uuid)>,
	/// The local directories the remote holds nothing at: a case-only rename's local end.
	local_only: Vec<&'m str>,
	/// The local directories no row records: where a pushed move can end.
	local_new: Vec<&'m str>,
	/// The remote directories, with their uuids: where a pulled move can end.
	remote: Vec<(&'m str, Uuid)>,
	/// The remote directories the local side holds nothing at: a case-only rename's remote end.
	remote_only: Vec<(&'m str, Uuid)>,
}

impl<'m> FoldDirs<'m> {
	/// What one iteration enumerates at `paths`: nothing read up front for a whole pass, and one
	/// walk of its own paths for a scoped one.
	fn read(
		paths: PassPaths<'m>,
		baseline: &Baseline,
		local: &impl NodesAt<Node = LocalNode>,
		remote: &impl NodesAt<Node = RemoteNode>,
	) -> Self {
		let PassPaths::Changed(changed) = paths else {
			return Self::Whole;
		};
		let mut dirs = ChangedDirs::default();
		let mut rows = baseline.cursor();
		for path in changed {
			// First: every question below about `path` is answered out of the page this keeps.
			let row = rows.get(path);
			if let Some(uuid) = row.as_ref().and_then(dir_move_source) {
				dirs.sources.push((path, uuid));
			}
			if local
				.at(path)
				.is_some_and(|node| node.kind == NodeKind::Dir)
			{
				if !remote.holds(path) {
					dirs.local_only.push(path);
				}
				if row.is_none() {
					dirs.local_new.push(path);
				}
			}
			if let Some(node) = remote.at(path)
				&& node.kind == NodeKind::Dir
			{
				dirs.remote.push((path, node.remote_uuid));
				if !local.holds(path) {
					dirs.remote_only.push((path, node.remote_uuid));
				}
			}
		}
		Self::Changed(dirs)
	}

	/// Hand `visit` the path and remote uuid of every row a directory move can start from. A whole
	/// pass reads them off the index that holds nothing but directory rows: a few per cent of the
	/// rows, where a walk of every row would read all of them once per move the fold carries.
	fn visit_sources(&self, baseline: &Baseline, mut visit: impl FnMut(&str, Uuid)) {
		match self {
			Self::Whole => baseline.visit_dir_rows(|row| {
				if let Some(uuid) = dir_move_source(row) {
					visit(&row.rel_path, uuid);
				}
			}),
			Self::Changed(dirs) => {
				for &(path, uuid) in &dirs.sources {
					visit(path, uuid);
				}
			}
		}
	}

	/// Hand `visit` every local directory the remote holds nothing at under that exact spelling.
	fn visit_local_only(
		&self,
		local: &'m impl Nodes<Node = LocalNode>,
		remote: &impl NodesAt<Node = RemoteNode>,
		mut visit: impl FnMut(Cow<'m, str>),
	) {
		match self {
			Self::Whole => {
				for (path, node) in local.iter() {
					if node.kind == NodeKind::Dir && !remote.holds(&path) {
						visit(path);
					}
				}
			}
			Self::Changed(dirs) => {
				for &path in &dirs.local_only {
					visit(Cow::Borrowed(path));
				}
			}
		}
	}

	/// Hand `visit` every local directory no row records.
	fn visit_local_new(
		&self,
		local: &'m impl Nodes<Node = LocalNode>,
		baseline: &Baseline,
		mut visit: impl FnMut(Cow<'m, str>),
	) {
		match self {
			Self::Whole => {
				for (path, node) in local.iter() {
					if node.kind == NodeKind::Dir && !baseline.contains_key(&path) {
						visit(path);
					}
				}
			}
			Self::Changed(dirs) => {
				for &path in &dirs.local_new {
					visit(Cow::Borrowed(path));
				}
			}
		}
	}

	/// Hand `visit` every remote directory, with its uuid.
	fn visit_remote(
		&self,
		remote: &'m impl Nodes<Node = RemoteNode>,
		mut visit: impl FnMut(Cow<'m, str>, Uuid),
	) {
		match self {
			Self::Whole => {
				for (path, node) in remote.iter() {
					if node.kind == NodeKind::Dir {
						visit(path, node.remote_uuid);
					}
				}
			}
			Self::Changed(dirs) => {
				for &(path, uuid) in &dirs.remote {
					visit(Cow::Borrowed(path), uuid);
				}
			}
		}
	}

	/// Hand `visit` every remote directory the local side holds nothing at under that exact
	/// spelling, with its uuid.
	fn visit_remote_only(
		&self,
		remote: &'m impl Nodes<Node = RemoteNode>,
		local: &impl NodesAt<Node = LocalNode>,
		mut visit: impl FnMut(Cow<'m, str>, Uuid),
	) {
		match self {
			Self::Whole => {
				for (path, node) in remote.iter() {
					if node.kind == NodeKind::Dir && !local.holds(&path) {
						visit(path, node.remote_uuid);
					}
				}
			}
			Self::Changed(dirs) => {
				for &(path, uuid) in &dirs.remote_only {
					visit(Cow::Borrowed(path), uuid);
				}
			}
		}
	}
}

/// The remote uuid of `row` where a directory move can start from it: a synced directory row's.
fn dir_move_source(row: &BaselineEntry) -> Option<Uuid> {
	(row.kind == NodeKind::Dir && row.state == BaselineState::Synced)
		.then_some(row.remote_uuid)
		.flatten()
}

/// The shallowest case-only directory rename left in the inputs (see [`fold_dir_moves`]).
fn next_case_only_dir_rename<'m>(
	mode: super::SyncMode,
	baseline: &Baseline,
	local: &'m impl Nodes<Node = LocalNode>,
	remote: &'m impl Nodes<Node = RemoteNode>,
	held: &BTreeSet<String>,
	dirs: &FoldDirs<'m>,
) -> Option<SyncAction> {
	// Collision key -> the local directory no remote item holds under that exact spelling. The scan
	// refuses two local entries with one key, so the map loses nothing.
	let mut local_only: HashMap<String, Cow<'m, str>> = HashMap::new();
	dirs.visit_local_only(local, remote, |path| {
		local_only.insert(collision_key(&path), path);
	});
	if local_only.is_empty() {
		return None;
	}
	let mut candidates: Vec<(Cow<'m, str>, Cow<'m, str>, Uuid)> = Vec::new();
	dirs.visit_remote_only(remote, local, |remote_path, remote_uuid| {
		let Some(local_path) = local_only.get(&collision_key(&remote_path)) else {
			return;
		};
		if parent_path(local_path) == parent_path(&remote_path) {
			candidates.push((local_path.clone(), remote_path, remote_uuid));
		}
	});
	candidates.sort_unstable_by(|(a, ..), (b, ..)| {
		(a.matches('/').count(), a.as_ref()).cmp(&(b.matches('/').count(), b.as_ref()))
	});
	candidates
		.into_iter()
		.find_map(|(local_path, remote_path, remote_uuid)| {
			let at_local = baseline.get(&local_path);
			let at_remote = baseline.get(&remote_path);
			if at_local
				.iter()
				.chain(at_remote.iter())
				.any(|row| row.state.is_conflict())
				|| [local_path.as_ref(), remote_path.as_ref()]
					.into_iter()
					.any(|end| touches_held(end, held))
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
///   becomes a `MoveRemote`. A directory nested in the new one that is itself such a move does not
///   count against the match (see [`new_local_dir_signatures`]). A move that also changed something
///   inside does not match; it falls back to the per-path plan, which is correct and only slower.
///
/// Either is left to the per-path plan when any of these holds:
/// - a baseline row at or under the source is not `Synced` (a held conflict, an adopted row);
/// - the destination is taken, or has something under it, in the baseline or on the side being
///   moved — compared by collision key, which is how the server dedups names;
/// - one end lies under the other, or under a path the cache is holding (`held`);
/// - a directory above the destination is missing on the side being moved and the pass will not
///   create it there (see [`parents_ready`]). One the other side has new is created just before the
///   move, and one another move of this fold is headed to waits for that move;
/// - on the remote, the path a directory passes through between its re-parent and its rename is
///   taken.
fn next_dir_move<'m>(
	mode: super::SyncMode,
	baseline: &Baseline,
	local: &'m impl Nodes<Node = LocalNode>,
	remote: &'m impl Nodes<Node = RemoteNode>,
	held: &BTreeSet<String>,
	dirs: &FoldDirs<'m>,
) -> Option<(SyncAction, Matched)> {
	// Only a synced directory can be the source of a directory move, and at any ordinary shape the
	// directories are a small fraction of the rows — so a path is copied only for a row that is
	// actually a candidate.
	let mut sources: Vec<(String, Uuid)> = Vec::new();
	dirs.visit_sources(baseline, |path, uuid| {
		sources.push((path.to_string(), uuid))
	});
	if sources.is_empty() {
		return None;
	}
	sources.sort_unstable_by(|(a, _), (b, _)| {
		(a.matches('/').count(), a.as_str()).cmp(&(b.matches('/').count(), b.as_str()))
	});
	let mut remote_dir_at: HashMap<Uuid, Cow<'m, str>> = HashMap::new();
	if mode.pulls() {
		dirs.visit_remote(remote, |path, uuid| {
			remote_dir_at.insert(uuid, path);
		});
	}
	// The sources gone locally with their signatures, and the new local directories with theirs,
	// built only once a pushed move is possible — and each read once for every source it answers.
	let mut gone: Option<Vec<(&str, Signature)>> = None;
	let mut new_local_dirs: Option<Vec<NewLocalDir<'m>>> = None;

	// Every move that holds up but for the parents of its destination. A missing parent another of
	// them is moving into place is not one the pass creates: that move goes first, or this one would
	// fill its destination and refuse it.
	let candidates: Vec<(SyncAction, Matched)> = sources
		.iter()
		.filter_map(|(from, uuid)| {
			dir_move_from(
				mode,
				baseline,
				local,
				remote,
				held,
				&sources,
				&remote_dir_at,
				&mut gone,
				&mut new_local_dirs,
				from,
				*uuid,
				dirs,
			)
		})
		.collect();
	let pending: HashSet<String> = candidates
		.iter()
		.map(|(action, _)| collision_key(action.rel_path()))
		.collect();
	candidates.into_iter().find(|(action, _)| {
		let to = action.rel_path();
		match action {
			SyncAction::MoveRemote { .. } => {
				parents_ready(remote, baseline, &pending, to, |n| n.kind == NodeKind::Dir)
			}
			_ => parents_ready(local, baseline, &pending, to, |n| n.kind == NodeKind::Dir),
		}
	})
}

/// The directory move of the baseline directory `from` (remote uuid `uuid`), checked against
/// everything [`next_dir_move`] requires except the parents of its destination.
#[allow(clippy::too_many_arguments)] // the pass's inputs plus the indexes built once per call
fn dir_move_from<'m, 's>(
	mode: super::SyncMode,
	baseline: &Baseline,
	local: &'m impl Nodes<Node = LocalNode>,
	remote: &impl Nodes<Node = RemoteNode>,
	held: &BTreeSet<String>,
	sources: &'s [(String, Uuid)],
	remote_dir_at: &HashMap<Uuid, Cow<'m, str>>,
	gone: &mut Option<Vec<(&'s str, Signature)>>,
	new_local_dirs: &mut Option<Vec<NewLocalDir<'m>>>,
	from: &str,
	uuid: Uuid,
	dirs: &FoldDirs<'m>,
) -> Option<(SyncAction, Matched)> {
	let (action, matched) = if local.at(from).is_some_and(|n| n.kind == NodeKind::Dir) {
		let to = remote_dir_at.get(&uuid)?.as_ref();
		// The steady-state answer, before the scan that would reach it the slow way: the directory
		// is where the baseline recorded it. `occupied(local, from)` is true whenever it is — the
		// branch only runs with a local DIRECTORY at `from` — so this refuses exactly what it
		// refused before, without folding the case of every local key to find that out.
		if to == from {
			return None;
		}
		let action = (!local.occupied(to)).then(|| SyncAction::MoveLocal {
			from_path: from.to_string(),
			to_path: to.to_string(),
			kind: NodeKind::Dir,
		})?;
		(action, Matched::Identity)
	} else if mode.pushes()
		&& remote
			.at(from)
			.is_some_and(|n| n.kind == NodeKind::Dir && n.remote_uuid == uuid)
		&& !local.occupied(from)
	{
		// Nothing local is at `from` under any spelling, so it holds nothing there either: it is one
		// of the sources gone locally, and in `gone` exactly when the baseline vouches for it. The
		// first source to get here reads its own signature first, and only one the baseline vouches
		// for goes on to read the others'.
		let gone: &[(&str, Signature)] = match gone {
			Some(gone) => gone,
			None => {
				let signature = baseline_dir_signature(baseline, from)?;
				gone.insert(gone_signatures(baseline, local, sources, from, signature))
			}
		};
		let (_, signature) = gone.iter().find(|(path, _)| *path == from)?;
		let new_dirs = new_local_dirs
			.get_or_insert_with(|| new_local_dir_signatures(baseline, local, gone, dirs));
		let mut matches = new_dirs.iter().filter(|dir| dir.matches(signature));
		let to = matches.next()?.path.as_ref();
		if matches.next().is_some() {
			return None;
		}
		// The same subtree vanished from somewhere else too: which of the two moved is a guess.
		let twin = gone
			.iter()
			.any(|(other, other_signature)| *other != from && other_signature == signature);
		let (to_parent, to_name) = (parent_path(to), leaf(to));
		let via = match to_parent.is_empty() {
			true => leaf(from).to_string(),
			false => format!("{to_parent}/{}", leaf(from)),
		};
		let passes_free =
			parent_path(from) == to_parent || leaf(from) == to_name || !remote.occupied(&via);
		let action =
			(!twin && passes_free && !remote.occupied(to)).then(|| SyncAction::MoveRemote {
				from_path: from.to_string(),
				to_path: to.to_string(),
				kind: NodeKind::Dir,
				remote_uuid: uuid,
			})?;
		(action, Matched::Signature)
	} else {
		return None;
	};
	let (from_key, to_key) = (collision_key(from), collision_key(action.rel_path()));
	let to = action.rel_path();
	let clear = from_key != to_key
		&& !is_under(&to_key, &from_key)
		&& !is_under(&from_key, &to_key)
		&& !baseline.occupied(to)
		&& ![from, to].into_iter().any(|end| touches_held(end, held))
		&& baseline.subtree_all_synced(from);
	clear.then_some((action, matched))
}

/// A new local directory and the signatures a pushed move into it is matched by: everything it holds,
/// and — when a directory nested in it is a move of its own — what it holds without that one.
struct NewLocalDir<'a> {
	path: Cow<'a, str>,
	full: Signature,
	without_nested_moves: Option<Signature>,
}

impl NewLocalDir<'_> {
	fn matches(&self, signature: &Signature) -> bool {
		self.full == *signature || self.without_nested_moves.as_ref() == Some(signature)
	}
}

/// Every one of `sources` gone locally whose subtree the baseline vouches for, with its signature,
/// in `sources`' order: what a pushed move is matched from, and what makes one a guess. `from`'s is
/// the `signature` already in hand, and is not read again.
fn gone_signatures<'s>(
	baseline: &Baseline,
	local: &impl NodesAt<Node = LocalNode>,
	sources: &'s [(String, Uuid)],
	from: &str,
	signature: Signature,
) -> Vec<(&'s str, Signature)> {
	let mut in_hand = Some(signature);
	sources
		.iter()
		.filter(|(path, _)| !local.holds(path))
		.filter_map(|(path, _)| {
			let signature = match path == from {
				true => in_hand.take(),
				false => baseline_dir_signature(baseline, path),
			};
			Some((path.as_str(), signature?))
		})
		.collect()
}

/// The local directories the baseline does not record, with their signatures (see [`NewLocalDir`]).
/// A nested directory is a move of its own when either of its signatures is that of a synced
/// directory gone locally (`gone`, see [`gone_signatures`]); `mv z new; mv b new/b` then matches
/// `new` to `z` and `new/b` to `b`, while a subdirectory that moved along inside its parent still
/// leaves the parent's full signature to match. A directory whose full signature matches a gone one
/// gets no reduced signature at all: the full match already explains it. Computed deepest first, so
/// a chain of such moves reduces from the inside out.
fn new_local_dir_signatures<'m>(
	baseline: &Baseline,
	local: &'m impl Nodes<Node = LocalNode>,
	gone: &[(&str, Signature)],
	dirs: &FoldDirs<'m>,
) -> Vec<NewLocalDir<'m>> {
	let mut candidates: Vec<Cow<'m, str>> = Vec::new();
	dirs.visit_local_new(local, baseline, |path| candidates.push(path));
	candidates.sort_unstable_by_key(|path| Reverse(path.matches('/').count()));
	let mut new_dirs: Vec<NewLocalDir<'m>> = Vec::with_capacity(candidates.len());
	for path in candidates {
		let Some(full) = dir_signature(local, &path) else {
			continue;
		};
		let mut without_nested_moves: Option<Signature> = None;
		// A directory whose whole tree already matches one gone locally is that move, its nested
		// directories included: stripping one would let a deleted directory holding the rest claim it.
		let explained = gone.iter().any(|(_, sig)| *sig == full);
		for inner in &new_dirs {
			if !explained
				&& is_under(&inner.path, &path)
				&& gone.iter().any(|(_, sig)| inner.matches(sig))
			{
				let inner_rel = &inner.path[path.len()..];
				// The only copy of the signature, made once per directory that holds a nested move.
				without_nested_moves
					.get_or_insert_with(|| full.clone())
					.retain(|rel, _| rel != inner_rel && !is_under(rel, inner_rel));
			}
		}
		new_dirs.push(NewLocalDir {
			path,
			full,
			without_nested_moves,
		});
	}
	new_dirs
}

/// What a directory holds, relative to it: every path under it with its kind and, for a file, its
/// content hash and size. Two directories with equal signatures hold the same tree.
type Signature = BTreeMap<String, (NodeKind, Option<Blake3Hash>, u64)>;

/// The signature of the local directory at `root`; `None` when a file under it has no hash.
fn dir_signature(local: &impl Nodes<Node = LocalNode>, root: &str) -> Option<Signature> {
	local
		.under(root)
		.map(|(path, node)| {
			let (hash, size) = match node.kind {
				NodeKind::File => (Some(node.content_hash?), node.size),
				NodeKind::Dir => (None, 0),
			};
			Some((path[root.len()..].to_string(), (node.kind, hash, size)))
		})
		.collect()
}

/// The signature the baseline recorded for the directory at `root` — `None` when it cannot vouch
/// for the subtree: a row under it that is not `Synced`, a file row without a hash or a size, or no
/// file at all.
fn baseline_dir_signature(baseline: &Baseline, root: &str) -> Option<Signature> {
	let signature: Signature = baseline
		.subtree(root)
		.map(|row| {
			if row.state != BaselineState::Synced {
				return None;
			}
			let (hash, size) = match row.kind {
				NodeKind::File => (Some(row.content_hash?), row.size?),
				NodeKind::Dir => (None, 0),
			};
			Some((
				row.rel_path[root.len()..].to_string(),
				(row.kind, hash, size),
			))
		})
		.collect::<Option<_>>()?;
	signature
		.values()
		.any(|(kind, ..)| *kind == NodeKind::File)
		.then_some(signature)
}

/// Whether every directory above `to` exists on `side`, or is one the reconcile creates there
/// because the other side has it new: nothing holds that path on `side` or in the baseline, under any
/// spelling, and no other directory move of this fold is headed there (`pending`, collision keys) —
/// that one runs first and brings the parent. The engine runs those creates just before the move
/// that needs them.
fn parents_ready<N: NodesAt>(
	side: &N,
	baseline: &Baseline,
	pending: &HashSet<String>,
	to: &str,
	is_dir: impl Fn(&N::Node) -> bool,
) -> bool {
	let mut parent = parent_path(to);
	while !parent.is_empty() {
		match side.at(parent) {
			// An existing directory stands on existing ones.
			Some(node) => return is_dir(&node),
			None if side.occupied(parent)
				|| baseline.occupied(parent)
				|| pending.contains(&collision_key(parent)) =>
			{
				return false;
			}
			None => parent = parent_path(parent),
		}
	}
	true
}

/// The last component of a `/`-joined relative path.
fn leaf(rel_path: &str) -> &str {
	rel_path.rsplit_once('/').map_or(rel_path, |(_, name)| name)
}

/// The parent part of a `/`-joined relative path (`""` at the top level).
fn parent_path(rel_path: &str) -> &str {
	rel_path.rsplit_once('/').map_or("", |(parent, _)| parent)
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
	pub(crate) held_remote: BTreeSet<String>,
}

/// Which paths one [`reconcile`] decides.
///
/// A pass that read both sides whole knows every path and decides every path. A change-scoped pass
/// read only what its changelists named and DERIVED the rest from the baseline rows
/// (see [`derive`](super::derive)), so almost every path in its maps is one whose two nodes came
/// out of its own row — and the reconcile's answer at such a path is "the three inputs agree",
/// which is no action at all. Deciding them again is what the reconcile costs at rest, and this is
/// what takes that cost away.
///
/// # What [`Changed`](Self::Changed) requires of its caller
///
/// Every path where the local map, the remote map or the baseline row is not what
/// [`derive::carried`](super::derive) put there must be in the set. That is not a convention a
/// caller has to remember: `carried` hands back BOTH of a row's nodes or neither, and a row it
/// cannot carry goes into the dirty set in the same statement — so a derivation's own set already
/// names every path whose nodes are not its row's, and what the pass adds to it is every path its
/// observations moved off that row (see `derive::Derived::decided`).
///
/// A path left out of the set is a path nothing decides. That can never INVENT an absence — a
/// deletion is only ever planned at a path this loop visits — but it can DELAY one, which is
/// invariant I1's safe direction, and the reason the requirement above is written in terms of
/// evidence rather than of paths.
#[derive(Debug, Clone, Copy)]
pub(crate) enum PassPaths<'a> {
	/// Decide every path the three inputs hold.
	Whole,
	/// Decide only these, plus the rows under a held path (which cost their count and nothing
	/// else — see [`reconcile_keys`]).
	Changed(&'a BTreeSet<String>),
}

/// One reconciled pass.
pub(crate) struct Plan {
	pub(crate) actions: Vec<SyncAction>,
	/// How many paths the pass deliberately left alone (an unresolvable name, or a deletion this
	/// engine has already made). A pass that skips what it would otherwise have done is not the
	/// same as a pass with nothing to do, and the report has to be able to tell them apart.
	pub(crate) deferred_paths: usize,
}

/// The paths [`reconcile`] decides, in path order — siblings together, so its baseline cursor
/// resolves one directory and answers for everything in it.
///
/// [`Whole`](PassPaths::Whole) is every key either side holds, plus the baseline rows NEITHER side
/// holds any more. That third set is the only part of the baseline whose paths the two side maps
/// do not already carry — a row both sides still hold is keyed by a `String` that exists, and
/// borrowing it is the difference between one allocation per row and three — and it is exactly the
/// set the both-absent arm retires. So the tree is walked for it against one reused buffer and
/// only a row in neither side is copied: a converged pair copies no path here.
///
/// [`Changed`](PassPaths::Changed) is that same union intersected with the pass's own set. Nothing
/// is left unretired by the intersection: the both-absent arm retires a row whose path neither
/// side holds any more, and a path this leaves out is one whose two nodes were CARRIED from its
/// own row, which puts it on both sides. A row is retired by the pass that observes its absence,
/// and observing that absence is what put the path in the set.
fn reconcile_keys<'m>(
	paths: PassPaths<'m>,
	baseline: &Baseline,
	local: &'m impl Nodes<Node = LocalNode>,
	remote: &'m impl Nodes<Node = RemoteNode>,
	held: &BTreeSet<String>,
) -> BTreeSet<Cow<'m, str>> {
	let PassPaths::Changed(changed) = paths else {
		let mut keys: BTreeSet<Cow<'m, str>> = local.paths().chain(remote.paths()).collect();
		baseline.visit_row_paths(|path| {
			if !local.holds(path) && !remote.holds(path) {
				keys.insert(Cow::Owned(path.to_string()));
			}
		});
		return keys;
	};
	let mut keys: BTreeSet<Cow<'m, str>> = BTreeSet::new();
	// Intersected with the three inputs, not taken as given: a path in none of them is a path the
	// whole-tree key set does not hold either, and counting one as withheld under a held path
	// would report a deferral a whole read never reports.
	for path in changed {
		if local.holds(path) || remote.holds(path) || baseline.contains_key(path) {
			keys.insert(Cow::Borrowed(path.as_str()));
		}
	}
	// A held path withholds its whole subtree, and every path withheld is COUNTED
	// ([`Plan::deferred_paths`]). The rows under one are otherwise absent from this set — they are
	// carried, so nothing observed them — and they are the only paths whose absence would change a
	// number rather than an action. Held paths are the rows that record one side only and the ones
	// the cache is showing twice: few, and their subtrees with them.
	for root in held {
		if local.holds(root) || remote.holds(root) || baseline.contains_key(root) {
			keys.insert(Cow::Owned(root.clone()));
		}
		baseline.visit_subtree_paths(root, |path| {
			keys.insert(Cow::Owned(path.to_string()));
		});
	}
	keys
}

/// Reconcile a pair's three inputs into an ordered action plan. `baseline`/`local`/`remote` are all
/// keyed by the same NFC-normalized relative path; `holds` is what the pass must leave alone, and
/// `paths` which paths it decides (see [`PassPaths`]).
pub(crate) fn reconcile(
	mode: super::SyncMode,
	baseline: &Baseline,
	local: &impl Nodes<Node = LocalNode>,
	remote: &impl Nodes<Node = RemoteNode>,
	holds: &PassHolds,
	paths: PassPaths<'_>,
) -> Plan {
	let mut actions = Vec::new();
	let mut consumed = HashSet::new();
	tracing::debug!(
		"reconcile: mode {mode:?} — {} baseline / {} local / {} remote entries",
		baseline.len(),
		local.len(),
		remote.len()
	);
	let keys = reconcile_keys(paths, baseline, local, remote, &holds.held_remote);

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
				consumed.insert(key.to_string());
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
	detect_moves(
		mode,
		baseline,
		local,
		remote,
		paths,
		&mut actions,
		&mut consumed,
	);

	// The keys come out of the set in path order, so siblings arrive together: the cursor resolves
	// one directory and answers for everything under it.
	let mut rows = baseline.cursor();
	for key in &keys {
		let key: &str = key;
		if consumed.contains(key) {
			continue;
		}
		let base = rows.get(key);
		let base = base.as_ref();
		// A held conflict is never acted on until the caller resolves it — but it IS re-reported
		// every pass, so a caller watching the reports keeps seeing what is outstanding. Its
		// subtree is suppressed below, along with any conflict surfaced by this pass.
		if base.is_some_and(|b| b.state.is_conflict()) {
			actions.push(SyncAction::Conflict {
				rel_path: key.to_string(),
			});
			continue;
		}
		let local_at = local.at(key);
		let remote_at = remote.at(key);
		let (local_node, remote_node) = (local_at.as_deref(), remote_at.as_deref());

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

	use chrono::{DateTime, Utc};

	use super::{
		super::{
			engine::{Observations, PendingKind, PendingWrites},
			ignore::{IgnoreLevel, IgnoreSource, Origin},
		},
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
		reconcile(
			mode,
			&tree(baseline),
			local,
			remote,
			&PassHolds::default(),
			PassPaths::Whole,
		)
		.actions
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
		let mut remote = Side::from(remote.clone());
		let baseline = tree(baseline);
		writes.fold_into(PAIR, &baseline, &mut remote, &mut BTreeSet::new());
		reconcile(
			mode,
			&baseline,
			local,
			&remote.of(&baseline),
			&PassHolds::default(),
			PassPaths::Whole,
		)
		.actions
	}

	/// What a pass plans once the case-only directory renames are folded into its inputs: the
	/// renames first, then the reconciled rest.
	fn plan_with_renames(
		mode: SyncMode,
		baseline: &HashMap<String, BaselineEntry>,
		local: &HashMap<String, LocalNode>,
		remote: &HashMap<String, RemoteNode>,
	) -> Vec<SyncAction> {
		let (mut baseline, mut local, mut remote) = (
			tree(baseline),
			Side::from(local.clone()),
			Side::from(remote.clone()),
		);
		let (mut actions, _) = fold_dir_moves(
			mode,
			&mut baseline,
			&mut local,
			&mut remote,
			&BTreeSet::new(),
			PassPaths::Whole,
		);
		actions.extend(
			reconcile(
				mode,
				&baseline,
				&local.of(&baseline),
				&remote.of(&baseline),
				&PassHolds::default(),
				PassPaths::Whole,
			)
			.actions,
		);
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
		let (mut baseline, local, remote) = case_tree("Docs", "docs", "Docs", dir, file);
		let (mut local, mut remote) = (Side::from(local), Side::from(remote));
		baseline.get_mut("Docs").unwrap().state = BaselineState::Conflicted;
		let mut baseline = tree(&baseline);
		assert!(
			fold_dir_moves(
				SyncMode::TwoWay,
				&mut baseline,
				&mut local,
				&mut remote,
				&BTreeSet::new(),
				PassPaths::Whole,
			)
			.0
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
			// An unchanged directory inside may still move on its own, into the new parent.
			assert!(
				moves_onto(&actions, "documents").is_none(),
				"{label}: {actions:?}"
			);
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

	/// A move into a directory the other side has new is still one move: the reconcile plans the
	/// create of the new parent, which the engine runs just before the move. A parent the baseline
	/// records is not one the pass creates, so that move is left to the per-path plan.
	#[test]
	fn a_dir_move_into_a_new_parent_is_one_move() {
		let (baseline, mut local, remote) = moved_tree(TreeIds::new(), "fresh/documents", "docs");
		local.insert("fresh".to_string(), local_dir("fresh"));
		let actions = plan_with_renames(SyncMode::LocalToRemote, &baseline, &local, &remote);
		assert!(
			moves_onto(&actions, "fresh/documents").is_some(),
			"push: {actions:?}"
		);
		assert!(
			actions.contains(&SyncAction::CreateRemoteDir {
				rel_path: "fresh".to_string(),
			}),
			"push: {actions:?}"
		);

		let (baseline, local, mut remote) = moved_tree(TreeIds::new(), "docs", "fresh/documents");
		remote.insert(
			"fresh".to_string(),
			remote_dir_node("fresh", Uuid::new_v4()),
		);
		let actions = plan_with_renames(SyncMode::RemoteToLocal, &baseline, &local, &remote);
		assert!(
			moves_onto(&actions, "fresh/documents").is_some(),
			"pull: {actions:?}"
		);
		assert!(
			actions.contains(&SyncAction::CreateLocalDir {
				rel_path: "fresh".to_string(),
			}),
			"pull: {actions:?}"
		);

		let (mut baseline, mut local, remote) =
			moved_tree(TreeIds::new(), "fresh/documents", "docs");
		local.insert("fresh".to_string(), local_dir("fresh"));
		baseline.insert("fresh".to_string(), base_dir("fresh", Uuid::new_v4()));
		let actions = plan_with_renames(SyncMode::LocalToRemote, &baseline, &local, &remote);
		assert!(
			moves_onto(&actions, "fresh/documents").is_none(),
			"a recorded parent the remote lost: {actions:?}"
		);
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
		let (baseline, local, remote) = moved_tree(TreeIds::new(), "docs", "documents");
		let (mut local, mut remote) = (Side::from(local), Side::from(remote));
		let held = BTreeSet::from(["documents/sub/b.txt".to_string()]);
		let mut baseline = tree(&baseline);
		assert!(
			fold_dir_moves(
				SyncMode::TwoWay,
				&mut baseline,
				&mut local,
				&mut remote,
				&held,
				PassPaths::Whole,
			)
			.0
			.is_empty()
		);
	}

	/// A pushed directory move is carried at the scope a pass read, not just at the whole map. The
	/// two ends are all the detector is given here — the baseline row's path and the new local
	/// directory — which is what an observation that moved a directory names.
	#[test]
	fn a_dir_move_is_carried_at_the_scope_a_pass_read() {
		let (baseline, local, remote) = moved_tree(TreeIds::new(), "documents", "docs");
		let scope = BTreeSet::from(["docs".to_string(), "documents".to_string()]);
		let (mut whole_baseline, mut whole_local, mut whole_remote) = (
			tree(&baseline),
			Side::from(local.clone()),
			Side::from(remote.clone()),
		);
		let (mut scoped_baseline, mut scoped_local, mut scoped_remote) =
			(tree(&baseline), Side::from(local), Side::from(remote));
		let (by_whole, _) = fold_dir_moves(
			SyncMode::TwoWay,
			&mut whole_baseline,
			&mut whole_local,
			&mut whole_remote,
			&BTreeSet::new(),
			PassPaths::Whole,
		);
		let (by_scope, _) = fold_dir_moves(
			SyncMode::TwoWay,
			&mut scoped_baseline,
			&mut scoped_local,
			&mut scoped_remote,
			&BTreeSet::new(),
			PassPaths::Changed(&scope),
		);
		assert!(
			!by_whole.is_empty(),
			"the whole fold carried no directory move, so this compared nothing"
		);
		assert_eq!(by_whole, by_scope, "the narrowed fold missed the move");
		assert_eq!(whole_local, scoped_local);
		assert_eq!(whole_remote, scoped_remote);
	}

	/// What a change-scoped pass plans from carried sides: the fold at the pass's own set, then the
	/// reconcile at that set as the pass re-keys it — every move replayed over it, then the paths
	/// the fold hands back taken in.
	fn plan_scoped(
		mode: SyncMode,
		rows: &Baseline,
		mut local: Side<LocalNode>,
		mut remote: Side<RemoteNode>,
		mut decided: BTreeSet<String>,
	) -> Vec<SyncAction> {
		let mut rows = rows.clone();
		let (mut actions, stayed) = fold_dir_moves(
			mode,
			&mut rows,
			&mut local,
			&mut remote,
			&BTreeSet::new(),
			PassPaths::Changed(&decided),
		);
		for action in &actions {
			let (from, to) = action.endpoints();
			decided = decided
				.into_iter()
				.map(|path| moved_path(&path, from, to).unwrap_or(path))
				.collect();
		}
		decided.extend(stayed);
		actions.extend(
			reconcile(
				mode,
				&rows,
				&local.of(&rows),
				&remote.of(&rows),
				&PassHolds::default(),
				PassPaths::Changed(&decided),
			)
			.actions,
		);
		actions
	}

	/// The remote deleted a file and then moved its directory, inside one delta: the view
	/// tombstones the file where it was and carries the rest across. Folding the move moves the
	/// rows under BOTH sides, and a carried remote side that followed them held the deleted file
	/// again at its new path — so the pass planned nothing there, and no later scoped pass would
	/// either, where a whole pass deletes the local copy.
	#[test]
	fn a_file_the_remote_deleted_before_moving_its_directory_is_still_deleted() {
		let (dir, a, b) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
		let rows = [
			base_dir("docs", dir),
			base_file("docs/a.txt", a, [1; 32]),
			base_file("docs/b.txt", b, [2; 32]),
		];
		let baseline = Baseline::from_rows(rows.clone());
		let whole_baseline: HashMap<String, BaselineEntry> = rows
			.into_iter()
			.map(|row| (row.rel_path.clone(), row))
			.collect();
		let whole_local = map(vec![
			("docs", local_dir("docs")),
			("docs/a.txt", local_file("docs/a.txt", [1; 32])),
			("docs/b.txt", local_file("docs/b.txt", [2; 32])),
		]);
		let whole_remote = map(vec![
			("documents", remote_dir_node("documents", dir)),
			(
				"documents/a.txt",
				remote_file("documents/a.txt", a, [1; 32]),
			),
		]);
		let expected = vec![
			SyncAction::MoveLocal {
				from_path: "docs".to_string(),
				to_path: "documents".to_string(),
				kind: NodeKind::Dir,
			},
			SyncAction::DeleteLocal {
				rel_path: "documents/b.txt".to_string(),
				kind: NodeKind::File,
			},
		];
		for mode in [SyncMode::TwoWay, SyncMode::RemoteToLocal] {
			assert_eq!(
				plan_with_renames(mode, &whole_baseline, &whole_local, &whole_remote),
				expected,
				"whole, {mode:?}"
			);
			// What the delta leaves on a carried view: `b.txt` gone where it was, then the
			// directory and what is left in it moved across.
			let mut remote = Side::carried();
			for path in ["docs/b.txt", "docs", "docs/a.txt"] {
				remote.remove(&baseline, path);
			}
			remote.extend(whole_remote.clone());
			let decided = [
				"docs",
				"docs/a.txt",
				"docs/b.txt",
				"documents",
				"documents/a.txt",
			]
			.map(str::to_string)
			.into();
			assert_eq!(
				plan_scoped(mode, &baseline, Side::carried(), remote, decided),
				expected,
				"scoped, {mode:?}"
			);
		}
	}

	/// Both levels of a nested directory renamed by case on the local side, at a change-scoped
	/// pass. The outer rename moves the rows under the local side too, and a carried local side
	/// that followed them held the inner directory again under its old spelling — so the inner
	/// rename was never found, and the pass created the inner directory afresh instead.
	#[test]
	fn nested_case_only_dir_renames_fold_parent_first_at_a_pass_scope() {
		let (outer, inner, file) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
		let baseline = Baseline::from_rows([
			base_dir("Docs", outer),
			base_dir("Docs/Sub", inner),
			base_file("Docs/Sub/a.txt", file, [1; 32]),
		]);
		for mode in [SyncMode::TwoWay, SyncMode::LocalToRemote] {
			// What a pass holds once it observed the renames: the old spellings gone, the new
			// ones walked.
			let mut local = Side::carried();
			let mut decided = BTreeSet::new();
			for (old, new) in [
				("Docs", "docs"),
				("Docs/Sub", "docs/sub"),
				("Docs/Sub/a.txt", "docs/sub/a.txt"),
			] {
				local.remove(&baseline, old);
				decided.extend([old.to_string(), new.to_string()]);
			}
			local.extend([
				("docs".to_string(), local_dir("docs")),
				("docs/sub".to_string(), local_dir("docs/sub")),
				(
					"docs/sub/a.txt".to_string(),
					local_file("docs/sub/a.txt", [1; 32]),
				),
			]);
			assert_eq!(
				plan_scoped(mode, &baseline, local, Side::carried(), decided),
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
				],
				"{mode:?}"
			);
		}
	}

	/// A change-scoped pass folding a directory rename reads the renamed directory's rows a fixed
	/// number of times, whatever it holds. On the first iteration: once for the move sources and
	/// both sides' directories together, once for whether anything local is left at the source, and
	/// once for the source's signature — which the twin check and the new directories' match reuse
	/// rather than read again. On the second, which finds nothing: once more for the sources and the
	/// directories.
	#[test]
	fn a_scoped_directory_rename_fold_reads_the_renamed_rows_a_fixed_number_of_times() {
		const FILES: usize = 2_000;
		let dir = Uuid::new_v4();
		let files: Vec<(String, String)> = (0..FILES)
			.map(|n| (format!("dir/{n:05}.txt"), format!("moved_dir/{n:05}.txt")))
			.collect();
		let mut baseline = Baseline::from_rows(
			std::iter::once(base_dir("dir", dir)).chain(
				files
					.iter()
					.map(|(from, _)| base_file(from, Uuid::new_v4(), [1; 32])),
			),
		);
		// What a pass holds once it observed the rename: the old name gone, the new one walked.
		let (mut local, mut remote) = (Side::carried(), Side::carried());
		local.remove(&baseline, "dir");
		local.insert("moved_dir".to_string(), local_dir("moved_dir"));
		let mut decided = BTreeSet::from(["dir".to_string(), "moved_dir".to_string()]);
		for (from, to) in &files {
			local.remove(&baseline, from);
			local.insert(to.clone(), local_file(to, [1; 32]));
			decided.extend([from.clone(), to.clone()]);
		}
		let before = baseline.reads_for_test();
		let (moves, _) = fold_dir_moves(
			SyncMode::TwoWay,
			&mut baseline,
			&mut local,
			&mut remote,
			&BTreeSet::new(),
			PassPaths::Changed(&decided),
		);
		let after = baseline.reads_for_test();
		assert_eq!(
			moves,
			vec![SyncAction::MoveRemote {
				from_path: "dir".to_string(),
				to_path: "moved_dir".to_string(),
				kind: NodeKind::Dir,
				remote_uuid: dir,
			}]
		);
		let (statements, rows) = (after.0 - before.0, after.1 - before.1);
		assert!(
			rows < 5 * FILES,
			"the fold read {rows} row(s) in {statements} statement(s): more than four times the \
			 {FILES} file(s) it moved"
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

	/// One side renamed `z/` to `new/` and moved `b/` into it as `new/b`. Both are directory moves,
	/// the outer one first: `b` sorts before `z`, and folding `b` first would put it where `z` is
	/// going, leaving `z` to the per-path plan (a create, a move per file, and a deletion of `z`).
	#[test]
	fn a_dir_moved_into_another_moved_dir_folds_both_moves() {
		let (b, z) = (Uuid::new_v4(), Uuid::new_v4());
		let (b_file, z_file) = (Uuid::new_v4(), Uuid::new_v4());
		let baseline = map(vec![
			("b", base_dir("b", b)),
			("b/1.txt", base_file("b/1.txt", b_file, [1; 32])),
			("z", base_dir("z", z)),
			("z/2.txt", base_file("z/2.txt", z_file, [2; 32])),
		]);
		let paths = |z_at: &str, b_at: &str| {
			[
				z_at.to_string(),
				format!("{z_at}/2.txt"),
				b_at.to_string(),
				format!("{b_at}/1.txt"),
			]
		};
		let local = |z_at: &str, b_at: &str| {
			let [zd, zf, bd, bf] = paths(z_at, b_at);
			map(vec![
				(zd.as_str(), local_dir(&zd)),
				(zf.as_str(), local_file(&zf, [2; 32])),
				(bd.as_str(), local_dir(&bd)),
				(bf.as_str(), local_file(&bf, [1; 32])),
			])
		};
		let remote = |z_at: &str, b_at: &str| {
			let [zd, zf, bd, bf] = paths(z_at, b_at);
			map(vec![
				(zd.as_str(), remote_dir_node(&zd, z)),
				(zf.as_str(), remote_file(&zf, z_file, [2; 32])),
				(bd.as_str(), remote_dir_node(&bd, b)),
				(bf.as_str(), remote_file(&bf, b_file, [1; 32])),
			])
		};

		for mode in [SyncMode::TwoWay, SyncMode::RemoteToLocal] {
			assert_eq!(
				plan_with_renames(mode, &baseline, &local("z", "b"), &remote("new", "new/b")),
				vec![
					SyncAction::MoveLocal {
						from_path: "z".to_string(),
						to_path: "new".to_string(),
						kind: NodeKind::Dir,
					},
					SyncAction::MoveLocal {
						from_path: "b".to_string(),
						to_path: "new/b".to_string(),
						kind: NodeKind::Dir,
					},
				],
				"pull, {mode:?}"
			);
		}
		for mode in [SyncMode::TwoWay, SyncMode::LocalToRemote] {
			assert_eq!(
				plan_with_renames(mode, &baseline, &local("new", "new/b"), &remote("z", "b")),
				vec![
					SyncAction::MoveRemote {
						from_path: "z".to_string(),
						to_path: "new".to_string(),
						kind: NodeKind::Dir,
						remote_uuid: z,
					},
					SyncAction::MoveRemote {
						from_path: "b".to_string(),
						to_path: "new/b".to_string(),
						kind: NodeKind::Dir,
						remote_uuid: b,
					},
				],
				"push, {mode:?}"
			);
		}
	}

	/// `mv docs documents; rm -r backup`, where `backup` held only a file `docs` holds too. The
	/// subdirectory that moved along inside `docs` is not a move of its own, so `documents` without it
	/// must not pass for `backup`: `docs` is the move and `backup` is trashed.
	#[test]
	fn a_deleted_dir_does_not_claim_a_renamed_dir_that_holds_it() {
		let (backup, docs, sub) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
		let (backup_readme, docs_readme, f) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
		let baseline = map(vec![
			("backup", base_dir("backup", backup)),
			(
				"backup/readme.md",
				base_file("backup/readme.md", backup_readme, [1; 32]),
			),
			("docs", base_dir("docs", docs)),
			(
				"docs/readme.md",
				base_file("docs/readme.md", docs_readme, [1; 32]),
			),
			("docs/sub", base_dir("docs/sub", sub)),
			("docs/sub/f", base_file("docs/sub/f", f, [2; 32])),
		]);
		let local = map(vec![
			("documents", local_dir("documents")),
			(
				"documents/readme.md",
				local_file("documents/readme.md", [1; 32]),
			),
			("documents/sub", local_dir("documents/sub")),
			("documents/sub/f", local_file("documents/sub/f", [2; 32])),
		]);
		let remote = map(vec![
			("backup", remote_dir_node("backup", backup)),
			(
				"backup/readme.md",
				remote_file("backup/readme.md", backup_readme, [1; 32]),
			),
			("docs", remote_dir_node("docs", docs)),
			(
				"docs/readme.md",
				remote_file("docs/readme.md", docs_readme, [1; 32]),
			),
			("docs/sub", remote_dir_node("docs/sub", sub)),
			("docs/sub/f", remote_file("docs/sub/f", f, [2; 32])),
		]);
		for mode in [SyncMode::TwoWay, SyncMode::LocalToRemote] {
			let actions = plan_with_renames(mode, &baseline, &local, &remote);
			assert_eq!(
				dir_moves_in(&actions),
				vec![&SyncAction::MoveRemote {
					from_path: "docs".to_string(),
					to_path: "documents".to_string(),
					kind: NodeKind::Dir,
					remote_uuid: docs,
				}],
				"{mode:?}: {actions:?}"
			);
			assert!(
				actions.contains(&SyncAction::TrashRemote {
					rel_path: "backup".to_string(),
					kind: NodeKind::Dir,
					remote_uuid: backup,
				}),
				"{mode:?}: {actions:?}"
			);
		}
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
		// The unchanged `sub` inside may still move on its own, under the new `archive/documents`.
		assert!(
			moves_onto(&actions, "archive/documents").is_none(),
			"{actions:?}"
		);
	}

	fn ms(millis: i64) -> DateTime<Utc> {
		DateTime::from_timestamp_millis(millis).unwrap()
	}

	/// A remote file item with a fresh uuid, named `name` under `parent`.
	fn cacheable_file(parent: Uuid, name: &'static str) -> RemoteItem {
		let uuid = Uuid::new_v4();
		RemoteItem {
			uuid,
			parent,
			name: name.to_string(),
			stable_uuid: Some(filen_types::fs::StableUuid::new_for_test(uuid)),
			hash: Some(Blake3Hash::from([5; 32])),
			size: 5,
			modified_millis: 7,
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
		let mut baseline = tree(&map(vec![(
			"a.txt",
			base_file_pushed("a.txt", uuid, [1; 32], [0; 32]),
		)]));
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
			baseline.get("a.txt").unwrap().agreed_hash,
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
			let mut baseline = tree(&map(vec![("a.txt", row.clone())]));
			assert!(confirm_agreed_content(&mut baseline, &remote).is_empty());
			assert_eq!(
				baseline.get("a.txt").unwrap().agreed_hash,
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
		let mut baseline = tree(&map(vec![(
			"a.txt",
			BaselineEntry {
				state: BaselineState::Conflicted,
				..base_file_pushed("a.txt", uuid, [1; 32], [0; 32])
			},
		)]));
		let remote = map(vec![("a.txt", remote_file("a.txt", uuid, [1; 32]))]);
		assert!(confirm_agreed_content(&mut baseline, &remote).is_empty());
	}

	/// The tenure rule's other half: evidence from OUTSIDE the snapshot (the cache's announcement
	/// timestamps, or the server's version chain) names the version uuids it vouches for, and the
	/// same row rule applies to them.
	#[test]
	fn a_push_some_other_evidence_vouches_for_advances_the_agreed_content() {
		let uuid = Uuid::new_v4();
		let mut baseline = tree(&map(vec![
			("a.txt", base_file_pushed("a.txt", uuid, [1; 32], [0; 32])),
			(
				"b.txt",
				base_file_pushed("b.txt", Uuid::new_v4(), [2; 32], [0; 32]),
			),
		]));
		let advanced = confirm_agreed_pushes(&mut baseline, &HashSet::from([uuid]));

		assert_eq!(
			advanced
				.iter()
				.map(|e| e.rel_path.as_str())
				.collect::<Vec<_>>(),
			vec!["a.txt"],
			"only the vouched-for version's row moves"
		);
		let advanced_row = baseline.get("a.txt").unwrap();
		assert_eq!(advanced_row.agreed_hash, advanced_row.content_hash);
		assert_eq!(
			baseline.get("b.txt").unwrap().agreed_hash,
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
			let mut baseline = tree(&map(vec![("a.txt", row)]));
			assert!(confirm_agreed_pushes(&mut baseline, &HashSet::from([uuid])).is_empty());
			assert_eq!(baseline.get("a.txt").unwrap().agreed_hash, agreed);
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

		let rows =
			adopt_destination_rows(SyncMode::LocalToRemote, &tree(&baseline), &local, &remote);
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
			&tree(&baseline),
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

		let rows =
			adopt_destination_rows(SyncMode::LocalToRemote, &tree(&baseline), &local, &remote);
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
		assert!(
			adopt_destination_rows(SyncMode::TwoWay, &tree(&baseline), &local, &remote).is_empty()
		);
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

		let rows =
			adopt_destination_rows(SyncMode::RemoteToLocal, &tree(&baseline), &local, &remote);
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

	/// The re-seed adopts only from the sides it is handed, and `reconfigure_pair` hands it the two a
	/// pass reads — the FILTERED scan and view (pinned live by MODE-22c) — so an ignored path is at
	/// neither of them and nothing is adopted there, tracked or not. Were it adopted, the copy
	/// the rule hides would become an intended one, and the mirror would keep it for good instead of
	/// syncing the path like a first sync once the rule goes.
	#[test]
	fn the_reseed_adopts_nothing_at_an_ignored_path() {
		let (hidden, theirs) = (Uuid::new_v4(), Uuid::new_v4());
		// `secret.psd` was synced before a rule hid it; `theirs.psd` is a destination-only copy the
		// same rule hides. Neither is in the filtered sides the pass hands the re-seed.
		let baseline = map(vec![(
			"secret.psd",
			base_file("secret.psd", hidden, [1; 32]),
		)]);
		let (local, remote) = (map(vec![]), map(vec![]));
		for mode in [
			SyncMode::LocalToRemote,
			SyncMode::RemoteToLocal,
			SyncMode::TwoWay,
			SyncMode::LocalBackup,
			SyncMode::RemoteBackup,
		] {
			let rows = adopt_destination_rows(mode, &tree(&baseline), &local, &remote);
			assert!(rows.is_empty(), "{mode:?}: {rows:?}");
		}

		// The same switch on sides nothing hides: both copies ARE adopted, so it is the filtering
		// that decides, not the shape of the pair.
		let unfiltered = map(vec![
			("secret.psd", remote_file("secret.psd", hidden, [1; 32])),
			("theirs.psd", remote_file("theirs.psd", theirs, [2; 32])),
		]);
		let rows = adopt_destination_rows(
			SyncMode::LocalToRemote,
			&tree(&baseline),
			&local,
			&unfiltered,
		);
		let mut adopted: Vec<&str> = rows.iter().map(|r| r.rel_path.as_str()).collect();
		adopted.sort_unstable();
		assert_eq!(adopted, vec!["secret.psd", "theirs.psd"]);
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

	/// The push half of a two-way pass judges the rows the pull half's walk kept for it: a local
	/// rename is a remote move, and of two rows gone with the same content the first in path order
	/// is its source — as a walk of its own would have picked.
	#[test]
	fn a_two_way_local_rename_is_a_remote_move_and_the_first_same_hash_source_wins() {
		let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
		let baseline = map(vec![
			("a.txt", base_file("a.txt", a, [5; 32])),
			("b.txt", base_file("b.txt", b, [5; 32])),
		]);
		let remote = map(vec![
			("a.txt", remote_file("a.txt", a, [5; 32])),
			("b.txt", remote_file("b.txt", b, [5; 32])),
		]);
		let local = map(vec![("c.txt", local_file("c.txt", [5; 32]))]);
		let actions = plan(SyncMode::TwoWay, &baseline, &local, &remote);
		let moves: Vec<_> = actions
			.iter()
			.filter(|action| matches!(action, SyncAction::MoveRemote { .. }))
			.collect();
		assert_eq!(
			moves,
			vec![&SyncAction::MoveRemote {
				from_path: "a.txt".to_string(),
				to_path: "c.txt".to_string(),
				kind: NodeKind::File,
				remote_uuid: a,
			}],
			"{actions:?}"
		);
		assert!(
			!actions.contains(&SyncAction::UploadFile {
				rel_path: "c.txt".to_string()
			}),
			"the renamed file was uploaded again: {actions:?}"
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
				&tree(&baseline),
				&local,
				&remote,
				&PassHolds {
					trashed: HashSet::from([uuid]),
					..Default::default()
				},
				PassPaths::Whole,
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
				&Baseline::default(),
				&local,
				&remote,
				&PassHolds {
					trashed: HashSet::from([uuid]),
					..Default::default()
				},
				PassPaths::Whole,
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

	fn remote_dir(name: &str, parent: Uuid) -> RemoteItem {
		RemoteItem {
			uuid: Uuid::new_v4(),
			parent,
			name: name.to_string(),
			stable_uuid: None,
			hash: None,
			size: 0,
			modified_millis: 1,
		}
	}

	fn undecodable(parent: Uuid, stable_uuid: Option<StableUuid>) -> UndecodableItem {
		UndecodableItem {
			uuid: Uuid::new_v4(),
			parent,
			stable_uuid,
		}
	}

	#[test]
	fn a_traversal_name_never_enters_the_view_and_its_subtree_shares_its_record() {
		let root = Uuid::new_v4();
		// A direct child literally named ".." (it would escape the root on pull), and a normal
		// directory beside it.
		let evil = remote_dir("..", root);
		let ok = remote_dir("ok", root);
		// A child under the traversal-named dir is left out with it, recorded under its record.
		let under_evil = remote_dir("x", evil.uuid);
		let view = build_remote_view(
			root,
			&[evil.clone(), ok, under_evil.clone()],
			&[],
			&[],
			None,
		);
		assert_eq!(view.nodes.whole().paths().collect::<Vec<_>>(), vec!["ok"]);
		let record = |remote_uuid| SkippedRemote {
			remote_uuid,
			stable_uuid: None,
			rel_path: "..".to_string(),
			path_is_dir: true,
			reason: UnsyncableReason::RemoteInvalidName {
				name: "..".to_string(),
			},
		};
		assert_eq!(
			view.skipped,
			vec![record(evil.uuid), record(under_evil.uuid)]
		);
	}

	/// The cache's undecodable records carry every sync root's; only those under THIS root are the
	/// view's, placed at the directory that holds them — and one inside an undecodable directory
	/// takes that directory's record.
	#[test]
	fn undecodable_items_are_recorded_under_their_parent_when_they_belong_to_the_root() {
		let root = Uuid::new_v4();
		let sub = remote_dir("sub", root);
		let lineage = StableUuid::new_for_test(Uuid::new_v4());
		let in_sub = undecodable(sub.uuid, Some(lineage));
		let garbled_dir = undecodable(root, None);
		let under_garbled = undecodable(garbled_dir.uuid, Some(lineage));
		let elsewhere = undecodable(Uuid::new_v4(), None);
		let view = build_remote_view(
			root,
			std::slice::from_ref(&sub),
			&[],
			&[in_sub, garbled_dir, under_garbled, elsewhere],
			None,
		);
		assert_eq!(view.nodes.whole().paths().collect::<Vec<_>>(), vec!["sub"]);
		let recorded: Vec<(Uuid, &str)> = view
			.skipped
			.iter()
			.map(|s| {
				assert_eq!(s.reason, UnsyncableReason::RemoteUndecodable);
				(s.remote_uuid, s.rel_path.as_str())
			})
			.collect();
		assert_eq!(
			recorded,
			vec![
				(in_sub.uuid, "sub"),
				(garbled_dir.uuid, ""),
				(under_garbled.uuid, "")
			]
		);
		assert_eq!(view.skipped[0].stable_uuid, Some(lineage));
	}

	/// A synced item moved under a directory the view skips is still found, by its own uuid: its
	/// synced path is unknown, not absent, however deep under the skipped directory it sits. A synced
	/// directory's descendants add no line of their own, and the strangers under one skipped
	/// directory report that directory once.
	#[test]
	fn a_synced_item_moved_under_a_skipped_directory_is_unknown_not_absent() {
		let root = Uuid::new_v4();
		let evil = remote_dir("..", root);
		let moved = cacheable_file(evil.uuid, "a.txt");
		let stranger = cacheable_file(evil.uuid, "b.txt");
		let nested = remote_dir("deeper", evil.uuid);
		let deep = cacheable_file(nested.uuid, "c.txt");
		let garbled = undecodable(root, None);
		let in_garbled = undecodable(garbled.uuid, None);
		let view = build_remote_view(
			root,
			&[evil.clone(), nested.clone()],
			&[moved.clone(), stranger, deep.clone()],
			&[garbled, in_garbled],
			None,
		);
		assert!(
			view.nodes.whole().is_empty(),
			"{:?}",
			view.nodes.whole().paths().collect::<Vec<_>>()
		);
		let invalid = UnsyncableReason::RemoteInvalidName {
			name: "..".to_string(),
		};
		for uuid in [evil.uuid, moved.uuid, nested.uuid, deep.uuid] {
			let record = view
				.skipped
				.iter()
				.find(|s| s.remote_uuid == uuid)
				.unwrap_or_else(|| panic!("{uuid} not recorded: {:?}", view.skipped));
			assert_eq!((record.rel_path.as_str(), &record.reason), ("..", &invalid));
		}

		let baseline = HashMap::from([
			(
				"docs/a.txt".to_string(),
				base_file("docs/a.txt", moved.uuid, [1; 32]),
			),
			("sub".to_string(), base_dir("sub", nested.uuid)),
			(
				"sub/c.txt".to_string(),
				base_file("sub/c.txt", deep.uuid, [2; 32]),
			),
		]);
		let rows = tree(&baseline);
		let (unknown, never_synced) =
			unknown_remote_paths(&synced_for(&rows, &view.skipped), &view.skipped);
		assert_eq!(
			unknown,
			BTreeMap::from([
				("docs/a.txt".to_string(), invalid.clone()),
				("sub".to_string(), invalid.clone()),
			])
		);
		assert_eq!(
			never_synced,
			vec![
				UnsyncablePath {
					rel_path: String::new(),
					reason: UnsyncableReason::RemoteUndecodable,
				},
				UnsyncablePath {
					rel_path: "..".to_string(),
					reason: invalid,
				},
			]
		);
	}

	#[test]
	fn a_synced_path_whose_item_was_skipped_is_unknown_and_a_stranger_is_only_reported() {
		let synced_uuid = Uuid::new_v4();
		let lineage = StableUuid::new_for_test(synced_uuid);
		let baseline = HashMap::from([
			(
				"docs".to_string(),
				BaselineEntry {
					kind: NodeKind::Dir,
					remote_stable_uuid: None,
					..base_file("docs", Uuid::new_v4(), [0; 32])
				},
			),
			(
				"docs/a.txt".to_string(),
				base_file("docs/a.txt", synced_uuid, [1; 32]),
			),
		]);
		let skipped = vec![
			// A foreign same-name upload left `a.txt` as a NEW version the cache cannot decode: a
			// different uuid, the same lineage.
			SkippedRemote {
				remote_uuid: Uuid::new_v4(),
				stable_uuid: Some(lineage),
				rel_path: "docs".to_string(),
				path_is_dir: true,
				reason: UnsyncableReason::RemoteUndecodable,
			},
			// Two undecodable strangers in one directory report one line.
			SkippedRemote {
				remote_uuid: Uuid::new_v4(),
				stable_uuid: None,
				rel_path: "docs".to_string(),
				path_is_dir: true,
				reason: UnsyncableReason::RemoteUndecodable,
			},
			SkippedRemote {
				remote_uuid: Uuid::new_v4(),
				stable_uuid: None,
				rel_path: "docs".to_string(),
				path_is_dir: true,
				reason: UnsyncableReason::RemoteUndecodable,
			},
		];
		let rows = tree(&baseline);
		let (unknown, never_synced) = unknown_remote_paths(&synced_for(&rows, &skipped), &skipped);
		assert_eq!(
			unknown,
			BTreeMap::from([(
				"docs/a.txt".to_string(),
				UnsyncableReason::RemoteUndecodable
			)])
		);
		assert_eq!(
			never_synced,
			vec![UnsyncablePath {
				rel_path: "docs".to_string(),
				reason: UnsyncableReason::RemoteUndecodable,
			}]
		);
	}

	fn root_rules(text: &str) -> IgnoreRules {
		let (source, errors) = IgnoreSource::parse(text, Origin::File { dir: "" }).unwrap();
		assert!(errors.is_empty(), "{errors:?}");
		let mut rules = IgnoreRules::default();
		rules.insert_file(String::new(), source);
		rules
	}

	/// The rule the local scan follows, on the remote side: a BUILT-IN default hit is a root only
	/// where a row sits at or under it. The item is out of the view either way, so a `.DS_Store`
	/// synced before the defaults existed keeps its row, its report line and its untracking, while
	/// the ones nobody ever synced stop being carried as roots at all.
	#[test]
	fn a_default_rule_hit_on_the_remote_is_a_root_only_where_a_row_sits_at_or_under_it() {
		let root = Uuid::new_v4();
		let docs = remote_dir("docs", root);
		let dirs = [docs.clone()];
		let files = [
			cacheable_file(root, ".DS_Store"),
			cacheable_file(docs.uuid, ".DS_Store"),
			cacheable_file(docs.uuid, "keep.txt"),
		];
		// No lines of its own: the built-in defaults are what hides a `.DS_Store`.
		let rules = root_rules("");

		let untracked = build_remote_view(
			root,
			&dirs,
			&files,
			&[],
			Some(ViewFilter {
				rules: &rules,
				baseline: &Baseline::default(),
			}),
		);

		let mut paths: Vec<String> = untracked
			.nodes
			.whole()
			.paths()
			.map(|path| path.into_owned())
			.collect();
		paths.sort_unstable();
		assert_eq!(paths, ["docs", "docs/keep.txt"], "both are still hidden");
		assert!(
			untracked.ignored.is_empty(),
			"nothing was synced at either, so neither is a root: {:?}",
			untracked.ignored
		);
		assert_eq!(untracked.ignored_default_untracked, 2);

		let baseline = HashMap::from([(
			"docs/.DS_Store".to_string(),
			base_file("docs/.DS_Store", Uuid::new_v4(), [7; 32]),
		)]);

		let tracked = build_remote_view(
			root,
			&dirs,
			&files,
			&[],
			Some(ViewFilter {
				rules: &rules,
				baseline: &tree(&baseline),
			}),
		);

		assert_eq!(
			tracked.ignored.keys().collect::<Vec<_>>(),
			vec!["docs/.DS_Store"],
			"the one with a row is a root, and is reported and untracked as before"
		);
		assert_eq!(
			tracked.ignored_default_untracked, 1,
			"the root's own `.DS_Store` is still untracked"
		);
	}

	/// An ignored item is left out of the view with everything under it, and only the top of each
	/// ignored subtree is recorded. Ignored case-twins no longer refuse the pass, and an item the view
	/// cannot place inside an ignored directory is not reported: it is out of sync, not unsyncable.
	#[test]
	fn an_ignored_remote_item_leaves_the_view_and_only_its_top_is_recorded() {
		let root = Uuid::new_v4();
		let build = remote_dir("build", root);
		let deep = remote_dir("deep", build.uuid);
		let evil_in_build = remote_dir("..", build.uuid);
		let lineage = StableUuid::new_for_test(Uuid::new_v4());
		let dirs = [build.clone(), deep.clone(), evil_in_build];
		let files = [
			cacheable_file(build.uuid, "o.bin"),
			cacheable_file(deep.uuid, "x.bin"),
			cacheable_file(root, "a.TMP"),
			cacheable_file(root, "A.tmp"),
			cacheable_file(root, "keep.txt"),
		];
		let stranger = undecodable(root, None);
		let undecodables = [undecodable(build.uuid, Some(lineage)), stranger];

		let raw = build_remote_view(root, &dirs, &files, &undecodables, None);
		assert!(raw.has_collisions, "unfiltered, the twins collide");
		assert_eq!(raw.skipped.len(), 3, "{:?}", raw.skipped);
		assert!(raw.ignored.is_empty());

		let rules = root_rules("build/\n*.tmp");
		let view = build_remote_view(
			root,
			&dirs,
			&files,
			&undecodables,
			Some(ViewFilter {
				rules: &rules,
				baseline: &Baseline::default(),
			}),
		);
		assert_eq!(
			view.nodes.whole().paths().collect::<Vec<_>>(),
			vec!["keep.txt"]
		);
		assert!(!view.has_collisions);
		let by = |pattern: &str| IgnoreDecision {
			level: IgnoreLevel::File { dir: String::new() },
			pattern: pattern.to_string(),
		};
		assert_eq!(
			view.ignored,
			BTreeMap::from([
				("A.tmp".to_string(), by("*.tmp")),
				("a.TMP".to_string(), by("*.tmp")),
				("build".to_string(), by("build/")),
			])
		);
		assert_eq!(
			view.skipped,
			vec![SkippedRemote {
				remote_uuid: stranger.uuid,
				stable_uuid: None,
				rel_path: String::new(),
				path_is_dir: true,
				reason: UnsyncableReason::RemoteUndecodable,
			}]
		);
		let baseline = HashMap::new();
		let rows = tree(&baseline);
		let (_, never_synced) =
			unknown_remote_paths(&synced_for(&rows, &view.skipped), &view.skipped);
		assert_eq!(never_synced.len(), 1, "{never_synced:?}");
	}

	/// The narrowed filter a change-scoped pass runs asks the rules about the keys that pass
	/// DECIDED, and about nothing else.
	///
	/// Both directions have to hold or the narrowing is wrong in one of the two ways that matter. A
	/// decided path the rules hide has to leave with everything under it, exactly as the whole form
	/// drops it — otherwise a remote create inside an ignored directory would be reconciled and
	/// pulled. A path NO producer moved has to stay, which is the saving itself: its local half is
	/// carried from the same baseline row, so the pair reads as converged and the reconcile — which
	/// visits only the decided set — plans nothing at it.
	#[test]
	fn the_narrowed_filter_hides_the_decided_paths_and_leaves_the_carried_ones() {
		let root = Uuid::new_v4();
		let build = remote_dir("build", root);
		let deep = remote_dir("deep", build.uuid);
		let dirs = [build.clone(), deep.clone()];
		let files = [
			cacheable_file(build.uuid, "o.bin"),
			cacheable_file(deep.uuid, "x.bin"),
			cacheable_file(root, "keep.txt"),
		];
		let rules = root_rules("build/");
		let no_rows = Baseline::default();
		let filter = || ViewFilter {
			rules: &rules,
			baseline: &no_rows,
		};
		let decided = |paths: &[&str]| -> BTreeSet<String> {
			paths.iter().map(|path| (*path).to_string()).collect()
		};
		let sorted = |view: &RemoteView| {
			let mut paths: Vec<String> = view.nodes.whole().paths().map(Cow::into_owned).collect();
			paths.sort_unstable();
			paths
		};
		let all = [
			"build",
			"build/deep",
			"build/deep/x.bin",
			"build/o.bin",
			"keep.txt",
		];

		// Nothing the rules hide was decided: the whole ignored subtree is still carried, and the
		// pass reports no root of its own (the carried facts hold the ones earlier passes found).
		let mut carried = place_remote_items(root, &dirs, &files, &[]);
		carried.filter_changed(filter(), &decided(&["keep.txt"]));
		assert_eq!(
			sorted(&carried),
			all,
			"a node no producer moved must not cost the pass a rule match"
		);
		assert!(carried.ignored.is_empty(), "{:?}", carried.ignored);

		// The directory itself decided — a remote move of it into view — takes its subtree with it.
		let mut moved = place_remote_items(root, &dirs, &files, &[]);
		moved.filter_changed(filter(), &decided(&["build"]));
		assert_eq!(sorted(&moved), ["keep.txt"]);
		assert_eq!(moved.ignored.keys().collect::<Vec<_>>(), vec!["build"]);

		// One item created under an ALREADY ignored directory: only that key was decided, so only
		// it leaves — and the root recorded is the directory the rule names, not the item.
		let mut created = place_remote_items(root, &dirs, &files, &[]);
		created.filter_changed(filter(), &decided(&["build/deep/x.bin"]));
		assert_eq!(
			sorted(&created),
			["build", "build/deep", "build/o.bin", "keep.txt"]
		);
		assert_eq!(created.ignored.keys().collect::<Vec<_>>(), vec!["build"]);

		// The same three inputs read WHOLE: every hidden node goes, whatever was decided. This is
		// what the narrowed form is allowed to differ from, and the difference is the saving.
		let mut whole = place_remote_items(root, &dirs, &files, &[]);
		whole.filter(Some(filter()));
		assert_eq!(sorted(&whole), ["keep.txt"]);
	}

	/// The narrowed collision check folds each decided key against the other decided keys, the held
	/// paths and the CARRIED keys — the last asked of the baseline rather than by scanning the
	/// view — and refuses the pass on any of the three.
	///
	/// The last case in this test is the one the narrowing deliberately does not check: two keys
	/// the pass merely carried. A pass that finds a collision refuses, and a refusal forces the
	/// next pass to read whole, so no such pair can reach a scoped pass undetected. Pinned here so
	/// that reasoning has to be revisited if the refusal ever stops forcing a whole read.
	#[test]
	fn the_narrowed_collision_check_folds_every_key_a_decided_one_can_meet() {
		let root = Uuid::new_v4();
		let lower = cacheable_file(root, "note.txt");
		let upper = RemoteItem {
			uuid: Uuid::new_v4(),
			name: "Note.txt".to_string(),
			..lower.clone()
		};
		let no_rows = Baseline::default();
		let decided = |paths: &[&str]| -> BTreeSet<String> {
			paths.iter().map(|path| (*path).to_string()).collect()
		};

		// Against a CARRIED key: the row is what the carried node is, so the baseline answers.
		let rows = HashMap::from([(
			"note.txt".to_string(),
			base_file("note.txt", lower.uuid, [3; 32]),
		)]);
		let carried_rows = tree(&rows);
		let mut against_carried =
			place_remote_items(root, &[], &[lower.clone(), upper.clone()], &[]);
		against_carried.resolve_collisions_changed(&carried_rows, &decided(&["Note.txt"]));
		assert!(against_carried.has_collisions);
		assert!(
			!against_carried.nodes.whole().holds("Note.txt"),
			"the decided key is the loser; the carried one the pass is not touching stays"
		);
		assert!(against_carried.nodes.whole().holds("note.txt"));

		// Against another DECIDED key, with no row behind either.
		let mut both_decided = place_remote_items(root, &[], &[lower.clone(), upper.clone()], &[]);
		both_decided.resolve_collisions_changed(&no_rows, &decided(&["Note.txt", "note.txt"]));
		assert!(both_decided.has_collisions);
		assert_eq!(
			both_decided.nodes.whole().len(),
			1,
			"one of the twins leaves"
		);

		// Against a HELD path, which took its key when the view held it.
		let mut against_held = place_remote_items(root, &[], std::slice::from_ref(&upper), &[]);
		against_held.held_paths = decided(&["note.txt"]);
		against_held.resolve_collisions_changed(&no_rows, &decided(&["Note.txt"]));
		assert!(against_held.has_collisions);
		assert!(!against_held.nodes.whole().holds("Note.txt"));

		// Two keys the pass merely CARRIED: not folded, by the induction above.
		let mut carried_only = place_remote_items(root, &[], &[lower, upper], &[]);
		carried_only.resolve_collisions_changed(&carried_rows, &BTreeSet::new());
		assert!(
			!carried_only.has_collisions,
			"a pass that found this pair would have refused and forced the next read whole"
		);
		assert_eq!(carried_only.nodes.whole().len(), 2);
	}

	/// The narrowed filter reads the rows at the decided keys once for both of its halves: the
	/// rules need each key's node, and whether the view holds a key is what that read already
	/// answered. After a directory rename those keys are every row under the old name, which the
	/// collision check then reads once more out of the folded index, for the carried keys a
	/// decided one could fold onto.
	#[test]
	fn a_scoped_filter_reads_the_decided_rows_once_for_both_of_its_halves() {
		const FILES: usize = 2_000;
		let files: Vec<(String, String)> = (0..FILES)
			.map(|n| (format!("dir/{n:05}.txt"), format!("moved_dir/{n:05}.txt")))
			.collect();
		let baseline = Baseline::from_rows(
			std::iter::once(base_dir("dir", Uuid::new_v4())).chain(
				files
					.iter()
					.map(|(from, _)| base_file(from, Uuid::new_v4(), [1; 32])),
			),
		);
		// Both ends of every re-keyed path, as a pass records them; the remote holds the old ends.
		let mut decided = BTreeSet::from(["dir".to_string(), "moved_dir".to_string()]);
		for (from, to) in &files {
			decided.extend([from.clone(), to.clone()]);
		}
		let rules = IgnoreRules::default();
		let mut view = RemoteView {
			nodes: Side::carried(),
			has_collisions: false,
			held_paths: BTreeSet::new(),
			skipped: Vec::new(),
			ignored: BTreeMap::new(),
			ignored_default_untracked: 0,
		};
		let before = baseline.reads_for_test();
		view.filter_changed(
			ViewFilter {
				rules: &rules,
				baseline: &baseline,
			},
			&decided,
		);
		let after = baseline.reads_for_test();
		assert!(!view.has_collisions);
		let (statements, rows) = (after.0 - before.0, after.1 - before.1);
		assert!(
			rows < 5 * FILES / 2,
			"the filter read {rows} row(s) in {statements} statement(s): more than twice the \
			 {FILES} file(s) under the renamed directory"
		);
	}

	/// A root pattern of `*` empties the filtered view, rule file included, while the unfiltered one
	/// still lists the remote: the pass reads its rule files and an emptied remote from that one, or
	/// every such pass would read as a vanished remote.
	#[test]
	fn ignoring_everything_empties_only_the_filtered_view() {
		let root = Uuid::new_v4();
		let sub = remote_dir("sub", root);
		let files = [
			cacheable_file(root, ".filenignore"),
			cacheable_file(sub.uuid, "f.txt"),
		];
		let dirs = std::slice::from_ref(&sub);
		let raw = build_remote_view(root, dirs, &files, &[], None);
		assert_eq!(raw.nodes.whole().len(), 3);
		assert!(raw.nodes.whole().holds(".filenignore"));

		let view = build_remote_view(
			root,
			dirs,
			&files,
			&[],
			Some(ViewFilter {
				rules: &root_rules("*"),
				baseline: &Baseline::default(),
			}),
		);
		assert!(
			view.nodes.whole().is_empty(),
			"{:?}",
			view.nodes.whole().paths().collect::<Vec<_>>()
		);
		assert_eq!(
			view.ignored.keys().collect::<Vec<_>>(),
			vec![".filenignore", "sub"]
		);
	}

	/// The streamed build answers what the materialized one answers — the nodes, the held paths
	/// and the records for everything that could not be placed — including when every item
	/// arrives BEFORE the directory that holds it. That order is the one thing a read can do to a
	/// builder that a pair of slices cannot, and the statement promises no other.
	#[test]
	fn a_streamed_view_is_the_view_the_slices_build() {
		let root = Uuid::new_v4();
		let a = remote_dir("A", root);
		let deep = remote_dir("deep", a.uuid);
		// Legal on the server, unusable as one local path component: placed nowhere, and nothing
		// under it is placeable either.
		let bad = remote_dir("bad\\x", root);
		let leaf = cacheable_file(deep.uuid, "leaf.txt");
		let under_bad = cacheable_file(bad.uuid, "file.txt");
		let orphan = cacheable_file(Uuid::new_v4(), "orphan.txt");
		let undecodable = [UndecodableItem {
			uuid: Uuid::new_v4(),
			parent: deep.uuid,
			stable_uuid: None,
		}];

		let dirs = [a.clone(), deep.clone(), bad.clone()];
		let files = [leaf.clone(), under_bad.clone(), orphan.clone()];
		let materialized = place_remote_items(root, &dirs, &files, &undecodable);
		assert!(
			materialized.nodes.whole().holds("A/deep/leaf.txt") && materialized.skipped.len() == 4,
			"the fixture has to exercise placement AND every way of failing it: {:?} {:?}",
			materialized.nodes.whole().paths().collect::<Vec<_>>(),
			materialized.skipped,
		);

		let mut builder = ViewBuilder::with_capacity(root, 0);
		crate::cache::SnapshotSink::undecodable(&mut builder, undecodable.to_vec());
		// Children first, every parent last: the worst order a read could hand these over in.
		for (is_dir, item) in [
			(false, &leaf),
			(false, &under_bad),
			(false, &orphan),
			(true, &deep),
			(true, &bad),
			(true, &a),
		] {
			crate::cache::SnapshotSink::item(&mut builder, is_dir, item.clone());
		}
		let streamed = builder.finish();

		assert_eq!(streamed.nodes, materialized.nodes, "the same node set");
		assert_eq!(streamed.held_paths, materialized.held_paths);
		// The records are the same set; only their ORDER follows the order the rows arrived in,
		// and nothing reads them in order (`unknown_remote_paths` keys them by path).
		let sorted = |mut skipped: Vec<SkippedRemote>| {
			skipped.sort_by(|left, right| {
				(&left.rel_path, left.remote_uuid).cmp(&(&right.rel_path, right.remote_uuid))
			});
			skipped
		};
		assert_eq!(sorted(streamed.skipped), sorted(materialized.skipped));
	}

	/// A file recorded under an unplaceable DIRECTORY takes that directory's path, so the rules have
	/// to be asked about that path as the directory it is. A rule written for directories only
	/// (`bad*/`) hides the directory; asked as a file it missed, the record outlived the directory
	/// it describes, and every pass then reported the path the rule exists to silence.
	#[test]
	fn a_directory_only_rule_hides_what_was_recorded_under_an_unplaceable_directory() {
		let root = Uuid::new_v4();
		// Legal on the server, unusable as one local path component, so the view cannot place it.
		let bad = remote_dir("bad\\x", root);
		let child = cacheable_file(bad.uuid, "file.txt");
		let dirs = [bad];
		let files = [child];

		let placed = place_remote_items(root, &dirs, &files, &[]);
		assert!(
			placed.nodes.whole().is_empty(),
			"{:?}",
			placed.nodes.whole().paths().collect::<Vec<_>>()
		);
		assert_eq!(
			placed.skipped.len(),
			2,
			"the directory and the file that takes its record: {:?}",
			placed.skipped
		);
		assert!(
			placed.skipped.iter().all(|skip| skip.path_is_dir),
			"both records name the directory's path, so both are directory paths: {:?}",
			placed.skipped
		);

		let mut view = place_remote_items(root, &dirs, &files, &[]);
		view.filter(Some(ViewFilter {
			rules: &root_rules("bad*/"),
			baseline: &Baseline::default(),
		}));
		assert!(
			view.skipped.is_empty(),
			"the rule hides the directory, so nothing recorded under it is unsyncable: {:?}",
			view.skipped
		);
	}

	/// The placed view keeps BOTH halves of a case-only pair — the consumers that read the view
	/// before it is filtered see the remote as it is — and the filter is what resolves them and
	/// refuses the pass.
	#[test]
	fn the_placed_view_keeps_both_case_twins_and_the_filter_resolves_them() {
		let root = Uuid::new_v4();
		let lower = cacheable_file(root, "note.txt");
		let upper = RemoteItem {
			uuid: Uuid::new_v4(),
			name: "Note.txt".to_string(),
			..lower.clone()
		};
		let files = [lower, upper];

		let mut view = place_remote_items(root, &[], &files, &[]);
		let mut placed: Vec<String> = view
			.nodes
			.whole()
			.paths()
			.map(|path| path.into_owned())
			.collect();
		placed.sort_unstable();
		assert_eq!(placed, vec!["Note.txt", "note.txt"]);
		assert!(
			!view.has_collisions,
			"placement itself refuses nothing: the reads that come before the filter are about the \
			 remote as it is"
		);

		view.filter(None);
		assert!(view.has_collisions, "the filter is what refuses the pass");
		assert_eq!(view.nodes.whole().len(), 1, "the loser leaves the view");
	}

	/// A path the cache lists twice byte-identically is HELD, and stays held where a rule hides it:
	/// both halves are hidden together and nothing can act on the path, so the pass that finds the
	/// cache no longer mid-transition there is the one that untracks it and reports its rule. Until
	/// then the path is in neither `nodes` nor `ignored`.
	#[test]
	fn a_held_duplicate_the_rules_hide_stays_held_and_is_not_reported_as_ignored() {
		let root = Uuid::new_v4();
		let build = remote_dir("build", root);
		let twin = cacheable_file(build.uuid, "out.log");
		let second = RemoteItem {
			uuid: Uuid::new_v4(),
			..twin.clone()
		};
		let dirs = [build];
		let files = [twin, second];

		let mut view = place_remote_items(root, &dirs, &files, &[]);
		assert_eq!(
			view.held_paths
				.iter()
				.map(String::as_str)
				.collect::<Vec<_>>(),
			vec!["build/out.log"]
		);
		assert!(!view.nodes.whole().holds("build/out.log"));

		view.filter(Some(ViewFilter {
			rules: &root_rules("build/"),
			baseline: &Baseline::default(),
		}));
		assert_eq!(
			view.held_paths
				.iter()
				.map(String::as_str)
				.collect::<Vec<_>>(),
			vec!["build/out.log"],
			"a held path stays held whatever the rules say"
		);
		assert_eq!(
			view.ignored.keys().collect::<Vec<_>>(),
			vec!["build"],
			"the held path itself was never a node, so no rule reports it"
		);
	}

	#[test]
	fn build_remote_view_excludes_the_quarantine_dir_name() {
		let root = Uuid::new_v4();
		let trash = remote_dir(QUARANTINE_DIR, root);
		let view = build_remote_view(root, std::slice::from_ref(&trash), &[], &[], None);
		assert!(
			view.nodes.whole().is_empty(),
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

	/// The paths a pass decides are the union of all three sides, and the baseline's third of that
	/// union is the one part the two side maps do not already carry: a row for a path NEITHER side
	/// holds any more. Those rows are contributed by a walk of the tree rather than by materializing
	/// every row's path, so it is worth pinning that such a row is still reached — at the top level
	/// and nested under a directory both sides still hold — beside the keys the maps do carry.
	#[test]
	fn a_row_neither_side_holds_is_reconciled_beside_the_keys_the_sides_carry() {
		let (dir, kept, nested, top) = (
			Uuid::new_v4(),
			Uuid::new_v4(),
			Uuid::new_v4(),
			Uuid::new_v4(),
		);
		let baseline = map(vec![
			("d", base_dir("d", dir)),
			("d/kept.txt", base_file("d/kept.txt", kept, [1; 32])),
			// Gone from both sides, under a directory both sides still hold ...
			("d/gone.txt", base_file("d/gone.txt", nested, [2; 32])),
			// ... and at the top level.
			("gone.txt", base_file("gone.txt", top, [3; 32])),
		]);
		let local = map(vec![
			("d", local_dir("d")),
			("d/kept.txt", local_file("d/kept.txt", [1; 32])),
			// A key only one side carries, which no baseline row names.
			("d/fresh.txt", local_file("d/fresh.txt", [4; 32])),
		]);
		let remote = map(vec![
			("d", remote_dir_node("d", dir)),
			("d/kept.txt", remote_file("d/kept.txt", kept, [1; 32])),
		]);

		let mut actions = plan(SyncMode::TwoWay, &baseline, &local, &remote);
		actions.sort_by(|a, b| a.rel_path().cmp(b.rel_path()));
		assert_eq!(
			actions,
			vec![
				SyncAction::UploadFile {
					rel_path: "d/fresh.txt".to_string(),
				},
				SyncAction::AdoptBaseline {
					rel_path: "d/gone.txt".to_string(),
				},
				SyncAction::AdoptBaseline {
					rel_path: "gone.txt".to_string(),
				},
			],
			"a row neither side holds is retired; a path both sides hold unchanged plans nothing"
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
		let successor = RemoteItem {
			uuid: Uuid::new_v4(),
			..predecessor.clone()
		};
		let sibling = cacheable_file(root, "other.txt");

		let view = build_remote_view(
			root,
			&[],
			&[predecessor, successor, sibling.clone()],
			&[],
			None,
		);

		assert!(
			!view.has_collisions,
			"a cache in transition must not refuse the pass"
		);
		assert_eq!(view.held_paths, BTreeSet::from(["note.txt".to_string()]));
		assert!(
			!view.nodes.whole().holds("note.txt"),
			"neither half of the transition may be reconciled against"
		);
		assert_eq!(
			view.nodes.whole().at("other.txt").unwrap().remote_uuid,
			sibling.uuid,
			"every other path still syncs"
		);
	}

	/// The counter-example that rules out seeking to the nearest key at or before the path: with `a`
	/// and `a/b` recorded, the key before `a/c` is `a/b`, which is no ancestor of it, while the
	/// ancestor `a` lies further back. Walking the path's own ancestors finds it.
	#[test]
	fn a_root_lookup_walks_ancestors_rather_than_seeking_to_the_nearest_key() {
		let roots: BTreeSet<String> = ["a", "a/b"].map(String::from).into();

		assert!(
			at_or_under_root(&roots, "a/c"),
			"`a` is an ancestor of `a/c`"
		);
		assert!(at_or_under_root(&roots, "a"), "a root is at itself");
		assert!(at_or_under_root(&roots, "a/b/deep"));
		// Neighbours that merely share the prefix are not under it.
		for outside in ["ab", "a0", "a.", "b", ""] {
			assert!(!at_or_under_root(&roots, outside), "{outside:?}");
		}
	}

	/// `""` is the pair root: the ancestor walk never yields it and its subtree range holds nothing,
	/// which is exactly what `is_under(path, "")` says. A caller for which it means "everything is
	/// under it" has to say so itself.
	#[test]
	fn the_pair_root_covers_only_itself() {
		let roots: BTreeSet<String> = ["", "a"].map(String::from).into();

		assert!(at_or_under_root(&roots, ""), "the pair root is at itself");
		assert!(!is_under("x/y", ""));
		assert!(roots.range(subtree_bounds("")).next().is_none());
	}

	/// The subtree range is the subtree and nothing else: `/` is 0x2F and `0` is 0x30, so the
	/// half-open range stops exactly where the names that only share the prefix begin.
	#[test]
	fn a_subtree_range_holds_exactly_the_subtree() {
		let roots: BTreeSet<String> = ["a", "a/b", "a/b/c", "a.", "a0", "ab", "b"]
			.map(String::from)
			.into();

		let under: Vec<&str> = roots
			.range(subtree_bounds("a"))
			.map(String::as_str)
			.collect();

		assert_eq!(under, ["a/b", "a/b/c"]);
		assert!(roots.range(subtree_bounds("b")).next().is_none());
	}

	/// A case-only collision is a real one — the server does allow both names, and no 1:1 local
	/// mapping exists — so the whole-pass refusal stays.
	#[test]
	fn a_case_only_remote_collision_still_refuses_the_pass() {
		let root = Uuid::new_v4();
		let lower = cacheable_file(root, "note.txt");
		let upper = RemoteItem {
			uuid: Uuid::new_v4(),
			name: "Note.txt".to_string(),
			..lower.clone()
		};

		let view = build_remote_view(root, &[], &[lower, upper], &[], None);

		assert!(
			view.has_collisions,
			"a case-only collision is not a transition"
		);
		assert!(view.held_paths.is_empty());
	}

	#[test]
	fn build_remote_view_resolves_paths_and_flags_collisions() {
		let root = Uuid::new_v4();
		let sub = remote_dir("sub", root);
		let file = cacheable_file(sub.uuid, "f.txt");
		let orphan = RemoteItem {
			uuid: Uuid::new_v4(),
			parent: Uuid::new_v4(), // a parent not in the snapshot -> orphan, skipped
			..file.clone()
		};

		let view = build_remote_view(
			root,
			std::slice::from_ref(&sub),
			&[file.clone(), orphan.clone()],
			&[],
			None,
		);
		assert!(!view.has_collisions);
		let mut paths: Vec<String> = view
			.nodes
			.whole()
			.paths()
			.map(|path| path.into_owned())
			.collect();
		paths.sort();
		assert_eq!(
			paths,
			vec!["sub", "sub/f.txt"],
			"orphan skipped, path resolved"
		);
		assert_eq!(
			view.skipped,
			vec![SkippedRemote {
				remote_uuid: orphan.uuid,
				stable_uuid: orphan.stable_uuid,
				rel_path: "f.txt".to_string(),
				path_is_dir: false,
				reason: UnsyncableReason::RemoteBrokenParent,
			}],
			"the orphan is recorded, not silently dropped"
		);
		assert_eq!(
			view.nodes.whole().at("sub/f.txt").unwrap().remote_uuid,
			file.uuid
		);
		assert_eq!(
			view.nodes.whole().at("sub/f.txt").unwrap().content_hash,
			Some(Blake3Hash::from([5; 32]))
		);
	}

	/// The rows a test spells as a path-keyed map, as the pass's resident baseline.
	/// Where the rows record each skipped item, as a pass resolves it off the store before it asks.
	fn synced_for(baseline: &Baseline, skipped: &[SkippedRemote]) -> SyncedPaths {
		let (uuids, lineages) = skipped_ids(skipped);
		baseline.synced_paths(&uuids, &lineages)
	}

	fn tree(rows: &HashMap<String, BaselineEntry>) -> Baseline {
		Baseline::from_rows(rows.values().cloned())
	}

	/// A dirty-set reconcile plans exactly what a whole-map reconcile plans.
	///
	/// Generated rather than illustrative, because the narrowing is only safe by virtue of what the
	/// DERIVATION guarantees about the paths it leaves out. So the cases are built the way a pass
	/// builds them: [`derive::from_baseline`] carries the rows into the two maps, then mutations
	/// stand in for what the pass observed, each recording the paths it moved off their row — which
	/// is the whole of [`PassPaths::Changed`]'s contract. The corpus carries the shapes that break a
	/// naive narrowing: every row state the baseline can hold, sibling names that fold together,
	/// paths that are prefixes of one another, held conflicts, pushes no snapshot confirmed, moves
	/// on either side, and a directory rename for the fold to find.
	mod scoped {
		use rand::{Rng, SeedableRng, rngs::StdRng};

		use super::*;
		use crate::sync_engine::derive;

		/// How many generated cases each property runs over.
		const CASES: u64 = 400;

		/// The paths a case is drawn from, `(path, is_dir)`: nesting, two pairs of names that fold
		/// together (`a`/`A`, `a/b.txt`/`a/B.txt`), a name that is a string prefix of a sibling's
		/// (`a/b` and `a/b.txt`) and of another directory's (`a`, `ab`), a file and a directory
		/// sharing a stem (`z`, `z.txt`), a rule file, and a childless directory spelled two ways
		/// (`empty`, `Empty`) for the directory-move fold to have something it can actually carry.
		const TREE: &[(&str, bool)] = &[
			("a", true),
			("a/b", true),
			("a/b/c.txt", false),
			("a/b.txt", false),
			("a/B.txt", false),
			("a/c", true),
			("a/c/d.txt", false),
			("ab", true),
			("ab/x.txt", false),
			("A", true),
			("A/y.txt", false),
			("d", true),
			("d/e", true),
			("d/e/f", true),
			("d/e/f/g.txt", false),
			("d/.filenignore", false),
			("top.txt", false),
			("Top.txt", false),
			// Two fold pairs an ASCII-only lowering would keep apart: `Ä`/`ä`, and
			// `İ`, whose fold is a character LONGER than the name it came from.
			("\u{c4}.txt", false),
			("\u{e4}.txt", false),
			("\u{130}.txt", false),
			("i\u{307}.txt", false),
			("z", true),
			("z/1.txt", false),
			("z.txt", false),
			("empty", true),
			("Empty", true),
		];

		const MODES: [SyncMode; 5] = [
			SyncMode::TwoWay,
			SyncMode::LocalToRemote,
			SyncMode::LocalBackup,
			SyncMode::RemoteToLocal,
			SyncMode::RemoteBackup,
		];

		/// What a generated baseline row records. Every state a row can be in, plus the two
		/// half-recorded shapes a resolution writes on purpose.
		#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
		enum Shape {
			/// No row at all: a path the baseline has never tracked.
			NoRow,
			Synced,
			/// A push no snapshot has confirmed yet.
			Unconfirmed,
			/// What a `KeepLocal` resolution leaves: the remote half cleared.
			NoRemoteHalf,
			/// What a `KeepRemote` resolution leaves: the local half cleared.
			NoLocalHalf,
			Conflicted,
			Overwritten,
			Adopted,
		}

		const SHAPES: [Shape; 8] = [
			Shape::NoRow,
			Shape::Synced,
			Shape::Unconfirmed,
			Shape::NoRemoteHalf,
			Shape::NoLocalHalf,
			Shape::Conflicted,
			Shape::Overwritten,
			Shape::Adopted,
		];

		/// What a generated observation found — one path moved off the node its row carries.
		#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
		enum Change {
			LocalEdit,
			LocalGone,
			LocalNew,
			LocalFlip,
			RemoteEdit,
			RemoteGone,
			RemoteNew,
			RemoteFlip,
			LocalMove,
			RemoteMove,
		}

		const CHANGES: [Change; 10] = [
			Change::LocalEdit,
			Change::LocalGone,
			Change::LocalNew,
			Change::LocalFlip,
			Change::RemoteEdit,
			Change::RemoteGone,
			Change::RemoteNew,
			Change::RemoteFlip,
			Change::LocalMove,
			Change::RemoteMove,
		];

		fn index_of(path: &str) -> usize {
			TREE.iter()
				.position(|(candidate, _)| *candidate == path)
				.expect("the generated tree holds that path")
		}

		fn kind_at(index: usize) -> NodeKind {
			if TREE[index].1 {
				NodeKind::Dir
			} else {
				NodeKind::File
			}
		}

		fn uuid_at(index: usize) -> Uuid {
			Uuid::from_u128(1000 + index as u128)
		}

		fn hash_at(index: usize, version: u8) -> [u8; 32] {
			[(index as u8).wrapping_mul(7).wrapping_add(version); 32]
		}

		/// The row a converged pass would have written for `index`, bent into `shape`.
		fn row(index: usize, shape: Shape) -> Option<BaselineEntry> {
			let (path, is_dir) = TREE[index];
			let uuid = uuid_at(index);
			let mut entry = if is_dir {
				base_dir(path, uuid)
			} else {
				base_file(path, uuid, hash_at(index, 0))
			};
			match shape {
				Shape::NoRow => return None,
				Shape::Synced => {}
				Shape::Unconfirmed => entry.agreed_hash = None,
				Shape::NoRemoteHalf => {
					entry.remote_uuid = None;
					entry.remote_stable_uuid = None;
				}
				Shape::NoLocalHalf => {
					entry.local_mtime = None;
					entry.content_hash = None;
				}
				Shape::Conflicted => entry.state = BaselineState::Conflicted,
				Shape::Overwritten => entry.state = BaselineState::Overwritten,
				Shape::Adopted => entry.state = BaselineState::Adopted,
			}
			Some(entry)
		}

		fn local_node_at(index: usize, kind: NodeKind, version: u8) -> LocalNode {
			let (path, _) = TREE[index];
			match kind {
				NodeKind::Dir => local_dir(path),
				NodeKind::File => local_file(path, hash_at(index, version)),
			}
		}

		fn remote_node_at(index: usize, kind: NodeKind, version: u8) -> RemoteNode {
			let (path, _) = TREE[index];
			match kind {
				NodeKind::Dir => remote_dir_node(path, uuid_at(index)),
				// A new version of the same lineage where `version` moved, which is what an edit
				// announced on an existing file looks like.
				NodeKind::File => remote_version(
					path,
					uuid_at(index),
					Uuid::from_u128(9000 + index as u128 * 4 + u128::from(version)),
					hash_at(index, version),
				),
			}
		}

		fn at(path: &str, mut node: LocalNode) -> LocalNode {
			node.rel_path = path.to_string();
			node
		}

		fn at_remote(path: &str, mut node: RemoteNode) -> RemoteNode {
			node.rel_path = path.to_string();
			node
		}

		/// Put one observed change into the maps, and the paths it is evidence about into the set.
		fn observe(
			change: Change,
			index: usize,
			elsewhere: usize,
			baseline: &Baseline,
			local: &mut Side<LocalNode>,
			remote: &mut Side<RemoteNode>,
			decided: &mut BTreeSet<String>,
		) {
			let (path, _) = TREE[index];
			let (other, _) = TREE[elsewhere];
			let kind = kind_at(index);
			let flipped = match kind {
				NodeKind::Dir => NodeKind::File,
				NodeKind::File => NodeKind::Dir,
			};
			decided.insert(path.to_string());
			match change {
				Change::LocalEdit => {
					local.insert(path.to_string(), local_node_at(index, kind, 1));
				}
				Change::LocalGone => {
					local.remove(baseline, path);
				}
				Change::LocalNew => {
					local.insert(path.to_string(), local_node_at(index, kind, 0));
				}
				Change::LocalFlip => {
					local.insert(path.to_string(), local_node_at(index, flipped, 1));
				}
				Change::RemoteEdit => {
					remote.insert(path.to_string(), remote_node_at(index, kind, 1));
				}
				Change::RemoteGone => {
					remote.remove(baseline, path);
				}
				Change::RemoteNew => {
					remote.insert(path.to_string(), remote_node_at(index, kind, 0));
				}
				Change::RemoteFlip => {
					remote.insert(path.to_string(), remote_node_at(index, flipped, 1));
				}
				// A rename carries what the row records to another path. Both ends are evidence, so
				// both are decided — which is what the watcher's rename pair and the cache's
				// `Renamed` entry each give the pass.
				Change::LocalMove => {
					decided.insert(other.to_string());
					local.remove(baseline, path);
					local.insert(other.to_string(), at(other, local_node_at(index, kind, 0)));
				}
				Change::RemoteMove => {
					decided.insert(other.to_string());
					remote.remove(baseline, path);
					remote.insert(
						other.to_string(),
						at_remote(other, remote_node_at(index, kind, 0)),
					);
				}
			}
		}

		/// One generated case: what a change-scoped pass holds when it reaches the reconcile.
		struct Case {
			baseline: Baseline,
			local: Side<LocalNode>,
			remote: Side<RemoteNode>,
			decided: BTreeSet<String>,
			holds: PassHolds,
		}

		/// What the corpus exercised, so a generator that degenerated cannot pass by proving
		/// nothing.
		#[derive(Default)]
		struct Coverage {
			shapes: BTreeSet<Shape>,
			changes: BTreeSet<Change>,
			actions: usize,
			folds: usize,
		}

		fn generate(seed: u64, seen: &mut Coverage) -> Case {
			let mut rng = StdRng::seed_from_u64(seed);
			// Decided up front because it constrains the rows: a case-only directory rename is only
			// ever folded where nothing at either end is withheld, and a row that records one side
			// only withholds its path (`derive::Derived::held`). Drawn here, applied below.
			let stage_rename = rng.random_range(0..3) == 0;
			let renamed = [index_of("empty"), index_of("Empty")];
			let mut rows = Vec::new();
			for index in 0..TREE.len() {
				let shape = if stage_rename && renamed.contains(&index) {
					Shape::Synced
				} else {
					SHAPES[rng.random_range(0..SHAPES.len())]
				};
				seen.shapes.insert(shape);
				if let Some(entry) = row(index, shape) {
					rows.push(entry);
				}
			}
			let baseline = Baseline::from_rows(rows);
			// Exactly as a pass assembles them: the rows carried into both maps, and every row that
			// cannot stand in for itself in the set and held.
			let derive::Derived {
				mut local,
				mut remote,
				mut decided,
				held,
				..
			} = derive::from_baseline(&baseline, BTreeSet::new());
			for _ in 0..rng.random_range(0..7) {
				let change = CHANGES[rng.random_range(0..CHANGES.len())];
				seen.changes.insert(change);
				observe(
					change,
					rng.random_range(0..TREE.len()),
					rng.random_range(0..TREE.len()),
					&baseline,
					&mut local,
					&mut remote,
					&mut decided,
				);
			}
			// One directory spelled `empty` on disk and `Empty` on the remote, for the fold to
			// find. Staged outright: hitting both halves of a case-only rename at random is rare,
			// and the fold is one of the shapes this has to cover.
			if stage_rename {
				local.insert("empty".to_string(), local_dir("empty"));
				local.remove(&baseline, "Empty");
				remote.insert(
					"Empty".to_string(),
					remote_dir_node("Empty", uuid_at(index_of("Empty"))),
				);
				remote.remove(&baseline, "empty");
				decided.insert("empty".to_string());
				decided.insert("Empty".to_string());
			}
			let mut held_remote = held;
			// The cache showing one name twice withholds that path. A pass always decides such a
			// path: what withheld it was an announcement about it.
			if rng.random_range(0..4) == 0 {
				let path = TREE[rng.random_range(0..TREE.len())].0.to_string();
				decided.insert(path.clone());
				held_remote.insert(path);
			}
			// A path the set names that nothing sits on — a changelist entry for something created
			// and deleted between two passes. Spurious dirt must change nothing.
			if rng.random_range(0..3) == 0 {
				decided.insert(format!(
					"{}/never-synced",
					TREE[rng.random_range(0..TREE.len())].0
				));
			}
			Case {
				baseline,
				local,
				remote,
				decided,
				holds: PassHolds {
					held_remote,
					..PassHolds::default()
				},
			}
		}

		/// Both reconciles over one case's inputs, asserted equal down to the deferral count.
		fn assert_same_plan(seed: u64, mode: SyncMode, case: &Case, seen: &mut Coverage) {
			let Case {
				baseline,
				local,
				remote,
				decided,
				holds,
			} = case;
			let (local_ref, remote_ref) = (local.of(baseline), remote.of(baseline));
			let whole = reconcile(
				mode,
				baseline,
				&local_ref,
				&remote_ref,
				holds,
				PassPaths::Whole,
			);
			let scoped = reconcile(
				mode,
				baseline,
				&local_ref,
				&remote_ref,
				holds,
				PassPaths::Changed(decided),
			);
			assert_eq!(
				whole.actions, scoped.actions,
				"seed {seed}, {mode:?}: the narrowed reconcile planned something else"
			);
			assert_eq!(
				whole.deferred_paths, scoped.deferred_paths,
				"seed {seed}, {mode:?}: the narrowed reconcile counted different deferrals"
			);
			seen.actions += whole.actions.len();
		}

		#[test]
		fn a_dirty_set_reconcile_plans_what_a_whole_map_reconcile_plans() {
			let mut seen = Coverage::default();
			for seed in 0..CASES {
				let case = generate(seed, &mut seen);
				for mode in MODES {
					assert_same_plan(seed, mode, &case, &mut seen);
				}
			}
			assert_eq!(seen.shapes.len(), SHAPES.len(), "{:?}", seen.shapes);
			assert_eq!(seen.changes.len(), CHANGES.len(), "{:?}", seen.changes);
			assert!(
				seen.actions > 1_000,
				"the corpus planned only {} action(s), so the two plans agreed on almost nothing",
				seen.actions
			);
		}

		/// The case as it stands once the directory-move fold has run over it at `paths`, with the
		/// set re-keyed exactly as `Prepared::fold_dir_moves` re-keys it — every move replayed over
		/// it, then what the side a move did not carry recorded to stay put (see `Side::stay_put`)
		/// taken in — and the moves that took.
		fn fold(case: &Case, mode: SyncMode, paths: PassPaths<'_>) -> (Case, Vec<SyncAction>) {
			let mut baseline = case.baseline.clone();
			let mut local = case.local.clone();
			let mut remote = case.remote.clone();
			let mut decided = case.decided.clone();
			let (moves, stayed) = fold_dir_moves(
				mode,
				&mut baseline,
				&mut local,
				&mut remote,
				&case.holds.held_remote,
				paths,
			);
			for action in &moves {
				let (from, to) = action.endpoints();
				decided = decided
					.into_iter()
					.map(|path| moved_path(&path, from, to).unwrap_or(path))
					.collect();
			}
			decided.extend(stayed);
			let folded = Case {
				baseline,
				local,
				remote,
				decided,
				holds: PassHolds {
					held_remote: case.holds.held_remote.clone(),
					..PassHolds::default()
				},
			};
			(folded, moves)
		}

		/// The same property after the directory-move fold a pass runs first, at the same scope the
		/// pass gives both of them. The fold re-keys the maps and the baseline together, so the set
		/// has to follow them — which is what `Prepared::fold_dir_moves` does for the real pass, and
		/// what this pins.
		#[test]
		fn a_dirty_set_reconcile_agrees_once_a_directory_move_is_folded() {
			let mut seen = Coverage::default();
			for seed in 0..CASES {
				let case = generate(seed, &mut seen);
				for mode in MODES {
					let (after, moves) = fold(&case, mode, PassPaths::Changed(&case.decided));
					seen.folds += moves.len();
					assert_same_plan(seed, mode, &after, &mut seen);
				}
			}
			assert!(
				seen.folds > 100,
				"only {} directory move(s) were folded, so this tested almost nothing",
				seen.folds
			);
		}

		/// The rows a pass reads out of the store, held to the resident tree they used to be read
		/// out of, over every generated case: the case's own rows, then the same rows once the
		/// directory-move fold has run over them in each mode — the fold window, with the table
		/// still holding every moved row under its source — then a few more random moves and
		/// confirmations on top.
		///
		/// The expectation is the TREE, real code given the same rows and the same edits, not an
		/// answer this generator writes down: `rows::tests::assert_alike` asks both every question
		/// the boundary answers, at every path either side has held, every ancestor, every folded
		/// spelling and every string prefix that is no path prefix. The corpus is what reaches the
		/// shapes the hand-written ones cannot enumerate: case-only collisions, `Ä`/`ä` and `İ`,
		/// nested ignored roots, a decided path whose collision partner is not decided.
		#[test]
		fn the_store_backed_rows_answer_every_case_as_the_resident_tree_does() {
			use crate::sync_engine::{rows::tests as rows, tree::Tree};

			let mut seen = Coverage::default();
			let (mut folds, mut edits) = (0, 0);
			for seed in 0..CASES {
				let case = generate(seed, &mut seen);
				let held: Vec<BaselineEntry> = case.baseline.iter().collect();
				let paths = || {
					TREE.iter()
						.map(|(path, _)| path.to_string())
						.chain(case.decided.iter().cloned())
						.chain(["moved".to_string(), "a/moved/deep".to_string()])
				};
				let unedited = Tree::from_rows(held.clone());
				rows::assert_alike(
					&format!("seed {seed}"),
					&case.baseline,
					&unedited,
					&rows::probes(paths()),
				);
				for mode in MODES {
					let (mut after, moves) = fold(&case, mode, PassPaths::Changed(&case.decided));
					let mut oracle = unedited.clone();
					for action in &moves {
						let (from, to) = action.endpoints();
						oracle.move_subtree(from, to);
					}
					folds += moves.len();
					let mut probes = rows::probes(paths().chain(oracle.paths()));
					rows::assert_alike(
						&format!("seed {seed} {mode:?}, {} fold(s)", moves.len()),
						&after.baseline,
						&oracle,
						&probes,
					);
					if mode != SyncMode::TwoWay {
						continue;
					}
					let mut rng = StdRng::seed_from_u64(seed ^ 0x5eed);
					for step in 0..3 {
						let (what, _) = rows::random_edit(
							&mut rng,
							step,
							&mut after.baseline,
							&mut oracle,
							&probes,
						);
						edits += 1;
						probes = rows::probes(probes.into_iter().chain(oracle.paths()));
						rows::assert_alike(
							&format!("seed {seed}, then {what}"),
							&after.baseline,
							&oracle,
							&probes,
						);
					}
				}
			}
			assert!(
				folds > 100 && edits > 1_000,
				"only {folds} fold(s) and {edits} edit(s): this tested almost nothing"
			);
		}

		/// The fold at the pass's scope carries exactly the moves a whole-map fold carries, and
		/// leaves the three inputs in the same shape.
		///
		/// This is the half the property above cannot see: it reconciles whatever the fold left, so
		/// a narrowed fold that MISSED a move would still agree with itself — the reconcile would
		/// simply plan the delete-and-create the fold exists to avoid, at both ends, and both
		/// reconciles would plan it alike.
		#[test]
		fn a_scoped_directory_move_fold_carries_what_a_whole_one_carries() {
			let mut seen = Coverage::default();
			for seed in 0..CASES {
				let case = generate(seed, &mut seen);
				for mode in MODES {
					let (whole, by_whole) = fold(&case, mode, PassPaths::Whole);
					let (scoped, by_scope) = fold(&case, mode, PassPaths::Changed(&case.decided));
					seen.folds += by_whole.len();
					assert_eq!(
						by_whole, by_scope,
						"seed {seed}, {mode:?}: the narrowed fold carried other moves"
					);
					assert_eq!(
						whole.local, scoped.local,
						"seed {seed}, {mode:?}: the narrowed fold left another local side"
					);
					assert_eq!(
						whole.remote, scoped.remote,
						"seed {seed}, {mode:?}: the narrowed fold left another remote side"
					);
					assert_eq!(
						whole.decided, scoped.decided,
						"seed {seed}, {mode:?}: the narrowed fold re-keyed the set differently"
					);
				}
			}
			assert!(
				seen.folds > 100,
				"only {} directory move(s) were folded, so this tested almost nothing",
				seen.folds
			);
		}

		/// The two names of every fold class [`TREE`] spells twice — what a case-only sibling
		/// collision is generated out of, which is the one collision that loses user data (the
		/// server dedups on the lowercased name hash, so the second spelling never lands).
		///
		/// The last two pairs fold together only under [`collision_key`]'s full-Unicode lowering.
		/// An ASCII-only fold keeps both spellings apart, and the second pair's fold is LONGER
		/// than the name it came from (`U+0130` lowers to two scalars), so a check written with
		/// `to_ascii_lowercase` — or one that assumed a fold preserves length — stops refusing a
		/// pass the server would have collapsed.
		const TWINS: [(&str, &str); 6] = [
			("a", "A"),
			("a/b.txt", "a/B.txt"),
			("top.txt", "Top.txt"),
			("empty", "Empty"),
			("\u{c4}.txt", "\u{e4}.txt"),
			("\u{130}.txt", "i\u{307}.txt"),
		];

		/// The `.filenignore` bodies a view case is filtered with: the pair root's, and the one in
		/// `d` — the directory [`TREE`] holds a rule-file path for, so a case can be hidden by a
		/// level that is not the root's.
		///
		/// Set 3 is the nested pair: `d/e` and `d/e/f` both match a rule of their own, and only
		/// the top-most is ever the root a hit is recorded under. Set 5 hides through the file at
		/// depth alone, and nests inside it.
		const RULE_SETS: [(&str, &str); 6] = [
			("", ""),
			("z/\n", ""),
			("*.txt\n", ""),
			("d/e\nd/e/f\n", ""),
			("a\nab\n", ""),
			("", "e/f\ne/f/g.txt\n"),
		];

		fn rules_of(set: usize) -> IgnoreRules {
			let (at_root, at_d) = RULE_SETS[set];
			let mut rules = IgnoreRules::default();
			for (dir, text) in [("", at_root), ("d", at_d)] {
				if text.is_empty() {
					continue;
				}
				let (source, errors) = IgnoreSource::parse(text, Origin::File { dir })
					.expect("the generated rule text compiles");
				assert!(errors.is_empty(), "{errors:?}");
				rules.insert_file(dir.to_owned(), source);
			}
			rules
		}

		/// What a change-scoped pass reaches [`RemoteView::filter_changed`] with, plus everything
		/// the WHOLE form needs to be run over the very same input.
		struct ViewCase {
			nodes: Side<RemoteNode>,
			held_paths: BTreeSet<String>,
			skipped: Vec<SkippedRemote>,
			decided: BTreeSet<String>,
			rules: IgnoreRules,
			baseline: Baseline,
		}

		impl ViewCase {
			/// A fresh view over this case's input: each form is run over its own copy, so neither
			/// can be handed what the other left.
			fn view(&self) -> RemoteView {
				RemoteView {
					nodes: self.nodes.clone(),
					has_collisions: false,
					held_paths: self.held_paths.clone(),
					skipped: self.skipped.clone(),
					ignored: BTreeMap::new(),
					ignored_default_untracked: 0,
				}
			}

			fn filter(&self) -> ViewFilter<'_> {
				ViewFilter {
					rules: &self.rules,
					baseline: &self.baseline,
				}
			}
		}

		/// What the view corpus provably reached, counted off the FINISHED cases rather than off
		/// the generator's intentions — a shape that was staged and then scrubbed away again is
		/// not a shape the properties tested.
		#[derive(Debug, Default)]
		struct ViewCoverage {
			cases: usize,
			/// Fold classes the view still holds more than one spelling of, with at least one of
			/// them decided: the collision the narrowed check has to find.
			fold_classes: usize,
			/// Of those, the ones whose partner is NOT decided — answered out of the baseline's
			/// rows instead of by scanning the view.
			partner_not_decided: usize,
			/// Of those, the ones where a spelling is HELD rather than in the view.
			held_folds: usize,
			/// Of those, the ones an ASCII-only fold would have kept apart.
			non_ascii_folds: usize,
			/// Decided nodes the rules hide.
			hidden_decided: usize,
			/// Of those, the directories with nodes under them — the subtree the narrowed form
			/// drops without asking the rules about it.
			hidden_subtrees: usize,
			/// Of those, the ones that match a rule of their OWN and are still recorded under an
			/// ancestor's root: nested ignored roots.
			nested_roots: usize,
			/// Of those, the ones decided by a `.filenignore` below the pair root.
			depth_rule_hits: usize,
			/// Carried nodes the rules hide, counted as [`scrub`] REMOVES them. Not a bound on
			/// anything the properties saw: by construction they never see one.
			scrubbed_under_hidden: usize,
			/// Carried nodes folding onto a held path or onto another carried node, likewise
			/// counted as the scrub removes them.
			scrubbed_folded: usize,
		}

		impl ViewCoverage {
			/// The bounds every one of the three properties is only worth running above. Each is a
			/// shape the narrowing can be wrong at; a generator that stopped producing one would
			/// otherwise let the property pass by proving nothing.
			fn assert_reached(&self) {
				assert!(self.cases == CASES as usize, "{self:?}");
				assert!(self.fold_classes > 300, "{self:?}");
				assert!(self.partner_not_decided > 100, "{self:?}");
				assert!(self.held_folds > 70, "{self:?}");
				assert!(self.non_ascii_folds > 80, "{self:?}");
				assert!(self.hidden_decided > 350, "{self:?}");
				assert!(self.hidden_subtrees > 70, "{self:?}");
				assert!(self.nested_roots > 40, "{self:?}");
				assert!(self.depth_rule_hits > 30, "{self:?}");
				self.assert_scrub_stayed_active();
			}

			/// A different claim from the nine above, kept apart from them so it cannot be read as
			/// a tenth: these two count what [`scrub`] took OUT of each case, so they say the
			/// scrub is still doing its work — never that a property exercised either shape. The
			/// two shapes it removes are covered by
			/// [`a_scoped_filter_keeps_the_two_carried_shapes_a_whole_one_drops`] instead.
			fn assert_scrub_stayed_active(&self) {
				assert!(self.scrubbed_under_hidden > 350, "{self:?}");
				assert!(self.scrubbed_folded > 70, "{self:?}");
			}
		}

		/// Take the carried half of a view back to the state the narrowing's safety argument
		/// ASSUMES, which is the state the pass before this one left.
		///
		/// Both removals are things an earlier whole read already did. A carried node came off a
		/// baseline row, and a row only exists where the rules let the node through — the rules
		/// stand still across a scoped pass, so no carried node is hidden by them. And a pass that
		/// finds a fold REFUSES, which forces the next pass to read whole, so no two carried keys
		/// fold together either. Without this the properties would be comparing the narrowed form
		/// against inputs no pass can be handed, and the divergences they found would be the two
		/// this module pins deliberately in
		/// [`a_scoped_filter_keeps_the_two_carried_shapes_a_whole_one_drops`].
		fn scrub(case: &mut ViewCase, seen: &mut ViewCoverage) {
			let mut memo = HashMap::new();
			let held_keys: HashSet<String> = case
				.held_paths
				.iter()
				.map(|path| collision_key(path))
				.collect();
			let mut claimed: HashSet<String> = HashSet::new();
			// Sorted, so which spelling of a carried pair survives the scrub is not the hash
			// order of a map — the one thing the two forms are allowed to disagree about.
			let mut carried: Vec<(String, bool)> = Nodes::iter(&case.nodes.of(&case.baseline))
				.filter(|(path, _)| !case.decided.contains(path.as_ref()))
				.map(|(path, node)| (path.into_owned(), node.kind == NodeKind::Dir))
				.collect();
			carried.sort();
			for (path, is_dir) in carried {
				if case.rules.ignored_root(&path, is_dir, &mut memo).is_some() {
					seen.scrubbed_under_hidden += 1;
					case.nodes.remove(&case.baseline, &path);
					continue;
				}
				let key = collision_key(&path);
				if held_keys.contains(&key) || !claimed.insert(key) {
					seen.scrubbed_folded += 1;
					case.nodes.remove(&case.baseline, &path);
				}
			}
		}

		/// What the finished case actually holds, which is what the bounds above are read off.
		fn measure(case: &ViewCase, seen: &mut ViewCoverage) {
			seen.cases += 1;
			let mut classes: HashMap<String, Vec<String>> = HashMap::new();
			for path in case.nodes.of(&case.baseline).paths() {
				classes
					.entry(collision_key(&path))
					.or_default()
					.push(path.into_owned());
			}
			// A held path took its key when the view held it, so it is part of its fold class.
			for held in &case.held_paths {
				classes
					.entry(collision_key(held))
					.or_default()
					.push(held.clone());
			}
			for spellings in classes.into_values() {
				let decided = spellings
					.iter()
					.filter(|path| case.decided.contains(*path))
					.count();
				if spellings.len() < 2 || decided == 0 {
					continue;
				}
				seen.fold_classes += 1;
				if decided < spellings.len() {
					seen.partner_not_decided += 1;
				}
				if spellings.iter().any(|path| case.held_paths.contains(path)) {
					seen.held_folds += 1;
				}
				if spellings
					.iter()
					.any(|path| !path.eq_ignore_ascii_case(&spellings[0]))
				{
					seen.non_ascii_folds += 1;
				}
			}
			let mut memo = HashMap::new();
			for (path, node) in Nodes::iter(&case.nodes.of(&case.baseline)) {
				if !case.decided.contains(path.as_ref()) {
					continue;
				}
				let is_dir = node.kind == NodeKind::Dir;
				let Some((root, decision)) = case.rules.ignored_root(&path, is_dir, &mut memo)
				else {
					continue;
				};
				seen.hidden_decided += 1;
				if is_dir && !case.nodes.subtree_paths(&case.baseline, &path).is_empty() {
					seen.hidden_subtrees += 1;
				}
				if root != path && case.rules.decide(&path, is_dir).is_some() {
					seen.nested_roots += 1;
				}
				if matches!(decision.level, IgnoreLevel::File { ref dir } if !dir.is_empty()) {
					seen.depth_rule_hits += 1;
				}
			}
		}

		/// The remote half of one generated case, as a view the two filter forms can be run over.
		///
		/// The fold class is STAGED rather than left to the draw: two spellings of one name reach
		/// the collision check together far too rarely for a corpus this size to pin anything
		/// about them, which is the same reason [`generate`] stages its directory rename.
		fn view_case(seed: u64, case: &Case, seen: &mut ViewCoverage) -> ViewCase {
			// Its own stream, so the case's draws do not shift when this one changes.
			let mut rng = StdRng::seed_from_u64(seed ^ 0x7fff_ffff_0000_0001);
			let mut out = ViewCase {
				nodes: case.remote.clone(),
				held_paths: BTreeSet::new(),
				skipped: Vec::new(),
				decided: case.decided.clone(),
				rules: rules_of(rng.random_range(0..RULE_SETS.len())),
				baseline: case.baseline.clone(),
			};
			let place = |nodes: &mut Side<RemoteNode>, path: &str| {
				let index = index_of(path);
				nodes.insert(path.to_owned(), remote_node_at(index, kind_at(index), 0));
			};
			let (lower, upper) = TWINS[rng.random_range(0..TWINS.len())];
			match rng.random_range(0..4) {
				// Both spellings decided: what one pass's worth of remote announcements can
				// introduce on its own.
				0 => {
					for path in [lower, upper] {
						place(&mut out.nodes, path);
						out.decided.insert(path.to_owned());
					}
				}
				// One decided, its partner CARRIED — the case the narrowed check answers out of
				// the baseline's rows instead of by scanning the view, so the partner needs one.
				// Its node is built rather than carried off that row: `hide` and the collision
				// check read a node's path and its kind and nothing else, and both match the row.
				1 => {
					place(&mut out.nodes, lower);
					place(&mut out.nodes, upper);
					out.decided.insert(upper.to_owned());
					out.decided.remove(lower);
					out.baseline.upsert_for_test(
						&row(index_of(lower), Shape::Synced).expect("a synced row"),
					);
				}
				// One spelling HELD — the cache mid-transition — and the other decided. A held
				// path is always decided: what withheld it was an announcement about it.
				2 => {
					place(&mut out.nodes, upper);
					out.nodes.remove(&out.baseline, lower);
					for path in [lower, upper] {
						out.decided.insert(path.to_owned());
					}
					out.held_paths.insert(lower.to_owned());
				}
				_ => {}
			}
			// A decided path the rules HIDE, and — where it is a directory — its whole subtree
			// decided with it. That is the one shape a scoped pass ever pays a subtree walk for: a
			// remote create or move INTO an already-ignored directory, whose re-key records every
			// end. Left to the draw it happens too rarely to test the narrowed hide's own removal.
			if rng.random_range(0..2) == 0 {
				let mut memo = HashMap::new();
				let hidden: Vec<&str> = TREE
					.iter()
					.filter(|(path, is_dir)| {
						out.rules.ignored_root(path, *is_dir, &mut memo).is_some()
					})
					.map(|(path, _)| *path)
					.collect();
				if let Some(root) = hidden
					.get(rng.random_range(0..hidden.len().max(1)))
					.copied()
				{
					let under = format!("{root}/");
					for (path, _) in TREE
						.iter()
						.filter(|(path, _)| *path == root || path.starts_with(&under))
					{
						place(&mut out.nodes, path);
						out.decided.insert((*path).to_owned());
					}
				}
			}
			out.skipped = (0..rng.random_range(0..3u32))
				.map(|n| {
					let (path, is_dir) = TREE[rng.random_range(0..TREE.len())];
					SkippedRemote {
						remote_uuid: Uuid::from_u128(7000 + u128::from(n)),
						stable_uuid: None,
						rel_path: path.to_owned(),
						path_is_dir: is_dir,
						reason: match n % 3 {
							0 => UnsyncableReason::RemoteUndecodable,
							1 => UnsyncableReason::RemoteInvalidName {
								name: path.to_owned(),
							},
							_ => UnsyncableReason::RemoteBrokenParent,
						},
					}
				})
				.collect();
			scrub(&mut out, seen);
			measure(&out, seen);
			out
		}

		/// Every view case of the corpus, with the coverage the bounds are checked against.
		fn view_cases() -> (Vec<(u64, ViewCase)>, ViewCoverage) {
			let mut generated = Coverage::default();
			let mut seen = ViewCoverage::default();
			let cases = (0..CASES)
				.map(|seed| {
					(
						seed,
						view_case(seed, &generate(seed, &mut generated), &mut seen),
					)
				})
				.collect();
			(cases, seen)
		}

		fn sorted_paths(baseline: &Baseline, view: &RemoteView) -> Vec<String> {
			let mut paths: Vec<String> = view
				.nodes
				.of(baseline)
				.paths()
				.map(Cow::into_owned)
				.collect();
			paths.sort();
			paths
		}

		/// The names the surviving nodes CLAIM, folded the way the server dedups them. This is
		/// what the two collision checks have to agree on even where they dropped different
		/// spellings of one name.
		fn claimed_keys(baseline: &Baseline, view: &RemoteView) -> BTreeSet<String> {
			view.nodes
				.of(baseline)
				.paths()
				.map(|path| collision_key(&path))
				.collect()
		}

		/// Everything [`RemoteView::hide`] touches, at both scopes. Exact down to the path: the
		/// rules answer per node, so nothing here is free to depend on an iteration order.
		fn assert_same_hidden(
			seed: u64,
			baseline: &Baseline,
			whole: &RemoteView,
			scoped: &RemoteView,
		) {
			assert_eq!(
				sorted_paths(baseline, whole),
				sorted_paths(baseline, scoped),
				"seed {seed}: the narrowed hide left another set of nodes"
			);
			assert_eq!(
				whole.ignored, scoped.ignored,
				"seed {seed}: the narrowed hide recorded other ignored roots"
			);
			assert_eq!(
				whole.ignored_default_untracked, scoped.ignored_default_untracked,
				"seed {seed}: the narrowed hide counted other untracked default hits"
			);
			assert_eq!(
				whole.skipped, scoped.skipped,
				"seed {seed}: the narrowed hide kept other unplaceable records"
			);
		}

		/// Everything the two collision checks must agree on.
		///
		/// The REFUSAL is the contract at every case: a fold either leaves no 1:1 local mapping or
		/// it does not, and [`has_collisions`](RemoteView::has_collisions) is the whole of what the
		/// engine reads of this step. Where none fired, the two views must also be identical
		/// outright — there is no licence to differ at all.
		///
		/// Where one DID fire, the pass is refused before it reconciles anything and the view is
		/// dropped unread, so what a refused view still holds is not a contract — and the two forms
		/// do differ there. The narrowed one can empty a whole fold class where the whole form keeps
		/// a spelling: its losers leave only once the loop is over, so two decided twins each still
		/// find the other's row and node and both are dropped. What is still owed in that case is
		/// the direction a LOST refusal would show up in — the narrowed form never keeps a name the
		/// whole one dropped.
		fn assert_same_claimed(
			seed: u64,
			baseline: &Baseline,
			what: &str,
			whole: &RemoteView,
			scoped: &RemoteView,
		) {
			assert_eq!(
				whole.has_collisions, scoped.has_collisions,
				"seed {seed}, {what}: the two forms disagree about refusing the pass"
			);
			if whole.has_collisions {
				assert!(
					claimed_keys(baseline, scoped).is_subset(&claimed_keys(baseline, whole)),
					"seed {seed}, {what}: the narrowed form kept a name the whole one dropped"
				);
				return;
			}
			assert_eq!(
				sorted_paths(baseline, whole),
				sorted_paths(baseline, scoped),
				"seed {seed}, {what}: nothing was refused, so the two views must be identical"
			);
		}

		/// The narrowed ignore step hides exactly what the whole one hides.
		///
		/// The expectation comes from [`RemoteView::hide`] at [`PassPaths::Whole`] — the form a
		/// whole-read pass still runs in production, not a copy of it written here.
		#[test]
		fn a_scoped_hide_removes_what_a_whole_hide_removes() {
			let (cases, seen) = view_cases();
			for (seed, case) in &cases {
				let mut whole = case.view();
				whole.hide(case.filter(), PassPaths::Whole);
				let mut scoped = case.view();
				scoped.hide(case.filter(), PassPaths::Changed(&case.decided));
				assert_same_hidden(*seed, &case.baseline, &whole, &scoped);
			}
			seen.assert_reached();
		}

		/// The narrowed collision check refuses exactly what the whole one refuses, and leaves the
		/// same names claimed.
		#[test]
		fn a_scoped_collision_check_refuses_what_a_whole_one_refuses() {
			let (cases, seen) = view_cases();
			for (seed, case) in &cases {
				let mut whole = case.view();
				whole.resolve_collisions(&case.baseline);
				let mut scoped = case.view();
				// The decided keys the view holds, which is what `hide` hands over when it runs
				// first.
				let kept: Vec<&String> = case
					.decided
					.iter()
					.filter(|path| scoped.nodes.of(&case.baseline).holds(path))
					.collect();
				scoped.resolve_collisions_changed(&case.baseline, kept);
				assert_same_claimed(*seed, &case.baseline, "collisions", &whole, &scoped);
			}
			seen.assert_reached();
		}

		/// The two steps composed, in the order a pass runs them: the narrowed filter leaves the
		/// view the whole filter leaves.
		///
		/// Not implied by the two properties above. They each run over the view as it arrives,
		/// where this runs the collision check over whatever the ignore step left — and a
		/// narrowing that hid the wrong node would hand the fold a different set of names to
		/// resolve than the whole form ever sees.
		#[test]
		fn a_scoped_filter_leaves_the_view_a_whole_filter_leaves() {
			let (cases, seen) = view_cases();
			for (seed, case) in &cases {
				let mut whole = case.view();
				whole.filter(Some(case.filter()));
				let mut scoped = case.view();
				scoped.filter_changed(case.filter(), &case.decided);
				assert_same_claimed(*seed, &case.baseline, "filter", &whole, &scoped);
				assert_eq!(
					whole.ignored, scoped.ignored,
					"seed {seed}: the narrowed filter recorded other ignored roots"
				);
				assert_eq!(
					whole.ignored_default_untracked, scoped.ignored_default_untracked,
					"seed {seed}: the narrowed filter counted other untracked default hits"
				);
				assert_eq!(
					whole.skipped, scoped.skipped,
					"seed {seed}: the narrowed filter kept other unplaceable records"
				);
			}
			seen.assert_reached();
		}

		/// The two carried shapes the narrowed filter KEEPS where the whole one drops them — the
		/// exact divergence [`scrub`] takes out of the corpus above, on the record rather than
		/// left as a gap nobody wrote down.
		///
		/// Both are the safe direction. A carried node's LOCAL half comes off the same row, so the
		/// pair reads as converged there and a reconcile driven by the decided set plans nothing
		/// at it; dropping the remote half alone is what would read as a local deletion. The
		/// second shape only DELAYS a refusal: the held path is decided, so the pass after the one
		/// that unholds it folds it against the carried row and refuses then.
		#[test]
		fn a_scoped_filter_keeps_the_two_carried_shapes_a_whole_one_drops() {
			let of = |path: &str| {
				let index = index_of(path);
				(path.to_owned(), remote_node_at(index, kind_at(index), 0))
			};
			let case = ViewCase {
				nodes: ["z", "z/1.txt", "top.txt"].into_iter().map(of).collect(),
				// `Top.txt` listed twice by a cache mid-transition, so the view holds neither copy.
				held_paths: BTreeSet::from(["Top.txt".to_owned()]),
				skipped: Vec::new(),
				// Nothing moved off its row: every node here is one the pass CARRIED.
				decided: BTreeSet::new(),
				rules: rules_of(1),
				baseline: Baseline::from_rows([
					base_dir("z", uuid_at(index_of("z"))),
					base_file("z/1.txt", uuid_at(index_of("z/1.txt")), hash_at(0, 0)),
					base_file("top.txt", uuid_at(index_of("top.txt")), hash_at(1, 0)),
				]),
			};

			let mut whole = case.view();
			whole.filter(Some(case.filter()));
			assert!(
				sorted_paths(&case.baseline, &whole).is_empty(),
				"the whole form drops the rows left at a hidden path and refuses the folded one"
			);
			assert!(whole.has_collisions);
			assert_eq!(whole.ignored.keys().collect::<Vec<_>>(), vec!["z"]);

			let mut scoped = case.view();
			scoped.filter_changed(case.filter(), &case.decided);
			assert_eq!(
				sorted_paths(&case.baseline, &scoped),
				["top.txt", "z", "z/1.txt"],
				"a carried row at a hidden path, and one folding onto a held path, both stay"
			);
			assert!(
				!scoped.has_collisions,
				"the pass is not refused over a pair it is not reconciling"
			);
			assert!(
				scoped.ignored.is_empty(),
				"a root an earlier pass found is re-supplied by the carried facts, not re-derived"
			);
		}

		/// The same two shapes once the pass DECIDED their keys — a directory renamed away records
		/// every row under its old name — are asked about like any other decided key, and the
		/// narrowed filter drops them exactly as the whole one does: the hidden rows leave with
		/// their root recorded, and the row folding onto a held path refuses the pass. The nodes
		/// are carried off the rows here, so what the view holds at each key is the rows' answer.
		#[test]
		fn a_scoped_filter_drops_the_two_carried_shapes_once_they_are_decided() {
			let case = ViewCase {
				nodes: Side::carried(),
				held_paths: BTreeSet::from(["Top.txt".to_owned()]),
				skipped: Vec::new(),
				decided: ["top.txt", "z", "z/1.txt"]
					.into_iter()
					.map(str::to_owned)
					.collect(),
				rules: rules_of(1),
				baseline: Baseline::from_rows([
					base_dir("z", uuid_at(index_of("z"))),
					base_file("z/1.txt", uuid_at(index_of("z/1.txt")), hash_at(0, 0)),
					base_file("top.txt", uuid_at(index_of("top.txt")), hash_at(1, 0)),
				]),
			};

			let mut whole = case.view();
			whole.filter(Some(case.filter()));
			let mut scoped = case.view();
			scoped.filter_changed(case.filter(), &case.decided);
			for (form, view) in [("whole", &whole), ("scoped", &scoped)] {
				assert!(
					sorted_paths(&case.baseline, view).is_empty(),
					"{form}: the rows at a hidden path leave, and the folded one is refused"
				);
				assert!(view.has_collisions, "{form}");
				assert_eq!(view.ignored.keys().collect::<Vec<_>>(), vec!["z"], "{form}");
			}
		}
	}
}

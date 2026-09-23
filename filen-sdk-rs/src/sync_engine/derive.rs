//! The two path-keyed maps a change-scoped pass reconciles from, assembled out of the resident
//! baseline and the pass's own observations.
//!
//! A whole-tree pass reads both sides and knows everything. A change-scoped pass reads the paths
//! its changelists name and DERIVES the rest from the rows the last pass wrote: a row is the record
//! of an observation, and carrying it forward is what makes "nothing was announced here" mean
//! "nothing changed here".
//!
//! # A row answers for both sides or for neither
//!
//! The rule that keeps invariant I1 — absence is only ever produced by a stat or an event on that
//! path or an ancestor — is enforced in ONE place, [`carried`]: a row stands in for a pass only
//! when it records BOTH sides, and every row that does not is put in the dirty set (re-observed on
//! disk) and in the held set (no action may be planned at it or under it) in the same breath. There
//! is no path through this module that leaves a row out of a map without holding its path, so a
//! half-recorded row cannot read downstream as a deletion. A path the rules HIDE is the one other
//! row a pass cannot answer for, and [`merge_local`] takes it out of the local map under the same
//! rule: dropped so the two halves of a hidden path agree, held so the pair of absences over a
//! live row is not read as a convergent delete.
//!
//! The rows that record one side only are the ones the engine writes that way on purpose: a
//! `KeepLocal` resolution clears the local half and a `KeepRemote` one clears the remote half
//! (`engine::resolution_entry`), an [`Adopted`](super::baseline::BaselineState::Adopted) row
//! records whichever side the destination held, and a
//! [`Conflicted`](super::baseline::BaselineState::Conflicted) /
//! [`Overwritten`](super::baseline::BaselineState::Overwritten) row's two halves describe a
//! divergence rather than an agreement — an `Overwritten` row's remote half names the version this
//! engine BURIED, not what the remote holds now. Deriving either side from any of them would be a
//! guess.
//!
//! # What derivation cannot see
//!
//! A path the baseline has no row for is invisible here: a directory that exists on disk and was
//! never synced (a destination-only item's parent, say) is in a whole-tree scan's nodes and in no
//! derived map. That defers the CREATE such a path would plan to the next full pass; it can never
//! produce a deletion, because a path in neither map is a path the reconcile never decides.
//!
//! A directory's timestamps are the other gap, and the same shape. A row written for a pushed
//! directory records no local mtime and no remote stamp at all (`apply::dir_entry`), so a derived
//! directory node carries `0` where a whole-tree read carries the filesystem's mtime and the
//! server's created stamp; and a directory that is not itself dirty keeps the mtime its row
//! recorded even when a change inside it moved the real one. Neither stamp is evidence any part of
//! a pass reads — `plan::classify_local` and `plan::classify_remote` answer `Unchanged` for a
//! directory pair whatever their stamps say — and the tests below pin that by planning against the
//! derived maps and the whole-tree ones and comparing the plans.

use std::collections::BTreeSet;

use super::{
	baseline::{BaselineEntry, NodeKind},
	engine::written_node,
	observe::{LocalObservation, LocalObservations},
	plan::{self, RemoteNode},
	scan::LocalNode,
	side::{Nodes, Side},
	tree::Baseline,
};

/// What a pass reconciles, before its observations are merged in.
#[derive(Debug)]
pub(super) struct Derived {
	/// The local side: one node per row that records local evidence. [`merge_local`] then corrects
	/// it with what the pass observed on disk.
	pub(super) local: Side<LocalNode>,
	/// The remote side: one node per row that records a remote item. It is the input
	/// `remote::observe_remote` applies the announced changes to.
	pub(super) remote: Side<RemoteNode>,
	/// Every path this pass must look at: what the changelist named, plus every row that cannot
	/// stand in for itself.
	pub(super) dirty: BTreeSet<String>,
	/// Paths no action may be planned at or under, because no derived map describes them: the rows
	/// that record one side only, and the paths an observation found HIDDEN with a row still
	/// carried there ([`merge_local`]). Merged into the pass's
	/// [`held_remote`](super::plan::PassHolds::held_remote), which is what makes the missing side a
	/// deferral rather than an absence.
	pub(super) held: BTreeSet<String>,
	/// Every path the reconcile must DECIDE — what the pass hands it as
	/// [`PassPaths::Changed`](super::plan::PassPaths::Changed).
	///
	/// Not [`dirty`](Self::dirty) under another name. `dirty` says what to LOOK AT and answers for
	/// a whole subtree: one entry for a directory stands for the walk of everything under it. This
	/// says what to DECIDE and is exact, because a path missing from it is a path the reconcile
	/// does not visit — so it starts as the dirty set and grows by every key an observation
	/// actually moved off the node its row carries ([`merge_local`], `remote::RemoteObservation`,
	/// the cache-lag fold).
	pub(super) decided: BTreeSet<String>,
}

/// Carry the baseline forward into the two maps, and grow `dirty` by every row that cannot be
/// carried.
///
/// `dirty` comes in as the pass's own change-scoped set (`PassScope`'s local paths) and comes back
/// with the rows that record one side only added to it — which is what makes the "always in the
/// dirty set" rule a property of this function instead of a check every caller has to remember.
pub(super) fn from_baseline(baseline: &Baseline, dirty: BTreeSet<String>) -> Derived {
	let mut out = Derived {
		local: Side::with_capacity(baseline.len()),
		remote: Side::with_capacity(baseline.len()),
		// The pass decides everything it set out to look at, whether or not it found a change
		// there: a path the changelist named and the disk answered "unchanged" for costs one
		// no-op decision, and leaving it out would need the observation to be consulted first.
		decided: dirty.clone(),
		dirty,
		held: BTreeSet::new(),
	};
	// One walk, one classification: the maps and the dirty set come out of the same decision per
	// row, so a row can never be left out of a map without its path being held.
	baseline.visit_rows(|row| match carried(row) {
		Some((local, remote)) => {
			out.local.insert(row.rel_path.clone(), local);
			out.remote.insert(row.rel_path.clone(), remote);
		}
		None => {
			out.dirty.insert(row.rel_path.clone());
			out.held.insert(row.rel_path.clone());
			out.decided.insert(row.rel_path.clone());
		}
	});
	out
}

/// The two nodes a row stands in for, or `None` when it stands in for neither.
///
/// Both halves together, deliberately: a row that describes one side and not the other is exactly
/// the shape that fabricates an absence on the other, and returning them as a pair means no caller
/// can take the half it likes. See the module docs for which rows record one side only.
fn carried(row: &BaselineEntry) -> Option<(LocalNode, RemoteNode)> {
	// The RULE is [`BaselineEntry::carryable`], on the row, because the resident tree indexes the
	// rows that fail it and a carried side answers "does this side hold the path" off that index:
	// three readers, one field list (see there). What is left is the EXTRACTION, below.
	if !row.carryable() {
		return None;
	}
	extracted(row)
}

/// The two nodes a row's FIELDS describe, with no judgement about whether they may be carried.
///
/// Split from the gate on purpose. Inlined behind it, every row the rule refuses would extract
/// nothing BY CONSTRUCTION, so a test holding the rule and the extraction together could only
/// assert `false == false` over most of its corpus — it would pass whatever either one did. Apart,
/// the field half of the rule has a second implementation here, the `?`s, and
/// `the_carryable_rule_and_the_extraction_agree_on_every_row_shape` can fail in BOTH directions: a
/// rule grown looser than these fields, which would derive a side from a row that describes none,
/// and a rule grown stricter, which would strand its path in `dirty` and `held` on every pass.
fn extracted(row: &BaselineEntry) -> Option<(LocalNode, RemoteNode)> {
	// Names the remote item this row last recorded; `None` for a row whose remote half was cleared
	// or never written.
	let remote = written_node(Some(row))?;
	let local = match row.kind {
		// A directory has no content and no stamp a pass reads (see the module docs).
		NodeKind::Dir => LocalNode {
			rel_path: row.rel_path.clone(),
			kind: NodeKind::Dir,
			size: 0,
			mtime_millis: row.local_mtime.unwrap_or(0),
			content_hash: None,
		},
		// A file's every field IS read — the hash classifies it, the `(size, mtime)` pair is the
		// scanner's fast path, and the remote stamp is written back into the rows an adopt writes.
		// A row missing any of them describes no side fully, so it describes neither.
		NodeKind::File => {
			row.remote_modified?;
			LocalNode {
				rel_path: row.rel_path.clone(),
				kind: NodeKind::File,
				size: row.size?,
				mtime_millis: row.local_mtime?,
				content_hash: Some(row.content_hash?),
			}
		}
	};
	Some((local, remote))
}

/// Correct `local` with what the pass observed on disk — plan 3.4's step 3, local half.
///
/// The observations are keyed by the path they are ABOUT, which answers for everything under it
/// (`observe::LocalObservations::observed`), so they are applied nearest-ancestor-first: a
/// `BTreeMap` hands them over in path order and a path under an earlier key was never observed
/// separately.
///
/// A dirty path with no observation is NOT absent — it is a path this pass got no evidence for —
/// so its row stays carried, which is why this only ever acts on what was observed.
///
/// Every key it writes or drops goes into [`Derived::decided`] as it goes, which is what lets the
/// reconcile skip the rest: the nodes it did not touch are still the ones their rows carry.
pub(super) fn merge_local(
	derived: &mut Derived,
	baseline: &Baseline,
	observed: &LocalObservations,
) {
	for (at, observation) in &observed.observed {
		match observation {
			// Still a node, even where the remote would reject its name: the caller screens the
			// path out through the scan's `invalid_names`, and dropping it here would read as a
			// local deletion.
			LocalObservation::File { node, .. } => {
				derived.decided.insert(node.rel_path.clone());
				derived.local.insert(node.rel_path.clone(), node.clone());
			}
			LocalObservation::Dir(scan) => {
				// The walk of a directory is evidence for everything it reached — and only for
				// that. What it pruned (an ignored entry, a symlinked directory it reads under its
				// real path) it never looked at, and an incomplete walk may have missed anything,
				// so both keep their rows.
				if scan.complete {
					let pruned: BTreeSet<String> = observation.uncovered_roots().cloned().collect();
					drop_rows(derived, baseline, at, &pruned);
				}
				for (rel_path, node) in scan.nodes.iter() {
					derived.decided.insert(rel_path.to_string());
					derived
						.local
						.insert(rel_path.into_owned(), node.into_owned());
				}
			}
			// The one construct that means deletion: a `stat` on this very path answered NotFound,
			// so every row at or under it has no local node.
			LocalObservation::Absent(absence) => {
				drop_rows(derived, baseline, absence.path(), &BTreeSet::new());
			}
			// The rules hide it, and a walk of the directory above it would have pruned it, so the
			// local map must not go on carrying a node here: the view is filtered with the very same
			// rules a few lines later (`engine::prepare_scoped`), and a path that reads
			// local-present/remote-absent is a path the reconcile plans a local delete at.
			//
			// Dropped is not absent, though. Where a row was carried here the path is HELD as well:
			// two absences over a live row read as a convergent delete and retire it, and what sits
			// under a hidden root is the untrack-on-ignore path's to remove, which retries its own
			// delete every pass. What the rules normally hide has no row at all — the pass that
			// first hid it untracked what was there — and drops, holds and costs nothing here.
			LocalObservation::Hidden(_) => {
				if drop_rows(derived, baseline, at, &BTreeSet::new()) {
					derived.held.insert(at.clone());
				}
			}
		}
	}
}

/// Take the rows at or under `at` out of the local map, except those at or under a path the walk
/// pruned. Answers whether it took any, which is what tells a hidden path with rows still behind
/// it from one the rules have always hidden.
///
/// Driven by the baseline's own subtree rather than by the map, so it costs the dirty subtree and
/// not the whole tree. Nodes an EARLIER observation inserted are never under `at`: an observation
/// answers for everything below its key, and `observe_local` files them in ancestor-first order —
/// rewriting a dirty rule file onto its directory BEFORE it walks the set, so that directory is
/// never filed after an entry it holds — so no two of them nest.
fn drop_rows(
	derived: &mut Derived,
	baseline: &Baseline,
	at: &str,
	pruned: &BTreeSet<String>,
) -> bool {
	let mut dropped = false;
	if !at.is_empty() && !plan::at_or_under_root(pruned, at) && derived.local.remove(at).is_some() {
		derived.decided.insert(at.to_owned());
		dropped = true;
	}
	for row in baseline.subtree(at) {
		if !plan::at_or_under_root(pruned, &row.rel_path)
			&& derived.local.remove(&row.rel_path).is_some()
		{
			derived.decided.insert(row.rel_path);
			dropped = true;
		}
	}
	dropped
}

#[cfg(test)]
mod tests {
	use super::super::side::NodesAt;
	use std::{
		collections::BTreeMap,
		fs,
		path::{Path, PathBuf},
	};

	use filen_types::{crypto::Blake3Hash, fs::StableUuid};
	use uuid::Uuid;

	use super::*;
	use crate::{
		cache::{CacheEvent, CacheEventType, FileEvent, RemoteItem},
		sync_engine::{
			SyncMode,
			baseline::BaselineState,
			changes::{
				PairChanges, RemoteDeltaEntry,
				tests::{cache_event, cacheable_file},
			},
			facts::carry_over,
			ignore::{FILENIGNORE, IgnoreRules},
			observe::observe_local,
			plan::{PassHolds, RemoteView, SyncAction, place_remote_items},
			remote::{RemoteObserved, observe_remote},
			scan::{LocalScan, RuleFiles, scan_local},
		},
	};

	/// The pair's remote root: every derived path is relative to it.
	const REMOTE_ROOT: Uuid = Uuid::from_u128(1);

	/// A converged pair: a real tree on disk, the remote items that mirror it, and the baseline the
	/// pass after convergence would have written — rows shaped exactly as `apply` writes them.
	struct Pair {
		root: PathBuf,
		/// One uuid per synced path, so the remote view and the rows agree on identity.
		uuids: BTreeMap<String, Uuid>,
		rows: Vec<BaselineEntry>,
		items: Vec<RemoteItem>,
	}

	fn temp_root() -> PathBuf {
		let dir = std::env::temp_dir().join(format!("filen_derive_test_{}", Uuid::new_v4()));
		fs::create_dir_all(&dir).unwrap();
		dir
	}

	fn write(root: &Path, rel_path: &str, body: &[u8]) {
		let path = root.join(rel_path);
		if let Some(parent) = path.parent() {
			fs::create_dir_all(parent).unwrap();
		}
		fs::write(path, body).unwrap();
	}

	fn scan(root: &Path, baseline: &Baseline) -> LocalScan {
		scan_local(root, baseline, IgnoreRules::default(), RuleFiles::Read).0
	}

	/// The remote item one scanned node stands for, keyed by the uuids `Pair` handed out: a file
	/// carries the content the scan hashed, a directory carries nothing but its name.
	fn item_of(node: &LocalNode, uuids: &BTreeMap<String, Uuid>) -> RemoteItem {
		let (dir, name) = node
			.rel_path
			.rsplit_once('/')
			.map_or(("", node.rel_path.as_str()), |(dir, name)| (dir, name));
		let uuid = uuids[&node.rel_path];
		RemoteItem {
			uuid,
			parent: if dir.is_empty() {
				REMOTE_ROOT
			} else {
				uuids[dir]
			},
			name: name.to_owned(),
			stable_uuid: (node.kind == NodeKind::File).then(|| StableUuid::new_for_test(uuid)),
			hash: node.content_hash,
			size: if node.kind == NodeKind::File {
				node.size
			} else {
				0
			},
			modified_millis: if node.kind == NodeKind::File {
				node.mtime_millis
			} else {
				99
			},
		}
	}

	fn view(items: &[RemoteItem]) -> RemoteView {
		let (dirs, files): (Vec<RemoteItem>, Vec<RemoteItem>) = items
			.iter()
			.cloned()
			.partition(|item| item.stable_uuid.is_none());
		let mut view = place_remote_items(REMOTE_ROOT, &dirs, &files, &[]);
		view.filter(None);
		view
	}

	/// The row a converged pass writes for one path, shaped as `apply`'s own writers shape it: a
	/// directory records no content and no remote stamp, a file records both sides whole.
	fn synced_row(local: &LocalNode, remote: &RemoteNode) -> BaselineEntry {
		let is_file = local.kind == NodeKind::File;
		BaselineEntry {
			rel_path: local.rel_path.clone(),
			kind: local.kind,
			remote_uuid: Some(remote.remote_uuid),
			content_hash: is_file.then_some(local.content_hash).flatten(),
			size: is_file.then_some(local.size),
			local_mtime: Some(local.mtime_millis),
			remote_modified: is_file.then_some(remote.modified_millis),
			state: BaselineState::Synced,
			local_kind: None,
			remote_kind: None,
			remote_hash: None,
			remote_size: None,
			remote_stable_uuid: remote.stable_uuid,
			agreed_hash: is_file.then_some(local.content_hash).flatten(),
		}
	}

	impl Pair {
		/// A tree with a bit of everything a scan keys: nesting, a non-ASCII name, an empty
		/// directory, and files whose content differs.
		fn converged() -> Self {
			let root = temp_root();
			write(&root, "top.txt", b"top");
			write(&root, "docs/notes.txt", b"notes");
			write(&root, "docs/deep/inner.bin", b"inner-bytes");
			write(&root, "docs/Ärger.txt", b"umlaut");
			fs::create_dir_all(root.join("empty")).unwrap();

			let scanned = scan(&root, &Baseline::default());
			assert!(scanned.complete, "{:?}", scanned.errors);
			let mut paths: Vec<String> = scanned
				.nodes
				.paths()
				.map(|path| path.into_owned())
				.collect();
			paths.sort();
			let uuids: BTreeMap<String, Uuid> = paths
				.iter()
				.enumerate()
				.map(|(at, path)| (path.clone(), Uuid::from_u128(100 + at as u128)))
				.collect();
			let items: Vec<RemoteItem> = scanned
				.nodes
				.iter()
				.map(|(_, node)| item_of(&node, &uuids))
				.collect();
			let remote = view(&items);
			let rows = scanned
				.nodes
				.iter()
				.map(|(_, node)| {
					let at = remote.nodes.at(&node.rel_path).expect("the view holds it");
					synced_row(&node, &at)
				})
				.collect();
			Self {
				root,
				uuids,
				rows,
				items,
			}
		}

		fn baseline(&self) -> Baseline {
			Baseline::from_rows(self.rows.clone())
		}

		/// What a whole-tree pass reads: the scan of the tree as it is, and the view of the items as
		/// they are.
		fn whole_tree(&self, baseline: &Baseline) -> (Side<LocalNode>, RemoteView) {
			let scanned = scan(&self.root, baseline);
			assert!(scanned.complete, "{:?}", scanned.errors);
			(scanned.nodes, view(&self.items))
		}

		/// What a change-scoped pass assembles: the baseline carried forward, the announced remote
		/// changes applied to it, and the dirty paths re-observed on disk.
		fn derived(
			&self,
			baseline: &Baseline,
			dirty: BTreeSet<String>,
			delta: &[RemoteDeltaEntry],
		) -> Derived {
			let mut derived = from_baseline(baseline, dirty);
			let mut ancestry = |uuid: Uuid| -> rusqlite::Result<Vec<RemoteItem>> {
				panic!("no ancestry read was expected, but one was made for {uuid}")
			};
			let mut observed = match observe_remote(
				REMOTE_ROOT,
				baseline,
				std::mem::take(&mut derived.remote),
				delta,
				&mut ancestry,
			) {
				RemoteObserved::Applied(observation) => *observation,
				RemoteObserved::Full(reason) => panic!("expected a derived view: {reason}"),
			};
			derived.remote = observed.nodes;
			derived.held.extend(observed.held_paths);
			derived.dirty.extend(observed.touched);
			// The exact keys the delta moved, as `prepare_scoped` records them: what the pass
			// DECIDES, where `dirty` says where to look.
			derived.decided.append(&mut observed.changed);
			let (local, _) = observe_local(
				&self.root,
				baseline,
				IgnoreRules::default(),
				&RuleFiles::Read,
				&derived.dirty,
			);
			assert!(local.complete, "{:?}", local.errors);
			merge_local(&mut derived, baseline, &local);
			derived
		}
	}

	impl Drop for Pair {
		fn drop(&mut self) {
			fs::remove_dir_all(&self.root).ok();
		}
	}

	/// The dirty set a pass starts from, in the shape [`from_baseline`] takes it.
	fn dirty_paths(dirty: &[&str]) -> BTreeSet<String> {
		dirty.iter().map(|path| (*path).to_string()).collect()
	}

	/// The delta a pass takes from its changelist for those events — minted through
	/// `PairChanges`, which is the one way a `Gone` can exist at all.
	fn delta_of(events: &[CacheEvent<'static>]) -> Vec<RemoteDeltaEntry> {
		let changes = PairChanges::new();
		changes.note_tree_size(1_000);
		changes.note_remote_batch(&mut events.iter());
		changes.take().take_remote()
	}

	/// What the reconcile makes of the maps a change-scoped pass derived — over the paths that
	/// pass DECIDED, so a plan here is one a narrowed read really produces.
	fn scoped_plan(baseline: &Baseline, derived: &Derived) -> Vec<SyncAction> {
		let holds = PassHolds {
			held_remote: derived.held.clone(),
			..PassHolds::default()
		};
		plan::reconcile(
			SyncMode::TwoWay,
			baseline,
			&derived.local,
			&derived.remote,
			&holds,
			plan::PassPaths::Changed(&derived.decided),
		)
		.actions
	}

	fn sorted(keys: impl IntoIterator<Item = String>) -> Vec<String> {
		let mut keys: Vec<String> = keys.into_iter().collect();
		keys.sort();
		keys
	}

	/// The two maps, node for node, against what a whole-tree read produces — with the one
	/// difference derivation cannot avoid named rather than skipped: a directory row records no
	/// remote stamp, so a derived directory carries `0` where the view carries the server's created
	/// stamp (see the module docs).
	fn assert_same_maps(derived: &Derived, local: &Side<LocalNode>, remote: &Side<RemoteNode>) {
		assert_eq!(
			sorted(derived.local.paths().map(|path| path.into_owned())),
			sorted(local.paths().map(|path| path.into_owned())),
			"the derived local map holds exactly the paths a whole-tree scan does"
		);
		for (path, node) in local.iter() {
			let at = derived.local.at(&path).expect("the derived map holds it");
			let (derived_node, node) = (&*at, &*node);
			if node.kind == NodeKind::Dir {
				assert_eq!(
					derived_node,
					&LocalNode {
						mtime_millis: derived_node.mtime_millis,
						..node.clone()
					},
					"local directory at {path:?} differs by more than its mtime"
				);
			} else {
				assert_eq!(derived_node, node, "local node at {path:?}");
			}
		}
		let visible: Vec<String> = remote
			.paths()
			.filter(|path| !derived.held.contains(path.as_ref()))
			.map(|path| path.into_owned())
			.collect();
		assert_eq!(
			sorted(derived.remote.paths().map(|path| path.into_owned())),
			sorted(visible),
			"the derived remote map holds every path the view does, bar the withheld ones"
		);
		for (path, node) in remote.iter() {
			let Some(at) = derived.remote.at(&path) else {
				continue;
			};
			let (derived_node, node) = (&*at, &*node);
			if node.kind == NodeKind::Dir {
				assert_eq!(
					derived_node,
					&RemoteNode {
						modified_millis: derived_node.modified_millis,
						..node.clone()
					},
					"remote directory at {path:?} differs by more than its created stamp"
				);
			} else {
				assert_eq!(derived_node, node, "remote node at {path:?}");
			}
		}
	}

	/// The plan is what the maps are for: the same actions, in the same order, from the derived
	/// maps as from the whole-tree ones. This is what pins the directory stamps as the non-evidence
	/// the module docs claim they are.
	///
	/// Both sides are reconciled with the SAME holds, the derived pass's: the question here is what
	/// the two map pairs plan where they both describe a path, and a withheld path is one the
	/// derived maps deliberately describe on neither side.
	fn assert_same_plan(
		baseline: &Baseline,
		derived: &Derived,
		local: &Side<LocalNode>,
		remote: &Side<RemoteNode>,
	) -> Vec<SyncAction> {
		let holds = PassHolds {
			held_remote: derived.held.clone(),
			..PassHolds::default()
		};
		let whole = plan::reconcile(
			SyncMode::TwoWay,
			baseline,
			local,
			remote,
			&holds,
			plan::PassPaths::Whole,
		);
		let scoped = plan::reconcile(
			SyncMode::TwoWay,
			baseline,
			&derived.local,
			&derived.remote,
			&holds,
			plan::PassPaths::Whole,
		);
		assert_eq!(
			scoped.actions, whole.actions,
			"the change-scoped maps planned something else"
		);
		// And at the scope the pass really hands the reconcile. The maps above are compared over
		// every key they hold; this asks the narrower question the pass asks, so a producer that
		// moved a key off the node its row carries and did NOT record it is a key nothing decides
		// — which shows up here as a missing action and nowhere else.
		assert_eq!(
			scoped_plan(baseline, derived),
			whole.actions,
			"a key the producers moved is missing from the decided set"
		);
		whole.actions
	}

	/// An unchanged tree: the maps a change-scoped pass assembles are the maps a whole-tree read
	/// produces, path for path and value for value.
	#[test]
	fn an_unchanged_tree_derives_the_maps_a_whole_tree_read_produces() {
		let pair = Pair::converged();
		let baseline = pair.baseline();

		let derived = pair.derived(&baseline, dirty_paths(&[]), &[]);
		let (local, remote) = pair.whole_tree(&baseline);

		assert!(
			derived.held.is_empty(),
			"every row records both sides: {:?}",
			derived.held
		);
		assert_same_maps(&derived, &local, &remote.nodes);
		let actions = assert_same_plan(&baseline, &derived, &local, &remote.nodes);
		assert!(
			actions.is_empty(),
			"a converged pair plans nothing: {actions:?}"
		);
	}

	/// Rows that record one side only — the states the engine writes on purpose — are in the dirty
	/// set and in the held set, and in neither map. The local side then comes back from the
	/// observation, so the local map still equals a whole-tree scan's.
	#[test]
	fn rows_that_record_one_side_are_dirty_held_and_derived_from_neither() {
		let mut pair = Pair::converged();
		write(&pair.root, "conflict.txt", b"conflicted");
		write(&pair.root, "overwritten.txt", b"overwritten");
		write(&pair.root, "adopted.txt", b"adopted");
		write(&pair.root, "kept-local.txt", b"kept local");
		write(&pair.root, "kept-remote.txt", b"kept remote");

		// Every one of them is synced on both sides as far as the world is concerned: they are on
		// disk, they are in the remote view, and only the ROW is half-written.
		let scanned = scan(&pair.root, &Baseline::default());
		let anomalies = [
			"conflict.txt",
			"overwritten.txt",
			"adopted.txt",
			"kept-local.txt",
			"kept-remote.txt",
		];
		for (at, path) in anomalies.iter().enumerate() {
			let uuid = Uuid::from_u128(900 + at as u128);
			pair.uuids.insert((*path).to_string(), uuid);
			let node = scanned.nodes.at(path).expect("the scan holds it");
			pair.items.push(item_of(&node, &pair.uuids));
		}
		let remote_view = view(&pair.items);
		for path in anomalies {
			let node = scanned.nodes.at(path).expect("the scan holds it");
			let at = remote_view.nodes.at(path).expect("the view holds it");
			let row = synced_row(&node, &at);
			pair.rows.push(match path {
				"conflict.txt" => BaselineEntry {
					state: BaselineState::Conflicted,
					local_kind: Some(NodeKind::File),
					remote_kind: Some(NodeKind::File),
					remote_hash: Some(Blake3Hash::from([7; 32])),
					remote_size: Some(11),
					..row
				},
				"overwritten.txt" => BaselineEntry {
					state: BaselineState::Overwritten,
					local_kind: Some(NodeKind::File),
					remote_kind: Some(NodeKind::File),
					remote_hash: Some(Blake3Hash::from([8; 32])),
					remote_size: Some(12),
					..row
				},
				"adopted.txt" => BaselineEntry {
					state: BaselineState::Adopted,
					local_mtime: None,
					..row
				},
				// A `KeepLocal` resolution: the remote anchor stands, the local half is cleared
				// until the re-push.
				"kept-local.txt" => BaselineEntry {
					content_hash: None,
					size: None,
					local_mtime: None,
					agreed_hash: None,
					..row
				},
				// A `KeepRemote` resolution, mirrored: the local anchor stands and the remote half
				// is cleared, so the remote reads as the change and is pulled.
				_ => BaselineEntry {
					remote_uuid: None,
					remote_stable_uuid: None,
					remote_modified: None,
					..row
				},
			});
		}
		let baseline = pair.baseline();

		let derived = pair.derived(&baseline, dirty_paths(&[]), &[]);
		let (local, remote) = pair.whole_tree(&baseline);

		assert_eq!(
			sorted(derived.held.iter().cloned()),
			sorted(anomalies.iter().map(|path| (*path).to_string())),
			"exactly the half-written rows are withheld"
		);
		assert!(
			anomalies.iter().all(|path| derived.dirty.contains(*path)),
			"a withheld row is always re-observed: {:?}",
			derived.dirty
		);
		assert!(
			anomalies.iter().all(|path| !derived.remote.holds(path)),
			"nothing is derived from a half-written row"
		);
		// The local side is not derived from them either — it is observed, which is why the map is
		// still whole.
		assert_same_maps(&derived, &local, &remote.nodes);
		let actions = assert_same_plan(&baseline, &derived, &local, &remote.nodes);
		assert!(
			!actions.iter().any(|action| matches!(
				action,
				SyncAction::DeleteLocal { .. } | SyncAction::TrashRemote { .. }
			)),
			"a withheld path must never plan a deletion: {actions:?}"
		);
	}

	/// The mirror of the equality test: a known change set on both sides, and the derived maps are
	/// the maps of the CHANGED tree — the local ones from the observations, the remote ones from
	/// the announced changes.
	#[test]
	fn a_changed_tree_derives_the_maps_of_the_change() {
		let mut pair = Pair::converged();
		let baseline = pair.baseline();

		// Local: an edit, a deletion, a new file, a new directory with a file in it, and a whole
		// subtree removed.
		write(&pair.root, "top.txt", b"top, edited");
		fs::remove_file(pair.root.join("docs/notes.txt")).unwrap();
		write(&pair.root, "fresh.txt", b"fresh");
		write(&pair.root, "added/one.txt", b"one");
		fs::remove_dir_all(pair.root.join("docs/deep")).unwrap();

		// Remote: one file created, one removed. The delta is minted from real cache events, which
		// is the only way a `Gone` can exist at all.
		let born = Uuid::from_u128(500);
		let doomed = pair.uuids["docs/Ärger.txt"];
		pair.items.push(RemoteItem {
			uuid: born,
			parent: REMOTE_ROOT,
			name: "remote-new.txt".to_owned(),
			stable_uuid: Some(StableUuid::new_for_test(born)),
			hash: None,
			size: 7,
			modified_millis: 1_234,
		});
		pair.items.retain(|item| item.uuid != doomed);
		let delta = [
			cache_event(
				Some(1),
				CacheEventType::File(FileEvent::New(cacheable_file(
					born,
					REMOTE_ROOT,
					"remote-new.txt",
				))),
			),
			cache_event(Some(2), CacheEventType::File(FileEvent::Removed(doomed))),
		];

		let derived = pair.derived(
			&baseline,
			dirty_paths(&[
				"top.txt",
				"docs/notes.txt",
				"fresh.txt",
				"added",
				"docs/deep",
			]),
			&delta_of(&delta),
		);
		let (local, remote) = pair.whole_tree(&baseline);

		assert!(derived.held.is_empty(), "{:?}", derived.held);
		assert_same_maps(&derived, &local, &remote.nodes);
		let actions = assert_same_plan(&baseline, &derived, &local, &remote.nodes);
		assert!(
			actions.len() >= 6,
			"the change set plans a pass's worth of work: {actions:?}"
		);
	}

	/// An interrupted pass hands the next one BOTH halves of what it took, and that pass re-plans
	/// the same action from them without reading either side whole.
	///
	/// The action here is REMOTE-originated — the local delete answering a `Gone` — which is the
	/// one the local half alone cannot reproduce: the baseline row a dirty path's remote side is
	/// derived from still records the item, so a hand-back missing the delta plans nothing at all
	/// and the deletion waits for the safety net. Both shapes are asserted, the broken one first.
	#[test]
	fn an_interrupted_passs_remote_action_is_re_planned_from_both_halves() {
		let mut pair = Pair::converged();
		let doomed = pair.uuids["docs/Ärger.txt"];
		pair.items.retain(|item| item.uuid != doomed);
		let baseline = pair.baseline();
		let announced = delta_of(&[cache_event(
			Some(1),
			CacheEventType::File(FileEvent::Removed(doomed)),
		)]);

		// The pass that plans the delete: nothing announced locally, one removal announced
		// remotely.
		let first = pair.derived(&baseline, dirty_paths(&[]), &announced);
		let planned = scoped_plan(&baseline, &first);
		assert_eq!(planned.len(), 1, "{planned:?}");
		assert!(
			matches!(planned[0], SyncAction::DeleteLocal { .. })
				&& planned[0].rel_path() == "docs/Ärger.txt",
			"the announced removal plans the local delete: {planned:?}"
		);

		// It is cut short before applying it, so no row was written and the file is still on disk.
		// What it owes is its plan's paths...
		let owed = carry_over(
			&planned,
			&[],
			&BTreeSet::new(),
			&std::collections::HashMap::new(),
		);
		assert_eq!(owed, BTreeSet::from(["docs/Ärger.txt".to_string()]));

		// ...and handing back that half ALONE — the shape this stage's first attempt shipped —
		// plans nothing: the row the remote side is derived from still records the item.
		let local_only = pair.derived(&baseline, owed.clone(), &[]);
		assert!(
			scoped_plan(&baseline, &local_only).is_empty(),
			"the remote half is what carries the absence; without it the deletion is lost"
		);

		// Both halves, through the changelist the next pass takes from: the same action again.
		let changes = PairChanges::new();
		changes.cover_local();
		changes.note_owed(owed);
		changes.note_owed_remote(announced);
		let mut scope = changes.take();
		assert_eq!(
			scope.full_pass_reason(baseline.len()),
			None,
			"a hand-back must not send the next pass back to a whole read"
		);
		let second = pair.derived(&baseline, scope.take_local(), &scope.take_remote());
		assert_eq!(
			scoped_plan(&baseline, &second),
			planned,
			"the next pass must re-plan what the interrupted one did not get to"
		);
	}

	/// A path the baseline has no row for cannot be derived — and the consequence is a deferred
	/// CREATE, never a deletion. (A whole-tree pass pushes it; this one leaves it for the next one.)
	#[test]
	fn a_path_with_no_row_is_missed_by_derivation_and_never_deleted() {
		let pair = Pair::converged();
		// Drop the row for a directory that still holds rows under it, exactly as an untracked
		// destination-only parent leaves the tree: a node in the baseline with no row of its own.
		let rows: Vec<BaselineEntry> = pair
			.rows
			.iter()
			.filter(|row| row.rel_path != "docs")
			.cloned()
			.collect();
		let baseline = Baseline::from_rows(rows);

		let derived = pair.derived(&baseline, dirty_paths(&[]), &[]);
		let (local, remote) = pair.whole_tree(&baseline);

		assert!(
			local.holds("docs") && !derived.local.holds("docs"),
			"a whole-tree scan sees the directory; derivation has no row to carry"
		);
		assert!(!derived.remote.holds("docs"), "and no remote node either");
		let holds = PassHolds::default();
		let scoped = plan::reconcile(
			SyncMode::TwoWay,
			&baseline,
			&derived.local,
			&derived.remote,
			&holds,
			plan::PassPaths::Whole,
		);
		assert!(
			!scoped.actions.iter().any(|action| matches!(
				action,
				SyncAction::DeleteLocal { .. } | SyncAction::TrashRemote { .. }
			)),
			"a path in neither map is decided by nobody: {:?}",
			scoped.actions
		);
		let whole = plan::reconcile(
			SyncMode::TwoWay,
			&baseline,
			&local,
			&remote.nodes,
			&holds,
			plan::PassPaths::Whole,
		);
		assert!(
			whole.actions.len() > scoped.actions.len(),
			"the whole-tree pass is the one that picks it up: {:?}",
			whole.actions
		);
	}

	/// Two siblings whose names fold together are two rows, and two keys, in both maps. The store
	/// compares bytewise and so do the maps; the collision is the reconcile's business, not the
	/// assembly's. (Only the baseline can hold such a pair on a case-insensitive filesystem, which
	/// is why this is asserted on the rows rather than on disk.)
	#[test]
	fn case_only_siblings_stay_two_rows_in_both_maps() {
		let uuid = |at: u128| Uuid::from_u128(700 + at);
		let row = |rel_path: &str, at: u128| BaselineEntry {
			rel_path: rel_path.to_string(),
			kind: NodeKind::File,
			remote_uuid: Some(uuid(at)),
			content_hash: Some(Blake3Hash::from([at as u8; 32])),
			size: Some(3),
			local_mtime: Some(10),
			remote_modified: Some(20),
			state: BaselineState::Synced,
			local_kind: None,
			remote_kind: None,
			remote_hash: None,
			remote_size: None,
			remote_stable_uuid: Some(StableUuid::new_for_test(uuid(at))),
			agreed_hash: Some(Blake3Hash::from([at as u8; 32])),
		};
		let baseline = Baseline::from_rows([row("A.txt", 1), row("a.txt", 2)]);

		let derived = from_baseline(&baseline, BTreeSet::new());

		assert_eq!(
			sorted(derived.local.paths().map(|path| path.into_owned())),
			vec!["A.txt", "a.txt"]
		);
		assert_eq!(
			derived.remote.at("A.txt").unwrap().remote_uuid,
			uuid(1),
			"the folded name is not the key"
		);
		assert_eq!(derived.remote.at("a.txt").unwrap().remote_uuid, uuid(2));
	}

	/// What a walk PRUNED it never looked at: the rows under an ignored entry stay, while the rows
	/// the walk covered and did not find are gone.
	#[test]
	fn a_walk_carries_the_rows_it_pruned_and_drops_the_ones_it_covered() {
		let pair = Pair::converged();
		// `.DS_Store` is hidden by the built-in defaults, and tracked, so the walk records it as a
		// pruned root rather than as noise.
		write(&pair.root, "docs/.DS_Store", b"x");
		let mut rows = pair.rows.clone();
		let template = pair
			.rows
			.iter()
			.find(|row| row.rel_path == "docs/notes.txt")
			.unwrap()
			.clone();
		rows.push(BaselineEntry {
			rel_path: "docs/.DS_Store".to_string(),
			remote_uuid: Some(Uuid::from_u128(800)),
			..template
		});
		let baseline = Baseline::from_rows(rows);
		// Everything under `docs` is covered by the walk of it — except what the walk pruned.
		fs::remove_file(pair.root.join("docs/notes.txt")).unwrap();

		let derived = pair.derived(&baseline, dirty_paths(&["docs"]), &[]);

		assert!(
			!derived.local.holds("docs/notes.txt"),
			"the walk covered it and did not find it"
		);
		assert!(
			derived.local.holds("docs/.DS_Store"),
			"the walk pruned it, so its row is not evidence of absence: {:?}",
			sorted(derived.local.paths().map(|path| path.into_owned()))
		);
		assert!(derived.local.holds("docs/deep/inner.bin"));
	}

	/// A path the rules hide is taken out of the LOCAL map as well as the remote one, and held
	/// where a row was carried there, so the two halves agree and nothing is planned at it.
	///
	/// The shape that reaches this is an `untrack_ignored` whose `delete_subtrees` failed: rows
	/// survive under a root the rules hide. Carrying the local node while the view's filter drops
	/// the remote one reads as a remote deletion — a local delete planned on a file the user still
	/// has, with only `drop_blocked` standing in front of it.
	#[test]
	fn a_hidden_path_with_a_carried_row_is_dropped_held_and_plans_nothing() {
		let pair = Pair::converged();
		let baseline = pair.baseline();
		// The rule arrives after the rows: what it hides is tracked, and stayed tracked.
		fs::write(pair.root.join(FILENIGNORE), "deep/\n").unwrap();

		let mut derived = from_baseline(&baseline, dirty_paths(&["docs/deep"]));
		let (observed, rules) = observe_local(
			&pair.root,
			&baseline,
			IgnoreRules::default(),
			&RuleFiles::Read,
			&derived.dirty,
		);
		assert!(
			matches!(observed.observed["docs/deep"], LocalObservation::Hidden(_)),
			"the rule has to be what the observation reports: {:?}",
			observed.observed
		);
		merge_local(&mut derived, &baseline, &observed);
		// The remote half, filtered with the very rules the observation matched with — which is
		// what `prepare_scoped` does a few lines after its own `merge_local`.
		let mut view = RemoteView {
			nodes: std::mem::take(&mut derived.remote),
			has_collisions: false,
			held_paths: BTreeSet::new(),
			skipped: Vec::new(),
			ignored: BTreeMap::new(),
			ignored_default_untracked: 0,
		};
		view.filter_changed(
			plan::ViewFilter {
				rules: &rules,
				baseline: &baseline,
			},
			&derived.decided,
		);
		derived.remote = view.nodes;

		for path in ["docs/deep", "docs/deep/inner.bin"] {
			assert!(
				!derived.local.holds(path),
				"the local half still describes {path:?}: {:?}",
				sorted(derived.local.paths().map(|path| path.into_owned()))
			);
			assert!(
				!derived.remote.holds(path),
				"the remote half still describes {path:?}"
			);
		}
		assert!(
			derived.held.contains("docs/deep"),
			"a hidden path with rows behind it is withheld, or its two absences retire the row: \
			 {:?}",
			derived.held
		);
		let planned = scoped_plan(&baseline, &derived);
		assert!(
			planned.is_empty(),
			"a hidden path is decided by nobody: {planned:?}"
		);
	}

	/// An incomplete walk may have missed anything, so it drops nothing: an absence it seems to
	/// show is exactly the absence I1 forbids inventing.
	#[cfg(unix)]
	#[test]
	fn an_incomplete_walk_drops_no_row() {
		use std::os::unix::fs::PermissionsExt;

		let pair = Pair::converged();
		let baseline = pair.baseline();
		let locked = pair.root.join("docs/deep");
		fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

		let mut derived = from_baseline(&baseline, BTreeSet::from(["docs".to_string()]));
		let (observed, _) = observe_local(
			&pair.root,
			&baseline,
			IgnoreRules::default(),
			&RuleFiles::Read,
			&derived.dirty,
		);
		fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
		assert!(!observed.complete, "the walk could not read a subtree");
		merge_local(&mut derived, &baseline, &observed);

		assert!(
			derived.local.holds("docs/deep/inner.bin"),
			"an unreadable subtree's rows are carried, not deleted: {:?}",
			sorted(derived.local.paths().map(|path| path.into_owned()))
		);
	}

	/// Every [`BaselineState`], listed once. [`state_index`] is what makes this exhaustive rather
	/// than merely long enough: it has no wildcard arm, so a new variant does not compile until it
	/// is named there, and the assertion in the test below then demands it be listed here too.
	const ALL_STATES: [BaselineState; 4] = [
		BaselineState::Synced,
		BaselineState::Conflicted,
		BaselineState::Overwritten,
		BaselineState::Adopted,
	];

	/// Where each state sits in [`ALL_STATES`].
	fn state_index(state: BaselineState) -> usize {
		match state {
			BaselineState::Synced => 0,
			BaselineState::Conflicted => 1,
			BaselineState::Overwritten => 2,
			BaselineState::Adopted => 3,
		}
	}

	/// [`BaselineEntry::carryable`] must be exactly "`Synced`, and the fields [`extracted`] reads
	/// are all there" — checked against a second implementation of each half, over every row shape.
	///
	/// Both directions matter and both can fail here. A rule LOOSER than the extraction would have
	/// the resident tree index a row as carryable that derives nothing: a path in neither map, held
	/// by nobody, which is the shape invariant I1 forbids. A rule STRICTER than it fabricates no
	/// absence, but [`from_baseline`] puts such a row in `dirty` AND `held` in one statement, so it
	/// stalls its path on every pass rather than one. Every state, both kinds, and each optional
	/// field the extraction reads cleared in turn — which is every way a row can fail to describe a
	/// side.
	#[test]
	fn the_carryable_rule_and_the_extraction_agree_on_every_row_shape() {
		let uuid = Uuid::from_u128(1);
		let hash = Blake3Hash::from([1; 32]);
		let whole = BaselineEntry {
			rel_path: "x".to_string(),
			kind: NodeKind::File,
			remote_uuid: Some(uuid),
			content_hash: Some(hash),
			size: Some(3),
			local_mtime: Some(10),
			remote_modified: Some(20),
			state: BaselineState::Synced,
			local_kind: None,
			remote_kind: None,
			remote_hash: None,
			remote_size: None,
			remote_stable_uuid: Some(StableUuid::new_for_test(uuid)),
			agreed_hash: Some(hash),
		};
		/// One way a row can fail to describe a side: what to call it in the failure message,
		/// and the field it clears.
		type Clear = (&'static str, fn(&mut BaselineEntry));
		let clears: [Clear; 8] = [
			("nothing", |_| {}),
			("remote_uuid", |row| row.remote_uuid = None),
			("content_hash", |row| row.content_hash = None),
			("size", |row| row.size = None),
			("local_mtime", |row| row.local_mtime = None),
			("remote_modified", |row| row.remote_modified = None),
			("remote_stable_uuid", |row| row.remote_stable_uuid = None),
			("agreed_hash", |row| row.agreed_hash = None),
		];
		let mut checked = 0;
		let mut carried_some = 0;
		let mut extracted_some = 0;
		for (at, state) in ALL_STATES.into_iter().enumerate() {
			assert_eq!(
				state_index(state),
				at,
				"{state:?} is not where the wildcard-free match puts it: the corpus has drifted \
				 from the enum"
			);
			for kind in [NodeKind::File, NodeKind::Dir] {
				for (what, clear) in &clears {
					let mut row = BaselineEntry {
						state,
						kind,
						..whole.clone()
					};
					clear(&mut row);
					assert_eq!(
						row.carryable(),
						row.state == BaselineState::Synced && extracted(&row).is_some(),
						"{state:?} {kind:?} with {what} cleared: the rule and the extraction disagree"
					);
					checked += 1;
					carried_some += usize::from(carried(&row).is_some());
					extracted_some += usize::from(extracted(&row).is_some());
				}
			}
		}
		assert_eq!(
			checked,
			ALL_STATES.len() * 2 * clears.len(),
			"every state/kind/field combination is covered"
		);
		// Both halves of the assertion have to answer both ways over this corpus, or it could not
		// have failed. The extraction succeeds for 10 rows of EVERY state — a directory needs only
		// its remote uuid (7 of 8 clears), a file needs all four fields (3 of 8) — and the state
		// term then keeps the `Synced` quarter of those.
		assert_eq!(
			extracted_some,
			10 * ALL_STATES.len(),
			"the extraction must answer both ways, or the state term alone would carry the test"
		);
		assert_eq!(
			carried_some, 10,
			"the corpus must exercise both answers, not just one"
		);
	}

	/// A dirty path the pass got no reading for — a socket, a vanished file, an unreadable
	/// ancestor — keeps its row: no observation is not an absence.
	#[test]
	fn a_dirty_path_with_no_observation_keeps_its_row() {
		let pair = Pair::converged();
		let baseline = pair.baseline();
		let mut derived = from_baseline(&baseline, BTreeSet::new());
		let observed = LocalObservations {
			observed: BTreeMap::new(),
			siblings: BTreeMap::new(),
			ignore_blocked: BTreeSet::new(),
			complete: true,
			errors: Vec::new(),
		};

		merge_local(&mut derived, &baseline, &observed);

		assert!(derived.local.holds("top.txt"));
		assert!(derived.local.holds("docs/deep/inner.bin"));
	}
}

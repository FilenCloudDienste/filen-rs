use std::collections::{HashMap, HashSet};

use uuid::Uuid;

use super::{
	Baseline, BaselineEntry, NodeKind, NodesAt, RemoteNode, SyncAction, ancestors, push_vouched,
	snapshot_confirms,
};

/// The deletions [`fold_subsumed_deletes`] took out of a plan, by the directory deletion of the
/// same side that carries each one out.
#[derive(Debug, Default)]
pub(in super::super) struct Subsumed {
	local: HashMap<String, Vec<String>>,
	remote: HashMap<String, Vec<String>>,
}

impl Subsumed {
	/// The paths whose deletion `action` carries out along with its own: empty for anything but
	/// a directory deletion that took others with it.
	pub(in super::super) fn under(&self, action: &SyncAction) -> &[String] {
		let by_dir = match action {
			SyncAction::DeleteLocal { .. } => &self.local,
			SyncAction::TrashRemote { .. } => &self.remote,
			_ => return &[],
		};
		by_dir.get(action.rel_path()).map_or(&[], Vec::as_slice)
	}
}

/// Take out of `actions` every deletion that a directory deletion of the same side, also in
/// `actions`, carries out anyway, and hand it to the top-most such directory. The server
/// trashes a directory with its whole subtree and a local deletion quarantines the directory in
/// one rename, so deleting each item under it first spent a call per item and left the remote
/// trash holding every file loose beside an emptied folder, and the quarantine bin a second,
/// empty copy of the directory beside the files.
///
/// The directory's apply drops the baseline rows of the deletions it took — exactly those, by
/// path, and not every row under the directory — so the rows end where the per-item plan left
/// them. None of them is the first half of a type flip, which would have to run before the
/// create it pairs with: nothing is created on a side under a directory that side deletes.
///
/// Runs on what the guard let through: the guard counts the items a pass deletes, not the calls
/// that delete them. The price is that the subtree goes in one piece or not at all: a directory
/// that cannot go (a file held open inside it, where Windows refuses the rename) keeps every
/// item under it until it can, where deleting item by item left only that file behind.
///
/// A pass folds its plan in its stage (see `stage::Screen::fold_subsumed`); this folds a list,
/// and is what the tests hold that one to.
pub(in super::super) fn fold_subsumed_deletes(actions: &mut Vec<SyncAction>) -> Subsumed {
	let mut subsumed = Subsumed::default();
	// Owned: the sets are read inside the `retain` below, which borrows `actions` mutably.
	let (mut local_dirs, mut remote_dirs) = (HashSet::new(), HashSet::new());
	for action in actions.iter() {
		match action {
			SyncAction::DeleteLocal {
				rel_path,
				kind: NodeKind::Dir,
			} => {
				local_dirs.insert(rel_path.clone());
			}
			SyncAction::TrashRemote {
				rel_path,
				kind: NodeKind::Dir,
				..
			} => {
				remote_dirs.insert(rel_path.clone());
			}
			_ => {}
		}
	}
	if local_dirs.is_empty() && remote_dirs.is_empty() {
		return subsumed;
	}
	actions.retain(|action| {
		let (dirs, by_dir) = match action {
			SyncAction::DeleteLocal { .. } => (&local_dirs, &mut subsumed.local),
			SyncAction::TrashRemote { .. } => (&remote_dirs, &mut subsumed.remote),
			_ => return true,
		};
		let path = action.rel_path();
		// Shallowest first, so the directory found is one no deletion above takes with it.
		let Some(dir) = ancestors(path).find(|dir| dirs.contains(*dir)) else {
			return true;
		};
		tracing::trace!(
			"plan: {} — the deletion of {dir:?} takes it along",
			action.describe()
		);
		match by_dir.get_mut(dir) {
			Some(paths) => paths.push(path.to_owned()),
			None => {
				by_dir.insert(dir.to_owned(), vec![path.to_owned()]);
			}
		}
		false
	});
	subsumed
}

/// Whether `action` is the first deletion of a directory at its side and path that `filed` has
/// seen: what [`Subsumed`] files carried deletions under, by that side and
/// path alone, so a plan that deletes one twice — or a file at the same path — would read them
/// out twice.
pub(in super::super) fn first_dir_deletion(
	filed: &mut HashSet<(bool, String)>,
	action: &SyncAction,
) -> bool {
	let local = match action {
		SyncAction::DeleteLocal {
			kind: NodeKind::Dir,
			..
		} => true,
		SyncAction::TrashRemote {
			kind: NodeKind::Dir,
			..
		} => false,
		_ => return false,
	};
	filed.insert((local, action.rel_path().to_owned()))
}

/// What [`fold_subsumed_deletes`] folds of `actions`: the actions left, and the carried
/// deletions by directory, sorted.
pub(in super::super) fn folded(
	mut actions: Vec<SyncAction>,
) -> (Vec<SyncAction>, Vec<(String, String)>) {
	let subsumed = fold_subsumed_deletes(&mut actions);
	let mut filed = HashSet::new();
	let mut carried: Vec<(String, String)> = actions
		.iter()
		.filter(|action| first_dir_deletion(&mut filed, action))
		.flat_map(|action| {
			subsumed
				.under(action)
				.iter()
				.map(|path| (action.rel_path().to_owned(), path.clone()))
		})
		.collect();
	carried.sort();
	(actions, carried)
}

/// Rule (c) over a whole view: advance every row of `baseline` awaiting confirmation that
/// [`snapshot_confirms`] accepts, and return the rows that moved. The engine applies the rule a
/// page of rows at a time (`SyncEngine::confirm`); this is the rule as the tests state it.
pub(in super::super) fn confirm_agreed_content(
	baseline: &mut Baseline,
	raw_remote: &impl NodesAt<Node = RemoteNode>,
) -> Vec<BaselineEntry> {
	baseline
		.confirm_where(|entry| snapshot_confirms(raw_remote.at(&entry.rel_path).as_deref(), entry))
}

/// [`push_vouched`] over a whole view, as [`confirm_agreed_content`] is rule (c) over one.
pub(in super::super) fn confirm_agreed_pushes(
	baseline: &mut Baseline,
	confirmed: &HashSet<Uuid>,
) -> Vec<BaselineEntry> {
	baseline.confirm_where(|entry| push_vouched(confirmed, entry))
}

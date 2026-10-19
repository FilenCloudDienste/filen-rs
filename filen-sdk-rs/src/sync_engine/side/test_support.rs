use super::{FromRow, Nodes, Side, SideRef};

/// A side holding what `side` holds, for a test that edits one and keeps the other: a whole or
/// a carried side's backing copied, and a staged side's nodes in a [`Side::Whole`], since a
/// staged side is its pass's own table and has no second one to copy into.
pub(in super::super) fn copied<T: Clone>(side: &Side<T>) -> Side<T> {
	match side {
		Side::Whole(map) => Side::Whole(map.clone()),
		Side::Carried(overlay) => Side::Carried(overlay.clone()),
		Side::Staged(staged) => Side::Whole(staged.pages().collect()),
	}
}

/// Every node `side` holds, with its path, in path order.
pub(in super::super) fn sorted_entries<T: FromRow>(side: &SideRef<'_, T>) -> Vec<(String, T)> {
	let mut out: Vec<(String, T)> = side
		.iter()
		.map(|(path, node)| (path.into_owned(), node.into_owned()))
		.collect();
	out.sort_by(|left, right| left.0.cmp(&right.0));
	out
}

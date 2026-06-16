use rusqlite::{CachedStatement, params};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i8)]
pub(super) enum ItemType {
	/// Discriminant 0. Never constructed from Rust — the account-root item is written directly by
	/// `root_item_insert.sql` with `type = 0`. It is kept here (rather than deleted) so the `repr(i8)`
	/// mapping stays explicit and stable: removing it would shift `Dir` to 0 and `File` to 1, silently
	/// corrupting `items.type` (and breaking the `CHECK (type IN (0, 1, 2))` semantics) for every
	/// existing database.
	#[allow(dead_code)]
	Root,
	Dir,
	File,
}

pub(super) fn delete_item_with_stmt(
	uuid: Uuid,
	delete_item_stmt: &mut CachedStatement<'_>,
) -> rusqlite::Result<()> {
	// A plain DELETE (no RETURNING): delete-of-missing is a no-op, so the row count is ignored.
	delete_item_stmt.execute(params![uuid]).map(|_| ())
}

use rusqlite::{CachedStatement, OptionalExtension, params};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i8)]
pub(super) enum ItemType {
	#[allow(dead_code)]
	Root,
	Dir,
	File,
}

pub(super) fn upsert_item_with_stmt(
	uuid: Uuid,
	parent: Option<Uuid>,
	item_type: ItemType,
	root: Uuid,
	upsert_item_stmt: &mut CachedStatement<'_>,
) -> rusqlite::Result<i64> {
	upsert_item_stmt.query_one(params![uuid, parent, item_type as i8, root], |row| {
		row.get(0)
	})
}

pub(super) fn delete_item_with_stmt(
	uuid: Uuid,
	delete_item_stmt: &mut CachedStatement<'_>,
) -> rusqlite::Result<Option<()>> {
	delete_item_stmt
		.query_one(params![uuid], |_| Ok(()))
		.optional()
}

use rusqlite::{Connection, params};
use uuid::Uuid;

use crate::cache::sql::statements::{ITEM_UPDATE_OWN_ROOT_ID, ROOT_INSERT, ROOT_ITEM_INSERT};

/// Insert the account-root item and its `roots` row in a single transaction.
///
/// Three ordered steps, all relying on the `items.root_id` FK being DEFERRABLE INITIALLY DEFERRED:
/// (1) `ROOT_ITEM_INSERT` writes the root item with a placeholder `root_id = 0` (the rowid is not known
/// until insertion and `roots` is still empty); (2) `ROOT_INSERT` inserts that rowid into `roots`;
/// (3) `ITEM_UPDATE_OWN_ROOT_ID` patches the item's `root_id` to point at itself. Reordering would
/// violate the FK even with deferred checking, since the check still runs at commit.
pub(super) fn insert_root(uuid: Uuid, connection: &mut Connection) -> rusqlite::Result<()> {
	let transaction = connection.transaction()?;
	{
		let mut upsert_root_item_stmt = transaction.prepare_cached(ROOT_ITEM_INSERT)?;
		let root_id: i64 = upsert_root_item_stmt.query_one(params![uuid], |row| row.get(0))?;
		transaction.execute(ROOT_INSERT, params![root_id])?;
		transaction.execute(ITEM_UPDATE_OWN_ROOT_ID, params![root_id])?;
	}

	transaction.commit()
}

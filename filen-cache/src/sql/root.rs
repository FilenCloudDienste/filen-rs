use rusqlite::{Connection, params};
use uuid::Uuid;

use crate::sql::statements::{ITEM_UPDATE_OWN_ROOT_ID, ROOT_INSERT, ROOT_ITEM_INSERT};

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

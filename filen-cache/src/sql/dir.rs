use filen_sdk_rs::fs::dir::cache::CacheableDir;
use rusqlite::CachedStatement;
use uuid::Uuid;

use super::item::ItemType;

pub(super) fn upsert_dir_with_stmts(
	dir: &CacheableDir,
	root: Uuid,
	upsert_dir_stmt: &mut CachedStatement<'_>,
	upsert_item_stmt: &mut CachedStatement<'_>,
) -> rusqlite::Result<()> {
	let id = super::item::upsert_item_with_stmt(
		dir.uuid,
		Some(dir.parent),
		ItemType::Dir,
		root,
		upsert_item_stmt,
	)?;
	upsert_dir_stmt.execute(rusqlite::params![
		id,
		dir.favorited,
		dir.color.as_ref(),
		dir.timestamp.timestamp_millis(),
		dir.name,
		dir.created.map(|t| t.timestamp_millis()),
	])?;
	Ok(())
}

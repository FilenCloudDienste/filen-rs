use filen_sdk_rs::fs::file::cache::CacheableFile;
use rusqlite::CachedStatement;
use uuid::Uuid;

use super::item::ItemType;

pub(super) fn upsert_file_with_stmts(
	file: &CacheableFile,
	root: Uuid,
	upsert_file_stmt: &mut CachedStatement<'_>,
	upsert_item_stmt: &mut CachedStatement<'_>,
) -> rusqlite::Result<()> {
	let id = super::item::upsert_item_with_stmt(
		file.uuid,
		Some(file.parent),
		ItemType::File,
		root,
		upsert_item_stmt,
	)?;

	upsert_file_stmt.execute(rusqlite::params![
		id,
		file.chunks_size,
		file.chunks,
		file.favorited,
		file.region,
		file.bucket,
		file.timestamp.timestamp_millis(),
		file.size,
		file.name,
		file.mime,
		file.key.as_ref().as_ref(),
		file.key.version() as i8,
		file.created.map(|c| c.timestamp_millis()),
		file.last_modified.timestamp_millis(),
		file.hash.as_ref().map(|h| h.as_ref()),
	])?;
	Ok(())
}

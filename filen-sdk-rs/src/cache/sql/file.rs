use crate::fs::file::cache::CacheableFile;
use rusqlite::CachedStatement;

use super::item::ItemType;

pub(super) fn upsert_file_with_stmts(
	file: &CacheableFile,
	upsert_file_stmt: &mut CachedStatement<'_>,
	upsert_item_stmt: &mut CachedStatement<'_>,
) -> rusqlite::Result<()> {
	let content_hash = file.content_fingerprint();
	let id = super::item::upsert_item_with_stmt(
		file.uuid,
		Some(file.parent),
		ItemType::File,
		Some(&content_hash),
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
		file.key.to_str().as_ref(),
		file.key.version() as i8,
		file.created.map(|c| c.timestamp_millis()),
		file.last_modified.timestamp_millis(),
		file.hash
			.as_ref()
			.map(|h| h.as_sized_str().to_str())
			.as_deref(),
	])?;
	Ok(())
}

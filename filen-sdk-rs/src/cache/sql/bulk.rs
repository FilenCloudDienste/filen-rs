//! Multi-row `VALUES (...),(...),...` upserts for `items` + `files`/`dirs`.
//!
//! The single-row path applied one logical item per `prepare_cached` + `execute`, so a 166k-item
//! resync populate paid 166k×2 single-row executes — which dominated apply time. Here a whole batch
//! goes through ONE prepared statement per table.
//!
//! The FK between `files`/`dirs` and `items` is bridged WITHOUT a per-row subquery (the mistake that
//! made an earlier multi-row attempt SLOWER than single-row): the `items` upsert targets the
//! `uuid` UNIQUE index directly — `ON CONFLICT (uuid) DO UPDATE` — so an existing uuid updates its
//! row IN PLACE (the rowid, hence the `files`/`dirs` FK, stays stable) and a new uuid gets a fresh
//! rowid; `RETURNING id, uuid` then hands every row's `items.id` back. We collect those into a
//! per-batch `uuid -> id` map and bind the ids into the multi-row `files`/`dirs` upsert. No
//! `RETURNING`-order reliance (SQLite leaves that order unspecified), no correlated subquery, no
//! rowid churn.
//!
//! Two further tuning wins (benchmarked on an M4, 166k items, file-backed DB):
//! - The full-`MULTI_ROW_CHUNK`-size statement is prepared ONCE and HELD across every full
//!   sub-batch (only a final partial sub-batch builds a one-off `prepare_cached`). Skipping the
//!   per-sub-batch SQL-string rebuild + `prepare_cached` re-hash is worth ~8-10%.
//! - Transactions are sized at [`CHUNK_SIZE`](super::CHUNK_SIZE) (50k) rather than 10k.
//!
//! PRECONDITION: a single batch must not contain the same `uuid` twice. All callers satisfy this —
//! resync diffs and directory listings key items by uuid (unique by construction), and the live
//! drain applies one event at a time (batch size 1).

use std::borrow::Borrow;
use std::collections::HashMap;
use std::fmt::Write;

use rusqlite::{Connection, Statement};
use uuid::Uuid;

use crate::fs::{dir::cache::CacheableDir, file::cache::CacheableFile};

use super::{CHUNK_SIZE, item::ItemType};

// Distinct bind parameters per VALUES row.
const ITEM_PARAMS_PER_ROW: usize = 5; // uuid, parent, type, root_id, content_hash
const FILE_PARAMS_PER_ROW: usize = 15; // id + 14 metadata columns
const DIR_PARAMS_PER_ROW: usize = 6; // id + 5 metadata columns

/// Rows per multi-row statement. Sized so the widest table (`files`, 15 params/row) stays under
/// `SQLITE_MAX_VARIABLE_NUMBER` (32766 in the bundled SQLite): 2000 × 15 = 30000. Divides
/// `CHUNK_SIZE` evenly so every sub-batch within a transaction is full-size (max held-statement
/// reuse); only the dataset's final sub-batch is partial. Benchmarked sweet spot (≈1024–2048).
pub(super) const MULTI_ROW_CHUNK: usize = 2_000;

const _: () = assert!(
	CHUNK_SIZE.is_multiple_of(MULTI_ROW_CHUNK),
	"CHUNK_SIZE must be a multiple of MULTI_ROW_CHUNK so transactions hold only full sub-batches",
);

/// `INSERT INTO items (...) VALUES <rows> ON CONFLICT(uuid) DO UPDATE ... RETURNING id, uuid` for
/// `rows` items. Per row k the params are `[uuid, parent, type, root_id, content_hash]` at base
/// `k * 5`. The `id` column is omitted — SQLite assigns a fresh rowid for a new uuid, and the
/// `ON CONFLICT(uuid)` update preserves the existing rowid otherwise; `RETURNING` reports it either
/// way.
fn items_upsert_sql(rows: usize) -> String {
	let mut sql = String::with_capacity(rows * 24 + 320);
	sql.push_str("INSERT INTO items (uuid, parent, type, root_id, content_hash) VALUES ");
	for k in 0..rows {
		if k > 0 {
			sql.push(',');
		}
		let b = k * ITEM_PARAMS_PER_ROW;
		let _ = write!(
			sql,
			"(?{}, ?{}, ?{}, ?{}, ?{})",
			b + 1,
			b + 2,
			b + 3,
			b + 4,
			b + 5
		);
	}
	sql.push_str(
		" ON CONFLICT (uuid) DO UPDATE SET parent = excluded.parent, type = excluded.type, \
		 root_id = excluded.root_id, content_hash = excluded.content_hash RETURNING id, uuid",
	);
	sql
}

/// Multi-row `INSERT INTO files (id, ...) VALUES <rows> ON CONFLICT(id) DO UPDATE ...`. Per row k the
/// params are `[id, chunks_size, chunks, favorite, region, bucket, timestamp, size, name, mime,
/// file_key, file_key_version, created, modified, hash]` at base `k * 15`.
fn files_upsert_sql(rows: usize) -> String {
	let mut sql = String::with_capacity(rows * 64 + 640);
	sql.push_str(
		"INSERT INTO files (id, chunks_size, chunks, favorite, region, bucket, timestamp, size, \
		 name, mime, file_key, file_key_version, created, modified, hash) VALUES ",
	);
	for k in 0..rows {
		if k > 0 {
			sql.push(',');
		}
		let b = k * FILE_PARAMS_PER_ROW;
		let _ = write!(
			sql,
			"(?{}, ?{}, ?{}, ?{}, ?{}, ?{}, ?{}, ?{}, ?{}, ?{}, ?{}, ?{}, ?{}, ?{}, ?{})",
			b + 1,
			b + 2,
			b + 3,
			b + 4,
			b + 5,
			b + 6,
			b + 7,
			b + 8,
			b + 9,
			b + 10,
			b + 11,
			b + 12,
			b + 13,
			b + 14,
			b + 15,
		);
	}
	sql.push_str(
		" ON CONFLICT (id) DO UPDATE SET chunks_size = excluded.chunks_size, \
		 chunks = excluded.chunks, favorite = excluded.favorite, region = excluded.region, \
		 bucket = excluded.bucket, timestamp = excluded.timestamp, size = excluded.size, \
		 name = excluded.name, mime = excluded.mime, file_key = excluded.file_key, \
		 file_key_version = excluded.file_key_version, created = excluded.created, \
		 modified = excluded.modified, hash = excluded.hash",
	);
	sql
}

/// Multi-row `INSERT INTO dirs (id, ...) VALUES <rows> ON CONFLICT(id) DO UPDATE ...`. Per row k the
/// params are `[id, favorite, color, timestamp, name, created]` at base `k * 6`.
fn dirs_upsert_sql(rows: usize) -> String {
	let mut sql = String::with_capacity(rows * 32 + 320);
	sql.push_str("INSERT INTO dirs (id, favorite, color, timestamp, name, created) VALUES ");
	for k in 0..rows {
		if k > 0 {
			sql.push(',');
		}
		let b = k * DIR_PARAMS_PER_ROW;
		let _ = write!(
			sql,
			"(?{}, ?{}, ?{}, ?{}, ?{}, ?{})",
			b + 1,
			b + 2,
			b + 3,
			b + 4,
			b + 5,
			b + 6,
		);
	}
	sql.push_str(
		" ON CONFLICT (id) DO UPDATE SET favorite = excluded.favorite, color = excluded.color, \
		 timestamp = excluded.timestamp, name = excluded.name, created = excluded.created",
	);
	sql
}

/// Look up the `items.id` for `uuid` in the map built from an `items` upsert's `RETURNING`. Every
/// upserted uuid is in the map (the upsert just `RETURNING`-ed it), so a miss is a logic error / DB
/// corruption — surfaced as an error, never a panic.
fn id_for(ids: &HashMap<Uuid, i64>, uuid: Uuid) -> rusqlite::Result<i64> {
	ids.get(&uuid)
		.copied()
		.ok_or(rusqlite::Error::QueryReturnedNoRows)
}

/// Bind the file batch into `stmt` (an `items` upsert prepared for `files.len()` rows) and read its
/// `RETURNING id, uuid` into a `uuid -> id` map.
fn file_items_into_map<'a>(
	stmt: &mut Statement<'_>,
	files: &[impl Borrow<CacheableFile<'a>>],
	root_id: i64,
) -> rusqlite::Result<HashMap<Uuid, i64>> {
	let mut idx = 1;
	for file in files {
		let file = file.borrow();
		let content_hash = file.content_fingerprint();
		stmt.raw_bind_parameter(idx, file.uuid)?;
		stmt.raw_bind_parameter(idx + 1, file.parent)?;
		stmt.raw_bind_parameter(idx + 2, ItemType::File as i8)?;
		stmt.raw_bind_parameter(idx + 3, root_id)?;
		stmt.raw_bind_parameter(idx + 4, &content_hash[..])?;
		idx += ITEM_PARAMS_PER_ROW;
	}
	let mut map = HashMap::with_capacity(files.len());
	let mut rows = stmt.raw_query();
	while let Some(row) = rows.next()? {
		map.insert(row.get::<_, Uuid>(1)?, row.get::<_, i64>(0)?);
	}
	Ok(map)
}

/// As [`file_items_into_map`] but for dirs.
fn dir_items_into_map<'a>(
	stmt: &mut Statement<'_>,
	dirs: &[impl Borrow<CacheableDir<'a>>],
	root_id: i64,
) -> rusqlite::Result<HashMap<Uuid, i64>> {
	let mut idx = 1;
	for dir in dirs {
		let dir = dir.borrow();
		let content_hash = dir.content_fingerprint();
		stmt.raw_bind_parameter(idx, dir.uuid)?;
		stmt.raw_bind_parameter(idx + 1, dir.parent)?;
		stmt.raw_bind_parameter(idx + 2, ItemType::Dir as i8)?;
		stmt.raw_bind_parameter(idx + 3, root_id)?;
		stmt.raw_bind_parameter(idx + 4, &content_hash[..])?;
		idx += ITEM_PARAMS_PER_ROW;
	}
	let mut map = HashMap::with_capacity(dirs.len());
	let mut rows = stmt.raw_query();
	while let Some(row) = rows.next()? {
		map.insert(row.get::<_, Uuid>(1)?, row.get::<_, i64>(0)?);
	}
	Ok(map)
}

/// Bind + run the `files` rows of a batch into `stmt`, taking each row's `items.id` from `ids`.
fn file_rows_exec<'a>(
	stmt: &mut Statement<'_>,
	files: &[impl Borrow<CacheableFile<'a>>],
	ids: &HashMap<Uuid, i64>,
) -> rusqlite::Result<()> {
	let mut idx = 1;
	for file in files {
		let file = file.borrow();
		let id = id_for(ids, file.uuid)?;
		// `to_str` / the hashed string borrow the key/hash; bound immediately (rusqlite copies via
		// SQLITE_TRANSIENT), so these locals only need to outlive the bind call.
		let key_str = file.key.to_str();
		let hash_str = file.hash.as_ref().map(|h| h.as_sized_str().to_str());
		stmt.raw_bind_parameter(idx, id)?;
		stmt.raw_bind_parameter(idx + 1, file.chunks_size)?;
		stmt.raw_bind_parameter(idx + 2, file.chunks)?;
		stmt.raw_bind_parameter(idx + 3, file.favorited)?;
		stmt.raw_bind_parameter(idx + 4, file.region.as_ref())?;
		stmt.raw_bind_parameter(idx + 5, file.bucket.as_ref())?;
		stmt.raw_bind_parameter(idx + 6, file.timestamp.timestamp_millis())?;
		stmt.raw_bind_parameter(idx + 7, file.size)?;
		stmt.raw_bind_parameter(idx + 8, file.name.as_ref())?;
		stmt.raw_bind_parameter(idx + 9, file.mime.as_ref())?;
		stmt.raw_bind_parameter(idx + 10, key_str.as_ref())?;
		stmt.raw_bind_parameter(idx + 11, file.key.version() as i8)?;
		stmt.raw_bind_parameter(idx + 12, file.created.map(|c| c.timestamp_millis()))?;
		stmt.raw_bind_parameter(idx + 13, file.last_modified.timestamp_millis())?;
		stmt.raw_bind_parameter(idx + 14, hash_str.as_deref())?;
		idx += FILE_PARAMS_PER_ROW;
	}
	stmt.raw_execute()?;
	Ok(())
}

/// As [`file_rows_exec`] but for dirs.
fn dir_rows_exec<'a>(
	stmt: &mut Statement<'_>,
	dirs: &[impl Borrow<CacheableDir<'a>>],
	ids: &HashMap<Uuid, i64>,
) -> rusqlite::Result<()> {
	let mut idx = 1;
	for dir in dirs {
		let dir = dir.borrow();
		let id = id_for(ids, dir.uuid)?;
		stmt.raw_bind_parameter(idx, id)?;
		stmt.raw_bind_parameter(idx + 1, dir.favorited)?;
		stmt.raw_bind_parameter(idx + 2, dir.color.as_ref())?;
		stmt.raw_bind_parameter(idx + 3, dir.timestamp.timestamp_millis())?;
		stmt.raw_bind_parameter(idx + 4, dir.name.as_ref())?;
		stmt.raw_bind_parameter(idx + 5, dir.created.map(|t| t.timestamp_millis()))?;
		idx += DIR_PARAMS_PER_ROW;
	}
	stmt.raw_execute()?;
	Ok(())
}

/// Apply one file sub-batch: upsert its `items` rows (held full-size statement, or a one-off for a
/// partial final sub-batch) then its `files` rows, bridging the FK via the `RETURNING` map.
fn apply_file_subchunk<'a>(
	conn: &Connection,
	sub: &[impl Borrow<CacheableFile<'a>>],
	root_id: i64,
	items_full: &mut Statement<'_>,
	files_full: &mut Statement<'_>,
) -> rusqlite::Result<()> {
	if sub.len() == MULTI_ROW_CHUNK {
		let ids = file_items_into_map(items_full, sub, root_id)?;
		file_rows_exec(files_full, sub, &ids)
	} else {
		let mut items_stmt = conn.prepare_cached(&items_upsert_sql(sub.len()))?;
		let ids = file_items_into_map(&mut items_stmt, sub, root_id)?;
		let mut files_stmt = conn.prepare_cached(&files_upsert_sql(sub.len()))?;
		file_rows_exec(&mut files_stmt, sub, &ids)
	}
}

/// As [`apply_file_subchunk`] but for dirs.
fn apply_dir_subchunk<'a>(
	conn: &Connection,
	sub: &[impl Borrow<CacheableDir<'a>>],
	root_id: i64,
	items_full: &mut Statement<'_>,
	dirs_full: &mut Statement<'_>,
) -> rusqlite::Result<()> {
	if sub.len() == MULTI_ROW_CHUNK {
		let ids = dir_items_into_map(items_full, sub, root_id)?;
		dir_rows_exec(dirs_full, sub, &ids)
	} else {
		let mut items_stmt = conn.prepare_cached(&items_upsert_sql(sub.len()))?;
		let ids = dir_items_into_map(&mut items_stmt, sub, root_id)?;
		let mut dirs_stmt = conn.prepare_cached(&dirs_upsert_sql(sub.len()))?;
		dir_rows_exec(&mut dirs_stmt, sub, &ids)
	}
}

/// Upsert all `files` (and their `items` rows). Prepares the full-size statements ONCE and reuses
/// them across every full sub-batch. When `conn` is already in a transaction (the resync apply's
/// chunk, the drain) the sub-batches run on it directly; otherwise this opens one transaction per
/// [`CHUNK_SIZE`] rows (`unchecked_transaction`, so the held statement borrows coexist — auto
/// rolls back on early return).
pub(super) fn bulk_upsert_files<'a>(
	conn: &Connection,
	files: &[impl Borrow<CacheableFile<'a>>],
	root_id: i64,
) -> rusqlite::Result<()> {
	if files.is_empty() {
		return Ok(());
	}
	let mut items_full = conn.prepare_cached(&items_upsert_sql(MULTI_ROW_CHUNK))?;
	let mut files_full = conn.prepare_cached(&files_upsert_sql(MULTI_ROW_CHUNK))?;

	if conn.is_autocommit() {
		for tx_chunk in files.chunks(CHUNK_SIZE) {
			let tx = conn.unchecked_transaction()?;
			for sub in tx_chunk.chunks(MULTI_ROW_CHUNK) {
				apply_file_subchunk(conn, sub, root_id, &mut items_full, &mut files_full)?;
			}
			tx.commit()?;
		}
	} else {
		for sub in files.chunks(MULTI_ROW_CHUNK) {
			apply_file_subchunk(conn, sub, root_id, &mut items_full, &mut files_full)?;
		}
	}
	Ok(())
}

/// As [`bulk_upsert_files`] but for dirs.
pub(super) fn bulk_upsert_dirs<'a>(
	conn: &Connection,
	dirs: &[impl Borrow<CacheableDir<'a>>],
	root_id: i64,
) -> rusqlite::Result<()> {
	if dirs.is_empty() {
		return Ok(());
	}
	let mut items_full = conn.prepare_cached(&items_upsert_sql(MULTI_ROW_CHUNK))?;
	let mut dirs_full = conn.prepare_cached(&dirs_upsert_sql(MULTI_ROW_CHUNK))?;

	if conn.is_autocommit() {
		for tx_chunk in dirs.chunks(CHUNK_SIZE) {
			let tx = conn.unchecked_transaction()?;
			for sub in tx_chunk.chunks(MULTI_ROW_CHUNK) {
				apply_dir_subchunk(conn, sub, root_id, &mut items_full, &mut dirs_full)?;
			}
			tx.commit()?;
		}
	} else {
		for sub in dirs.chunks(MULTI_ROW_CHUNK) {
			apply_dir_subchunk(conn, sub, root_id, &mut items_full, &mut dirs_full)?;
		}
	}
	Ok(())
}

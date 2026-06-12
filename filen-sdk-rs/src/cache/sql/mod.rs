use std::borrow::Borrow;

use crate::fs::{
	dir::{DecryptedDirectoryMeta, cache::CacheableDir},
	file::{cache::CacheableFile, meta::DecryptedFileMeta},
};
use filen_types::api::v3::dir::color::DirColor;
use itertools::Itertools;
use rusqlite::config::DbConfig;
use uuid::Uuid;

use crate::cache::{CacheState, sql::statements::VACUUM};

mod diff;
mod dir;
mod event;
pub(crate) use event::PersistedEvent;
mod file;
mod item;
mod membership;
mod root;
mod statements;

// Per-transaction batch size for `execute_chunked`: large enough to amortise the per-commit (WAL
// fsync) overhead, small enough that one chunk is not a giant long-held write transaction on mobile
// storage.
const CHUNK_SIZE: usize = 10_000;

impl CacheState {
	/// Run `for_chunk` over `items` in `CHUNK_SIZE` batches, each batch in its OWN transaction (so a huge
	/// bulk op commits incrementally instead of holding one giant transaction). `for_chunk` prepares its
	/// statements on the connection and applies the chunk. Collapses the chunk/loop/commit scaffolding
	/// that the bulk methods below would otherwise each duplicate.
	///
	/// NESTING-AWARE: when the caller already holds an open transaction (the drain's batched
	/// fast path), everything runs bare on that transaction — the caller's commit is the
	/// durability boundary — since SQLite cannot nest `BEGIN`.
	fn execute_chunked<T>(
		&mut self,
		items: impl Iterator<Item = T>,
		mut for_chunk: impl FnMut(
			&rusqlite::Connection,
			&mut dyn Iterator<Item = T>,
		) -> rusqlite::Result<()>,
	) -> rusqlite::Result<()> {
		let mut items = items;
		if !self.db.is_autocommit() {
			return for_chunk(&self.db, &mut items);
		}
		let chunks = items.chunks(CHUNK_SIZE);
		for mut chunk in &chunks {
			let transaction = self.db.transaction()?;
			for_chunk(&transaction, &mut chunk)?;
			transaction.commit()?;
		}
		Ok(())
	}

	pub(crate) fn upsert_dirs<'a>(
		&mut self,
		dirs: impl Iterator<Item = impl Borrow<CacheableDir<'a>>>,
	) -> rusqlite::Result<()> {
		self.execute_chunked(dirs, |transaction, chunk| {
			let mut upsert_item_stmt = transaction.prepare_cached(statements::ITEM_UPSERT)?;
			let mut upsert_dir_stmt = transaction.prepare_cached(statements::DIR_UPSERT)?;
			for cache_dir in chunk {
				dir::upsert_dir_with_stmts(
					cache_dir.borrow(),
					&mut upsert_dir_stmt,
					&mut upsert_item_stmt,
				)?;
			}
			Ok(())
		})
	}

	pub(crate) fn upsert_files<'a>(
		&mut self,
		files: impl Iterator<Item = impl Borrow<CacheableFile<'a>>>,
	) -> rusqlite::Result<()> {
		self.execute_chunked(files, |transaction, chunk| {
			let mut upsert_item_stmt = transaction.prepare_cached(statements::ITEM_UPSERT)?;
			let mut upsert_file_stmt = transaction.prepare_cached(statements::FILE_UPSERT)?;
			for cache_file in chunk {
				file::upsert_file_with_stmts(
					cache_file.borrow(),
					&mut upsert_file_stmt,
					&mut upsert_item_stmt,
				)?;
			}
			Ok(())
		})
	}

	pub(crate) fn delete_items(
		&mut self,
		items: impl Iterator<Item = Uuid>,
	) -> rusqlite::Result<()> {
		self.execute_chunked(items, |transaction, chunk| {
			let mut delete_item_stmt = transaction.prepare_cached(statements::ITEM_DELETE)?;
			for uuid in chunk {
				item::delete_item_with_stmt(uuid, &mut delete_item_stmt)?;
			}
			Ok(())
		})
	}

	/// Wipe every non-root item (cascading to `dirs`/`files` via FK + trigger). The account-root item,
	/// the `events` store, and the `cache_meta` watermark are intentionally left intact — so the caller
	/// is responsible for scheduling a resync to re-converge the now-empty item cache.
	pub(crate) fn delete_all_non_root(&mut self) -> rusqlite::Result<()> {
		self.db.execute(statements::ITEM_DELETE_ALL_NON_ROOT, [])?;
		Ok(())
	}

	/// KNOWN LIMITATION: this patches the metadata columns but does NOT recompute `items.content_hash`,
	/// which then goes stale relative to the new name/size/etc. A later resync therefore emits ONE
	/// spurious `Changed` for this item (the listing's fingerprint differs from the stale hash); applying
	/// it re-upserts the row and refreshes the hash, so the cache self-heals after that single resync. We
	/// accept this bounded churn rather than reconstruct the full `CacheableFile` here just to recompute
	/// the fingerprint — that reconstruction is exactly what the fingerprint column exists to avoid.
	pub(crate) fn update_file_meta(
		&mut self,
		uuid: Uuid,
		meta: &DecryptedFileMeta<'_>,
	) -> rusqlite::Result<()> {
		self.db.execute(
			statements::FILE_UPDATE_META,
			rusqlite::params![
				meta.size,
				meta.name,
				meta.mime,
				meta.key.to_str().as_ref(),
				meta.key.version() as i8,
				meta.created.map(|c| c.timestamp_millis()),
				meta.last_modified.timestamp_millis(),
				meta.hash
					.as_ref()
					.map(|h| h.as_sized_str().to_str())
					.as_deref(),
				uuid,
			],
		)?;
		Ok(())
	}

	/// Like [`update_file_meta`](Self::update_file_meta), this patches `name`/`created` without
	/// refreshing `items.content_hash` (both fields participate in the dir fingerprint), so a later
	/// resync emits one self-healing `Changed`. Bounded churn, accepted for the same reason.
	pub(crate) fn update_dir_name(
		&mut self,
		uuid: Uuid,
		meta: &DecryptedDirectoryMeta<'_>,
	) -> rusqlite::Result<()> {
		self.db.execute(
			statements::DIR_UPDATE_NAME,
			rusqlite::params![meta.name, meta.created.map(|c| c.timestamp_millis()), uuid],
		)?;
		Ok(())
	}

	pub(crate) fn update_dir_color(
		&mut self,
		uuid: Uuid,
		color: &DirColor<'_>,
	) -> rusqlite::Result<()> {
		self.db.execute(
			statements::DIR_UPDATE_COLOR,
			rusqlite::params![color.as_ref(), uuid],
		)?;
		Ok(())
	}

	pub(crate) fn init_db(&mut self) -> rusqlite::Result<()> {
		// These settings are PER-CONNECTION (not persisted in the DB file), so they must be applied on
		// EVERY open — before the version check below, which early-returns on a matching reopen without
		// running `INIT`. `foreign_keys` + `recursive_triggers` are what make the cascade-on-delete
		// trigger recurse through a whole subtree (and the `files`/`dirs` FK cascades fire); skipping them
		// on a reopen would silently leave the cascade non-recursive after a restart. `temp_store=MEMORY`
		// keeps the resync staging TEMP table in RAM; `busy_timeout` retries a transient `SQLITE_BUSY`
		// instead of surfacing it as a mis-quarantined poison event in the drain. (`journal_mode=WAL`
		// DOES persist in the file header, so it stays in `INIT`.)
		self.db
			.busy_timeout(std::time::Duration::from_millis(5000))?;
		self.db.execute_batch(
			"PRAGMA foreign_keys = ON; PRAGMA recursive_triggers = ON; PRAGMA temp_store = MEMORY;",
		)?;

		let version: i64 = self
			.db
			.query_one(statements::GET_USER_VERSION, (), |row| row.get(0))?;

		if version == statements::SQL_USER_VERSION {
			return Ok(());
		}

		self.db
			.set_db_config(DbConfig::SQLITE_DBCONFIG_RESET_DATABASE, true)?;
		self.db.execute(VACUUM, [])?;
		self.db
			.set_db_config(DbConfig::SQLITE_DBCONFIG_RESET_DATABASE, false)?;

		self.db.execute_batch(statements::INIT)?;

		self.db.execute(statements::SET_USER_VERSION, [])?;

		// Propagate a root-insert failure instead of panicking — `init_db` already returns
		// `rusqlite::Result`, so a SQLite error surfaces to the caller (`CacheState::new`/test ctors).
		root::insert_root(self.root_uuid, &mut self.db)
	}
}

#[cfg(test)]
mod tests;

use std::borrow::Borrow;

use filen_sdk_rs::fs::{
	dir::{DecryptedDirectoryMeta, cache::CacheableDir},
	file::{cache::CacheableFile, meta::DecryptedFileMeta},
};
use filen_types::api::v3::dir::color::DirColor;
use itertools::Itertools;
use rusqlite::config::DbConfig;
use uuid::Uuid;

use crate::{CacheState, sql::statements::VACUUM};

mod dir;
mod file;
mod item;
mod root;
mod statements;

const CHUNK_SIZE: usize = 10_000;

impl CacheState {
	pub(crate) fn upsert_dirs<'a>(
		&mut self,
		dirs: impl Iterator<Item = impl Borrow<CacheableDir<'a>>>,
	) -> rusqlite::Result<()> {
		let dirs = dirs.chunks(CHUNK_SIZE);

		for dirs in dirs.into_iter() {
			let transaction = self.db.transaction()?;
			{
				let mut upsert_item_stmt = transaction.prepare_cached(statements::ITEM_UPSERT)?;
				let mut upsert_dir_stmt = transaction.prepare_cached(statements::DIR_UPSERT)?;

				for cache_dir in dirs {
					dir::upsert_dir_with_stmts(
						cache_dir.borrow(),
						self.root_uuid,
						&mut upsert_dir_stmt,
						&mut upsert_item_stmt,
					)?;
				}
			}

			transaction.commit()?;
		}
		Ok(())
	}

	pub(crate) fn upsert_files<'a>(
		&mut self,
		files: impl Iterator<Item = impl Borrow<CacheableFile<'a>>>,
	) -> rusqlite::Result<()> {
		let files = files.chunks(CHUNK_SIZE);

		for files in files.into_iter() {
			let transaction = self.db.transaction()?;
			{
				let mut upsert_item_stmt = transaction.prepare_cached(statements::ITEM_UPSERT)?;
				let mut upsert_file_stmt = transaction.prepare_cached(statements::FILE_UPSERT)?;

				for cache_file in files {
					file::upsert_file_with_stmts(
						cache_file.borrow(),
						self.root_uuid,
						&mut upsert_file_stmt,
						&mut upsert_item_stmt,
					)?;
				}
			}

			transaction.commit()?;
		}
		Ok(())
	}

	pub(crate) fn delete_items(
		&mut self,
		items: impl Iterator<Item = Uuid>,
	) -> rusqlite::Result<()> {
		let items = items.chunks(CHUNK_SIZE);

		for items in items.into_iter() {
			let transaction = self.db.transaction()?;
			{
				let mut delete_item_stmt = transaction.prepare_cached(statements::ITEM_DELETE)?;

				for uuid in items {
					item::delete_item_with_stmt(uuid, &mut delete_item_stmt)?;
				}
			}

			transaction.commit()?;
		}
		Ok(())
	}

	pub(crate) fn delete_all_non_root(&mut self) -> rusqlite::Result<()> {
		self.db.execute(statements::ITEM_DELETE_ALL_NON_ROOT, [])?;
		Ok(())
	}

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
				meta.key.as_ref().to_str().as_ref(),
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

		root::insert_root(self.root_uuid, &mut self.db).unwrap();
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use std::{borrow::Cow, iter::once};

	use chrono::Utc;
	use filen_sdk_rs::{
		crypto::file::FileKey,
		fs::{dir::cache::CacheableDir, file::cache::CacheableFile},
	};
	use filen_types::{api::v3::dir::color::DirColor, auth::FileEncryptionVersion};
	use rusqlite::params;
	use uuid::Uuid;

	use super::*;

	fn test_cache_state() -> CacheState {
		CacheState::new_in_memory()
	}

	fn make_file_key() -> FileKey {
		let key_hex = "a".repeat(64);
		FileKey::from_string_with_version(Cow::Owned(key_hex), FileEncryptionVersion::V3).unwrap()
	}

	fn make_cacheable_file(parent: Uuid) -> CacheableFile<'static> {
		let now = Utc::now();
		CacheableFile {
			uuid: Uuid::new_v4(),
			parent,
			chunks_size: 1024,
			chunks: 1,
			favorited: false,
			region: Cow::Owned("us-east-1".to_string()),
			bucket: Cow::Owned("test-bucket".to_string()),
			timestamp: now,
			name: Cow::Owned("test_file.txt".to_string()),
			size: 1024,
			mime: Cow::Owned("text/plain".to_string()),
			key: Cow::Owned(make_file_key()),
			last_modified: now,
			created: Some(now),
			hash: None,
		}
	}

	fn make_cacheable_dir(parent: Uuid) -> CacheableDir<'static> {
		let now = Utc::now();
		CacheableDir {
			uuid: Uuid::new_v4(),
			parent,
			color: DirColor::Default,
			favorited: false,
			timestamp: now,
			name: Cow::Owned("test_dir".to_string()),
			created: Some(now),
		}
	}

	// ─── Schema Tests ───────────────────────────────────────────────────────

	#[test]
	fn test_init_db_creates_all_tables() {
		let state = test_cache_state();
		let tables: Vec<String> = state
			.db
			.prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
			.unwrap()
			.query_map([], |row| row.get(0))
			.unwrap()
			.collect::<Result<_, _>>()
			.unwrap();

		assert!(tables.contains(&"items".to_string()));
		assert!(tables.contains(&"roots".to_string()));
		assert!(tables.contains(&"files".to_string()));
		assert!(tables.contains(&"dirs".to_string()));
		assert!(tables.contains(&"file_versions".to_string()));
	}

	#[test]
	fn test_init_db_inserts_root() {
		let state = test_cache_state();

		let root_exists: bool = state
			.db
			.query_row(
				"SELECT COUNT(*) > 0 FROM items WHERE uuid = ? AND type = 0",
				params![state.root_uuid],
				|row| row.get(0),
			)
			.unwrap();
		assert!(root_exists, "root item should exist");

		let root_in_roots: bool = state
			.db
			.query_row(
				"SELECT COUNT(*) > 0 FROM roots r JOIN items i ON i.id = r.id WHERE i.uuid = ?",
				params![state.root_uuid],
				|row| row.get(0),
			)
			.unwrap();
		assert!(root_in_roots, "root should be in roots table");
	}

	#[test]
	fn test_init_db_is_idempotent() {
		let mut state = test_cache_state();

		// Insert some data
		let dir = make_cacheable_dir(state.root_uuid);
		state.upsert_dirs(once(&dir)).unwrap();

		// Re-init should not error (just returns early because version matches)
		state.init_db().unwrap();

		// Data should still be there
		let exists: bool = state
			.db
			.query_row(
				"SELECT COUNT(*) > 0 FROM items WHERE uuid = ?",
				params![dir.uuid],
				|row| row.get(0),
			)
			.unwrap();
		assert!(exists, "data should survive idempotent init");
	}

	#[test]
	fn test_root_has_null_parent() {
		let state = test_cache_state();

		let parent: Option<Vec<u8>> = state
			.db
			.query_row(
				"SELECT parent FROM items WHERE uuid = ?",
				params![state.root_uuid],
				|row| row.get(0),
			)
			.unwrap();
		assert!(parent.is_none(), "root should have NULL parent");
	}

	// ─── Directory Upsert Tests ─────────────────────────────────────────────

	#[test]
	fn test_upsert_single_dir() {
		let mut state = test_cache_state();
		let dir = make_cacheable_dir(state.root_uuid);

		state.upsert_dirs(once(&dir)).unwrap();

		let (name,): (String,) = state
			.db
			.query_row(
				"SELECT d.name FROM items i JOIN dirs d ON d.id = i.id WHERE i.uuid = ?",
				params![dir.uuid],
				|row| Ok((row.get(0)?,)),
			)
			.unwrap();
		assert_eq!(name, "test_dir");
	}

	#[test]
	fn test_upsert_multiple_dirs() {
		let mut state = test_cache_state();
		let dirs: Vec<_> = (0..10)
			.map(|_| make_cacheable_dir(state.root_uuid))
			.collect();

		state.upsert_dirs(dirs.iter()).unwrap();

		let count: usize = state
			.db
			.query_row("SELECT COUNT(*) FROM dirs", [], |row| row.get(0))
			.unwrap();
		assert_eq!(count, 10);
	}

	#[test]
	fn test_upsert_dir_stores_correct_type() {
		let mut state = test_cache_state();
		let dir = make_cacheable_dir(state.root_uuid);

		state.upsert_dirs(once(&dir)).unwrap();

		let item_type: i8 = state
			.db
			.query_row(
				"SELECT type FROM items WHERE uuid = ?",
				params![dir.uuid],
				|row| row.get(0),
			)
			.unwrap();
		assert_eq!(item_type, 1, "dir should have type 1");
	}

	#[test]
	fn test_upsert_dir_stores_metadata() {
		let mut state = test_cache_state();
		let now = Utc::now();
		let dir = CacheableDir {
			uuid: Uuid::new_v4(),
			parent: state.root_uuid,
			color: DirColor::Blue,
			favorited: true,
			timestamp: now,
			name: Cow::Owned("colored_dir".to_string()),
			created: Some(now),
		};

		state.upsert_dirs(once(&dir)).unwrap();

		let (name, favorite, color): (String, bool, Option<String>) = state
			.db
			.query_row(
				"SELECT d.name, d.favorite, d.color FROM items i JOIN dirs d ON d.id = i.id WHERE i.uuid = ?",
				params![dir.uuid],
				|row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
			)
			.unwrap();
		assert_eq!(name, "colored_dir");
		assert!(favorite);
		assert_eq!(color.as_deref(), Some("blue"));
	}

	// ─── File Upsert Tests ──────────────────────────────────────────────────

	#[test]
	fn test_upsert_single_file() {
		let mut state = test_cache_state();
		let file = make_cacheable_file(state.root_uuid);

		state.upsert_files(once(&file)).unwrap();

		let (name, size, mime): (String, i64, String) = state
			.db
			.query_row(
				"SELECT f.name, f.size, f.mime FROM items i JOIN files f ON f.id = i.id WHERE i.uuid = ?",
				params![file.uuid],
				|row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
			)
			.unwrap();
		assert_eq!(name, "test_file.txt");
		assert_eq!(size, 1024);
		assert_eq!(mime, "text/plain");
	}

	#[test]
	fn test_upsert_multiple_files() {
		let mut state = test_cache_state();
		let files: Vec<_> = (0..10)
			.map(|_| make_cacheable_file(state.root_uuid))
			.collect();

		state.upsert_files(files.iter()).unwrap();

		let count: usize = state
			.db
			.query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
			.unwrap();
		assert_eq!(count, 10);
	}

	#[test]
	fn test_upsert_file_stores_correct_type() {
		let mut state = test_cache_state();
		let file = make_cacheable_file(state.root_uuid);

		state.upsert_files(once(&file)).unwrap();

		let item_type: i8 = state
			.db
			.query_row(
				"SELECT type FROM items WHERE uuid = ?",
				params![file.uuid],
				|row| row.get(0),
			)
			.unwrap();
		assert_eq!(item_type, 2, "file should have type 2");
	}

	#[test]
	fn test_upsert_file_stores_key_and_version() {
		let mut state = test_cache_state();
		let file = make_cacheable_file(state.root_uuid);

		state.upsert_files(once(&file)).unwrap();

		let (key, version): (String, i8) = state
			.db
			.query_row(
				"SELECT f.file_key, f.file_key_version FROM items i JOIN files f ON f.id = i.id WHERE i.uuid = ?",
				params![file.uuid],
				|row| Ok((row.get(0)?, row.get(1)?)),
			)
			.unwrap();
		assert!(!key.is_empty());
		assert_eq!(version, 3, "V3 key should have version 3");
	}

	#[test]
	fn test_upsert_file_with_favorite() {
		let mut state = test_cache_state();
		let mut file = make_cacheable_file(state.root_uuid);
		file.favorited = true;

		state.upsert_files(once(&file)).unwrap();

		let favorite: bool = state
			.db
			.query_row(
				"SELECT f.favorite FROM items i JOIN files f ON f.id = i.id WHERE i.uuid = ?",
				params![file.uuid],
				|row| row.get(0),
			)
			.unwrap();
		assert!(favorite);
	}

	// ─── Delete Tests ───────────────────────────────────────────────────────

	#[test]
	fn test_delete_file() {
		let mut state = test_cache_state();
		let file = make_cacheable_file(state.root_uuid);
		state.upsert_files(once(&file)).unwrap();

		// Verify it exists
		let count: usize = state
			.db
			.query_row(
				"SELECT COUNT(*) FROM items WHERE uuid = ?",
				params![file.uuid],
				|row| row.get(0),
			)
			.unwrap();
		assert_eq!(count, 1);

		// Delete it
		state.delete_items(once(file.uuid)).unwrap();

		// Verify it's gone from items
		let count: usize = state
			.db
			.query_row(
				"SELECT COUNT(*) FROM items WHERE uuid = ?",
				params![file.uuid],
				|row| row.get(0),
			)
			.unwrap();
		assert_eq!(count, 0);

		// Verify cascade: file metadata also gone
		let file_count: usize = state
			.db
			.query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
			.unwrap();
		assert_eq!(file_count, 0, "file row should be cascade-deleted");
	}

	#[test]
	fn test_delete_dir() {
		let mut state = test_cache_state();
		let dir = make_cacheable_dir(state.root_uuid);
		state.upsert_dirs(once(&dir)).unwrap();

		state.delete_items(once(dir.uuid)).unwrap();

		let count: usize = state
			.db
			.query_row(
				"SELECT COUNT(*) FROM items WHERE uuid = ?",
				params![dir.uuid],
				|row| row.get(0),
			)
			.unwrap();
		assert_eq!(count, 0);

		let dir_count: usize = state
			.db
			.query_row("SELECT COUNT(*) FROM dirs", [], |row| row.get(0))
			.unwrap();
		assert_eq!(dir_count, 0, "dir row should be cascade-deleted");
	}

	#[test]
	fn test_delete_nonexistent_item() {
		let mut state = test_cache_state();
		// Should not error
		state.delete_items(once(Uuid::new_v4())).unwrap();
	}

	// ─── Cascade Delete Tests ───────────────────────────────────────────────

	#[test]
	fn test_cascade_delete_dir_removes_children() {
		let mut state = test_cache_state();

		// Insert a directory under root
		let parent_dir = make_cacheable_dir(state.root_uuid);
		state.upsert_dirs(once(&parent_dir)).unwrap();

		// Insert children under that directory (bypassing root_id lookup by using raw SQL)
		// We need to manually insert items with the dir as parent since the normal
		// upsert path requires the parent to be a root.
		let child_uuid_1 = Uuid::new_v4();
		let child_uuid_2 = Uuid::new_v4();

		// Get root_id for reference
		let root_id: i64 = state
			.db
			.query_row(
				"SELECT id FROM items WHERE uuid = ?",
				params![state.root_uuid],
				|row| row.get(0),
			)
			.unwrap();

		// Insert child items directly via SQL
		state
			.db
			.execute(
				"INSERT INTO items (root_id, uuid, parent, type) VALUES (?, ?, ?, 2)",
				params![root_id, child_uuid_1, parent_dir.uuid],
			)
			.unwrap();
		state
			.db
			.execute(
				"INSERT INTO items (root_id, uuid, parent, type) VALUES (?, ?, ?, 2)",
				params![root_id, child_uuid_2, parent_dir.uuid],
			)
			.unwrap();

		// Verify children exist
		let count: usize = state
			.db
			.query_row(
				"SELECT COUNT(*) FROM items WHERE parent = ?",
				params![parent_dir.uuid],
				|row| row.get(0),
			)
			.unwrap();
		assert_eq!(count, 2, "should have 2 child items");

		// Delete the parent directory
		state.delete_items(once(parent_dir.uuid)).unwrap();

		// Children should be cascade-deleted via the trigger
		let count: usize = state
			.db
			.query_row(
				"SELECT COUNT(*) FROM items WHERE parent = ?",
				params![parent_dir.uuid],
				|row| row.get(0),
			)
			.unwrap();
		assert_eq!(count, 0, "children should be cascade-deleted");

		// Verify the specific UUIDs are gone too
		let c1: usize = state
			.db
			.query_row(
				"SELECT COUNT(*) FROM items WHERE uuid = ?",
				params![child_uuid_1],
				|row| row.get(0),
			)
			.unwrap();
		let c2: usize = state
			.db
			.query_row(
				"SELECT COUNT(*) FROM items WHERE uuid = ?",
				params![child_uuid_2],
				|row| row.get(0),
			)
			.unwrap();
		assert_eq!(c1, 0, "child 1 should be gone");
		assert_eq!(c2, 0, "child 2 should be gone");
	}

	#[test]
	fn test_cascade_delete_is_recursive() {
		let mut state = test_cache_state();

		// Create a 3-level hierarchy: root -> dir_a -> dir_b -> file_c
		let dir_a = make_cacheable_dir(state.root_uuid);
		state.upsert_dirs(once(&dir_a)).unwrap();

		let root_id: i64 = state
			.db
			.query_row(
				"SELECT id FROM items WHERE uuid = ?",
				params![state.root_uuid],
				|row| row.get(0),
			)
			.unwrap();

		let dir_b_uuid = Uuid::new_v4();
		let file_c_uuid = Uuid::new_v4();

		// Insert dir_b under dir_a (type=1 for dir)
		state
			.db
			.execute(
				"INSERT INTO items (root_id, uuid, parent, type) VALUES (?, ?, ?, 1)",
				params![root_id, dir_b_uuid, dir_a.uuid],
			)
			.unwrap();

		// Insert file_c under dir_b (type=2 for file)
		state
			.db
			.execute(
				"INSERT INTO items (root_id, uuid, parent, type) VALUES (?, ?, ?, 2)",
				params![root_id, file_c_uuid, dir_b_uuid],
			)
			.unwrap();

		// Delete dir_a — should cascade-delete dir_b and file_c
		state.delete_items(once(dir_a.uuid)).unwrap();

		let total: usize = state
			.db
			.query_row(
				// Only root should remain
				"SELECT COUNT(*) FROM items",
				[],
				|row| row.get(0),
			)
			.unwrap();
		assert_eq!(
			total, 1,
			"only root should remain after recursive cascade delete"
		);
	}

	#[test]
	fn test_cascade_delete_does_not_affect_siblings() {
		let mut state = test_cache_state();

		// Create two sibling directories under root
		let dir_a = make_cacheable_dir(state.root_uuid);
		let dir_b = make_cacheable_dir(state.root_uuid);
		state.upsert_dirs([&dir_a, &dir_b].into_iter()).unwrap();

		// Delete dir_a — dir_b should survive
		state.delete_items(once(dir_a.uuid)).unwrap();

		let a_count: usize = state
			.db
			.query_row(
				"SELECT COUNT(*) FROM items WHERE uuid = ?",
				params![dir_a.uuid],
				|row| row.get(0),
			)
			.unwrap();
		let b_count: usize = state
			.db
			.query_row(
				"SELECT COUNT(*) FROM items WHERE uuid = ?",
				params![dir_b.uuid],
				|row| row.get(0),
			)
			.unwrap();
		assert_eq!(a_count, 0, "deleted dir should be gone");
		assert_eq!(b_count, 1, "sibling dir should survive");
	}

	// ─── File Upsert (Update) Tests ─────────────────────────────────────────

	#[test]
	fn test_upsert_file_update_preserves_uuid() {
		let mut state = test_cache_state();
		let file = make_cacheable_file(state.root_uuid);
		state.upsert_files(once(&file)).unwrap();

		// Create an updated version with the same uuid
		let mut updated = make_cacheable_file(state.root_uuid);
		updated.uuid = file.uuid;
		updated.name = Cow::Owned("renamed_file.txt".to_string());
		updated.size = 2048;

		// This exercises the ON CONFLICT path in file_upsert.sql
		state.upsert_files(once(&updated)).unwrap();

		// Should still only have one entry
		let count: usize = state
			.db
			.query_row(
				"SELECT COUNT(*) FROM items WHERE uuid = ?",
				params![file.uuid],
				|row| row.get(0),
			)
			.unwrap();
		assert_eq!(count, 1, "upsert should not create duplicate");

		let (name, size): (String, i64) = state
			.db
			.query_row(
				"SELECT f.name, f.size FROM items i JOIN files f ON f.id = i.id WHERE i.uuid = ?",
				params![file.uuid],
				|row| Ok((row.get(0)?, row.get(1)?)),
			)
			.unwrap();
		assert_eq!(name, "renamed_file.txt");
		assert_eq!(size, 2048);
	}

	// ─── Mixed Operation Tests ──────────────────────────────────────────────

	#[test]
	fn test_interleaved_insert_and_delete() {
		let mut state = test_cache_state();

		let file_1 = make_cacheable_file(state.root_uuid);
		let file_2 = make_cacheable_file(state.root_uuid);
		let dir_1 = make_cacheable_dir(state.root_uuid);

		// Insert all three
		state.upsert_files([&file_1, &file_2].into_iter()).unwrap();
		state.upsert_dirs(once(&dir_1)).unwrap();

		// Delete file_1
		state.delete_items(once(file_1.uuid)).unwrap();

		// file_1 gone, file_2 and dir_1 still present
		let items: Vec<Vec<u8>> = state
			.db
			.prepare("SELECT uuid FROM items WHERE type != 0")
			.unwrap()
			.query_map([], |row| row.get(0))
			.unwrap()
			.collect::<Result<_, _>>()
			.unwrap();

		assert_eq!(items.len(), 2, "should have 2 non-root items remaining");
	}

	#[test]
	fn test_delete_multiple_items_in_one_call() {
		let mut state = test_cache_state();

		let files: Vec<_> = (0..5)
			.map(|_| make_cacheable_file(state.root_uuid))
			.collect();
		state.upsert_files(files.iter()).unwrap();

		let uuids_to_delete = files.iter().take(3).map(|f| f.uuid);
		state.delete_items(uuids_to_delete).unwrap();

		let remaining: usize = state
			.db
			.query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
			.unwrap();
		assert_eq!(remaining, 2, "should have 2 files left after deleting 3");
	}

	// ─── File Meta Update Tests ─────────────────────────────────────────────

	#[test]
	fn test_update_file_meta() {
		let mut state = test_cache_state();
		let file = make_cacheable_file(state.root_uuid);
		state.upsert_files(once(&file)).unwrap();

		let new_key = make_file_key();
		let new_modified = Utc::now();

		let meta = DecryptedFileMeta {
			name: Cow::Borrowed("renamed.txt"),
			size: 2048,
			mime: Cow::Borrowed("application/octet-stream"),
			key: Cow::Borrowed(&new_key),
			last_modified: new_modified,
			created: None,
			hash: None,
		};

		state.update_file_meta(file.uuid, &meta).unwrap();

		let (name, size, mime): (String, i64, String) = state
			.db
			.query_row(
				"SELECT f.name, f.size, f.mime FROM items i JOIN files f ON f.id = i.id WHERE i.uuid = ?",
				params![file.uuid],
				|row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
			)
			.unwrap();
		assert_eq!(name, "renamed.txt");
		assert_eq!(size, 2048);
		assert_eq!(mime, "application/octet-stream");
	}

	#[test]
	fn test_update_file_meta_preserves_non_meta_fields() {
		let mut state = test_cache_state();
		let file = make_cacheable_file(state.root_uuid);
		state.upsert_files(once(&file)).unwrap();

		let meta = DecryptedFileMeta {
			name: Cow::Borrowed("new_name.txt"),
			size: 999,
			mime: Cow::Borrowed("text/html"),
			key: Cow::Borrowed(&file.key),
			last_modified: file.last_modified,
			created: file.created,
			hash: None,
		};

		// Update only the metadata
		state.update_file_meta(file.uuid, &meta).unwrap();

		// Verify non-meta fields (region, bucket, favorite, chunks) are unchanged
		let (region, bucket, favorite, chunks): (String, String, bool, i64) = state
			.db
			.query_row(
				"SELECT f.region, f.bucket, f.favorite, f.chunks FROM items i JOIN files f ON f.id = i.id WHERE i.uuid = ?",
				params![file.uuid],
				|row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
			)
			.unwrap();
		assert_eq!(region, "us-east-1");
		assert_eq!(bucket, "test-bucket");
		assert!(!favorite);
		assert_eq!(chunks, 1);
	}

	#[test]
	fn test_update_file_meta_nonexistent_is_noop() {
		let mut state = test_cache_state();

		let meta = DecryptedFileMeta {
			name: Cow::Borrowed("ghost.txt"),
			size: 0,
			mime: Cow::Borrowed("text/plain"),
			key: Cow::Borrowed(&make_file_key()),
			last_modified: Utc::now(),
			created: None,
			hash: None,
		};
		// Updating a nonexistent file should not error (0 rows affected)
		state.update_file_meta(Uuid::new_v4(), &meta).unwrap();
	}

	// ─── Dir Name Update Tests ──────────────────────────────────────────────

	#[test]
	fn test_update_dir_name() {
		let mut state = test_cache_state();
		let dir = make_cacheable_dir(state.root_uuid);
		state.upsert_dirs(once(&dir)).unwrap();

		let decrypted_meta = DecryptedDirectoryMeta {
			name: Cow::Borrowed("renamed_dir"),
			created: None,
		};

		state.update_dir_name(dir.uuid, &decrypted_meta).unwrap();

		let name: String = state
			.db
			.query_row(
				"SELECT d.name FROM items i JOIN dirs d ON d.id = i.id WHERE i.uuid = ?",
				params![dir.uuid],
				|row| row.get(0),
			)
			.unwrap();
		assert_eq!(name, "renamed_dir");
	}

	#[test]
	fn test_update_dir_name_preserves_other_fields() {
		let mut state = test_cache_state();
		let dir = CacheableDir {
			uuid: Uuid::new_v4(),
			parent: state.root_uuid,
			color: DirColor::Purple,
			favorited: true,
			timestamp: Utc::now(),
			name: Cow::Owned("original".to_string()),
			created: Some(Utc::now()),
		};
		state.upsert_dirs(once(&dir)).unwrap();

		let decrypted_meta = DecryptedDirectoryMeta {
			name: Cow::Borrowed("updated"),
			created: dir.created,
		};

		state.update_dir_name(dir.uuid, &decrypted_meta).unwrap();

		let (name, favorite, color): (String, bool, Option<String>) = state
			.db
			.query_row(
				"SELECT d.name, d.favorite, d.color FROM items i JOIN dirs d ON d.id = i.id WHERE i.uuid = ?",
				params![dir.uuid],
				|row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
			)
			.unwrap();
		assert_eq!(name, "updated");
		assert!(favorite, "favorite should be preserved");
		assert_eq!(
			color.as_deref(),
			Some("purple"),
			"color should be preserved"
		);
	}

	#[test]
	fn test_update_dir_name_nonexistent_is_noop() {
		let mut state = test_cache_state();
		let decrypted_meta = DecryptedDirectoryMeta {
			name: Cow::Borrowed("ghost_dir"),
			created: None,
		};

		state
			.update_dir_name(Uuid::new_v4(), &decrypted_meta)
			.unwrap();
	}

	// ─── Dir Color Update Tests ─────────────────────────────────────────────

	#[test]
	fn test_update_dir_color() {
		let mut state = test_cache_state();
		let dir = make_cacheable_dir(state.root_uuid);
		state.upsert_dirs(once(&dir)).unwrap();

		state.update_dir_color(dir.uuid, &DirColor::Red).unwrap();

		let color: Option<String> = state
			.db
			.query_row(
				"SELECT d.color FROM items i JOIN dirs d ON d.id = i.id WHERE i.uuid = ?",
				params![dir.uuid],
				|row| row.get(0),
			)
			.unwrap();
		assert_eq!(color.as_deref(), Some("red"));
	}

	#[test]
	fn test_update_dir_color_preserves_other_fields() {
		let mut state = test_cache_state();
		let dir = CacheableDir {
			uuid: Uuid::new_v4(),
			parent: state.root_uuid,
			color: DirColor::Blue,
			favorited: true,
			timestamp: Utc::now(),
			name: Cow::Owned("my_dir".to_string()),
			created: Some(Utc::now()),
		};
		state.upsert_dirs(once(&dir)).unwrap();

		state.update_dir_color(dir.uuid, &DirColor::Green).unwrap();

		let (name, favorite, color): (String, bool, Option<String>) = state
			.db
			.query_row(
				"SELECT d.name, d.favorite, d.color FROM items i JOIN dirs d ON d.id = i.id WHERE i.uuid = ?",
				params![dir.uuid],
				|row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
			)
			.unwrap();
		assert_eq!(name, "my_dir", "name should be preserved");
		assert!(favorite, "favorite should be preserved");
		assert_eq!(color.as_deref(), Some("green"));
	}

	#[test]
	fn test_update_dir_color_nonexistent_is_noop() {
		let mut state = test_cache_state();
		state
			.update_dir_color(Uuid::new_v4(), &DirColor::Blue)
			.unwrap();
	}

	// ─── Delete All Non-Root Tests ──────────────────────────────────────────

	#[test]
	fn test_delete_all_non_root() {
		let mut state = test_cache_state();

		// Insert some dirs and files
		let dirs: Vec<_> = (0..5)
			.map(|_| make_cacheable_dir(state.root_uuid))
			.collect();
		let files: Vec<_> = (0..5)
			.map(|_| make_cacheable_file(state.root_uuid))
			.collect();
		state.upsert_dirs(dirs.iter()).unwrap();
		state.upsert_files(files.iter()).unwrap();

		// Verify items exist
		let total: usize = state
			.db
			.query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
			.unwrap();
		assert_eq!(total, 11, "should have 1 root + 5 dirs + 5 files");

		// Delete all non-root
		state.delete_all_non_root().unwrap();

		// Only root should remain
		let total: usize = state
			.db
			.query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
			.unwrap();
		assert_eq!(total, 1, "only root should remain");

		let root_exists: bool = state
			.db
			.query_row(
				"SELECT COUNT(*) > 0 FROM items WHERE uuid = ? AND type = 0",
				params![state.root_uuid],
				|row| row.get(0),
			)
			.unwrap();
		assert!(root_exists, "root should still exist");
	}

	#[test]
	fn test_delete_all_non_root_cascades_metadata() {
		let mut state = test_cache_state();

		state
			.upsert_dirs(once(&make_cacheable_dir(state.root_uuid)))
			.unwrap();
		state
			.upsert_files(once(&make_cacheable_file(state.root_uuid)))
			.unwrap();

		state.delete_all_non_root().unwrap();

		let file_count: usize = state
			.db
			.query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
			.unwrap();
		let dir_count: usize = state
			.db
			.query_row("SELECT COUNT(*) FROM dirs", [], |row| row.get(0))
			.unwrap();
		assert_eq!(file_count, 0, "files table should be empty");
		assert_eq!(dir_count, 0, "dirs table should be empty");
	}

	#[test]
	fn test_delete_all_non_root_on_empty_db() {
		let mut state = test_cache_state();
		// Should not error when only root exists
		state.delete_all_non_root().unwrap();

		let total: usize = state
			.db
			.query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
			.unwrap();
		assert_eq!(total, 1, "root should still exist");
	}

	// ─── Stress Tests ───────────────────────────────────────────────────────

	#[test]
	fn test_bulk_insert_many_items() {
		let mut state = test_cache_state();

		let dirs: Vec<_> = (0..500)
			.map(|_| make_cacheable_dir(state.root_uuid))
			.collect();
		let files: Vec<_> = (0..1000)
			.map(|_| make_cacheable_file(state.root_uuid))
			.collect();

		state.upsert_dirs(dirs.iter()).unwrap();
		state.upsert_files(files.iter()).unwrap();

		let total: usize = state
			.db
			.query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
			.unwrap();
		// 1 root + 500 dirs + 1000 files = 1501
		assert_eq!(total, 1501);
	}
}

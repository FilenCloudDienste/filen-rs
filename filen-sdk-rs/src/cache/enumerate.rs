//! The sync engine's read side of the cache: a consistent whole-subtree snapshot of one sync
//! root, hydrated into the same [`CacheableDir`]/[`CacheableFile`] payloads the event dispatch and
//! search expose, PLUS the contiguous-prefix watermark captured in the SAME read transaction.
//!
//! Pairing the snapshot with its watermark is the point: the sync engine subscribes to the root's
//! event stream FIRST, then takes this snapshot, then discards buffered events whose
//! `drive_message_id <= watermark` (already reflected here) and applies the rest — aligning the
//! snapshot with the live stream with no gap and no double-apply. Reading the items and the
//! watermark inside one deferred read transaction guarantees they describe the same committed
//! instant even while the worker writes concurrently (WAL).
//!
//! Native only: like the search engine it opens its OWN read-only connection, which the wasm
//! single-connection VFS does not support — and the sync engine that consumes it is native-only.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

use crate::{
	Error, ErrorKind,
	auth::Client,
	cache::{
		CacheError, SearchResult,
		search::{open_read_connection, row_to_result},
		sql::{
			UndecodableItem,
			columns::ITEMS_UUID,
			list_undecodable,
			statements::{ANCESTRY_OF_UUID, CACHE_META_GET, ENUMERATE_SUBTREE, WATERMARK_KEY},
		},
	},
	fs::{dir::cache::CacheableDir, file::cache::CacheableFile},
};

/// A consistent point-in-time view of one sync root's cached subtree: every descendant directory
/// and file, plus the contiguous-prefix [`watermark`](Self::watermark) at the same instant.
#[derive(Debug)]
pub(crate) struct SubtreeSnapshot {
	pub(crate) dirs: Vec<CacheableDir<'static>>,
	pub(crate) files: Vec<CacheableFile<'static>>,
	/// The cache's contiguous-prefix watermark (`last_drive_message_id`) at the snapshot instant.
	/// `None` on a fresh cache that has applied nothing yet. Every buffered event whose
	/// `drive_message_id <= watermark` is already reflected in `dirs`/`files`.
	pub(crate) watermark: Option<u64>,
	/// The listed records whose metadata did not decode, and so are in neither `dirs` nor `files`.
	/// NOT scoped to `root`: the table holds every sync root's, and a record is only placeable by
	/// its parent, which the caller resolves against `dirs`.
	pub(crate) undecodable: Vec<UndecodableItem>,
}

/// Hydrate every descendant of `root` from `conn` into split dir/file vecs, reusing the search
/// engine's [`row_to_result`] column contract. The anchor itself is never returned.
fn enumerate_subtree(
	conn: &Connection,
	root: Uuid,
) -> rusqlite::Result<(Vec<CacheableDir<'static>>, Vec<CacheableFile<'static>>)> {
	let mut stmt = conn.prepare_cached(ENUMERATE_SUBTREE)?;
	let mut dirs = Vec::new();
	let mut files = Vec::new();
	let rows = stmt.query_map(params![root], row_to_result)?;
	for row in rows {
		match row? {
			SearchResult::Dir(dir) => dirs.push(dir),
			SearchResult::File(file) => files.push(file),
		}
	}
	Ok((dirs, files))
}

/// Read the contiguous-prefix watermark from `conn`, mirroring `CacheState::watermark` (the
/// `u64 ← i64` SQLite-boundary cast; the account counter never nears `i64::MAX`). The outer
/// `Option` guards an absent seed row; the inner is the NULL-able value.
fn read_watermark(conn: &Connection) -> rusqlite::Result<Option<u64>> {
	let row: Option<Option<i64>> = conn
		.prepare_cached(CACHE_META_GET)?
		.query_row(params![WATERMARK_KEY], |row| row.get::<_, Option<i64>>(0))
		.optional()?;
	Ok(row.flatten().map(|id| id as u64))
}

/// Read a consistent [`SubtreeSnapshot`] of `root` from the cache DB at `path`. Opens a fresh
/// read-only connection (WAL-concurrent with the worker's writer) and reads the subtree AND the
/// watermark inside ONE deferred read transaction, so the two describe the same committed instant.
/// Synchronous SQLite work — async callers go through [`Client::enumerate_sync_root_snapshot`].
pub(crate) fn read_subtree_snapshot(path: &Path, root: Uuid) -> rusqlite::Result<SubtreeSnapshot> {
	let mut conn = open_read_connection(path)?;
	// One deferred read transaction = one stable snapshot for both reads, even mid-worker-write.
	let tx = conn.transaction()?;
	let (dirs, files) = enumerate_subtree(&tx, root)?;
	let watermark = read_watermark(&tx)?;
	let undecodable = list_undecodable(&tx)?;
	// Read-only: dropping the deferred transaction just ends the snapshot (nothing to commit).
	drop(tx);
	Ok(SubtreeSnapshot {
		dirs,
		files,
		watermark,
		undecodable,
	})
}

/// The cached upward ancestor chain of `uuid` — the item itself plus every ancestor up to the
/// account root — read from the cache DB at `path`. One indexed recursive walk of `items.parent`,
/// cycle-safe (see the SQL).
///
/// EMPTY means the cache has never seen `uuid`, which is not the same as "it has no ancestors":
/// callers must treat an empty answer as UNKNOWN, never as evidence of where the item sits.
pub(crate) fn read_ancestors(path: &Path, uuid: Uuid) -> rusqlite::Result<Vec<Uuid>> {
	let conn = open_read_connection(path)?;
	let mut stmt = conn.prepare(ANCESTRY_OF_UUID)?;
	let rows = stmt.query_map(params![uuid], |row| row.get::<_, Uuid>(ITEMS_UUID))?;
	rows.collect()
}

impl Client {
	/// The cached ancestor chain of `uuid` (see [`read_ancestors`] — an empty answer means the
	/// cache does not know the item, not that it has no ancestors). Errors if the cache was never
	/// configured. The blocking SQLite read runs on a blocking thread.
	pub(crate) async fn cached_ancestors(&self, uuid: Uuid) -> Result<Vec<Uuid>, Error> {
		let path = self.cache_slot.lock().await.db_path().ok_or_else(|| {
			Error::custom(
				ErrorKind::InvalidState,
				"cache is not configured; call configure_cache first",
			)
		})?;
		tokio::task::spawn_blocking(move || read_ancestors(&path, uuid))
			.await
			.map_err(|e| {
				Error::custom(
					ErrorKind::Internal,
					format!("ancestry read task failed: {e}"),
				)
			})?
			.map_err(|e| {
				Error::custom_with_source(
					ErrorKind::Internal,
					CacheError::db(e, format!("reading the ancestry of {uuid}")),
					Some("reading cached ancestry".to_string()),
				)
			})
	}

	/// Take a consistent [`SubtreeSnapshot`] of the cached subtree under sync root `root` — the
	/// remote-state half the sync engine reconciles against. Errors if the cache was never
	/// configured. The blocking SQLite read runs on a blocking thread so it never stalls the
	/// async runtime.
	pub(crate) async fn enumerate_sync_root_snapshot(
		&self,
		root: Uuid,
	) -> Result<SubtreeSnapshot, Error> {
		let path = self.cache_slot.lock().await.db_path().ok_or_else(|| {
			Error::custom(
				ErrorKind::InvalidState,
				"cache is not configured; call configure_cache first",
			)
		})?;
		tokio::task::spawn_blocking(move || read_subtree_snapshot(&path, root))
			.await
			.map_err(|e| {
				Error::custom(
					ErrorKind::Internal,
					format!("snapshot read task failed: {e}"),
				)
			})?
			.map_err(|e| {
				Error::custom_with_source(
					ErrorKind::Internal,
					CacheError::db(e, format!("enumerating sync root {root}")),
					Some("reading cache subtree snapshot".to_string()),
				)
			})
	}
}

#[cfg(test)]
mod tests {
	use std::borrow::Cow;

	use chrono::{DateTime, Utc};
	use filen_types::{
		api::v3::dir::color::DirColor, auth::FileEncryptionVersion, crypto::Blake3Hash,
		fs::StableUuid,
	};
	use uuid::Uuid;

	use crate::{cache::CacheState, crypto::file::FileKey};

	use super::*;

	fn temp_db_path() -> std::path::PathBuf {
		std::env::temp_dir().join(format!("filen_enumerate_test_{}.db", Uuid::new_v4()))
	}

	fn ms(millis: i64) -> DateTime<Utc> {
		DateTime::from_timestamp_millis(millis).unwrap()
	}

	fn test_dir(uuid: Uuid, parent: Uuid, name: &str) -> CacheableDir<'static> {
		CacheableDir {
			uuid,
			parent,
			color: DirColor::Custom(Cow::Borrowed("#123456")),
			favorited: true,
			timestamp: ms(1_700_000_000_000),
			name: Cow::Owned(name.to_string()),
			created: Some(ms(1_700_000_000_001)),
		}
	}

	fn test_file(uuid: Uuid, parent: Uuid, name: &str) -> CacheableFile<'static> {
		CacheableFile {
			uuid,
			stable_uuid: StableUuid::new_for_test(uuid),
			parent,
			chunks_size: 7,
			chunks: 2,
			favorited: false,
			region: Cow::Borrowed("eu-central-1"),
			bucket: Cow::Borrowed("bucket-x"),
			timestamp: ms(1_700_000_000_002),
			name: Cow::Owned(name.to_string()),
			size: 1234,
			mime: Cow::Borrowed("image/png"),
			key: FileKey::from_str_with_version(&"b".repeat(64), FileEncryptionVersion::V3)
				.unwrap(),
			last_modified: ms(1_700_000_000_003),
			created: Some(ms(1_700_000_000_004)),
			hash: Some(Blake3Hash::from([7u8; 32])),
		}
	}

	/// account_root → { f1, A(dir) → { a1, AA(dir) → { aa1 } }, B(dir) → { b1 } }.
	struct Fixture {
		path: std::path::PathBuf,
		root: Uuid,
		a: CacheableDir<'static>,
		aa: CacheableDir<'static>,
		b: CacheableDir<'static>,
		f1: CacheableFile<'static>,
		a1: CacheableFile<'static>,
		aa1: CacheableFile<'static>,
		b1: CacheableFile<'static>,
		// Held open: the read connection works alongside the writer (WAL).
		state: CacheState,
	}

	fn fixture() -> Fixture {
		let path = temp_db_path();
		let root = Uuid::new_v4();
		let mut state = CacheState::new_on_path(&path, root);
		let a = test_dir(Uuid::new_v4(), root, "A");
		let aa = test_dir(Uuid::new_v4(), a.uuid, "AA");
		let b = test_dir(Uuid::new_v4(), root, "B");
		let f1 = test_file(Uuid::new_v4(), root, "f1.txt");
		let a1 = test_file(Uuid::new_v4(), a.uuid, "a1.txt");
		let aa1 = test_file(Uuid::new_v4(), aa.uuid, "aa1.txt");
		let b1 = test_file(Uuid::new_v4(), b.uuid, "b1.txt");
		state.upsert_dirs([&a, &aa, &b].into_iter()).unwrap();
		state
			.upsert_files([&f1, &a1, &aa1, &b1].into_iter())
			.unwrap();
		Fixture {
			path,
			root,
			a,
			aa,
			b,
			f1,
			a1,
			aa1,
			b1,
			state,
		}
	}

	fn uuids(items: impl IntoIterator<Item = Uuid>) -> std::collections::HashSet<Uuid> {
		items.into_iter().collect()
	}

	#[test]
	fn account_root_snapshot_returns_the_whole_subtree_split_by_type() {
		let f = fixture();
		let snapshot = read_subtree_snapshot(&f.path, f.root).unwrap();

		// Every descendant of the account root, dirs and files separated, across both the A/AA
		// branch and the sibling B branch. The root node itself is never returned (the engine syncs
		// its contents, not the root).
		assert_eq!(
			uuids(snapshot.dirs.iter().map(|d| d.uuid)),
			uuids([f.a.uuid, f.aa.uuid, f.b.uuid]),
			"all descendant dirs, anchor excluded"
		);
		assert_eq!(
			uuids(snapshot.files.iter().map(|file| file.uuid)),
			uuids([f.f1.uuid, f.a1.uuid, f.aa1.uuid, f.b1.uuid]),
			"all descendant files across every depth and both branches"
		);
	}

	#[test]
	fn subdir_snapshot_is_scoped_to_that_root_only() {
		let f = fixture();
		// Anchored at A: only A's subtree (AA, a1, aa1) — never B's b1, never the account-root f1,
		// and never A itself.
		let snapshot = read_subtree_snapshot(&f.path, f.a.uuid).unwrap();
		assert_eq!(
			uuids(snapshot.dirs.iter().map(|d| d.uuid)),
			uuids([f.aa.uuid])
		);
		assert_eq!(
			uuids(snapshot.files.iter().map(|file| file.uuid)),
			uuids([f.a1.uuid, f.aa1.uuid]),
			"A's own file plus its grandchild, nothing from sibling B"
		);
	}

	#[test]
	fn payloads_hydrate_faithfully() {
		let f = fixture();
		let snapshot = read_subtree_snapshot(&f.path, f.root).unwrap();
		let got_file = snapshot
			.files
			.iter()
			.find(|file| file.uuid == f.a1.uuid)
			.expect("a1 present");
		assert_eq!(*got_file, f.a1, "full file payload round-trips");
		let got_dir = snapshot
			.dirs
			.iter()
			.find(|d| d.uuid == f.aa.uuid)
			.expect("AA present");
		assert_eq!(*got_dir, f.aa, "full dir payload round-trips");
	}

	#[test]
	fn empty_subtree_yields_no_items() {
		let f = fixture();
		// AA has a file child (aa1) but no dir children — anchor at aa1's grandparent's leaf: use a
		// brand-new empty dir to prove an empty result.
		let empty = test_dir(Uuid::new_v4(), f.root, "empty");
		// Re-upsert through a fresh borrow of state would need &mut; instead anchor at a uuid with
		// no children at all: aa1 is a file, so its "subtree" is empty.
		let snapshot = read_subtree_snapshot(&f.path, f.aa1.uuid).unwrap();
		assert!(snapshot.dirs.is_empty() && snapshot.files.is_empty());
		// Anchoring at a uuid that isn't even in the cache is likewise empty (not an error).
		let absent = read_subtree_snapshot(&f.path, empty.uuid).unwrap();
		assert!(absent.dirs.is_empty() && absent.files.is_empty());
	}

	/// A record a listing could not decode rides along with the snapshot until the uuid is gone or
	/// decodes again: a removal drops it, a decodable upsert of the same uuid hides it, and the next
	/// listing of its root replaces it.
	#[test]
	fn undecodable_records_are_listed_until_removed_decoded_or_relisted() {
		let mut f = fixture();
		let garbled_file = UndecodableItem {
			uuid: Uuid::new_v4(),
			parent: f.a.uuid,
			stable_uuid: Some(StableUuid::new_for_test(Uuid::new_v4())),
		};
		let garbled_dir = UndecodableItem {
			uuid: Uuid::new_v4(),
			parent: f.root,
			stable_uuid: None,
		};
		let decodes_later = UndecodableItem {
			uuid: f.b1.uuid,
			parent: f.b.uuid,
			stable_uuid: Some(f.b1.stable_uuid),
		};
		f.state
			.replace_undecodable(f.root, &[garbled_file, garbled_dir, decodes_later])
			.unwrap();
		let listed = |f: &Fixture| {
			let mut got = read_subtree_snapshot(&f.path, f.root).unwrap().undecodable;
			got.sort_by_key(|item| item.uuid);
			got
		};
		let mut expected = vec![garbled_file, garbled_dir];
		expected.sort_by_key(|item| item.uuid);
		assert_eq!(
			listed(&f),
			expected,
			"b1 is a cached item, so its stale record is not reported"
		);

		f.state
			.delete_items(std::iter::once(garbled_file.uuid))
			.unwrap();
		assert_eq!(listed(&f), vec![garbled_dir], "a removal drops the record");

		f.state.replace_undecodable(f.root, &[]).unwrap();
		assert!(listed(&f).is_empty(), "a clean relisting clears the root");
	}

	#[test]
	fn watermark_is_none_on_a_fresh_cache_and_reflects_a_set_value() {
		let f = fixture();
		// init.sql seeds the watermark row NULL; nothing has applied an event.
		assert_eq!(
			read_subtree_snapshot(&f.path, f.root).unwrap().watermark,
			None
		);

		f.state.set_watermark(4242).unwrap();
		assert_eq!(
			read_subtree_snapshot(&f.path, f.root).unwrap().watermark,
			Some(4242),
			"the snapshot reads the same watermark the worker advances"
		);
	}
}

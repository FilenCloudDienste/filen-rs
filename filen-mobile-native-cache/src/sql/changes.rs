//! Change-tracking substrate (workstream A / Phase 4).
//!
//! The schema keeps a single monotonic `sync_state.seq`, bumped by triggers (see `init.sql`) on
//! every observable item/metadata mutation and recorded per row in `items.seq`; deletions land in
//! the `deletions` tombstone table with the seq at deletion time. A [`SyncAnchor`] is the opaque
//! 16-byte cursor the File Provider round-trips: an 8-byte DB-generation `epoch` (so a rebuilt cache
//! DB invalidates old anchors) followed by the little-endian `seq` the caller last observed.

use filen_types::fs::Uuid;
use rusqlite::Connection;

use crate::sql::{SQLResult, object::DBNonRootObject, statements::*};

/// A decoded sync anchor: the DB-generation `epoch` plus the change `seq` last observed by a caller.
///
/// Wire format is exactly 16 bytes: `epoch` (8 bytes, verbatim from the `sync_state.epoch` BLOB)
/// followed by `seq` as 8 little-endian bytes. `seq` is a non-negative counter, so its two's
/// complement `i64` encoding is byte-identical to the `u64` the contract describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SyncAnchor {
	pub(crate) epoch: [u8; 8],
	pub(crate) seq: i64,
}

impl SyncAnchor {
	pub(crate) const LEN: usize = 16;

	pub(crate) fn to_bytes(self) -> Vec<u8> {
		let mut out = Vec::with_capacity(Self::LEN);
		out.extend_from_slice(&self.epoch);
		out.extend_from_slice(&self.seq.to_le_bytes());
		out
	}

	/// Decodes an anchor. Returns `None` for any input that is not exactly [`Self::LEN`] bytes or that
	/// carries a negative `seq` (the seq is a non-negative counter, so a high-bit-set encoding is
	/// malformed); the caller treats `None` as an expired anchor.
	pub(crate) fn from_bytes(bytes: &[u8]) -> Option<Self> {
		if bytes.len() != Self::LEN {
			return None;
		}
		let epoch: [u8; 8] = bytes[0..8].try_into().ok()?;
		let seq = i64::from_le_bytes(bytes[8..16].try_into().ok()?);
		if seq < 0 {
			return None;
		}
		Some(Self { epoch, seq })
	}
}

/// Reads the current `(epoch, seq)` from `sync_state` as a [`SyncAnchor`].
pub(crate) fn current_anchor(conn: &Connection) -> rusqlite::Result<SyncAnchor> {
	conn.query_row(SELECT_SYNC_STATE, [], |row| {
		let epoch_vec: Vec<u8> = row.get(0)?;
		let seq: i64 = row.get(1)?;
		let epoch: [u8; 8] = epoch_vec.try_into().map_err(|v: Vec<u8>| {
			rusqlite::Error::FromSqlConversionFailure(
				v.len(),
				rusqlite::types::Type::Blob,
				format!("sync_state.epoch is {} bytes, expected 8", v.len()).into(),
			)
		})?;
		Ok(SyncAnchor { epoch, seq })
	})
}

/// Wide-join live (non-trashed) children of `parent` changed since `from_seq` (`items.seq >
/// from_seq`). A child trashed since the anchor drops out here (it left the parent's listing) — the
/// caller surfaces its removal via the working-set/trash feeds.
pub(crate) fn select_changed_children(
	conn: &Connection,
	parent: Uuid,
	from_seq: i64,
) -> SQLResult<Vec<DBNonRootObject>> {
	let mut stmt = conn.prepare_cached(SELECT_CHANGED_CHILDREN)?;
	stmt.query_and_then((parent, from_seq), DBNonRootObject::from_row)?
		.collect::<SQLResult<Vec<_>>>()
}

/// Wide-join trashed items changed since `from_seq` — the trash container's delta feed. Trash is
/// addressed by the `trashed` flag, not a parent uuid (trashed rows keep their original `parent`).
pub(crate) fn select_changed_trash(
	conn: &Connection,
	from_seq: i64,
) -> SQLResult<Vec<DBNonRootObject>> {
	let mut stmt = conn.prepare_cached(SELECT_CHANGED_TRASH)?;
	stmt.query_and_then([from_seq], DBNonRootObject::from_row)?
		.collect::<SQLResult<Vec<_>>>()
}

/// Wide-join non-root items changed anywhere since `from_seq` — the working-set delta feed.
pub(crate) fn select_changed_workingset(
	conn: &Connection,
	from_seq: i64,
) -> SQLResult<Vec<DBNonRootObject>> {
	let mut stmt = conn.prepare_cached(SELECT_CHANGED_WORKINGSET)?;
	stmt.query_and_then([from_seq], DBNonRootObject::from_row)?
		.collect::<SQLResult<Vec<_>>>()
}

/// Wide-join current working set: favorited (file or dir), recent, or trashed non-root items.
pub(crate) fn select_working_set(conn: &Connection) -> SQLResult<Vec<DBNonRootObject>> {
	let mut stmt = conn.prepare_cached(SELECT_WORKING_SET)?;
	stmt.query_and_then([], DBNonRootObject::from_row)?
		.collect::<SQLResult<Vec<_>>>()
}

/// Tombstoned `stable_uuid`s (as text) for `parent` since `from_seq`.
pub(crate) fn select_deletions_by_parent(
	conn: &Connection,
	parent: Uuid,
	from_seq: i64,
) -> rusqlite::Result<Vec<String>> {
	let mut stmt = conn.prepare_cached(SELECT_DELETIONS_BY_PARENT)?;
	stmt.query_map((parent, from_seq), |row| row.get::<_, String>(0))?
		.collect()
}

/// Tombstoned `stable_uuid`s anywhere since `from_seq` — the working-set delta.
pub(crate) fn select_deletions_all(
	conn: &Connection,
	from_seq: i64,
) -> rusqlite::Result<Vec<String>> {
	let mut stmt = conn.prepare_cached(SELECT_DELETIONS_ALL)?;
	stmt.query_map([from_seq], |row| row.get::<_, String>(0))?
		.collect()
}

#[cfg(test)]
mod trigger_tests {
	use std::collections::{BTreeMap, BTreeSet};

	use filen_types::fs::{ParentUuid, Uuid};
	use rusqlite::{Connection, params};

	use super::{
		SyncAnchor, current_anchor, select_changed_children, select_changed_workingset,
		select_deletions_all, select_deletions_by_parent,
	};
	use crate::{
		ffi::ItemType,
		sql::{
			item::{self, DBItemTrait},
			object::DBNonRootObject,
			select_children_page,
			statements::{
				DELETE_BY_UUID, DELETE_STALE_WITH_PARENT, INIT, MARK_STALE_WITH_PARENT,
				UPDATE_DIR_LAST_LISTED, UPDATE_FILE_FAVORITE_RANK, UPDATE_ITEM_SET_RECENT,
				UPSERT_DIR, UPSERT_DIR_META, UPSERT_FILE, UPSERT_FILE_META,
			},
		},
	};

	fn setup() -> Connection {
		let conn = Connection::open_in_memory().unwrap();
		// Register the `uuid_text` SQL function (used by the deletions readers) and the PRAGMAs, exactly
		// as a real connection does.
		crate::auth::configure_connection(&conn).unwrap();
		conn.execute_batch(INIT).unwrap();
		conn
	}

	fn global_seq(conn: &Connection) -> i64 {
		conn.query_row("SELECT seq FROM sync_state WHERE id = 1", [], |r| r.get(0))
			.unwrap()
	}

	fn item_seq(conn: &Connection, uuid: Uuid) -> i64 {
		conn.query_row("SELECT seq FROM items WHERE uuid = ?1", [uuid], |r| {
			r.get(0)
		})
		.unwrap()
	}

	fn tombstone_exists(conn: &Connection, stable_uuid: Uuid) -> bool {
		conn.query_row(
			"SELECT EXISTS(SELECT 1 FROM deletions WHERE stable_uuid = ?1)",
			[stable_uuid],
			|r| r.get(0),
		)
		.unwrap()
	}

	fn item_exists(conn: &Connection, uuid: Uuid) -> bool {
		conn.query_row(
			"SELECT EXISTS(SELECT 1 FROM items WHERE uuid = ?1)",
			[uuid],
			|r| r.get(0),
		)
		.unwrap()
	}

	// Runs the real UPSERT_FILE + UPSERT_FILE_META statements (metadata_state = 0 decoded, so
	// raw_metadata is NULL per the files CHECK constraint), exactly as a re-list would.
	fn upsert_file_rows(conn: &Connection, id: i64, name: &str, size: i64, favorite_rank: i64) {
		conn.query_row(
			UPSERT_FILE,
			params![
				id,
				size,
				0_i64,
				favorite_rank,
				"r",
				"b",
				0_i64,
				0_i64,
				Option::<Vec<u8>>::None
			],
			|r| r.get::<_, i64>(0),
		)
		.unwrap();
		conn.execute(
			UPSERT_FILE_META,
			params![
				id,
				name,
				"text/plain",
				"k",
				3_i64,
				Option::<i64>::None,
				0_i64,
				Option::<Vec<u8>>::None
			],
		)
		.unwrap();
	}

	fn list_file(
		conn: &Connection,
		uuid: Uuid,
		parent: ParentUuid,
		name: &str,
		size: i64,
		favorite_rank: i64,
	) -> i64 {
		let (id, _, _) =
			item::upsert_item(conn, uuid, Some(parent), Some(name), None, ItemType::File).unwrap();
		upsert_file_rows(conn, id, name, size, favorite_rank);
		id
	}

	fn list_dir(conn: &Connection, uuid: Uuid, parent: ParentUuid, name: &str) -> i64 {
		let (id, _, _) =
			item::upsert_item(conn, uuid, Some(parent), Some(name), None, ItemType::Dir).unwrap();
		conn.query_row(
			UPSERT_DIR,
			// color = "default" (never NULL): matches what upsert_from_remote writes via DirColor's
			// ToSql, so the dir reads back cleanly through the delta-feed from_row path.
			params![id, 0_i64, "default", 0_i64, 0_i64, Option::<Vec<u8>>::None],
			|r| r.get::<_, i64>(0),
		)
		.unwrap();
		conn.execute(UPSERT_DIR_META, params![id, name, Option::<i64>::None])
			.unwrap();
		id
	}

	// Inserts a bare drive-root items row (type = 0). The change-tracking readers filter it out; used
	// to prove the root never leaks into the working-set delta feed.
	fn insert_root(conn: &Connection, uuid: Uuid) {
		item::upsert_item(conn, uuid, None, None, None, ItemType::Root).unwrap();
	}

	// The stable identity of a delta-feed row (== uuid for dirs and freshly-inserted files).
	fn stable_of(obj: &DBNonRootObject) -> Uuid {
		match obj {
			DBNonRootObject::Dir(dir) => dir.stable_uuid,
			DBNonRootObject::File(file) => file.stable_uuid,
		}
	}

	fn stable_uuids(objs: &[DBNonRootObject]) -> Vec<Uuid> {
		objs.iter().map(stable_of).collect()
	}

	// Drains one anchor window onto `live`, exactly as a File Provider does each time it reads its
	// change feed: apply the working-set delta (upserts keyed by stable_uuid) then the deletion delta
	// (removals) since `*anchor`, then advance `*anchor` to the current seq. Value is (parent, name).
	fn replay(
		conn: &Connection,
		live: &mut BTreeMap<String, (String, Option<String>)>,
		anchor: &mut i64,
	) {
		for obj in select_changed_workingset(conn, *anchor).unwrap() {
			live.insert(
				stable_of(&obj).to_string(),
				(
					obj.parent().unwrap().to_string(),
					obj.name().map(str::to_string),
				),
			);
		}
		for stale in select_deletions_all(conn, *anchor).unwrap() {
			live.remove(&stale);
		}
		*anchor = global_seq(conn);
	}

	#[test]
	fn bare_item_insert_bumps_seq_once() {
		let conn = setup();
		assert_eq!(global_seq(&conn), 0, "a fresh DB starts at seq 0");
		let uuid = Uuid::new_v4();
		item::upsert_item(
			&conn,
			uuid,
			Some(ParentUuid::Uuid(Uuid::new_v4())),
			Some("a"),
			None,
			ItemType::File,
		)
		.unwrap();
		assert_eq!(
			global_seq(&conn),
			1,
			"a single insert bumps seq by exactly one"
		);
		assert_eq!(
			item_seq(&conn, uuid),
			1,
			"the new row is stamped with the new seq"
		);
	}

	#[test]
	fn noop_relist_does_not_bump_or_tombstone() {
		let conn = setup();
		let parent = ParentUuid::Uuid(Uuid::new_v4());
		let uuid = Uuid::new_v4();
		list_file(&conn, uuid, parent, "f.txt", 10, 0);
		let seq_before = global_seq(&conn);
		let item_seq_before = item_seq(&conn, uuid);
		assert!(seq_before > 0);
		assert_eq!(item_seq_before, seq_before);

		// The exact mark_stale -> upsert identical values -> delete_stale cycle every re-list runs.
		conn.execute(MARK_STALE_WITH_PARENT, [Uuid::try_from(parent).unwrap()])
			.unwrap();
		list_file(&conn, uuid, parent, "f.txt", 10, 0);
		conn.execute(DELETE_STALE_WITH_PARENT, [Uuid::try_from(parent).unwrap()])
			.unwrap();

		assert_eq!(
			global_seq(&conn),
			seq_before,
			"a no-op re-list must not bump seq"
		);
		assert_eq!(item_seq(&conn, uuid), item_seq_before, "item seq unchanged");
		assert!(
			!tombstone_exists(&conn, uuid),
			"a surviving row is not tombstoned"
		);
		assert!(item_exists(&conn, uuid), "the surviving row is not deleted");
	}

	#[test]
	fn files_meta_name_change_bumps_owning_item() {
		let conn = setup();
		let parent = ParentUuid::Uuid(Uuid::new_v4());
		let uuid = Uuid::new_v4();
		let id = list_file(&conn, uuid, parent, "old.txt", 10, 0);
		let seq_before = global_seq(&conn);

		// A metadata-only rename: files_meta.name changes, items/files rows unchanged.
		conn.execute(
			UPSERT_FILE_META,
			params![
				id,
				"new.txt",
				"text/plain",
				"k",
				3_i64,
				Option::<i64>::None,
				0_i64,
				Option::<Vec<u8>>::None
			],
		)
		.unwrap();

		assert!(
			global_seq(&conn) > seq_before,
			"a metadata change bumps seq"
		);
		assert_eq!(
			item_seq(&conn, uuid),
			global_seq(&conn),
			"the owning item is stamped with the new seq"
		);
	}

	#[test]
	fn favorite_rank_bumps_but_last_listed_and_recent_do_not() {
		let conn = setup();
		let parent = ParentUuid::Uuid(Uuid::new_v4());
		let file_uuid = Uuid::new_v4();
		let file_id = list_file(&conn, file_uuid, parent, "f.txt", 1, 0);
		let dir_uuid = Uuid::new_v4();
		let dir_id = list_dir(&conn, dir_uuid, parent, "d");

		let seq_before = global_seq(&conn);
		conn.execute(UPDATE_FILE_FAVORITE_RANK, params![5_i64, file_id])
			.unwrap();
		assert!(
			global_seq(&conn) > seq_before,
			"favorite_rank change bumps seq"
		);
		assert_eq!(item_seq(&conn, file_uuid), global_seq(&conn));

		let seq_before = global_seq(&conn);
		conn.execute(UPDATE_DIR_LAST_LISTED, params![12345_i64, dir_id])
			.unwrap();
		assert_eq!(
			global_seq(&conn),
			seq_before,
			"last_listed must not bump seq"
		);

		let seq_before = global_seq(&conn);
		conn.execute(UPDATE_ITEM_SET_RECENT, [file_id]).unwrap();
		assert_eq!(
			global_seq(&conn),
			seq_before,
			"an is_recent flip must not bump seq"
		);
	}

	#[test]
	fn delete_creates_tombstone_with_parent_and_uuid() {
		let conn = setup();
		let parent = ParentUuid::Uuid(Uuid::new_v4());
		let uuid = Uuid::new_v4();
		list_file(&conn, uuid, parent, "f.txt", 1, 0);
		let seq_before = global_seq(&conn);

		conn.execute(DELETE_BY_UUID, [uuid]).unwrap();

		assert!(global_seq(&conn) > seq_before, "a delete bumps seq");
		// uuid/parent are stored as BLOBs; read them back as `Uuid`.
		let (t_uuid, t_parent, t_seq): (Uuid, Uuid, i64) = conn
			.query_row(
				"SELECT uuid, parent, seq FROM deletions WHERE stable_uuid = ?1",
				[uuid],
				|r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
			)
			.unwrap();
		assert_eq!(t_uuid, uuid, "tombstone records the real uuid");
		let expected_parent: Uuid = parent.try_into().unwrap();
		assert_eq!(t_parent, expected_parent, "tombstone records the parent");
		assert_eq!(
			t_seq,
			global_seq(&conn),
			"tombstone seq is the delete's seq"
		);
		assert!(!item_exists(&conn, uuid));
	}

	#[test]
	fn cascade_delete_tombstones_every_descendant() {
		let conn = setup();
		let root_parent = ParentUuid::Uuid(Uuid::new_v4());
		let d = Uuid::new_v4();
		let s = Uuid::new_v4();
		let f = Uuid::new_v4();
		// D (dir) / S (dir) / F (file) — a three-level subtree.
		item::upsert_item(&conn, d, Some(root_parent), Some("D"), None, ItemType::Dir).unwrap();
		item::upsert_item(
			&conn,
			s,
			Some(ParentUuid::Uuid(d)),
			Some("S"),
			None,
			ItemType::Dir,
		)
		.unwrap();
		item::upsert_item(
			&conn,
			f,
			Some(ParentUuid::Uuid(s)),
			Some("F"),
			None,
			ItemType::File,
		)
		.unwrap();

		conn.execute(DELETE_BY_UUID, [d]).unwrap();

		for uuid in [d, s, f] {
			assert!(
				tombstone_exists(&conn, uuid),
				"descendant {uuid} is tombstoned"
			);
			assert!(!item_exists(&conn, uuid), "descendant {uuid} is deleted");
		}
	}

	#[test]
	fn reinsert_same_stable_uuid_clears_tombstone() {
		let conn = setup();
		let parent = ParentUuid::Uuid(Uuid::new_v4());
		let uuid = Uuid::new_v4();
		item::upsert_item(&conn, uuid, Some(parent), Some("a"), None, ItemType::File).unwrap();
		conn.execute(DELETE_BY_UUID, [uuid]).unwrap();
		assert!(
			tombstone_exists(&conn, uuid),
			"delete tombstones the stable id"
		);

		// A fresh row for the same uuid defaults stable_uuid to that uuid, resurrecting the identity.
		item::upsert_item(&conn, uuid, Some(parent), Some("a"), None, ItemType::File).unwrap();
		assert!(
			!tombstone_exists(&conn, uuid),
			"re-inserting the stable id clears its tombstone"
		);
	}

	#[test]
	fn uuid_swap_bumps_keeps_stable_and_does_not_tombstone() {
		let conn = setup();
		let parent = ParentUuid::Uuid(Uuid::new_v4());
		let uuid_a = Uuid::new_v4();
		let id = list_file(&conn, uuid_a, parent, "f.txt", 1, 0);
		let seq_before = global_seq(&conn);

		// Content re-mint: same (parent, name), new uuid -> the row is reused, uuid swapped in place.
		let uuid_b = Uuid::new_v4();
		let (id2, _, stable) = item::upsert_item(
			&conn,
			uuid_b,
			Some(parent),
			Some("f.txt"),
			None,
			ItemType::File,
		)
		.unwrap();

		assert_eq!(id2, id, "the re-mint reuses the same row");
		assert_eq!(
			stable, uuid_a,
			"stable_uuid is preserved across the uuid swap"
		);
		assert!(global_seq(&conn) > seq_before, "a uuid swap bumps seq");
		assert_eq!(item_seq(&conn, uuid_b), global_seq(&conn));
		assert!(
			!tombstone_exists(&conn, uuid_a),
			"a uuid swap does not tombstone the old uuid"
		);
		assert!(!tombstone_exists(&conn, uuid_b));

		let (row_uuid, row_stable): (Uuid, Uuid) = conn
			.query_row(
				"SELECT uuid, stable_uuid FROM items WHERE id = ?1",
				[id],
				|r| Ok((r.get(0)?, r.get(1)?)),
			)
			.unwrap();
		assert_eq!(row_uuid, uuid_b);
		assert_eq!(row_stable, uuid_a);
	}

	#[test]
	fn anchor_roundtrips() {
		let anchor = SyncAnchor {
			epoch: [1, 2, 3, 4, 5, 6, 7, 8],
			seq: 0x1122_3344_5566_7788,
		};
		let bytes = anchor.to_bytes();
		assert_eq!(bytes.len(), SyncAnchor::LEN);
		assert_eq!(SyncAnchor::from_bytes(&bytes), Some(anchor));
	}

	#[test]
	fn anchor_rejects_wrong_length() {
		assert_eq!(SyncAnchor::from_bytes(&[0u8; 15]), None);
		assert_eq!(SyncAnchor::from_bytes(&[0u8; 17]), None);
		assert_eq!(SyncAnchor::from_bytes(&[]), None);
	}

	#[test]
	fn anchor_rejects_negative_seq() {
		// seq is a non-negative counter; a negative value (any high-bit-set encoding) is malformed, so
		// the caller reports anchor_expired rather than trusting a corrupt cursor.
		for seq in [-1, i64::MIN] {
			let bytes = SyncAnchor {
				epoch: [1, 2, 3, 4, 5, 6, 7, 8],
				seq,
			}
			.to_bytes();
			assert_eq!(
				SyncAnchor::from_bytes(&bytes),
				None,
				"seq {seq} must be rejected"
			);
		}
	}

	#[test]
	fn current_anchor_reads_seq_and_epoch_mismatch_is_observable() {
		let conn = setup();
		let a0 = current_anchor(&conn).unwrap();
		assert_eq!(a0.seq, 0, "a fresh DB reports seq 0");

		let parent = ParentUuid::Uuid(Uuid::new_v4());
		item::upsert_item(
			&conn,
			Uuid::new_v4(),
			Some(parent),
			Some("x"),
			None,
			ItemType::File,
		)
		.unwrap();
		let a1 = current_anchor(&conn).unwrap();
		assert_eq!(a1.epoch, a0.epoch, "epoch is stable within a DB generation");
		assert!(a1.seq > a0.seq, "a change advances seq");

		// An anchor minted under a different generation carries a different epoch -> expired.
		let mut stale = a1;
		stale.epoch[0] ^= 0xFF;
		let decoded = SyncAnchor::from_bytes(&stale.to_bytes()).unwrap();
		assert_ne!(
			decoded.epoch, a1.epoch,
			"a mismatched epoch is observable, so the caller returns anchor_expired"
		);
	}

	#[test]
	fn changed_workingset_reports_inserts_excludes_root_and_noop_relist() {
		let conn = setup();
		let parent = ParentUuid::Uuid(Uuid::new_v4());
		let root = Uuid::new_v4();
		insert_root(&conn, root);
		let file = Uuid::new_v4();
		list_file(&conn, file, parent, "f.txt", 10, 0);

		// A fresh insert appears in the from_seq = 0 delta; the drive root (type = 0) never does.
		let delta = select_changed_workingset(&conn, 0).unwrap();
		let seen = stable_uuids(&delta);
		assert!(
			seen.contains(&file),
			"a fresh insert appears in the working-set delta"
		);
		assert!(
			!seen.contains(&root),
			"the drive root (items.type = 0) is never in the working-set feed"
		);

		let mid = global_seq(&conn);
		// The exact no-op re-list cycle: mark stale -> upsert identical values -> delete stale.
		conn.execute(MARK_STALE_WITH_PARENT, [Uuid::try_from(parent).unwrap()])
			.unwrap();
		list_file(&conn, file, parent, "f.txt", 10, 0);
		conn.execute(DELETE_STALE_WITH_PARENT, [Uuid::try_from(parent).unwrap()])
			.unwrap();
		assert_eq!(global_seq(&conn), mid, "a no-op re-list must not bump seq");

		let after = select_changed_workingset(&conn, mid).unwrap();
		assert!(
			after.is_empty(),
			"a no-op re-list surfaces no row at a later anchor"
		);
	}

	#[test]
	fn changed_children_scopes_to_one_parent() {
		let conn = setup();
		let p = Uuid::new_v4();
		let q = Uuid::new_v4();
		let under_p = Uuid::new_v4();
		let under_q = Uuid::new_v4();
		list_file(&conn, under_p, ParentUuid::Uuid(p), "in_p.txt", 1, 0);
		list_file(&conn, under_q, ParentUuid::Uuid(q), "in_q.txt", 1, 0);

		let p_children = stable_uuids(&select_changed_children(&conn, p, 0).unwrap());
		let q_children = stable_uuids(&select_changed_children(&conn, q, 0).unwrap());
		assert_eq!(
			p_children,
			vec![under_p],
			"P sees only its own changed child"
		);
		assert_eq!(
			q_children,
			vec![under_q],
			"Q sees only its own changed child"
		);
		assert!(
			!p_children.contains(&under_q),
			"a change under Q must not surface for sibling parent P"
		);
	}

	#[test]
	fn deletions_readers_scope_by_parent_and_seq() {
		let conn = setup();
		let p = Uuid::new_v4();
		let q = Uuid::new_v4();
		let victim = Uuid::new_v4();
		list_file(&conn, victim, ParentUuid::Uuid(p), "v.txt", 1, 0);
		list_file(&conn, Uuid::new_v4(), ParentUuid::Uuid(q), "s.txt", 1, 0);

		let before_delete = global_seq(&conn);
		conn.execute(DELETE_BY_UUID, [victim]).unwrap();
		let delete_seq = global_seq(&conn);
		let victim_stable = victim.to_string();

		let all = select_deletions_all(&conn, before_delete).unwrap();
		assert!(
			all.contains(&victim_stable),
			"select_deletions_all surfaces the tombstoned stable_uuid"
		);

		let by_p = select_deletions_by_parent(&conn, p, before_delete).unwrap();
		let by_q = select_deletions_by_parent(&conn, q, before_delete).unwrap();
		assert!(
			by_p.contains(&victim_stable),
			"the deletion is scoped to its real parent"
		);
		assert!(
			!by_q.contains(&victim_stable),
			"the deletion must not surface for the wrong parent"
		);

		// An anchor already at/past the deletion's seq (seq > ?1 is exclusive) excludes it.
		let at_deletion = select_deletions_all(&conn, delete_seq).unwrap();
		assert!(
			!at_deletion.contains(&victim_stable),
			"a from_seq >= the deletion seq excludes the tombstone"
		);
	}

	#[test]
	fn incremental_deltas_compose_to_the_same_live_set_as_one_full_read() {
		let conn = setup();
		let root_parent = ParentUuid::Uuid(Uuid::new_v4());
		let a = Uuid::new_v4();
		let b = Uuid::new_v4();
		let c = Uuid::new_v4();

		let mut inc: BTreeMap<String, (String, Option<String>)> = BTreeMap::new();
		let mut anchor = 0i64;

		// Step 1: create dir A at root.
		list_dir(&conn, a, root_parent, "A");
		replay(&conn, &mut inc, &mut anchor);

		// Step 2: create file B under A.
		let b_id = list_file(&conn, b, ParentUuid::Uuid(a), "B.txt", 5, 0);
		replay(&conn, &mut inc, &mut anchor);

		// Step 3: rename B (a metadata-only name change bumps its owning item).
		conn.execute(
			UPSERT_FILE_META,
			params![
				b_id,
				"B-renamed.txt",
				"text/plain",
				"k",
				3_i64,
				Option::<i64>::None,
				0_i64,
				Option::<Vec<u8>>::None
			],
		)
		.unwrap();
		replay(&conn, &mut inc, &mut anchor);

		// Step 4: create dir C at root.
		list_dir(&conn, c, root_parent, "C");
		replay(&conn, &mut inc, &mut anchor);

		// Step 5: delete A's subtree (delete A cascades to B).
		conn.execute(DELETE_BY_UUID, [a]).unwrap();
		replay(&conn, &mut inc, &mut anchor);

		// A single full read: every non-root row changed since seq 0, minus any tombstoned identity.
		let tombstoned: BTreeSet<String> = select_deletions_all(&conn, 0)
			.unwrap()
			.into_iter()
			.collect();
		let full: BTreeMap<String, (String, Option<String>)> = select_changed_workingset(&conn, 0)
			.unwrap()
			.into_iter()
			.filter(|obj| !tombstoned.contains(&stable_of(obj).to_string()))
			.map(|obj| {
				(
					stable_of(&obj).to_string(),
					(
						obj.parent().unwrap().to_string(),
						obj.name().map(str::to_string),
					),
				)
			})
			.collect();

		assert_eq!(
			inc, full,
			"incremental deltas across anchor boundaries compose to the same live set as one full read"
		);
		let survivors: BTreeSet<String> = inc.keys().cloned().collect();
		assert_eq!(
			survivors,
			BTreeSet::from([c.to_string()]),
			"only C survives; A and its subtree B are gone with no change lost or double-counted"
		);
	}

	#[test]
	fn move_reparents_in_place_surfaces_under_new_parent_and_does_not_tombstone_old() {
		let conn = setup();
		let p = Uuid::new_v4();
		let q = Uuid::new_v4();
		let item = Uuid::new_v4();
		list_dir(&conn, item, ParentUuid::Uuid(p), "M");

		let before = global_seq(&conn);
		assert!(
			stable_uuids(&select_changed_children(&conn, p, 0).unwrap()).contains(&item),
			"the dir is a live child of P before the move"
		);

		// A move is a parent update on the same row (same uuid), exactly what upsert_from_remote does.
		item::upsert_item(
			&conn,
			item,
			Some(ParentUuid::Uuid(q)),
			Some("M"),
			None,
			ItemType::Dir,
		)
		.unwrap();
		let after = global_seq(&conn);
		assert!(after > before, "a move bumps seq");
		assert_eq!(
			item_seq(&conn, item),
			after,
			"the moved row is stamped with the new seq"
		);

		assert!(
			stable_uuids(&select_changed_children(&conn, q, before).unwrap()).contains(&item),
			"the moved item surfaces under the new parent Q"
		);
		assert!(
			!stable_uuids(&select_changed_children(&conn, p, before).unwrap()).contains(&item),
			"the moved item is no longer a live child of the old parent P"
		);

		// Discovered behavior: an in-place parent update is NOT a deletion. The old parent P gets no
		// tombstone -- the item simply vanishes from P's live child feed with no removal signal.
		assert!(
			!tombstone_exists(&conn, item),
			"a move updates parent in place and does not tombstone the item"
		);
		assert!(
			select_deletions_by_parent(&conn, p, before)
				.unwrap()
				.is_empty(),
			"no tombstone is recorded for the old parent on a move"
		);
	}

	// Pagination (File Provider enumeration): pages partition a parent's children exactly — full
	// coverage, no overlap — and a short/empty page marks the end.
	#[test]
	fn select_children_page_partitions_children() {
		let conn = setup();
		let parent = ParentUuid::Uuid(Uuid::new_v4());
		let parent_uuid = Uuid::try_from(parent).unwrap();
		for name in ["e", "b", "d", "a", "c"] {
			list_file(&conn, Uuid::new_v4(), parent, name, 1, 0);
		}
		let page = |offset: u32, limit: u32| {
			select_children_page(&conn, None, parent_uuid, limit, offset).unwrap()
		};
		assert_eq!(page(0, 2).len(), 2);
		assert_eq!(page(2, 2).len(), 2);
		assert_eq!(page(4, 2).len(), 1); // short final page
		assert!(page(6, 2).is_empty()); // past the end

		let mut seen = BTreeSet::new();
		for offset in [0u32, 2, 4] {
			for obj in page(offset, 2) {
				assert!(seen.insert(stable_of(&obj)), "a child appears in two pages");
			}
		}
		assert_eq!(seen.len(), 5, "every child appears in exactly one page");
	}
}

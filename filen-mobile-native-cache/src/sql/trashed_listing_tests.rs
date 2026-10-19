use std::borrow::Cow;

use chrono::Utc;
use filen_sdk_rs::{
	cache::open_sqlite_in_memory, crypto::file::FileKey, fs::file::meta::DecryptedFileMeta,
};
use filen_types::{auth::FileEncryptionVersion, fs::ParentUuid};

use super::*;

const MARKED_AT: i64 = 1_700_000_000_000;

/// Configured the way every real connection is, so the name-slot lookup the guard performs
/// has the `uuid_text` function those statements are written against.
fn db() -> Connection {
	let conn = open_sqlite_in_memory().unwrap();
	crate::auth::configure_conn(&conn).unwrap();
	conn.execute_batch(INIT).unwrap();
	conn
}

fn uuid(byte: u8) -> Uuid {
	Uuid::from_bytes([byte; 16])
}

fn stable(byte: u8) -> StableUuid {
	StableUuid::new_for_test(uuid(byte))
}

/// A file the way a listing hands it over.
fn remote_file(uuid_: Uuid, stable: StableUuid, parent: ParentUuid, name: &str) -> RemoteFile {
	let now = Utc::now();
	RemoteFile::from_meta(
		uuid_,
		stable,
		parent,
		0,
		0,
		"us-east-1",
		"test-bucket",
		now,
		false,
		FileMeta::Decoded(DecryptedFileMeta {
			name: Cow::Owned(name.to_string()),
			size: 0,
			mime: Cow::Owned("text/plain".to_string()),
			key: FileKey::from_str_with_version(&"a".repeat(64), FileEncryptionVersion::V3)
				.unwrap(),
			last_modified: now,
			created: Some(now),
			hash: None,
		}),
	)
}

/// The live head of a file with an edit that has not reached the server yet, seeded through
/// the same bulk ingest a directory listing uses.
fn seed_live_file(conn: &mut Connection, uuid_: Uuid, stable: StableUuid, parent: Uuid) {
	update_items_with_parent(
		conn,
		[],
		[remote_file(
			uuid_,
			stable,
			ParentUuid::Uuid(parent),
			"edited.txt",
		)],
		parent,
		&[],
	)
	.unwrap();
	mark_pending_upload(conn, stable, MARKED_AT).unwrap();
}

/// A versioning-disabled edit leaves the retired uuid listed as trashed for ~60s, re-stamped
/// with a stable id that belongs to no lineage. Our own upload has already moved the row to
/// the new uuid, so that ghost matches nothing by uuid or stable id and falls through to the
/// `(parent, name)` tier — where the live head is sitting, holding the user's edit.
#[test]
fn a_trash_listing_must_not_adopt_a_live_row_by_name() {
	let mut conn = db();
	let parent = uuid(9);
	seed_live_file(&mut conn, uuid(2), stable(1), parent);

	update_trashed_items(
		&mut conn,
		[],
		[remote_file(
			uuid(3),
			stable(4),
			ParentUuid::Trash(parent),
			"edited.txt",
		)],
		&[],
	)
	.unwrap();

	let row = RawDBItem::select(&conn, uuid(2))
		.unwrap()
		.expect("the live row must survive the trash refresh");
	assert_eq!(
		row.parent,
		Some(ParentUuid::Uuid(parent)),
		"a ghost must not drag the live head into the trash"
	);
	assert_eq!(
		select_pending_uploads(&conn).unwrap(),
		vec![stable(1)],
		"the outstanding edit must stay drainable"
	);
}

/// The same ghost, seen before the cache learned about the new uuid: it still carries the
/// retired uuid, so it matches our row outright — but its stable id is not ours, and adopting
/// it would overwrite the lineage id the live head is about to arrive with.
#[test]
fn a_trash_listing_must_not_adopt_a_re_stamped_stable_id() {
	let mut conn = db();
	let parent = uuid(9);
	seed_live_file(&mut conn, uuid(1), stable(2), parent);

	update_trashed_items(
		&mut conn,
		[],
		[remote_file(
			uuid(1),
			stable(5),
			ParentUuid::Trash(parent),
			"edited.txt",
		)],
		&[],
	)
	.unwrap();

	let row = RawDBItem::select(&conn, uuid(1))
		.unwrap()
		.expect("the live row must survive the trash refresh");
	assert_eq!(
		row.parent,
		Some(ParentUuid::Uuid(parent)),
		"a re-stamped corpse must not trash the row it was minted from"
	);
	assert_eq!(
		select_pending_uploads(&conn).unwrap(),
		vec![stable(2)],
		"the row must keep its own stable id, and its edit"
	);
}

/// The guard must only refuse records that are not what they claim to be: an item genuinely
/// trashed elsewhere arrives with both ids intact and has to land, or the trash listing stops
/// converging.
#[test]
fn a_genuinely_trashed_file_still_lands() {
	let mut conn = db();
	let parent = uuid(9);
	seed_live_file(&mut conn, uuid(1), stable(2), parent);

	update_trashed_items(
		&mut conn,
		[],
		[remote_file(
			uuid(1),
			stable(2),
			ParentUuid::Trash(parent),
			"edited.txt",
		)],
		&[],
	)
	.unwrap();

	let row = RawDBItem::select(&conn, uuid(1)).unwrap().unwrap();
	assert_eq!(
		row.parent,
		Some(ParentUuid::Trash(parent)),
		"a real trashing must still be applied"
	);
}

/// One listing, hostile order: a new file that took over a renamed
/// sibling's old name is enumerated before the sibling's own record. The
/// newcomer must not reach the `(parent, name)` tier while the sibling's
/// row still sits at the old name — that would steal the row, its stable
/// id and its pending-upload marker.
#[test]
fn a_batch_newcomer_must_not_steal_a_renamed_siblings_row() {
	let mut conn = db();
	let parent = uuid(9);
	// A lives at "report.txt" with an outstanding edit marker.
	update_items_with_parent(
		&mut conn,
		[],
		[remote_file(
			uuid(1),
			stable(1),
			ParentUuid::Uuid(parent),
			"report.txt",
		)],
		parent,
		&[],
	)
	.unwrap();
	mark_pending_upload(&conn, stable(1), MARKED_AT).unwrap();

	// The server listing: newcomer C now owns "report.txt", A moved to
	// "renamed.txt" — and C is enumerated first.
	update_items_with_parent(
		&mut conn,
		[],
		[
			remote_file(uuid(2), stable(2), ParentUuid::Uuid(parent), "report.txt"),
			remote_file(uuid(1), stable(1), ParentUuid::Uuid(parent), "renamed.txt"),
		],
		parent,
		&[],
	)
	.unwrap();

	let a = RawDBItem::select(&conn, uuid(1))
		.unwrap()
		.expect("the renamed sibling keeps its row");
	assert_eq!(
		select_pending_uploads(&conn).unwrap(),
		vec![stable(1)],
		"the sibling's pending-upload marker must survive the batch, and only its own"
	);
	let c = RawDBItem::select(&conn, uuid(2))
		.unwrap()
		.expect("the newcomer gets its own fresh row");
	assert_ne!(a.id, c.id, "two files, two rows");
	assert_eq!(
		c.local_data, None,
		"the newcomer must not inherit the sibling's local_data"
	);
}

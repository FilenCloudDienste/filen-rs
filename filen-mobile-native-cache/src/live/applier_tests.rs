use std::borrow::Cow;

use chrono::{DateTime, Utc};
use filen_sdk_rs::{
	cache::open_sqlite_in_memory,
	fs::{dir::meta::DirectoryMeta, file::meta::FileMeta},
	socket as ev,
};
use filen_types::{api::v3::dir::color::DirColor, crypto::EncryptedString};

use super::*;

fn uuid(byte: u8) -> Uuid {
	Uuid::from_bytes([byte; 16])
}

fn stable(byte: u8) -> StableUuid {
	StableUuid::new_for_test(uuid(byte))
}

const ROOT: u8 = 9;

fn db() -> Mutex<Connection> {
	let conn = open_sqlite_in_memory().unwrap();
	crate::auth::configure_conn(&conn).unwrap();
	conn.execute_batch(crate::sql::statements::INIT).unwrap();
	conn.execute(
		"INSERT INTO items (uuid, parent, type) VALUES (?1, NULL, 0);",
		[uuid(ROOT)],
	)
	.unwrap();
	Mutex::new(conn)
}

fn file_record(uuid_: u8, stable_: u8, parent: u8) -> RemoteFile {
	RemoteFile {
		uuid: uuid(uuid_),
		stable_uuid: stable(stable_),
		meta: FileMeta::Encrypted(EncryptedString(Cow::Borrowed("enc-file"))),
		parent: ParentUuid::Uuid(uuid(parent)),
		size: 3,
		favorited: false,
		region: "de-1".into(),
		bucket: "b".into(),
		timestamp: DateTime::<Utc>::from_timestamp_millis(1_700_000_000_000).unwrap(),
		chunks: 1,
	}
}

fn dir_record(uuid_: u8, parent: u8) -> RemoteDirectory {
	RemoteDirectory {
		uuid: uuid(uuid_),
		parent: ParentUuid::Uuid(uuid(parent)),
		color: DirColor::Default,
		favorited: false,
		timestamp: DateTime::<Utc>::from_timestamp_millis(1_700_000_000_000).unwrap(),
		meta: DirectoryMeta::Encrypted(EncryptedString(Cow::Borrowed("enc-dir"))),
	}
}

fn apply(conn: &Mutex<Connection>, event: DecryptedDriveEvent<'static>) -> LocalApply {
	apply_drive_event_local(conn, event).unwrap()
}

fn row(conn: &Mutex<Connection>, uuid_: u8) -> Option<(Option<Uuid>, bool)> {
	super::conn(conn)
		.query_row(
			"SELECT parent, trashed FROM items WHERE uuid = ?1;",
			[uuid(uuid_)],
			|r| Ok((r.get(0)?, r.get(1)?)),
		)
		.optional()
		.unwrap()
}

fn item_count(conn: &Mutex<Connection>) -> i64 {
	super::conn(conn)
		.query_row("SELECT COUNT(*) FROM items WHERE type != 0;", [], |r| {
			r.get(0)
		})
		.unwrap()
}

fn stamp(conn: &Mutex<Connection>) -> Option<([u8; 16], i64)> {
	Some(crate::sql::select_change_meta(&super::conn(conn)).unwrap())
}

/// The signal gate. `Applied(true)` means the event was RELEVANT, not that anything changed —
/// this device's own uploads echo back over the socket and upsert to identical values — so
/// the working-set signal is gated on the change stamp instead, which is the very thing the
/// diff the signal provokes is computed from.
#[test]
fn the_signal_gate_follows_the_change_stamp_not_the_apply_result() {
	let conn = db();
	apply(
		&conn,
		DecryptedDriveEvent::FolderSubCreated(ev::FolderSubCreated(dir_record(8, ROOT))),
	);

	// A file the cache does not hold yet: a real change, and the stamp moves.
	let before = stamp(&conn);
	assert_eq!(
		apply(
			&conn,
			DecryptedDriveEvent::FileNew(ev::FileNew(file_record(1, 2, 8)))
		),
		LocalApply::Applied(true)
	);
	let after = stamp(&conn);
	assert!(
		stamp_moved(before, after),
		"a first upload must still signal"
	);

	// The same record again — the echo of our own upload. Still `Applied(true)`, still
	// nothing for a replica to be shown.
	assert_eq!(
		apply(
			&conn,
			DecryptedDriveEvent::FileNew(ev::FileNew(file_record(1, 2, 8)))
		),
		LocalApply::Applied(true)
	);
	assert!(
		!stamp_moved(after, stamp(&conn)),
		"an echo that changed no row must not cost an enumerateChanges round trip"
	);

	// A pass that touched nothing at all — the `gap_check` / `deleteAll` case — is the same
	// answer.
	let idle = stamp(&conn);
	assert!(!stamp_moved(idle, stamp(&conn)));

	// And an unreadable stamp falls through to signalling rather than swallowing a change.
	assert!(stamp_moved(None, idle));
	assert!(stamp_moved(idle, None));
}

/// The relevance gate, both ways: an event for a container this cache has never listed is a
/// no-op drop; once the container is held, the same event applies. This is what keeps the
/// live path O(1) per event and the cache from growing on its own.
#[test]
fn the_gate_drops_what_we_do_not_hold_and_applies_what_we_do() {
	let conn = db();

	// A file under a dir nobody here has listed: dropped.
	assert_eq!(
		apply(
			&conn,
			DecryptedDriveEvent::FileNew(ev::FileNew(file_record(1, 2, 8)))
		),
		LocalApply::Applied(false)
	);
	assert_eq!(item_count(&conn), 0);

	// The dir arrives under the root, which IS held — so it lands...
	assert_eq!(
		apply(
			&conn,
			DecryptedDriveEvent::FolderSubCreated(ev::FolderSubCreated(dir_record(8, ROOT)))
		),
		LocalApply::Applied(true)
	);
	// ...and the same file event is now relevant through its parent.
	assert_eq!(
		apply(
			&conn,
			DecryptedDriveEvent::FileNew(ev::FileNew(file_record(1, 2, 8)))
		),
		LocalApply::Applied(true)
	);
	assert_eq!(item_count(&conn), 2);
}

/// A remote edit re-mints the file's uuid; the stable tier must re-file the row IN PLACE —
/// one row, new uuid — and the archive of the superseded version (which names a successor)
/// must change nothing.
#[test]
fn a_remote_edit_refiles_the_row_in_place() {
	let conn = db();
	apply(
		&conn,
		DecryptedDriveEvent::FolderSubCreated(ev::FolderSubCreated(dir_record(8, ROOT))),
	);
	apply(
		&conn,
		DecryptedDriveEvent::FileNew(ev::FileNew(file_record(1, 2, 8))),
	);

	// The edit, as the socket delivers it: fileNew with a fresh uuid and the same stable id,
	// plus the fileArchived retiring the old version.
	assert_eq!(
		apply(
			&conn,
			DecryptedDriveEvent::FileNew(ev::FileNew(file_record(11, 2, 8)))
		),
		LocalApply::Applied(true)
	);
	assert_eq!(
		apply(
			&conn,
			DecryptedDriveEvent::FileArchived(ev::FileArchived {
				uuid: uuid(1),
				stable_uuid: stable(2),
				new_uuid: Some(uuid(11)),
			})
		),
		LocalApply::Applied(false),
		"an archive naming a successor is the edit's own bookkeeping, not a removal"
	);

	assert_eq!(
		item_count(&conn),
		2,
		"the same row, re-filed — never a second one"
	);
	assert!(row(&conn, 1).is_none());
	assert!(row(&conn, 11).is_some());
}

/// A remote trash keeps the row — marked trashed, original parent preserved for the restore —
/// and the retirement half of a versioning-disabled edit (which names a successor) must not
/// flicker the live row through the trash.
#[test]
fn a_remote_trash_marks_the_row_and_a_supersede_does_not() {
	let conn = db();
	apply(
		&conn,
		DecryptedDriveEvent::FolderSubCreated(ev::FolderSubCreated(dir_record(8, ROOT))),
	);
	apply(
		&conn,
		DecryptedDriveEvent::FileNew(ev::FileNew(file_record(1, 2, 8))),
	);

	// The supersede: never a trash. As on the wire, its stable id is the retired row's
	// freshly minted one, not the lineage's (2).
	assert_eq!(
		apply(
			&conn,
			DecryptedDriveEvent::FileTrash(ev::FileTrash {
				uuid: uuid(1),
				stable_uuid: stable(99),
				new_uuid: Some(uuid(11)),
			})
		),
		LocalApply::Applied(false)
	);
	assert_eq!(row(&conn, 1), Some((Some(uuid(8)), false)));

	// The real trash: marked, parent kept.
	assert_eq!(
		apply(
			&conn,
			DecryptedDriveEvent::FileTrash(ev::FileTrash {
				uuid: uuid(1),
				stable_uuid: stable(2),
				new_uuid: None,
			})
		),
		LocalApply::Applied(true)
	);
	assert_eq!(row(&conn, 1), Some((Some(uuid(8)), true)));

	// The replay another device's relist could produce: already where it belongs.
	assert_eq!(
		apply(
			&conn,
			DecryptedDriveEvent::FileTrash(ev::FileTrash {
				uuid: uuid(1),
				stable_uuid: stable(2),
				new_uuid: None,
			})
		),
		LocalApply::Applied(false)
	);
}

/// A rename arriving AFTER an edit re-minted the uuid still lands: the patch is keyed by the
/// whole-life id, which is the only name that survives the edit.
#[test]
fn a_rename_lands_across_a_uuid_remint() {
	let conn = db();
	apply(
		&conn,
		DecryptedDriveEvent::FolderSubCreated(ev::FolderSubCreated(dir_record(8, ROOT))),
	);
	apply(
		&conn,
		DecryptedDriveEvent::FileNew(ev::FileNew(file_record(1, 2, 8))),
	);
	apply(
		&conn,
		DecryptedDriveEvent::FileNew(ev::FileNew(file_record(11, 2, 8))),
	);

	assert_eq!(
		apply(
			&conn,
			DecryptedDriveEvent::FileMetadataChanged(ev::FileMetadataChanged {
				// The event may still carry the OLD uuid; the stable id is what resolves.
				uuid: uuid(1),
				stable_uuid: stable(2),
				metadata: FileMeta::Encrypted(EncryptedString(Cow::Borrowed("renamed-enc"))),
			})
		),
		LocalApply::Applied(true)
	);
	let raw: String = super::conn(&conn)
		.query_row(
			"SELECT files.raw_metadata FROM files
				JOIN items ON items.id = files.id WHERE items.uuid = ?1;",
			[uuid(11)],
			|r| r.get(0),
		)
		.unwrap();
	assert_eq!(raw, "renamed-enc");
}

/// The destructive arms answer with the follow-up they need rather than deleting here: the
/// forget path owns the pending-edit guard, and the trash relist owns phantom reconciliation.
/// The folds that mean "nothing died" must stay local no-ops.
#[test]
fn destructive_arms_defer_to_the_guarded_paths() {
	let conn = db();
	apply(
		&conn,
		DecryptedDriveEvent::FolderSubCreated(ev::FolderSubCreated(dir_record(8, ROOT))),
	);
	apply(
		&conn,
		DecryptedDriveEvent::FileNew(ev::FileNew(file_record(1, 2, 8))),
	);

	assert_eq!(
		apply(
			&conn,
			DecryptedDriveEvent::FileDeletedPermanent(ev::FileDeletedPermanent {
				uuid: uuid(1),
				stable_uuid: None,
			})
		),
		LocalApply::Applied(false),
		"a versions-only purge must not touch the live head"
	);
	assert_eq!(
		apply(
			&conn,
			DecryptedDriveEvent::FileDeletedPermanent(ev::FileDeletedPermanent {
				uuid: uuid(1),
				stable_uuid: Some(stable(2)),
			})
		),
		LocalApply::ForgetFile(stable(2))
	);
	assert_eq!(
		apply(
			&conn,
			DecryptedDriveEvent::FileArchived(ev::FileArchived {
				uuid: uuid(1),
				stable_uuid: stable(2),
				new_uuid: None,
			})
		),
		LocalApply::ForgetFile(stable(2)),
		"an archive with NO successor is the lineage's death"
	);
	assert_eq!(
		apply(
			&conn,
			DecryptedDriveEvent::FolderDeletedPermanent(ev::FolderDeletedPermanent {
				uuid: uuid(8),
			})
		),
		LocalApply::ForgetDir(uuid(8))
	);

	// TrashEmpty: nothing trashed, nothing to do; something trashed, relist.
	assert_eq!(
		apply(&conn, DecryptedDriveEvent::TrashEmpty),
		LocalApply::Applied(false)
	);
	apply(
		&conn,
		DecryptedDriveEvent::FileTrash(ev::FileTrash {
			uuid: uuid(1),
			stable_uuid: stable(2),
			new_uuid: None,
		}),
	);
	assert_eq!(
		apply(&conn, DecryptedDriveEvent::TrashEmpty),
		LocalApply::RelistTrash
	);
}

/// A move into a directory this cache has never listed still applies (the row is held), and
/// the row ends up naming a dangling parent the provider resolves by asking about it — the
/// documented one-level-per-ask behaviour.
#[test]
fn a_move_into_an_unheld_parent_applies_with_a_dangling_parent() {
	let conn = db();
	apply(
		&conn,
		DecryptedDriveEvent::FolderSubCreated(ev::FolderSubCreated(dir_record(8, ROOT))),
	);
	apply(
		&conn,
		DecryptedDriveEvent::FileNew(ev::FileNew(file_record(1, 2, 8))),
	);

	assert_eq!(
		apply(
			&conn,
			DecryptedDriveEvent::FileMove(ev::FileMove(file_record(1, 2, 77)))
		),
		LocalApply::Applied(true)
	);
	assert_eq!(row(&conn, 1), Some((Some(uuid(77)), false)));
	assert!(
		row(&conn, 77).is_none(),
		"the destination itself stays unknown"
	);
}

/// Folder metadata and color changes patch rows we hold and drop for rows we do not.
#[test]
fn folder_patches_respect_the_gate() {
	let conn = db();
	assert_eq!(
		apply(
			&conn,
			DecryptedDriveEvent::FolderMetadataChanged(ev::FolderMetadataChanged {
				uuid: uuid(8),
				meta: DirectoryMeta::Encrypted(EncryptedString(Cow::Borrowed("new-enc"))),
			})
		),
		LocalApply::Applied(false)
	);
	apply(
		&conn,
		DecryptedDriveEvent::FolderSubCreated(ev::FolderSubCreated(dir_record(8, ROOT))),
	);
	assert_eq!(
		apply(
			&conn,
			DecryptedDriveEvent::FolderMetadataChanged(ev::FolderMetadataChanged {
				uuid: uuid(8),
				meta: DirectoryMeta::Encrypted(EncryptedString(Cow::Borrowed("new-enc"))),
			})
		),
		LocalApply::Applied(true)
	);
	let raw: String = super::conn(&conn)
		.query_row(
			"SELECT dirs.raw_metadata FROM dirs
				JOIN items ON items.id = dirs.id WHERE items.uuid = ?1;",
			[uuid(8)],
			|r| r.get(0),
		)
		.unwrap();
	assert_eq!(raw, "new-enc");
}

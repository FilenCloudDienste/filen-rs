use std::borrow::Cow;

use crate::{
	crypto::file::FileKey,
	fs::{
		dir::{DecryptedDirectoryMeta, cache::CacheableDir},
		file::{cache::CacheableFile, meta::DecryptedFileMeta},
	},
};
use chrono::{DateTime, Utc};
use filen_types::{api::v3::dir::color::DirColor, auth::FileEncryptionVersion, fs::StableUuid};
use uuid::Uuid;

use super::*;
use crate::cache::{
	sql::columns::{COUNT, EVENTS_SYNTHETIC},
	state::{CacheEventType, DirEvent, FileEvent, GlobalEvent},
};

fn dt(ms: i64) -> DateTime<Utc> {
	DateTime::from_timestamp_millis(ms).expect("valid timestamp")
}

fn file_key() -> FileKey {
	FileKey::from_str_with_version(&"a".repeat(64), FileEncryptionVersion::V3).expect("valid key")
}

fn cacheable_file() -> CacheableFile<'static> {
	CacheableFile {
		uuid: Uuid::from_u128(1),
		stable_uuid: StableUuid::new_for_test(Uuid::from_u128(1)),
		parent: Uuid::from_u128(2),
		chunks_size: 1024,
		chunks: 1,
		favorited: false,
		region: Cow::Borrowed("de-1"),
		bucket: Cow::Borrowed("bucket-a"),
		timestamp: dt(1_700_000_000_000),
		name: Cow::Borrowed("file.txt"),
		size: 1024,
		mime: Cow::Borrowed("text/plain"),
		key: file_key(),
		last_modified: dt(1_700_000_000_000),
		created: Some(dt(1_699_000_000_000)),
		hash: None,
	}
}

fn cacheable_dir() -> CacheableDir<'static> {
	CacheableDir {
		uuid: Uuid::from_u128(3),
		parent: Uuid::from_u128(2),
		color: DirColor::Blue,
		favorited: true,
		timestamp: dt(1_700_000_000_000),
		name: Cow::Borrowed("folder"),
		created: Some(dt(1_699_000_000_000)),
	}
}

fn file_meta() -> DecryptedFileMeta<'static> {
	DecryptedFileMeta {
		name: Cow::Borrowed("file.txt"),
		size: 1024,
		mime: Cow::Borrowed("text/plain"),
		key: file_key(),
		last_modified: dt(1_700_000_000_000),
		created: Some(dt(1_699_000_000_000)),
		hash: None,
	}
}

fn dir_meta() -> DecryptedDirectoryMeta<'static> {
	DecryptedDirectoryMeta {
		name: Cow::Borrowed("folder"),
		created: Some(dt(1_699_000_000_000)),
	}
}

fn ev(id: Option<u64>, event: CacheEventType<'static>) -> CacheEvent<'static> {
	CacheEvent { id, event }
}

/// Serialize → checked-deserialize → assert the round-trip preserved every field.
/// `CacheEvent` does not derive `PartialEq`, so compare the (deterministic) `Debug` projections.
fn assert_roundtrip(event: CacheEvent<'static>) {
	let bytes = serialize(&event).expect("serialize");
	let decoded = deserialize(&bytes).expect("checked deserialize");
	assert_eq!(format!("{event:?}"), format!("{decoded:?}"));
}

#[test]
fn roundtrip_file_events() {
	assert_roundtrip(ev(
		Some(1),
		CacheEventType::File(FileEvent::New(cacheable_file())),
	));
	assert_roundtrip(ev(
		Some(2),
		CacheEventType::File(FileEvent::Move(cacheable_file())),
	));
	assert_roundtrip(ev(
		Some(3),
		CacheEventType::File(FileEvent::Changed(cacheable_file())),
	));
	assert_roundtrip(ev(
		Some(4),
		CacheEventType::File(FileEvent::Archived {
			uuid: Uuid::from_u128(1),
			stable_uuid: cacheable_file().stable_uuid,
			new_uuid: Some(Uuid::from_u128(2)),
		}),
	));
	assert_roundtrip(ev(
		Some(5),
		CacheEventType::File(FileEvent::Removed(Uuid::from_u128(1))),
	));
	assert_roundtrip(ev(
		Some(7),
		CacheEventType::File(FileEvent::Trashed {
			uuid: Uuid::from_u128(1),
			stable_uuid: cacheable_file().stable_uuid,
			new_uuid: None,
		}),
	));
	assert_roundtrip(ev(
		Some(6),
		CacheEventType::File(FileEvent::MetadataChanged {
			uuid: Uuid::from_u128(1),
			meta: file_meta(),
		}),
	));
}

#[test]
fn roundtrip_dir_events() {
	assert_roundtrip(ev(
		Some(1),
		CacheEventType::Dir(DirEvent::New(cacheable_dir())),
	));
	assert_roundtrip(ev(
		Some(2),
		CacheEventType::Dir(DirEvent::Move(cacheable_dir())),
	));
	assert_roundtrip(ev(
		Some(3),
		CacheEventType::Dir(DirEvent::Changed(cacheable_dir())),
	));
	assert_roundtrip(ev(
		Some(4),
		CacheEventType::Dir(DirEvent::Removed(Uuid::from_u128(3))),
	));
	assert_roundtrip(ev(
		Some(5),
		CacheEventType::Dir(DirEvent::MetadataChanged {
			uuid: Uuid::from_u128(3),
			meta: dir_meta(),
		}),
	));
	assert_roundtrip(ev(
		Some(6),
		CacheEventType::Dir(DirEvent::ColorChanged {
			uuid: Uuid::from_u128(3),
			color: DirColor::Red,
		}),
	));
	assert_roundtrip(ev(
		Some(7),
		CacheEventType::Dir(DirEvent::Restored(cacheable_dir())),
	));
}

/// `CacheEvent` as archived before `DirEvent::Restored` existed: what a build from then wrote
/// to, and reads from, the events store. Only `DirEvent` changed, so the rest is today's.
mod before_restored {
	use filen_types::api::v3::dir::color::DirColor;
	use uuid::Uuid;

	use crate::{
		cache::state::{FileEvent, GlobalEvent},
		fs::dir::{DecryptedDirectoryMeta, cache::CacheableDir},
	};

	#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
	pub(super) struct CacheEvent<'a> {
		pub(super) id: Option<u64>,
		pub(super) event: CacheEventType<'a>,
	}

	#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
	pub(super) enum CacheEventType<'a> {
		File(FileEvent<'a>),
		Dir(DirEvent<'a>),
		Global(GlobalEvent),
		NoOp,
	}

	#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
	pub(super) enum DirEvent<'a> {
		New(CacheableDir<'a>),
		Move(CacheableDir<'a>),
		Changed(CacheableDir<'a>),
		Removed(Uuid),
		MetadataChanged {
			uuid: Uuid,
			meta: DecryptedDirectoryMeta<'a>,
		},
		ColorChanged {
			uuid: Uuid,
			color: DirColor<'a>,
		},
	}
}

/// A row queued before `Restored` was added still reads back as the event it was: the variant
/// is appended, so the archived discriminants and the archived size are unchanged.
#[test]
fn a_row_queued_before_restored_existed_reads_back_unchanged() {
	let old = before_restored::CacheEvent {
		id: Some(1),
		event: before_restored::CacheEventType::Dir(
			before_restored::DirEvent::New(cacheable_dir()),
		),
	};
	let bytes = rkyv::to_bytes::<Error>(&old).expect("serialize");

	match deserialize(&bytes).expect("checked deserialize") {
		CacheEvent {
			id: Some(1),
			event: CacheEventType::Dir(DirEvent::New(dir)),
		} => assert_eq!(dir, cacheable_dir()),
		other => panic!("expected the queued Dir(New), got {other:?}"),
	}
}

/// A build from before `Restored` fails the checked read of a queued `Restored` row, rather than
/// misreading it as another event, so its drain quarantines the row.
#[test]
fn a_build_before_restored_rejects_a_queued_restored_row() {
	let bytes = serialize(&ev(
		Some(1),
		CacheEventType::Dir(DirEvent::Restored(cacheable_dir())),
	))
	.expect("serialize");

	assert!(
		rkyv::from_bytes::<before_restored::CacheEvent<'static>, Error>(&bytes).is_err(),
		"the old type must not accept a discriminant it does not have"
	);
}

#[test]
fn roundtrip_global_events() {
	assert_roundtrip(ev(Some(1), CacheEventType::Global(GlobalEvent::TrashEmpty)));
	assert_roundtrip(ev(Some(2), CacheEventType::Global(GlobalEvent::DeleteAll)));
	assert_roundtrip(ev(
		Some(3),
		CacheEventType::Global(GlobalEvent::DeleteVersioned),
	));
}

/// Synthetic diff events carry `id: None`; the blob must round-trip that faithfully.
#[test]
fn roundtrip_synthetic_null_id() {
	assert_roundtrip(ev(
		None,
		CacheEventType::File(FileEvent::New(cacheable_file())),
	));
}

/// The checked reader must reject garbage rather than risk UB on a corrupted/truncated row.
#[test]
fn deserialize_rejects_invalid_bytes() {
	assert!(deserialize(b"").is_err(), "empty blob must error");
	assert!(
		deserialize(b"not a valid rkyv archive at all").is_err(),
		"garbage blob must error"
	);
}

/// `insert_event` persists to the durable `events` table: it dedups a redelivered
/// `drive_message_id`, keeps synthetic (`id: None`) rows, round-trips the payload, and
/// the rows read back in the drain order (synthetic first, then by `drive_message_id`).
#[test]
fn insert_event_persists_dedups_and_orders() {
	let state = CacheState::new_in_memory();

	let real_2 = ev(Some(2), CacheEventType::Global(GlobalEvent::TrashEmpty));
	let real_1 = ev(
		Some(1),
		CacheEventType::File(FileEvent::Removed(Uuid::from_u128(7))),
	);
	let real_1_redelivered = ev(Some(1), CacheEventType::Global(GlobalEvent::DeleteAll));
	let synthetic = ev(None, CacheEventType::Dir(DirEvent::New(cacheable_dir())));

	state.insert_event(&real_2).unwrap();
	state.insert_event(&real_1).unwrap();
	state.insert_event(&real_1_redelivered).unwrap(); // same id=1 → IGNOREd
	state.insert_event(&synthetic).unwrap();

	// One row dropped by the unique-id index → 3 rows, not 4.
	let count: i64 = state
		.db
		.query_row("SELECT COUNT(*) AS count FROM events", [], |row| {
			row.get(COUNT)
		})
		.unwrap();
	assert_eq!(count, 3, "redelivered drive_message_id must be IGNOREd");

	// Read back in the drain order (idx_events_order: synthetic DESC, drive_message_id ASC, seq).
	let rows: Vec<(Option<i64>, bool, Vec<u8>)> = state
		.db
		.prepare(
			"SELECT drive_message_id, synthetic, payload FROM events \
				 ORDER BY synthetic DESC, drive_message_id ASC, seq ASC",
		)
		.unwrap()
		.query_map([], |row| {
			Ok((
				row.get(EVENTS_DRIVE_MESSAGE_ID)?,
				row.get(EVENTS_SYNTHETIC)?,
				row.get(EVENTS_PAYLOAD)?,
			))
		})
		.unwrap()
		.collect::<Result<_, _>>()
		.unwrap();

	// synthetic (NULL id) sorts first, then id=1, then id=2.
	assert_eq!(rows[0].0, None);
	assert!(rows[0].1, "first row is the synthetic event");
	assert_eq!(rows[1].0, Some(1));
	assert_eq!(rows[2].0, Some(2));

	// The surviving id=1 row is the FIRST insert (DeleteAll redelivery was dropped), and payloads
	// round-trip through the checked reader.
	let decoded_synthetic = deserialize(&rows[0].2).unwrap();
	assert_eq!(format!("{decoded_synthetic:?}"), format!("{synthetic:?}"));
	let decoded_1 = deserialize(&rows[1].2).unwrap();
	assert_eq!(format!("{decoded_1:?}"), format!("{real_1:?}"));
}

/// `insert_events_batch` is the production drain's persist path: an empty batch is a no-op, and a
/// multi-event batch persists in ONE transaction while still deduping a redelivered
/// `drive_message_id` via `INSERT OR IGNORE` — same semantics as the per-event path, one commit.
#[test]
fn insert_events_batch_persists_and_dedups_in_one_tx() {
	let mut state = CacheState::new_in_memory();

	// An empty batch inserts nothing (and opens no transaction).
	state.insert_events_batch(&[]).unwrap();
	let count0: i64 = state
		.db
		.query_row("SELECT COUNT(*) AS count FROM events", [], |row| {
			row.get(COUNT)
		})
		.unwrap();
	assert_eq!(count0, 0, "an empty batch inserts nothing");

	let real_1 = ev(
		Some(1),
		CacheEventType::File(FileEvent::Removed(Uuid::from_u128(7))),
	);
	let real_2 = ev(Some(2), CacheEventType::Global(GlobalEvent::TrashEmpty));
	let real_1_redelivered = ev(Some(1), CacheEventType::Global(GlobalEvent::DeleteAll));
	let synthetic = ev(None, CacheEventType::Dir(DirEvent::New(cacheable_dir())));

	state
		.insert_events_batch(&[real_1, real_2, real_1_redelivered, synthetic])
		.unwrap();

	// The in-batch redelivery of id=1 is IGNOREd → 3 rows, not 4.
	let count: i64 = state
		.db
		.query_row("SELECT COUNT(*) AS count FROM events", [], |row| {
			row.get(COUNT)
		})
		.unwrap();
	assert_eq!(
		count, 3,
		"a redelivered drive_message_id is IGNOREd within the batch"
	);
}

#[test]
fn watermark_round_trips() {
	let state = CacheState::new_in_memory();
	assert_eq!(state.watermark().unwrap(), None, "watermark starts unset");
	state.set_watermark(42).unwrap();
	assert_eq!(state.watermark().unwrap(), Some(42));
	state.set_watermark(100).unwrap();
	assert_eq!(state.watermark().unwrap(), Some(100));
}

/// `needs_resync` defaults clear and `mark` sets it durably (clearing is covered atomically by
/// `commit_resync_synthetics`).
#[test]
fn needs_resync_defaults_clear_and_mark_sets_it() {
	let state = CacheState::new_in_memory();
	assert!(
		!state.needs_resync().unwrap(),
		"needs_resync starts clear (seeded 0/absent)"
	);
	state.mark_needs_resync().unwrap();
	assert!(state.needs_resync().unwrap(), "mark sets the flag");
}

/// The resync commit persists every synthetic (NULL drive_message_id), advances the watermark to
/// the snapshot id, and clears `needs_resync` — all atomically.
#[test]
fn commit_resync_watermark_advances_watermark_and_clears_flag() {
	let mut state = CacheState::new_in_memory();
	state.mark_needs_resync().unwrap();

	state.commit_resync_watermark(99, false, &[]).unwrap();

	// Watermark advanced to the snapshot id; resync flag cleared — atomically, with no
	// events-store traffic (synthetics apply directly from RAM now).
	assert_eq!(state.watermark().unwrap(), Some(99));
	assert!(!state.needs_resync().unwrap());
	let events_rows: i64 = state
		.db
		.query_row("SELECT COUNT(*) AS count FROM events", [], |row| {
			row.get(COUNT)
		})
		.unwrap();
	assert_eq!(events_rows, 0, "the commit never touches the events store");
}

/// `mark_resync = true` (a resync that transiently skipped a root) still advances the
/// watermark for the converged roots but leaves the durable retry flag SET, atomically.
#[test]
fn commit_resync_watermark_with_mark_resync_keeps_the_flag_set() {
	let mut state = CacheState::new_in_memory();
	assert!(!state.needs_resync().unwrap());

	state.commit_resync_watermark(42, true, &[]).unwrap();

	assert_eq!(state.watermark().unwrap(), Some(42), "progress committed");
	assert!(
		state.needs_resync().unwrap(),
		"durable retry stays scheduled for the skipped root"
	);
}

#[test]
fn load_event_batch_orders_limits_and_delete_consumes() {
	let state = CacheState::new_in_memory();
	let synth = ev(None, CacheEventType::Global(GlobalEvent::TrashEmpty));
	let real_2 = ev(Some(2), CacheEventType::Global(GlobalEvent::DeleteAll));
	let real_1 = ev(
		Some(1),
		CacheEventType::Global(GlobalEvent::DeleteVersioned),
	);
	state.insert_event(&real_2).unwrap();
	state.insert_event(&synth).unwrap();
	state.insert_event(&real_1).unwrap();

	// Synthetic first, then drive_message_id ascending.
	let (batch, corrupt) = state.load_event_batch(10).unwrap();
	assert!(corrupt.is_empty(), "no corrupt rows");
	assert_eq!(batch.len(), 3);
	assert_eq!(batch[0].id, None);
	assert_eq!(batch[1].id, Some(1));
	assert_eq!(batch[2].id, Some(2));
	assert_eq!(format!("{:?}", batch[1].event), format!("{real_1:?}"));

	assert_eq!(state.load_event_batch(2).unwrap().0.len(), 2);

	for pe in &batch {
		state.delete_event(pe.seq).unwrap();
	}
	assert!(state.load_event_batch(10).unwrap().0.is_empty());
}

/// A corrupt/truncated payload is quarantined (returned in `corrupt_seqs`), not fatal (C1).
#[test]
fn load_event_batch_quarantines_corrupt_rows() {
	let state = CacheState::new_in_memory();
	let good = ev(Some(1), CacheEventType::Global(GlobalEvent::TrashEmpty));
	state.insert_event(&good).unwrap();
	// Insert a row with garbage bytes directly, bypassing serialize().
	state
		.db
		.execute(
			"INSERT INTO events (drive_message_id, synthetic, payload) VALUES (2, FALSE, ?1)",
			[b"not a valid rkyv archive".as_slice()],
		)
		.unwrap();

	let (batch, corrupt) = state.load_event_batch(10).unwrap();
	assert_eq!(batch.len(), 1, "only the good row is returned");
	assert_eq!(batch[0].id, Some(1));
	assert_eq!(
		corrupt.len(),
		1,
		"the garbage row is flagged for quarantine"
	);
}

//! The durable `events` store: serialization of [`CacheEvent`] to/from the rkyv blob in the
//! `events.payload` column, plus the persist/load/commit queries the drain and resync run against it.
//! Each event is stored as an rkyv archive; this module is the single place that encodes/decodes it.

use rkyv::rancor::Error;
use rusqlite::{OptionalExtension, params};

use crate::cache::{CacheError, CacheState, state::CacheEvent};

/// Serialize a [`CacheEvent`] into the rkyv byte blob persisted in `events.payload`.
fn serialize(event: &CacheEvent<'_>) -> Result<Vec<u8>, Error> {
	rkyv::to_bytes::<Error>(event).map(|bytes| bytes.to_vec())
}

/// Deserialize a [`CacheEvent`] from an `events.payload` blob.
///
/// Uses `rkyv::from_bytes`, which runs the derived `CheckBytes` validators before deserializing, so
/// a corrupted or truncated row yields `Err` rather than undefined behavior on the durable read
/// path. Returns an owned (`'static`) event.
fn deserialize(bytes: &[u8]) -> Result<CacheEvent<'static>, Error> {
	rkyv::from_bytes::<CacheEvent<'static>, Error>(bytes)
}

/// One row read back from the durable `events` store, ready for the ordered drain.
pub(crate) struct PersistedEvent {
	/// `events.seq` (rowid) — identifies the row for deletion after it is applied.
	pub seq: i64,
	/// The drive_message_id, or `None` for a synthetic diff event.
	pub id: Option<u64>,
	pub event: CacheEvent<'static>,
}

fn db_err(error: rusqlite::Error, context: &str) -> Box<CacheError> {
	Box::new(CacheError::db(error, context.to_string()))
}

impl CacheState {
	/// Persist ONE event to the durable `events` store. `INSERT OR IGNORE` drops an at-least-once
	/// redelivery of the same `drive_message_id`; synthetic events (`id: None`) are exempt from the dedup
	/// index and always insert. Test-only single-row helper: the production drain path persists a whole
	/// burst via [`insert_events_batch`](Self::insert_events_batch) (one transaction), and the resync
	/// path via `commit_resync_synthetics`.
	// `Box<CacheError>` matches the crate's pattern for the (large) `CacheError` and satisfies
	// `clippy::result_large_err`.
	#[cfg(test)]
	pub(crate) fn insert_event(&self, event: &CacheEvent<'_>) -> Result<(), Box<CacheError>> {
		let payload = serialize(event).map_err(|e| Box::new(CacheError::serialization(e)))?;
		// u64 → i64 at the SQLite boundary (the account counter never nears i64::MAX); NULL = synthetic.
		let drive_message_id = event.id.map(|id| id as i64);
		let synthetic = event.id.is_none();
		self.db
			.prepare_cached(super::statements::EVENT_INSERT)
			.and_then(|mut stmt| stmt.execute(params![drive_message_id, synthetic, payload]))
			.map_err(|e| db_err(e, "inserting event into events table"))?;
		Ok(())
	}

	/// Persist a batch of events to the durable `events` store in ONE transaction — so the worker clears
	/// a drained burst with a single WAL commit/fsync instead of one autocommit per event (the in-RAM
	/// channel residency stays small because the worker drains it fast). `INSERT OR IGNORE` still dedups
	/// a redelivered `drive_message_id`; a synthetic event (`id: None`) inserts with a NULL id.
	///
	/// All-or-nothing by design: if serialization or a row insert fails the WHOLE batch rolls back, and
	/// the caller records a durable resync to recover it (the events were already removed from the
	/// channel). A resync is a full subtree reconcile, so this coarser granularity costs only an extra
	/// re-list, never correctness — and a serialize failure is a should-never-happen path anyway.
	pub(crate) fn insert_events_batch(
		&mut self,
		events: &[CacheEvent<'_>],
	) -> Result<(), Box<CacheError>> {
		if events.is_empty() {
			return Ok(());
		}
		let tx = self
			.db
			.transaction()
			.map_err(|e| db_err(e, "begin events-insert transaction"))?;
		{
			let mut stmt = tx
				.prepare_cached(super::statements::EVENT_INSERT)
				.map_err(|e| db_err(e, "preparing batched event insert"))?;
			for event in events {
				let payload =
					serialize(event).map_err(|e| Box::new(CacheError::serialization(e)))?;
				let drive_message_id = event.id.map(|id| id as i64);
				let synthetic = event.id.is_none();
				stmt.execute(params![drive_message_id, synthetic, payload])
					.map_err(|e| db_err(e, "inserting event into events table (batch)"))?;
			}
		}
		tx.commit()
			.map_err(|e| db_err(e, "committing batched event insert"))?;
		Ok(())
	}

	/// Load the next drain batch from the durable store in apply order: synthetic events first, then by
	/// `drive_message_id` ascending, then by insertion order.
	///
	/// Returns `(good_rows, corrupt_seqs)`. A row whose payload fails the checked reader is NOT fatal:
	/// its `seq` is returned in `corrupt_seqs` so the drain can quarantine (delete) it and force a
	/// resync, rather than aborting the whole drain forever on one poison row.
	pub(crate) fn load_event_batch(
		&self,
		limit: usize,
	) -> Result<(Vec<PersistedEvent>, Vec<i64>), Box<CacheError>> {
		let mut stmt = self
			.db
			.prepare_cached(super::statements::EVENT_LOAD_BATCH)
			.map_err(|e| db_err(e, "preparing event load"))?;
		let raw: Vec<(i64, Option<i64>, Vec<u8>)> = stmt
			.query_map(params![limit as i64], |row| {
				Ok((row.get(0)?, row.get(1)?, row.get(2)?))
			})
			.map_err(|e| db_err(e, "querying events"))?
			.collect::<rusqlite::Result<_>>()
			.map_err(|e| db_err(e, "reading event row"))?;
		drop(stmt);

		let mut good = Vec::with_capacity(raw.len());
		let mut corrupt = Vec::new();
		for (seq, id, payload) in raw {
			match deserialize(&payload) {
				Ok(event) => good.push(PersistedEvent {
					seq,
					id: id.map(|id| id as u64),
					event,
				}),
				Err(e) => {
					log::error!(
						"quarantining corrupt event row seq={seq} ({} bytes): {e}",
						payload.len()
					);
					corrupt.push(seq);
				}
			}
		}
		Ok((good, corrupt))
	}

	/// Durably record that a resync is needed (an event was lost — a hole, a corrupt row, or a failed
	/// persist). The flag survives a restart so the gap-check/resync recovers the gap even if no later id
	/// ever exposes it as a hole.
	pub(crate) fn mark_needs_resync(&self) -> Result<(), Box<CacheError>> {
		self.db
			.prepare_cached(super::statements::CACHE_META_SET)
			.and_then(|mut stmt| stmt.execute(params![super::statements::NEEDS_RESYNC_KEY, 1_i64]))
			.map_err(|e| db_err(e, "marking needs_resync"))?;
		Ok(())
	}

	/// Whether a resync has been durably requested. Read by both the per-drain `maybe_run_resync` and the
	/// startup gap-check; cleared atomically with the watermark advance in [`commit_resync_synthetics`].
	pub(crate) fn needs_resync(&self) -> Result<bool, Box<CacheError>> {
		// Unlike the watermark, `needs_resync` has NO seed row in init.sql — the key is first written by
		// `mark_needs_resync` (UPSERT). `.optional()` maps the absent-key case to `None` (treated as clear)
		// instead of erroring with `QueryReturnedNoRows`. Outer `Option` = row present; inner = NULL-able value.
		let row: Option<Option<i64>> = self
			.db
			.prepare_cached(super::statements::CACHE_META_GET)
			.and_then(|mut stmt| {
				stmt.query_row(params![super::statements::NEEDS_RESYNC_KEY], |row| {
					row.get::<_, Option<i64>>(0)
				})
				.optional()
			})
			.map_err(|e| db_err(e, "reading needs_resync"))?;
		Ok(row.flatten().is_some_and(|v| v != 0))
	}

	/// Remove one consumed event from the durable store by its `seq` (rowid).
	#[cfg(test)] // the drain deletes inline in `commit_drain_batch`
	pub(crate) fn delete_event(&self, seq: i64) -> Result<(), Box<CacheError>> {
		self.db
			.prepare_cached(super::statements::EVENT_DELETE)
			.and_then(|mut stmt| stmt.execute(params![seq]))
			.map_err(|e| db_err(e, "deleting consumed event"))?;
		Ok(())
	}

	/// Read the contiguous-prefix watermark (`None` only on a fresh cache — set by the first applied
	/// event OR by a resync's snapshot id, whichever lands first).
	pub(crate) fn watermark(&self) -> Result<Option<u64>, Box<CacheError>> {
		// Outer `Option` = row present (defensive — `.optional()` maps an absent seed row to `None`
		// instead of wedging the drain with `QueryReturnedNoRows`); inner = the (NULL-able) value.
		let row: Option<Option<i64>> = self
			.db
			.prepare_cached(super::statements::CACHE_META_GET)
			.and_then(|mut stmt| {
				stmt.query_row(params![super::statements::WATERMARK_KEY], |row| {
					row.get::<_, Option<i64>>(0)
				})
				.optional()
			})
			.map_err(|e| db_err(e, "reading watermark"))?;
		// u64 ← i64 at the SQLite boundary (the account counter never nears i64::MAX).
		Ok(row.flatten().map(|id| id as u64))
	}

	/// Set the contiguous-prefix watermark (`cache_meta`, seeded by init so this only updates).
	#[cfg(test)] // the drain advances the watermark inline in `commit_drain_batch`
	pub(crate) fn set_watermark(&self, id: u64) -> Result<(), Box<CacheError>> {
		self.db
			.prepare_cached(super::statements::CACHE_META_SET)
			.and_then(|mut stmt| stmt.execute(params![super::statements::WATERMARK_KEY, id as i64]))
			.map_err(|e| db_err(e, "writing watermark"))?;
		Ok(())
	}

	/// Commit the result of one drain batch: advance the watermark (if it moved), delete the consumed
	/// rows, and — when this batch broke the contiguous frontier (`mark_resync`) — durably set
	/// `needs_resync`, all in ONE transaction. The crash-safety invariant is CROSS-transaction — the
	/// event applies already committed in their own transactions BEFORE this one, and this transaction
	/// then commits the watermark advance + row deletions + the resync flag together atomically.
	///
	/// Setting the flag HERE (rather than via a separate best-effort write after the drain) closes a
	/// window the adversarial review flagged: a hole's evidence (the rows above it) is deleted in this
	/// same transaction, so the "a resync is needed" signal can never be lost to a failed/again-crashed
	/// standalone write. (At startup the gap-check `remote > watermark` is an independent backstop —
	/// a held watermark always reads behind the remote drive id — but recording the flag atomically is
	/// the correct durable design and also covers the live path, which has no gap-check.) The flag write
	/// is idempotent, so re-passing `true` across batches is harmless.
	/// NESTING-AWARE like `execute_chunked`: inside an already-open transaction (the drain's
	/// batched fast path) the statements run bare and the caller's commit is the atomicity
	/// boundary; standalone (the per-event fallback path) it commits its own transaction.
	pub(crate) fn commit_drain_batch(
		&mut self,
		new_watermark: Option<u64>,
		deleted_seqs: &[i64],
		mark_resync: bool,
	) -> Result<(), Box<CacheError>> {
		let ambient = !self.db.is_autocommit();
		let tx = if ambient {
			None
		} else {
			Some(
				self.db
					.unchecked_transaction()
					.map_err(|e| db_err(e, "begin drain-commit transaction"))?,
			)
		};
		let conn: &rusqlite::Connection = tx.as_deref().unwrap_or(&self.db);
		if let Some(id) = new_watermark {
			conn.execute(
				super::statements::CACHE_META_SET,
				params![super::statements::WATERMARK_KEY, id as i64],
			)
			.map_err(|e| db_err(e, "advancing watermark in drain commit"))?;
		}
		if mark_resync {
			conn.execute(
				super::statements::CACHE_META_SET,
				params![super::statements::NEEDS_RESYNC_KEY, 1_i64],
			)
			.map_err(|e| db_err(e, "recording needs_resync in drain commit"))?;
		}
		// Delete every consumed row in ONE statement instead of a per-seq loop. The seqs are i64 rowids
		// read from `events` (never user input), so an inline IN-list is injection-safe and avoids the
		// per-row execute overhead — and a varying-length list cannot be `prepare_cached`d anyway.
		if !deleted_seqs.is_empty() {
			let in_list = deleted_seqs
				.iter()
				.map(i64::to_string)
				.collect::<Vec<_>>()
				.join(",");
			conn.execute(&format!("DELETE FROM events WHERE seq IN ({in_list})"), [])
				.map_err(|e| db_err(e, "deleting consumed events in drain commit"))?;
		}
		if let Some(tx) = tx {
			tx.commit()
				.map_err(|e| db_err(e, "committing drain batch"))?;
		}
		Ok(())
	}

	/// Atomically commit the result of a resync: persist every synthetic diff event (`id: None` →
	/// `drive_message_id` NULL, `synthetic` TRUE), advance the watermark to the listing's snapshot id
	/// (`remote_under_lock`), and clear `needs_resync` — all in ONE transaction. The drain runs AFTER this
	/// commits; it loads the synthetics first (`idx_events_order`) and applies them.
	///
	/// Crash-safety: either the whole snapshot+watermark+clear lands or none of it does. If a crash
	/// happens after this commit but before the drain finishes, the un-drained synthetics simply
	/// re-apply (every one is an idempotent upsert or delete-of-missing) while the watermark already
	/// reflects the snapshot, so no real event is lost or double-counted.
	///
	/// `remote_under_lock` is read under the drive lock, so it is >= any previously observed id — the
	/// watermark only ever moves forward here.
	///
	/// DEDUP CORRECTNESS. Jumping the watermark to `remote_under_lock` makes the drain dedup (skip) any
	/// later-arriving real event with `id <= remote_under_lock`. That is CORRECT, not lossy:
	/// `remote_under_lock` and the listing are read under the SAME drive lock, so the listing is a
	/// consistent snapshot AT that id and already reflects every event up to it — the synthetics carry
	/// that state. Concretely, if X was deleted (id 50) then recreated (id 60) with
	/// `remote_under_lock = 60`, the snapshot shows X live; a redelivered Remove(X) at id 50 is deduped,
	/// so X correctly stays live. (Deduping against the OLD watermark instead would RE-APPLY that stale
	/// Remove and wrongly delete X.) `GlobalEvent`s need no synthetic: `DeleteAll`'s effect is reproduced
	/// by the delete query (post-DeleteAll the listing is empty, so the cache is emptied), and
	/// `TrashEmpty`/`DeleteVersioned` are cache no-ops. This relies on the server giving read-after-write
	/// consistency under the lock, which is a foundational assumption of the resync design.
	///
	/// `mark_resync` writes `needs_resync` as SET instead of cleared, in the same transaction:
	/// used when the resync that produced these synthetics also SKIPPED at least one root
	/// transiently — the watermark still advances for the roots that did converge, while a durable
	/// retry stays scheduled for the one(s) that did not.
	pub(crate) fn commit_resync_synthetics(
		&mut self,
		synthetics: &[CacheEvent<'_>],
		remote_under_lock: u64,
		mark_resync: bool,
	) -> Result<(), Box<CacheError>> {
		let tx = self
			.db
			.transaction()
			.map_err(|e| db_err(e, "begin resync-commit transaction"))?;
		{
			let mut stmt = tx
				.prepare_cached(super::statements::EVENT_INSERT)
				.map_err(|e| db_err(e, "preparing resync synthetic insert"))?;
			for event in synthetics {
				debug_assert!(
					event.id.is_none(),
					"resync synthetics must carry id = None (NULL drive_message_id)"
				);
				let payload =
					serialize(event).map_err(|e| Box::new(CacheError::serialization(e)))?;
				stmt.execute(params![Option::<i64>::None, true, payload])
					.map_err(|e| db_err(e, "inserting resync synthetic"))?;
			}
		}
		tx.execute(
			super::statements::CACHE_META_SET,
			params![super::statements::WATERMARK_KEY, remote_under_lock as i64],
		)
		.map_err(|e| db_err(e, "advancing watermark in resync commit"))?;
		tx.execute(
			super::statements::CACHE_META_SET,
			params![
				super::statements::NEEDS_RESYNC_KEY,
				if mark_resync { 1_i64 } else { 0_i64 }
			],
		)
		.map_err(|e| db_err(e, "writing needs_resync in resync commit"))?;
		tx.commit()
			.map_err(|e| db_err(e, "committing resync batch"))?;
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use std::borrow::Cow;

	use crate::{
		crypto::file::FileKey,
		fs::{
			dir::{DecryptedDirectoryMeta, cache::CacheableDir},
			file::{cache::CacheableFile, meta::DecryptedFileMeta},
		},
	};
	use chrono::{DateTime, Utc};
	use filen_types::{api::v3::dir::color::DirColor, auth::FileEncryptionVersion};
	use uuid::Uuid;

	use super::*;
	use crate::cache::state::{CacheEventType, DirEvent, FileEvent, GlobalEvent};

	fn dt(ms: i64) -> DateTime<Utc> {
		DateTime::from_timestamp_millis(ms).expect("valid timestamp")
	}

	fn file_key() -> FileKey {
		FileKey::from_str_with_version(&"a".repeat(64), FileEncryptionVersion::V3)
			.expect("valid key")
	}

	fn cacheable_file() -> CacheableFile<'static> {
		CacheableFile {
			uuid: Uuid::from_u128(1),
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
			CacheEventType::File(FileEvent::Archived(Uuid::from_u128(1))),
		));
		assert_roundtrip(ev(
			Some(5),
			CacheEventType::File(FileEvent::Removed(Uuid::from_u128(1))),
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
			.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
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
			.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
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
			.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
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
			.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
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
	fn commit_resync_synthetics_persists_synthetics_watermark_and_clears_flag() {
		let mut state = CacheState::new_in_memory();
		state.mark_needs_resync().unwrap();

		let synthetics = vec![
			ev(None, CacheEventType::Dir(DirEvent::New(cacheable_dir()))),
			ev(
				None,
				CacheEventType::File(FileEvent::Removed(Uuid::from_u128(7))),
			),
		];
		state
			.commit_resync_synthetics(&synthetics, 99, false)
			.unwrap();

		// Both rows landed as synthetics (NULL id) and read back in insertion order.
		let (count, synth_count): (i64, i64) = state
			.db
			.query_row(
				"SELECT COUNT(*), COALESCE(SUM(synthetic), 0) FROM events",
				[],
				|row| Ok((row.get(0)?, row.get(1)?)),
			)
			.unwrap();
		assert_eq!(count, 2);
		assert_eq!(synth_count, 2, "both rows are synthetic");

		let (batch, corrupt) = state.load_event_batch(10).unwrap();
		assert!(corrupt.is_empty());
		assert_eq!(batch.len(), 2);
		assert!(batch.iter().all(|pe| pe.id.is_none()));
		assert_eq!(
			format!("{:?}", batch[0].event),
			format!("{:?}", synthetics[0])
		);
		assert_eq!(
			format!("{:?}", batch[1].event),
			format!("{:?}", synthetics[1])
		);

		// Watermark advanced to the snapshot id; resync flag cleared.
		assert_eq!(state.watermark().unwrap(), Some(99));
		assert!(!state.needs_resync().unwrap());
	}

	/// `mark_resync = true` (a resync that transiently skipped a root) still advances the
	/// watermark for the converged roots but leaves the durable retry flag SET, atomically.
	#[test]
	fn commit_resync_synthetics_with_mark_resync_keeps_the_flag_set() {
		let mut state = CacheState::new_in_memory();
		assert!(!state.needs_resync().unwrap());

		state.commit_resync_synthetics(&[], 42, true).unwrap();

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
}

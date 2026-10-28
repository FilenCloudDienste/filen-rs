//! The durable `events` store: serialization of [`CacheEvent`] to/from the rkyv blob in the
//! `events.payload` column, plus the persist/load/commit queries the drain and resync run against it.
//! Each event is stored as an rkyv archive; this module is the single place that encodes/decodes it.

use std::collections::HashMap;

use rkyv::rancor::Error;
use rusqlite::{OptionalExtension, params};
use uuid::Uuid;

use crate::cache::{
	CacheError, CacheState,
	sql::columns::{CACHE_META_VALUE, EVENTS_DRIVE_MESSAGE_ID, EVENTS_PAYLOAD, EVENTS_SEQ},
	state::CacheEvent,
};

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

/// Write the contiguous-prefix watermark to `cache_meta` on `conn` (a bare connection OR an open
/// transaction — `Transaction` derefs to `Connection`). The `u64 → i64` SQLite-boundary cast lives
/// here, once. The caller wraps the error with its own `db_err` context.
fn set_watermark_on(conn: &rusqlite::Connection, id: u64) -> rusqlite::Result<usize> {
	conn.execute(
		super::statements::CACHE_META_SET,
		params![super::statements::WATERMARK_KEY, id as i64],
	)
}

/// Write the durable `needs_resync` flag (`on as i64`) to `cache_meta` on `conn`. The caller wraps
/// the error with its own `db_err` context.
fn set_needs_resync_on(conn: &rusqlite::Connection, on: bool) -> rusqlite::Result<usize> {
	conn.execute(
		super::statements::CACHE_META_SET,
		params![super::statements::NEEDS_RESYNC_KEY, on as i64],
	)
}

/// A set of sync roots stamped with one number, as `cache_meta` stores it under one key: the stamp
/// (8 bytes, little endian) followed by each root's 16 uuid bytes.
struct RootRecord {
	stamp: u64,
	roots: Vec<Uuid>,
}

impl RootRecord {
	fn decode(bytes: &[u8]) -> Option<Self> {
		let (stamp, rest) = bytes.split_first_chunk::<8>()?;
		let (uuids, []) = rest.as_chunks::<16>() else {
			return None;
		};
		Some(Self {
			stamp: u64::from_le_bytes(*stamp),
			roots: uuids.iter().map(|bytes| Uuid::from_bytes(*bytes)).collect(),
		})
	}

	fn encode(stamp: u64, roots: &[Uuid]) -> Vec<u8> {
		let mut bytes = stamp.to_le_bytes().to_vec();
		for root in roots {
			bytes.extend_from_slice(root.as_bytes());
		}
		bytes
	}
}

/// The root record stored under `key` on `conn`, if there is one. A value that does not decode
/// reads as none; `what` names the record in the log line that says so.
fn root_record_on(
	conn: &rusqlite::Connection,
	key: &str,
	what: &str,
) -> rusqlite::Result<Option<RootRecord>> {
	let value: Option<Option<Vec<u8>>> = conn
		.prepare_cached(super::statements::CACHE_META_GET)?
		.query_row(params![key], |row| row.get(CACHE_META_VALUE))
		.optional()?;
	let Some(bytes) = value.flatten() else {
		return Ok(None);
	};
	let record = RootRecord::decode(&bytes);
	if record.is_none() {
		tracing::debug!(
			"the {what} record ({} bytes) does not decode; reading it as none",
			bytes.len()
		);
	}
	Ok(record)
}

/// Store `value` (an encoded [`RootRecord`], or none) under `key` on `conn`.
fn set_root_record_on(
	conn: &rusqlite::Connection,
	key: &str,
	value: Option<Vec<u8>>,
) -> rusqlite::Result<()> {
	conn.execute(super::statements::CACHE_META_SET, params![key, value])?;
	Ok(())
}

/// The listed-roots record on `conn`: the sync roots whose whole subtree the cache holds as the
/// drive stood at the watermark in its stamp. Each was listed by a resync that committed and
/// stored every record of its listing, and every event drained since applied in full while it was
/// registered. A root whose listing held a record the cache could not store (metadata that does
/// not decode) is never recorded: with no row for that record, events under it are gated out, a
/// move into it deletes what moved, and a rename that makes it readable patches nothing. Every
/// write that can leave a recorded root's rows short of that takes that root out of the record. A
/// socket upsert takes out the roots it lands in: of a dir that no registered root the cache holds
/// whole kept current (a restore, a move in from outside the registered roots or from a root that
/// is not recorded, whose gaps move along) or of a file it held no row for (a restore). A new dir
/// takes out nothing when such a root holds its parent, as it has no contents yet. The writes that
/// cannot name the roots they touch clear the record whole: a drained `NoOp` marker (an event the
/// cache could not apply), and the row writes that move no watermark (evictions, wipes, a manual
/// listing, a resync's apply until its commit). Any watermark move that does not carry the record
/// forward leaves the stamp behind, which voids it, so a watermark writer that predates the record
/// can only cost a relist. A build that predates it and changes rows without moving the watermark
/// (an eviction through `SyncRootHandle::evict`) leaves the record standing.
///
/// Stored under [`LISTED_ROOTS_KEY`](super::statements::LISTED_ROOTS_KEY). A value that does not
/// decode reads as none, which only costs a relist.
fn listed_roots_on(conn: &rusqlite::Connection) -> rusqlite::Result<Option<RootRecord>> {
	root_record_on(conn, super::statements::LISTED_ROOTS_KEY, "listed-roots")
}

/// Record `roots` as held whole at watermark `at` on `conn`; none clears the record.
fn set_listed_roots_on(
	conn: &rusqlite::Connection,
	at: u64,
	roots: &[Uuid],
) -> rusqlite::Result<()> {
	let value = (!roots.is_empty()).then(|| RootRecord::encode(at, roots));
	set_root_record_on(conn, super::statements::LISTED_ROOTS_KEY, value)
}

/// Clear the listed-roots record on `conn`: every root relists on its next add.
pub(super) fn forget_listed_roots_on(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
	set_listed_roots_on(conn, 0, &[])
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
				Ok((
					row.get(EVENTS_SEQ)?,
					row.get(EVENTS_DRIVE_MESSAGE_ID)?,
					row.get(EVENTS_PAYLOAD)?,
				))
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
					tracing::error!(
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

	/// Clear the durable `needs_resync` flag OUTSIDE a watermark commit. Only for a caller that set
	/// the flag itself to cover its own targeted work and completed all of it (the worker is
	/// single-threaded, so nothing can have flagged a new gap in between); a full resync clears the
	/// flag atomically with its watermark advance in `commit_resync_watermark` instead.
	pub(crate) fn clear_needs_resync(&self) -> Result<(), Box<CacheError>> {
		self.db
			.prepare_cached(super::statements::CACHE_META_SET)
			.and_then(|mut stmt| stmt.execute(params![super::statements::NEEDS_RESYNC_KEY, 0_i64]))
			.map_err(|e| db_err(e, "clearing needs_resync"))?;
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
					row.get::<_, Option<i64>>(CACHE_META_VALUE)
				})
				.optional()
			})
			.map_err(|e| db_err(e, "reading needs_resync"))?;
		Ok(row.flatten().is_some_and(|v| v != 0))
	}

	/// Whether this cache holds the whole subtree of every one of `roots` as the drive stood at its
	/// watermark, with no resync pending: each was listed by a resync that committed and stayed
	/// registered for every event applied since (see `listed_roots_on`). Says nothing about what the
	/// drive did after the watermark — that is the remote drive id's to tell.
	pub(crate) fn holds_listings_of(&self, roots: &[Uuid]) -> Result<bool, Box<CacheError>> {
		if self.needs_resync()? {
			return Ok(false);
		}
		let held = self.roots_held_whole()?;
		Ok(roots.iter().all(|root| held.contains(root)))
	}

	/// The roots the listed-roots record holds whole at the current watermark (see `listed_roots_on`);
	/// none when there is no record or it is void.
	pub(crate) fn roots_held_whole(&self) -> Result<Vec<Uuid>, Box<CacheError>> {
		let Some(listed) =
			listed_roots_on(&self.db).map_err(|e| db_err(e, "reading the listed roots"))?
		else {
			return Ok(Vec::new());
		};
		if Some(listed.stamp) != self.watermark()? {
			return Ok(Vec::new());
		}
		Ok(listed.roots)
	}

	/// Clear the listed-roots record, ahead of a change that can leave a root's cached rows short of
	/// a listing (see `listed_roots_on`): every root relists on its next add.
	pub(crate) fn forget_listed_roots(&self) -> Result<(), Box<CacheError>> {
		forget_listed_roots_on(&self.db).map_err(|e| db_err(e, "clearing the listed roots"))
	}

	/// Take `roots` out of the listed-roots record, ahead of a change that can leave their cached
	/// rows short of a listing (see `listed_roots_on`): each relists on its next add, and every other
	/// root keeps its claim. With no record, or none of `roots` in it, nothing is written.
	pub(crate) fn forget_listed_roots_among(&self, roots: &[Uuid]) -> Result<(), Box<CacheError>> {
		let Some(mut listed) =
			listed_roots_on(&self.db).map_err(|e| db_err(e, "reading the listed roots"))?
		else {
			return Ok(());
		};
		let before = listed.roots.len();
		listed.roots.retain(|root| !roots.contains(root));
		if listed.roots.len() == before {
			return Ok(());
		}
		set_listed_roots_on(&self.db, listed.stamp, &listed.roots)
			.map_err(|e| db_err(e, "taking roots out of the listed roots"))
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
					row.get::<_, Option<i64>>(CACHE_META_VALUE)
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
	///
	/// An advance also carries the listed-roots record forward, in the same transaction: a root keeps
	/// its claim only if it is in `registered`, the registrations that gated the events this batch
	/// applied — an unregistered root's subtree missed them. A record not made at the watermark this
	/// batch starts from is already void, and is cleared.
	pub(crate) fn commit_drain_batch<V>(
		&self,
		new_watermark: Option<u64>,
		deleted_seqs: &[i64],
		mark_resync: bool,
		registered: &HashMap<Uuid, V>,
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
			let listed = listed_roots_on(conn)
				.map_err(|e| db_err(e, "reading the listed roots in drain commit"))?;
			if let Some(mut listed) = listed {
				if Some(listed.stamp) == self.watermark()? {
					listed.roots.retain(|root| registered.contains_key(root));
				} else {
					listed.roots.clear();
				}
				set_listed_roots_on(conn, id, &listed.roots)
					.map_err(|e| db_err(e, "carrying the listed roots in drain commit"))?;
			}
			set_watermark_on(conn, id)
				.map_err(|e| db_err(e, "advancing watermark in drain commit"))?;
		}
		if mark_resync {
			set_needs_resync_on(conn, true)
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
	/// The resync's FINAL commit: jump the watermark to the locked snapshot id and clear (or,
	/// for a partial resync, keep) the `needs_resync` flag — atomically, and strictly AFTER the
	/// synthetics were applied (see `apply_synthetics_direct`). A crash before this leaves the
	/// old watermark and the durable flag/startup gap-check to force a fresh listing.
	///
	/// `listed` — the dir roots this resync listed and stored whole — replaces the listed-roots
	/// record in the same transaction: a root it did not list missed the events the jump skips.
	pub(crate) fn commit_resync_watermark(
		&mut self,
		remote_under_lock: u64,
		mark_resync: bool,
		listed: &[Uuid],
	) -> Result<(), Box<CacheError>> {
		let tx = self
			.db
			.transaction()
			.map_err(|e| db_err(e, "begin resync-commit transaction"))?;
		set_listed_roots_on(&tx, remote_under_lock, listed)
			.map_err(|e| db_err(e, "recording the listed roots in resync commit"))?;
		set_watermark_on(&tx, remote_under_lock)
			.map_err(|e| db_err(e, "advancing watermark in resync commit"))?;
		set_needs_resync_on(&tx, mark_resync)
			.map_err(|e| db_err(e, "writing needs_resync in resync commit"))?;
		tx.commit()
			.map_err(|e| db_err(e, "committing resync batch"))?;
		Ok(())
	}
}

#[cfg(test)]
mod tests;

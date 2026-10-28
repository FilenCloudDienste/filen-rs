//! The sync engine's read side of the cache: a consistent whole-subtree snapshot of one sync
//! root in the projection its remote view reads ([`RemoteItem`]), PLUS the contiguous-prefix
//! watermark captured in the SAME read transaction — and, for the handful of items a pass acts
//! on, the full [`CacheableDir`]/[`CacheableFile`] payload read back by uuid
//! ([`hydrate_by_uuids`], which shares the search engine's column contract).
//!
//! The read is STREAMED into a [`SnapshotSink`]: the engine copies each row into its pass's stage as
//! it arrives, so the whole subtree never exists in memory beside the view built from it.
//! [`read_subtree_snapshot`] collects the same rows into `Vec`s for the probe and the tests.
//!
//! Pairing the snapshot with its watermark is the point: the sync engine subscribes to the root's
//! event stream FIRST, then takes this snapshot, then discards buffered events whose
//! `drive_message_id <= watermark` (already reflected here) and applies the rest — aligning the
//! snapshot with the live stream with no gap and no double-apply. Reading the items and the
//! watermark inside one deferred read transaction guarantees they describe the same committed
//! instant even while the worker writes concurrently (WAL). The same transaction reads whether the
//! cache has finished listing the root ([`SnapshotMark::converged`]): a subtree it has not is not a
//! view of the remote, and the engine plans against no other.
//!
//! Native only: like the search engine it opens its OWN read-only connection, which the wasm
//! single-connection VFS does not support — and the sync engine that consumes it is native-only.

use std::path::{Path, PathBuf};

use filen_types::{crypto::Blake3Hash, fs::StableUuid};
use rusqlite::{Connection, OptionalExtension, Row, params};
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
			converged_generation_on, list_undecodable,
			statements::{
				ANCESTRY_OF_UUID, CACHE_META_GET, ENUMERATE_SUBTREE, HYDRATE_BY_UUIDS,
				WATERMARK_KEY,
			},
		},
	},
};

/// A consistent point-in-time view of one sync root's cached subtree: every descendant directory
/// and file, plus the contiguous-prefix [`watermark`](Self::watermark) at the same instant.
#[cfg(any(test, feature = "bench-internals"))]
#[derive(Debug)]
pub(crate) struct SubtreeSnapshot {
	pub(crate) dirs: Vec<RemoteItem>,
	pub(crate) files: Vec<RemoteItem>,
	/// The cache's contiguous-prefix watermark (`last_drive_message_id`) at the snapshot instant.
	/// `None` on a fresh cache that has applied nothing yet. Every buffered event whose
	/// `drive_message_id <= watermark` is already reflected in `dirs`/`files`.
	pub(crate) watermark: Option<u64>,
	/// The listed records whose metadata did not decode, and so are in neither `dirs` nor `files`.
	/// NOT scoped to `root`: the table holds every sync root's, and a record is only placeable by
	/// its parent, which the caller resolves against `dirs`.
	pub(crate) undecodable: Vec<UndecodableItem>,
}

/// What a snapshot read learnt about the cache beside the rows, in the same read transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SnapshotMark {
	/// The cache's contiguous-prefix watermark (`last_drive_message_id`) at the snapshot instant;
	/// see [`SubtreeSnapshot::watermark`].
	pub(crate) watermark: Option<u64>,
	/// Whether the rows read are a whole, current view of the root's remote subtree: the cache
	/// converged the root and no resync has started since (see `converged_roots_on`). False while a
	/// first listing, or the apply of a later one, has not committed, and from a write that can
	/// leave the rows short or stale (a dir moved in from outside or restored, a file restored, an
	/// event the cache could not apply, a manual listing) until a listing makes them whole again.
	pub(crate) converged: bool,
}

/// One item of a sync root's cached subtree, in the columns the sync engine's remote view reads
/// and no others (`plan::place_remote_items`). The full `CacheableFile`/`CacheableDir` payload of
/// the handful of items a pass ACTS on is read back by uuid ([`hydrate_by_uuids`]); carrying it
/// for the whole tree cost a file-key parse, a hash decode and four string allocations per row to
/// answer a few dozen questions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteItem {
	pub(crate) uuid: Uuid,
	pub(crate) parent: Uuid,
	/// The leaf name as the cache holds it — NOT normalized; the view NFC-normalizes it.
	pub(crate) name: String,
	/// The server-minted whole-life id of a FILE; `None` for a directory, which has none.
	pub(crate) stable_uuid: Option<StableUuid>,
	/// BLAKE3 of the content (files); `None` for a directory and for an older file the server
	/// stored without a hash.
	pub(crate) hash: Option<Blake3Hash>,
	/// The file's size; `0` for a directory.
	pub(crate) size: u64,
	/// A file's last-modified stamp, a directory's creation stamp, `0` where there is none.
	pub(crate) modified_millis: i64,
}

/// One `ENUMERATE_SUBTREE` row → its [`RemoteItem`], plus whether it is a directory. Read BY
/// INDEX, as the SQL says: this projection has exactly one reader, unlike the full payload, whose
/// by-NAME column contract [`row_to_result`] shares with the search windows. An item of neither
/// type is an error, exactly as it is there.
fn slim_item(row: &Row<'_>) -> rusqlite::Result<(bool, RemoteItem)> {
	let item_type: i64 = row.get(2)?;
	let out_of_range = |column: usize, value: i64| {
		rusqlite::Error::FromSqlConversionFailure(
			column,
			rusqlite::types::Type::Integer,
			Box::new(rusqlite::types::FromSqlError::OutOfRange(value)),
		)
	};
	// A file's whole-life id is NOT NULL in the schema, and the view tells a file from a
	// directory by its presence: a null one here would be a file the engine reads as a directory.
	let (is_dir, stable_uuid) = match item_type {
		1 => (true, None),
		2 => (false, Some(row.get(3)?)),
		other => return Err(out_of_range(2, other)),
	};
	let hash = row
		.get::<_, Option<String>>(4)?
		.map(|hex_str| {
			let mut bytes = [0u8; 32];
			hex::decode_to_slice(hex_str, &mut bytes).map_err(|e| {
				rusqlite::Error::FromSqlConversionFailure(
					4,
					rusqlite::types::Type::Text,
					Box::new(e),
				)
			})?;
			Ok::<_, rusqlite::Error>(Blake3Hash::from(bytes))
		})
		.transpose()?;
	Ok((
		is_dir,
		RemoteItem {
			uuid: row.get(0)?,
			parent: row.get(1)?,
			name: row.get(5)?,
			stable_uuid,
			hash,
			size: row.get(6)?,
			modified_millis: row.get(7)?,
		},
	))
}

/// Where a streamed subtree read hands the rows it reads.
///
/// A sink that takes each item as it arrives — copied into a pass's stage
/// (`sync_engine::stage::RemoteSink`), or placed into a view in memory
/// (`sync_engine::plan::ViewBuilder`) — is what keeps a million-item subtree from existing twice at
/// the widest point of a full pass: once as this read's `Vec`s, once as the structure built out of
/// them.
pub(crate) trait SnapshotSink {
	/// Every undecodable record, handed over BEFORE the first item: which directories the cache
	/// could not decode governs where anything under them can be placed, so a sink that places on
	/// arrival has to know them first.
	fn undecodable(&mut self, items: Vec<UndecodableItem>);

	/// One item of the subtree, and whether it is a directory. NOTHING here fixes the order the
	/// rows arrive in — the statement carries no `ORDER BY`, and sorting a million rows to give one
	/// would cost more than it saves — so a sink that needs an item's ancestry must be able to wait
	/// for it.
	fn item(&mut self, is_dir: bool, item: RemoteItem);
}

/// The undecodable records, then every descendant of `root`, then the watermark and whether the
/// root is converged — all read through the one open transaction `tx`, so they describe the same
/// committed instant. The anchor itself is never handed over.
fn read_into(
	tx: &Connection,
	root: Uuid,
	sink: &mut dyn SnapshotSink,
) -> rusqlite::Result<SnapshotMark> {
	sink.undecodable(list_undecodable(tx)?);
	let mut stmt = tx.prepare_cached(ENUMERATE_SUBTREE)?;
	let rows = stmt.query_map(params![root], slim_item)?;
	for row in rows {
		let (is_dir, item) = row?;
		sink.item(is_dir, item);
	}
	Ok(SnapshotMark {
		watermark: read_watermark(tx)?,
		converged: converged_generation_on(tx, root)?.is_some(),
	})
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
/// read-only connection (WAL-concurrent with the worker's writer) and reads the subtree AND its
/// [`SnapshotMark`] inside ONE deferred read transaction, so they describe the same committed
/// instant.
/// Synchronous SQLite work — async callers go through [`Client::enumerate_sync_root_snapshot`].
pub(crate) fn read_subtree_snapshot_into(
	path: &Path,
	root: Uuid,
	sink: &mut dyn SnapshotSink,
) -> rusqlite::Result<SnapshotMark> {
	let mut conn = open_read_connection(path)?;
	// One deferred read transaction = one stable snapshot for every read below, even
	// mid-worker-write.
	let tx = conn.transaction()?;
	let mark = read_into(&tx, root, sink)?;
	// Read-only: dropping the deferred transaction just ends the snapshot (nothing to commit).
	drop(tx);
	Ok(mark)
}

/// The sink that materializes: the whole subtree as the two `Vec`s [`read_subtree_snapshot`]
/// hands back.
#[cfg(any(test, feature = "bench-internals"))]
#[derive(Default)]
struct Materialize {
	dirs: Vec<RemoteItem>,
	files: Vec<RemoteItem>,
	undecodable: Vec<UndecodableItem>,
}

#[cfg(any(test, feature = "bench-internals"))]
impl SnapshotSink for Materialize {
	fn undecodable(&mut self, items: Vec<UndecodableItem>) {
		self.undecodable = items;
	}

	fn item(&mut self, is_dir: bool, item: RemoteItem) {
		if is_dir {
			self.dirs.push(item);
		} else {
			self.files.push(item);
		}
	}
}

/// [`read_subtree_snapshot_into`] with the whole subtree collected into two `Vec`s. What a pass
/// read before it learned to place rows as they arrive; the probe measures the two side by side
/// and the tests below read it because a `Vec` is what an assertion can look at.
#[cfg(any(test, feature = "bench-internals"))]
pub(crate) fn read_subtree_snapshot(path: &Path, root: Uuid) -> rusqlite::Result<SubtreeSnapshot> {
	let mut sink = Materialize::default();
	let mark = read_subtree_snapshot_into(path, root, &mut sink)?;
	Ok(sink.into_snapshot(mark.watermark))
}

#[cfg(any(test, feature = "bench-internals"))]
impl Materialize {
	fn into_snapshot(self, watermark: Option<u64>) -> SubtreeSnapshot {
		SubtreeSnapshot {
			dirs: self.dirs,
			files: self.files,
			watermark,
			undecodable: self.undecodable,
		}
	}
}

/// [`Materialize`] that panics on the item past `max_items`, so a test reading a corrupt tree
/// fails when the recursion loses its stop rule instead of collecting rows until the process runs
/// out of memory.
#[cfg(test)]
struct Capped {
	inner: Materialize,
	max_items: usize,
}

#[cfg(test)]
impl SnapshotSink for Capped {
	fn undecodable(&mut self, items: Vec<UndecodableItem>) {
		self.inner.undecodable(items);
	}

	fn item(&mut self, is_dir: bool, item: RemoteItem) {
		assert!(
			self.inner.dirs.len() + self.inner.files.len() < self.max_items,
			"the read went past {} item(s): the recursion is not stopping",
			self.max_items
		);
		self.inner.item(is_dir, item);
	}
}

/// [`read_subtree_snapshot`] for a test whose subtree holds at most `max_items` rows, panicking
/// past that instead of reading without end.
#[cfg(test)]
pub(crate) fn read_capped_subtree_snapshot(
	path: &Path,
	root: Uuid,
	max_items: usize,
) -> rusqlite::Result<SubtreeSnapshot> {
	let mut sink = Capped {
		inner: Materialize::default(),
		max_items,
	};
	let mark = read_subtree_snapshot_into(path, root, &mut sink)?;
	Ok(sink.inner.into_snapshot(mark.watermark))
}

/// How many uuids one [`hydrate_by_uuids`] statement binds. The sync engine's apply asks for what
/// one page of its plan acts on, its other callers for a handful of items; chunking keeps both the
/// statement and SQLite's bound-parameter limit bounded whatever a caller passes.
const HYDRATE_CHUNK: usize = 500;

/// Hydrate the FULL payload of `uuids` from the cache DB at `path`: the rows the sync engine's
/// apply and confirmation paths need whole, which the slim subtree snapshot above deliberately
/// does not carry. Read in chunks of [`HYDRATE_CHUNK`], one prepared statement per chunk.
///
/// A uuid the cache does not hold is simply absent from the answer — the item may be one this
/// engine has just written and the cache has not listed yet. The caller asks the server for what
/// is missing; an action whose object neither side can supply FAILS, it is never skipped.
pub(crate) fn hydrate_by_uuids(path: &Path, uuids: &[Uuid]) -> rusqlite::Result<Vec<SearchResult>> {
	if uuids.is_empty() {
		return Ok(Vec::new());
	}
	let conn = open_read_connection(path)?;
	let mut out = Vec::with_capacity(uuids.len());
	for chunk in uuids.chunks(HYDRATE_CHUNK) {
		// The statement carries one `?1`, so that the file is SQL the linter can parse; the chunk
		// needs one parameter per uuid, numbered as the rest of the raw statements are.
		let placeholders = (1..=chunk.len())
			.map(|i| format!("?{i}"))
			.collect::<Vec<_>>()
			.join(", ");
		let mut stmt =
			conn.prepare_cached(&HYDRATE_BY_UUIDS.replace("(?1)", &format!("({placeholders})")))?;
		let rows = stmt.query_map(rusqlite::params_from_iter(chunk), row_to_result)?;
		for row in rows {
			out.push(row?);
		}
	}
	Ok(out)
}

/// The full cached payloads of `uuids` (see [`hydrate_by_uuids`]) — the items a sync pass actually
/// acts on, which its subtree snapshot carries only the view's columns of — read from the cache DB
/// at `path` on a blocking thread.
///
/// It takes the path rather than reading the client's: the sync engine's apply reads here with
/// transfers in flight, and must not wait on the client's cache slot then (see the apply's
/// `ApplyContext::cache_db`). No uuids is no read: an apply page of uploads asks for none.
pub(crate) async fn hydrate_cached_items_at(
	path: PathBuf,
	uuids: Vec<Uuid>,
) -> Result<Vec<SearchResult>, Error> {
	if uuids.is_empty() {
		return Ok(Vec::new());
	}
	tokio::task::spawn_blocking(move || hydrate_by_uuids(&path, &uuids))
		.await
		.map_err(|e| {
			Error::custom(
				ErrorKind::Internal,
				format!("cached item read task failed: {e}"),
			)
		})?
		.map_err(|e| {
			Error::custom_with_source(
				ErrorKind::Internal,
				CacheError::db(e, "hydrating cached items by uuid".to_string()),
				Some("reading cached items".to_string()),
			)
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

	/// The full cached payloads of `uuids`, read from the cache DB this client is configured with
	/// (see [`hydrate_cached_items_at`]). Errors if the cache was never configured.
	pub(crate) async fn hydrate_cached_items(
		&self,
		uuids: Vec<Uuid>,
	) -> Result<Vec<SearchResult>, Error> {
		let path = self.cache_slot.lock().await.db_path().ok_or_else(|| {
			Error::custom(
				ErrorKind::InvalidState,
				"cache is not configured; call configure_cache first",
			)
		})?;
		hydrate_cached_items_at(path, uuids).await
	}

	/// Stream a consistent snapshot of the cached subtree under sync root `root` into `sink` — the
	/// remote-state half the sync engine reconciles against — and hand back the sink with the
	/// [`SnapshotMark`] read at the same instant. Errors if the cache was never configured.
	/// The blocking SQLite read runs on a blocking thread so it never stalls the async runtime.
	///
	/// A read that fails partway takes the sink down with it: what it held is PART of a subtree,
	/// which is not a view of one, and no caller can be handed a tree with holes.
	pub(crate) async fn stream_sync_root_snapshot<S>(
		&self,
		root: Uuid,
		mut sink: S,
	) -> Result<(S, SnapshotMark), Error>
	where
		S: SnapshotSink + Send + 'static,
	{
		let path = self.cache_slot.lock().await.db_path().ok_or_else(|| {
			Error::custom(
				ErrorKind::InvalidState,
				"cache is not configured; call configure_cache first",
			)
		})?;
		tokio::task::spawn_blocking(move || {
			let mark = read_subtree_snapshot_into(&path, root, &mut sink)?;
			Ok::<_, rusqlite::Error>((sink, mark))
		})
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

	/// The generation at which the cache this client is configured with holds sync root `root`
	/// converged, or none while it has not finished listing it (see [`SnapshotMark::converged`]).
	/// Two reads that return the same generation saw no resync start in between. Errors if the
	/// cache was never configured.
	pub(crate) async fn sync_root_converged(&self, root: Uuid) -> Result<Option<u64>, Error> {
		let path = self.cache_slot.lock().await.db_path().ok_or_else(|| {
			Error::custom(
				ErrorKind::InvalidState,
				"cache is not configured; call configure_cache first",
			)
		})?;
		tokio::task::spawn_blocking(move || {
			converged_generation_on(&open_read_connection(&path)?, root)
		})
		.await
		.map_err(|e| {
			Error::custom(
				ErrorKind::Internal,
				format!("convergence read task failed: {e}"),
			)
		})?
		.map_err(|e| {
			Error::custom_with_source(
				ErrorKind::Internal,
				CacheError::db(e, format!("reading whether sync root {root} converged")),
				Some("reading cache convergence".to_string()),
			)
		})
	}
}

#[cfg(test)]
mod tests;

-- WAL persists in the DB file header, so setting it once at schema
-- creation is enough. The per-connection pragmas (foreign_keys,
-- recursive_triggers, temp_store) are applied in `init_db` on every open
-- instead — they revert to their defaults on a fresh connection and would
-- otherwise be skipped on a version-matching reopen.
PRAGMA journal_mode = WAL;

CREATE TABLE items (
	id INTEGER PRIMARY KEY NOT NULL,
	root_id BIGINT NOT NULL,
	uuid BLOB NOT NULL UNIQUE,
	parent BLOB,
	type SMALLINT NOT NULL CHECK (type IN (0, 1, 2)),
	-- Change-detection fingerprint (blake3 of
	-- CacheableFile/Dir::content_fingerprint), maintained on every upsert.
	-- NULL for the account root (type 0, no cacheable form). The resync diff
	-- compares this against the freshly-listed fingerprint to detect content
	-- changes without re-reading every field.
	content_hash BLOB,
	FOREIGN KEY (root_id) REFERENCES roots (
		id
	) ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED
);

-- Critical for every recursive parent-walk (evict_sync_root,
-- diff_subtree_absent, ancestry_of_uuid) and the cascade_on_delete
-- trigger's `DELETE ... WHERE parent = old.uuid`. Without it each recursive
-- step is a full table scan of items.
CREATE INDEX idx_items_parent ON items (parent);

CREATE TABLE roots (
	id BIGINT PRIMARY KEY NOT NULL,
	FOREIGN KEY (id) REFERENCES items (id) ON DELETE CASCADE
);

CREATE TABLE files (
	id BIGINT PRIMARY KEY NOT NULL,
	chunks_size BIGINT NOT NULL,
	chunks BIGINT NOT NULL,
	favorite BOOLEAN NOT NULL CHECK (favorite IN (FALSE, TRUE)) DEFAULT FALSE,
	-- TODO: normalize this
	region TEXT NOT NULL,
	bucket TEXT NOT NULL,
	timestamp BIGINT NOT NULL,

	-- Metadata
	size BIGINT NOT NULL,
	name TEXT NOT NULL,
	mime TEXT NOT NULL,
	file_key TEXT NOT NULL,
	file_key_version SMALLINT NOT NULL CHECK (file_key_version IN (1, 2, 3)),
	created BIGINT,
	modified BIGINT NOT NULL,
	hash BLOB,

	FOREIGN KEY (id) REFERENCES items (id) ON DELETE CASCADE
);

CREATE TABLE dirs (
	id BIGINT PRIMARY KEY NOT NULL,
	favorite BOOLEAN NOT NULL CHECK (favorite IN (FALSE, TRUE)) DEFAULT FALSE,
	-- TODO: normalize this
	color TEXT,
	timestamp BIGINT NOT NULL,

	-- metadata
	name TEXT NOT NULL,
	created BIGINT,

	FOREIGN KEY (id) REFERENCES items (id) ON DELETE CASCADE
);

CREATE TRIGGER cascade_on_delete_delete_children
AFTER DELETE ON items
FOR EACH ROW
WHEN old.type != 2 -- Ensure it's not a file
BEGIN
	DELETE FROM items
	WHERE parent = old.uuid;
END;

-- A uuid whose type flips (Dir <-> File) would otherwise leave a stale row
-- in the OPPOSITE type-specific table, since an upsert only rewrites `items`
-- + the NEW type's table and never deletes the old one. This is not expected
-- on Filen (uuids are type-stable), but guard against it anyway: when an
-- item's type changes, drop any now-stale `dirs`/`files` rows for that id
-- before the upsert re-inserts the correct one. Protects both the resync
-- diff and the live socket-event apply path.
CREATE TRIGGER cleanup_stale_type_row
BEFORE UPDATE OF type ON items
FOR EACH ROW
WHEN old.type != new.type
BEGIN
	DELETE FROM dirs
	WHERE id = old.id;
	DELETE FROM files
	WHERE id = old.id;
END;

-- Durable ordered store for drive events: the in-memory channel spills here
-- so events survive a crash and apply in order. Account-global — an event
-- can touch descendants of several sync roots, so rows carry NO root_id;
-- they are filtered per-root at drain.
CREATE TABLE events (
	-- AUTOINCREMENT so seq is never reused after a quarantine/drain delete:
	-- the insertion-order tiebreaker stays monotonic even as rows are
	-- consumed mid-session.
	seq INTEGER PRIMARY KEY AUTOINCREMENT,
	-- NULL = synthetic diff event; else the account drive_message_id
	drive_message_id BIGINT,
	synthetic BOOLEAN NOT NULL DEFAULT FALSE CHECK (synthetic IN (FALSE, TRUE)),
	payload BLOB NOT NULL                -- rkyv::to_bytes of CacheEvent<'static>
);

-- "synthetic first, then by drive_message_id, then by seq" — the drain order.
CREATE INDEX idx_events_order ON events (
	synthetic DESC, drive_message_id ASC, seq ASC
);

-- Dedup at-least-once redelivery at persist time: two real rows with the
-- same drive_message_id would both pass `id > watermark` in one batch.
-- INSERT OR IGNORE drops the duplicate on the way in. Synthetic
-- (NULL drive_message_id) rows are exempt via the partial WHERE.
CREATE UNIQUE INDEX idx_events_unique_id ON events (drive_message_id)
WHERE drive_message_id IS NOT NULL;

-- Singleton key/value metadata. Holds the global watermark and the event
-- blob format version.
CREATE TABLE cache_meta (
	meta_key TEXT PRIMARY KEY NOT NULL,
	value BLOB
);

-- last_drive_message_id: the contiguous-prefix watermark (NULL until the
-- first apply). Stored as an INTEGER (u64 cast `as i64` on write / `as u64`
-- on read; the account counter never nears i64::MAX).
INSERT INTO cache_meta (meta_key, value) VALUES ('last_drive_message_id', NULL);
-- event_format_version: records the rkyv layout version of
-- `events.payload`. Currently informational (no code reads it yet). A layout
-- change is caught at READ time instead — a stale-layout row fails the
-- checked rkyv decode and is quarantined as corrupt (forcing a resync),
-- rather than proactively cleared.
INSERT INTO cache_meta (meta_key, value) VALUES ('event_format_version', 1);

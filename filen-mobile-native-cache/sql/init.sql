PRAGMA recursive_triggers = TRUE;
PRAGMA journal_mode = WAL;
PRAGMA temp_store = MEMORY;

-- Change-tracking counter (workstream A). `seq` is a single monotonic sequence bumped by the
-- triggers at the bottom of this file on every observable item/metadata mutation; `epoch` is 8
-- random bytes chosen once at DB creation so a rebuilt cache DB reports a fresh generation (a
-- mismatched anchor epoch surfaces as anchor_expired). Created and seeded early, before any trigger
-- exists, so the seed itself never bumps the counter.
CREATE TABLE sync_state (
	id INTEGER PRIMARY KEY CHECK (id = 1),
	seq BIGINT NOT NULL DEFAULT 0,
	epoch BLOB NOT NULL
);
INSERT INTO sync_state (id, seq, epoch) VALUES (1, 0, randomblob(8));

CREATE TABLE items (
	id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
	uuid BLOB NOT NULL UNIQUE,
	-- Identity that survives file-content re-mints (which swap `uuid` in place). Set to `uuid` on
	-- first insert and preserved forever across uuid re-mints. Dirs/roots never re-mint, so for them
	-- stable_uuid == uuid always.
	stable_uuid BLOB NOT NULL UNIQUE,
	-- The item's real parent UUID. For a trashed item this stays the *original* parent
	-- (where it will be restored to); `trashed` distinguishes the two. NULL for the root.
	parent BLOB,
	trashed BOOLEAN NOT NULL CHECK (trashed IN (FALSE, TRUE)) DEFAULT FALSE,
	type SMALLINT NOT NULL CHECK (type IN (0, 1, 2)),
	is_stale BOOLEAN NOT NULL CHECK (is_stale IN (FALSE, TRUE)) DEFAULT FALSE,
	local_data TEXT,
	is_recent BOOLEAN NOT NULL CHECK (is_recent IN (FALSE, TRUE)) DEFAULT FALSE,
	-- Change-tracking sequence: the value of sync_state.seq at this row's last observable mutation.
	-- Default 0 means "changed before any anchor", so every row is included for a from_seq=0 anchor.
	-- Bumped ONLY by the trigger set below (never in any trigger's UPDATE OF list, so seq-bump writes
	-- do not recurse); application code never writes it directly.
	seq BIGINT NOT NULL DEFAULT 0
);

CREATE INDEX idx_items_uuid ON items (uuid);
CREATE INDEX idx_items_stable_uuid ON items (stable_uuid);
CREATE INDEX idx_items_parent ON items (parent);
CREATE INDEX idx_items_is_recent ON items (is_recent);
CREATE INDEX idx_items_trashed ON items (trashed)
WHERE trashed = TRUE;
CREATE INDEX idx_items_parent_seq ON items (parent, seq);
CREATE INDEX idx_items_seq ON items (seq);

CREATE TABLE roots (
	id BIGINT PRIMARY KEY NOT NULL,
	storage_used BIGINT NOT NULL DEFAULT 0,
	max_storage BIGINT NOT NULL DEFAULT 0,
	last_updated BIGINT NOT NULL DEFAULT 0,
	FOREIGN KEY (id) REFERENCES items (id) ON DELETE CASCADE
);

CREATE INDEX idx_stale_items ON items (parent)
WHERE is_stale = TRUE;

CREATE TABLE files (
	id BIGINT PRIMARY KEY NOT NULL,
	size BIGINT NOT NULL,
	chunks BIGINT NOT NULL,
	favorite_rank INTEGER NOT NULL DEFAULT 0, -- IOS uses this for sorting
	region TEXT NOT NULL,
	bucket TEXT NOT NULL,
	timestamp BIGINT NOT NULL,
	-- 0 = decoded, 1 = decrypted(raw or utf8), 2 = encrypted, 3 = rsa encrypted
	metadata_state SMALLINT NOT NULL CHECK (
		metadata_state IN (0, 1, 2, 3)
	),
	-- if metadata is not decoded, this is the raw metadata
	raw_metadata TEXT,
	FOREIGN KEY (id) REFERENCES items (id) ON DELETE CASCADE,
	CHECK (
		(metadata_state = 0 AND raw_metadata IS NULL)
		OR (metadata_state != 0 AND raw_metadata IS NOT NULL)
	)
);

CREATE TABLE files_meta (
	id BIGINT PRIMARY KEY NOT NULL,
	name TEXT NOT NULL,
	mime TEXT NOT NULL,
	file_key TEXT NOT NULL,
	file_key_version SMALLINT NOT NULL CHECK (file_key_version IN (1, 2, 3)),
	created BIGINT,
	modified BIGINT NOT NULL,
	hash BLOB,
	FOREIGN KEY (id) REFERENCES files (id) ON DELETE CASCADE
);

CREATE TABLE dirs (
	id BIGINT PRIMARY KEY NOT NULL,
	favorite_rank INTEGER NOT NULL DEFAULT 0, -- IOS uses this for sorting
	-- DirColor type
	color TEXT,
	timestamp BIGINT NOT NULL,
	-- 0 = decoded, 1 = decrypted(raw or utf8), 2 = encrypted, 3 = rsa encrypted
	metadata_state SMALLINT NOT NULL CHECK (
		metadata_state IN (0, 1, 2, 3)
	),
	-- if metadata is not decoded, this is the raw metadata
	raw_metadata TEXT,
	last_listed BIGINT NOT NULL DEFAULT 0,
	FOREIGN KEY (id) REFERENCES items (id) ON DELETE CASCADE,
	CHECK (
		(metadata_state = 0 AND raw_metadata IS NULL)
		OR (metadata_state != 0 AND raw_metadata IS NOT NULL)
	)
);

CREATE TABLE dirs_meta (
	id BIGINT PRIMARY KEY NOT NULL,
	name TEXT NOT NULL,
	created BIGINT,
	FOREIGN KEY (id) REFERENCES dirs (id) ON DELETE CASCADE
);

-- Tombstones for deleted items (workstream A). Populated by items_tombstone_on_delete (one row per
-- deleted item, including every descendant of a cascaded subtree delete) and cleared on resurrection
-- (a re-insert or stable_uuid reassignment carrying the same stable identity). `seq` is the
-- sync_state.seq at deletion time, so a delta enumeration surfaces removals with `seq > from_seq`.
-- NOTE: a *trash* is NOT a delete (the row survives with trashed = TRUE), so trashing does not
-- tombstone; it bumps seq via items_bump_on_update (which now watches `trashed`), and the trashed=0
-- filter in the child-listing queries removes it from its old parent's listing.
CREATE TABLE deletions (
	stable_uuid BLOB PRIMARY KEY,
	uuid BLOB NOT NULL,
	parent BLOB,
	seq BIGINT NOT NULL
);
CREATE INDEX idx_deletions_parent_seq ON deletions (parent, seq);
CREATE INDEX idx_deletions_seq ON deletions (seq);

-- No synthetic 'trash' sentinel row: trash is represented by the `trashed` flag on each item
-- (which keeps its original `parent`), per agents/trash-parent. The trash container is addressed
-- via `trashed = TRUE`, not a parent = 'trash' row.

CREATE TRIGGER cascade_on_update_uuid_delete_children
AFTER UPDATE OF uuid ON items
FOR EACH ROW
WHEN old.uuid != new.uuid AND old.type != 2 -- Ensure it's not a file
BEGIN
	-- Trashed items are keyed off their original parent; they must survive the
	-- parent's churn (they live in the trash listing, not under the parent) so
	-- exclude them here.
	DELETE FROM items
	WHERE parent = old.uuid AND trashed = FALSE;
END;

CREATE TRIGGER cascade_on_delete_delete_children
AFTER DELETE ON items
FOR EACH ROW
WHEN old.type != 2 -- Ensure it's not a file
BEGIN
	DELETE FROM items
	WHERE parent = old.uuid AND trashed = FALSE;
END;

-- Change-tracking triggers (workstream A). Invariants:
--   * Every seq-bump advances sync_state.seq by exactly 1, then stamps that value onto the affected
--     items row (or records it in a tombstone). Seq-bump UPDATEs touch ONLY the `seq` column, which
--     is in no trigger's UPDATE OF list, so with recursive_triggers = ON they never re-fire a bump.
--   * Every UPDATE-OF trigger carries a NULL-safe (`IS NOT`) WHEN guard, because SQLite fires an
--     UPDATE-OF trigger whenever the column appears in the SET list even if the value is unchanged.
--     A re-list (mark_stale -> upsert identical values -> delete_stale) must therefore NOT bump.
--   * `is_stale`, `is_recent`, and `dirs.last_listed` are deliberately absent from every UPDATE OF
--     list, so re-list staleness churn, recents snapshots, and last-listed stamps never bump.

-- A new item is a change; also resurrect its stable identity by dropping any matching tombstone.
CREATE TRIGGER items_bump_on_insert
AFTER INSERT ON items
FOR EACH ROW
BEGIN
	UPDATE sync_state SET seq = seq + 1 WHERE id = 1;
	UPDATE items SET seq = (SELECT seq FROM sync_state WHERE id = 1) WHERE id = new.id;
	DELETE FROM deletions WHERE stable_uuid = new.stable_uuid;
END;

-- Identity-level mutations. A content re-mint swaps `uuid` in place on the same row (type = 2, so
-- cascade_on_update_uuid_delete_children does not fire): this bumps once and never tombstones.
-- `trashed` is watched too: trashing/restoring is an observable membership change (the item leaves
-- or re-enters its parent's listing) even though its `parent` column is unchanged.
CREATE TRIGGER items_bump_on_update
AFTER UPDATE OF uuid, parent, local_data, trashed ON items
FOR EACH ROW
WHEN old.uuid IS NOT new.uuid
	OR old.parent IS NOT new.parent
	OR old.local_data IS NOT new.local_data
	OR old.trashed IS NOT new.trashed
BEGIN
	UPDATE sync_state SET seq = seq + 1 WHERE id = 1;
	UPDATE items SET seq = (SELECT seq FROM sync_state WHERE id = 1) WHERE id = new.id;
END;

-- Reassigning a stable identity onto a live row (the modify_file_content re-mint fallback) resurrects
-- it: clear any tombstone so the delta feed does not report it as both updated and deleted.
CREATE TRIGGER items_resurrect_on_stable_uuid_update
AFTER UPDATE OF stable_uuid ON items
FOR EACH ROW
WHEN old.stable_uuid IS NOT new.stable_uuid
BEGIN
	DELETE FROM deletions WHERE stable_uuid = new.stable_uuid;
END;

CREATE TRIGGER files_bump_on_update
AFTER UPDATE OF
	size, chunks, favorite_rank, region, bucket, timestamp, metadata_state, raw_metadata
ON files
FOR EACH ROW
WHEN old.size IS NOT new.size
	OR old.chunks IS NOT new.chunks
	OR old.favorite_rank IS NOT new.favorite_rank
	OR old.region IS NOT new.region
	OR old.bucket IS NOT new.bucket
	OR old.timestamp IS NOT new.timestamp
	OR old.metadata_state IS NOT new.metadata_state
	OR old.raw_metadata IS NOT new.raw_metadata
BEGIN
	UPDATE sync_state SET seq = seq + 1 WHERE id = 1;
	UPDATE items SET seq = (SELECT seq FROM sync_state WHERE id = 1) WHERE id = new.id;
END;

CREATE TRIGGER files_meta_bump_on_insert
AFTER INSERT ON files_meta
FOR EACH ROW
BEGIN
	UPDATE sync_state SET seq = seq + 1 WHERE id = 1;
	UPDATE items SET seq = (SELECT seq FROM sync_state WHERE id = 1) WHERE id = new.id;
END;

CREATE TRIGGER files_meta_bump_on_update
AFTER UPDATE OF name, mime, file_key, file_key_version, created, modified, hash ON files_meta
FOR EACH ROW
WHEN old.name IS NOT new.name
	OR old.mime IS NOT new.mime
	OR old.file_key IS NOT new.file_key
	OR old.file_key_version IS NOT new.file_key_version
	OR old.created IS NOT new.created
	OR old.modified IS NOT new.modified
	OR old.hash IS NOT new.hash
BEGIN
	UPDATE sync_state SET seq = seq + 1 WHERE id = 1;
	UPDATE items SET seq = (SELECT seq FROM sync_state WHERE id = 1) WHERE id = new.id;
END;

-- Deleting metadata only bumps when the owning items row survives (an explicit delete_file_meta as a
-- file's metadata becomes non-decoded). A cascaded delete of the items row leaves no row to stamp.
CREATE TRIGGER files_meta_bump_on_delete
AFTER DELETE ON files_meta
FOR EACH ROW
WHEN EXISTS (SELECT 1 FROM items WHERE id = old.id)
BEGIN
	UPDATE sync_state SET seq = seq + 1 WHERE id = 1;
	UPDATE items SET seq = (SELECT seq FROM sync_state WHERE id = 1) WHERE id = old.id;
END;

-- dirs deliberately omits last_listed: a re-list stamps last_listed but must not be a change.
CREATE TRIGGER dirs_bump_on_update
AFTER UPDATE OF favorite_rank, color, timestamp, metadata_state, raw_metadata ON dirs
FOR EACH ROW
WHEN old.favorite_rank IS NOT new.favorite_rank
	OR old.color IS NOT new.color
	OR old.timestamp IS NOT new.timestamp
	OR old.metadata_state IS NOT new.metadata_state
	OR old.raw_metadata IS NOT new.raw_metadata
BEGIN
	UPDATE sync_state SET seq = seq + 1 WHERE id = 1;
	UPDATE items SET seq = (SELECT seq FROM sync_state WHERE id = 1) WHERE id = new.id;
END;

CREATE TRIGGER dirs_meta_bump_on_insert
AFTER INSERT ON dirs_meta
FOR EACH ROW
BEGIN
	UPDATE sync_state SET seq = seq + 1 WHERE id = 1;
	UPDATE items SET seq = (SELECT seq FROM sync_state WHERE id = 1) WHERE id = new.id;
END;

CREATE TRIGGER dirs_meta_bump_on_update
AFTER UPDATE OF name, created ON dirs_meta
FOR EACH ROW
WHEN old.name IS NOT new.name
	OR old.created IS NOT new.created
BEGIN
	UPDATE sync_state SET seq = seq + 1 WHERE id = 1;
	UPDATE items SET seq = (SELECT seq FROM sync_state WHERE id = 1) WHERE id = new.id;
END;

CREATE TRIGGER dirs_meta_bump_on_delete
AFTER DELETE ON dirs_meta
FOR EACH ROW
WHEN EXISTS (SELECT 1 FROM items WHERE id = old.id)
BEGIN
	UPDATE sync_state SET seq = seq + 1 WHERE id = 1;
	UPDATE items SET seq = (SELECT seq FROM sync_state WHERE id = 1) WHERE id = old.id;
END;

-- Every delete is a change: bump and tombstone. With recursive_triggers = ON, the cascade trigger's
-- child deletes re-fire this trigger, so an entire deleted subtree is tombstoned row by row.
CREATE TRIGGER items_tombstone_on_delete
AFTER DELETE ON items
FOR EACH ROW
BEGIN
	UPDATE sync_state SET seq = seq + 1 WHERE id = 1;
	INSERT OR REPLACE INTO deletions (stable_uuid, uuid, parent, seq)
	VALUES (old.stable_uuid, old.uuid, old.parent, (SELECT seq FROM sync_state WHERE id = 1));
END;

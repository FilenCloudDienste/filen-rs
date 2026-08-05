CREATE TABLE items (
	id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
	uuid BLOB NOT NULL UNIQUE,
	-- Server-minted whole-life id, FILES ONLY. Unlike `uuid` (which the server
	-- re-mints on every content edit and version restore of a file) this never
	-- changes for the file's lifetime, so it is what external identities (the
	-- providers) key on. Dirs and roots store NULL: the server never re-mints
	-- their `uuid`, so their `uuid` already is their whole-life id.
	-- Deliberately NOT UNIQUE: duplicate stables are reachable via
	-- same-account uuid-reuse abuse and must reconcile, never error.
	stable_uuid BLOB,
	-- The item's real parent UUID. For a trashed item this stays the *original*
	-- parent (where it will be restored to); `trashed` distinguishes the two.
	-- NULL for the root.
	parent BLOB,
	trashed BOOLEAN NOT NULL CHECK (trashed IN (FALSE, TRUE)) DEFAULT FALSE,
	type SMALLINT NOT NULL CHECK (type IN (0, 1, 2)),
	is_stale BOOLEAN NOT NULL CHECK (is_stale IN (FALSE, TRUE)) DEFAULT FALSE,
	local_data TEXT,
	is_recent BOOLEAN NOT NULL CHECK (is_recent IN (FALSE, TRUE)) DEFAULT FALSE,
	-- Millis at which a local edit was marked as not yet on the server; NULL
	-- means nothing is outstanding. A column of its own rather than a key in
	-- `local_data`, which is the app's to overwrite wholesale over the FFI —
	-- it has no way to know an internal key is in there.
	pending_upload_at INTEGER,
	-- A stable id is a files-only concept: every file has one, no dir (1) or
	-- root (0) may carry one.
	CHECK ((type = 2) = (stable_uuid IS NOT NULL)),
	-- So is an outstanding upload: only a file has bytes to send.
	CHECK (pending_upload_at IS NULL OR type = 2)
);

CREATE INDEX idx_items_uuid ON items (uuid);
-- Partial: only files carry a stable id, so the NULL half of the table is
-- dead weight in the index and never queried.
CREATE INDEX idx_items_stable_uuid ON items (stable_uuid)
WHERE stable_uuid IS NOT NULL;
CREATE INDEX idx_items_parent ON items (parent);
CREATE INDEX idx_items_is_recent ON items (is_recent);
CREATE INDEX idx_items_trashed ON items (trashed)
WHERE trashed = TRUE;

-- Partial: the drain scans for the handful of marked rows, never the rest.
CREATE INDEX idx_items_pending_upload ON items (pending_upload_at)
WHERE pending_upload_at IS NOT NULL;

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

-- A uuid arriving with a different type than the row that currently holds it
-- means the server reassigned the uuid to a new object (same-account
-- uuid-reuse abuse): the cached row is a different item's corpse, and adopting
-- it would silently flip its type, destroy a file's stable id and leak its
-- local_data onto an unrelated object. Retire the old row first — the delete
-- cascades to its children and per-type rows — so the insert lands fresh.
-- The upsert's uuid tier is type-scoped for the same reason.
CREATE TRIGGER retire_row_on_cross_type_uuid_reuse
BEFORE INSERT ON items
FOR EACH ROW
BEGIN
	DELETE FROM items
	WHERE uuid = new.uuid AND type != new.type;
END;

PRAGMA recursive_triggers = TRUE;
PRAGMA journal_mode = WAL;
PRAGMA temp_store = MEMORY;

CREATE TABLE items (
	id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
	uuid BLOB NOT NULL UNIQUE,
	-- The item's real parent UUID. For a trashed item this stays the *original*
	-- parent (where it will be restored to); `trashed` distinguishes the two.
	-- NULL for the root.
	parent BLOB,
	trashed BOOLEAN NOT NULL CHECK (trashed IN (FALSE, TRUE)) DEFAULT FALSE,
	type SMALLINT NOT NULL CHECK (type IN (0, 1, 2)),
	is_stale BOOLEAN NOT NULL CHECK (is_stale IN (FALSE, TRUE)) DEFAULT FALSE,
	local_data TEXT,
	is_recent BOOLEAN NOT NULL CHECK (is_recent IN (FALSE, TRUE)) DEFAULT FALSE
);

CREATE INDEX idx_items_uuid ON items (uuid);
CREATE INDEX idx_items_parent ON items (parent);
CREATE INDEX idx_items_is_recent ON items (is_recent);
CREATE INDEX idx_items_trashed ON items (trashed)
WHERE trashed = TRUE;

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

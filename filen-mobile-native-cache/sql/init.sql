PRAGMA recursive_triggers = TRUE;
PRAGMA journal_mode = WAL;
PRAGMA temp_store = MEMORY;

CREATE TABLE items (
	id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
	uuid TEXT NOT NULL UNIQUE,
	parent TEXT,
	type SMALLINT NOT NULL CHECK (type IN (0, 1, 2)),
	is_stale BOOLEAN NOT NULL CHECK (is_stale IN (FALSE, TRUE)) DEFAULT FALSE,
	local_data TEXT,
	is_recent BOOLEAN NOT NULL CHECK (is_recent IN (FALSE, TRUE)) DEFAULT FALSE,
	-- This is used if the item has been added by search
	-- In that case the parent might not be in the database yet
	-- so we have no way of resolving the item's path
	-- the /search/find endpoint does return the encrypted path
	-- so we store it here to avoid having to query the server again
	-- This is also used to identify items that have been recently searched for
	-- and a search should always clear previous
	parent_path TEXT
);

CREATE INDEX idx_items_uuid ON items (uuid);
CREATE INDEX idx_items_parent ON items (parent);
CREATE INDEX idx_items_is_recent ON items (is_recent);
CREATE INDEX idx_items_has_search ON items (parent_path IS NOT NULL);

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
	color TEXT,
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

INSERT INTO items (uuid, parent, type) VALUES ('trash', NULL, 1);
INSERT INTO dirs (id, metadata_state) VALUES (last_insert_rowid(), 0);
INSERT INTO dirs_meta (id, name) VALUES (last_insert_rowid(), 'Trash');

CREATE TRIGGER cascade_on_update_uuid_delete_children
AFTER UPDATE OF uuid ON items
FOR EACH ROW
WHEN old.uuid != new.uuid AND old.type != 2 -- Ensure it's not a file
BEGIN
DELETE FROM items
WHERE parent = old.uuid AND parent_path IS NULL;
END;

CREATE TRIGGER cascade_on_delete_delete_children
AFTER DELETE ON items
FOR EACH ROW
WHEN old.type != 2 -- Ensure it's not a file
BEGIN
DELETE FROM items
WHERE parent = old.uuid AND parent_path IS NULL;
END;

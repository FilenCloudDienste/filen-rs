PRAGMA recursive_triggers = TRUE;
PRAGMA journal_mode = WAL;
PRAGMA temp_store = MEMORY;
PRAGMA foreign_keys = ON;

CREATE TABLE items (
	id INTEGER PRIMARY KEY NOT NULL,
	root_id BIGINT NOT NULL,
	uuid BLOB NOT NULL UNIQUE,
	parent BLOB,
	type SMALLINT NOT NULL CHECK (type IN (0, 1, 2)),
	FOREIGN KEY (root_id) REFERENCES roots (
		id
	) ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED
);

CREATE INDEX idx_items_parent ON items (parent);

CREATE TABLE roots (
	id BIGINT PRIMARY KEY NOT NULL,
	last_global_message_id BIGINT,
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

CREATE TABLE file_versions (
	id BIGINT PRIMARY KEY NOT NULL,
	file_id BIGINT NOT NULL,
	version SMALLINT NOT NULL CHECK (version IN (1, 2, 3)),
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

	FOREIGN KEY (file_id) REFERENCES files (id) ON DELETE CASCADE
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


-- INSERT INTO items (uuid, parent, type) VALUES ('trash', NULL, 1);
-- INSERT INTO dirs (id, timestamp, metadata_state, name) VALUES (
-- 	last_insert_rowid(), 0, 0, 'Trash'
-- );

CREATE TRIGGER cascade_on_delete_delete_children
AFTER DELETE ON items
FOR EACH ROW
WHEN old.type != 2 -- Ensure it's not a file
BEGIN
	DELETE FROM items
	WHERE parent = old.uuid;
END;

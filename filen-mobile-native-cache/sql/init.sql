CREATE TABLE IF NOT EXISTS items (
    id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
    uuid TEXT NOT NULL UNIQUE,
    parent TEXT,
    name TEXT NOT NULL,
    type SMALLINT NOT NULL CHECK (type IN (0, 1, 2)),
    is_stale BOOLEAN NOT NULL CHECK (is_stale IN (FALSE, TRUE)) DEFAULT FALSE,
    local_data TEXT,
    FOREIGN KEY (parent) REFERENCES items (uuid) ON DELETE CASCADE
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_unique_name_parent_stale
ON items (name, parent, is_stale)
WHERE parent != 'trash';

INSERT OR IGNORE INTO items (uuid, parent, name, type)
VALUES ('trash', NULL, 'Trash', 1);

CREATE INDEX IF NOT EXISTS idx_items_uuid ON items (uuid);
CREATE INDEX IF NOT EXISTS idx_items_parent ON items (parent);

CREATE TABLE IF NOT EXISTS roots (
    id BIGINT PRIMARY KEY NOT NULL,
    storage_used BIGINT NOT NULL DEFAULT 0,
    max_storage BIGINT NOT NULL DEFAULT 0,
    last_updated BIGINT NOT NULL DEFAULT 0,
    FOREIGN KEY (id) REFERENCES items (id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_stale_items ON items (parent)
WHERE is_stale = TRUE;

CREATE TABLE IF NOT EXISTS files (
    id BIGINT PRIMARY KEY NOT NULL,
    mime TEXT NOT NULL,
    file_key TEXT NOT NULL,
    created BIGINT NOT NULL,
    modified BIGINT NOT NULL,
    size BIGINT NOT NULL,
    chunks BIGINT NOT NULL,
    favorite_rank INTEGER NOT NULL DEFAULT 0, -- IOS uses this for sorting
    region TEXT NOT NULL,
    bucket TEXT NOT NULL,
    hash BLOB,
    version SMALLINT NOT NULL CHECK (version IN (1, 2, 3)),
    FOREIGN KEY (id) REFERENCES items (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS dirs (
    id BIGINT PRIMARY KEY NOT NULL,
    created BIGINT,
    favorite_rank INTEGER NOT NULL DEFAULT 0, -- IOS uses this for sorting
    color TEXT,
    last_listed BIGINT NOT NULL DEFAULT 0,
    FOREIGN KEY (id) REFERENCES items (id) ON DELETE CASCADE
);

CREATE TRIGGER IF NOT EXISTS cascade_on_update_uuid_delete_children
AFTER UPDATE OF uuid ON items
FOR EACH ROW
WHEN old.uuid != new.uuid AND old.type != 2 -- Ensure it's not a file
BEGIN
DELETE FROM items
WHERE parent = old.uuid;
END;

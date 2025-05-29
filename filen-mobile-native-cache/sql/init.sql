CREATE TABLE items (
    id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
    uuid BLOB NOT NULL UNIQUE,
    parent BLOB NOT NULL,
    name TEXT NOT NULL,
    type SMALLINT NOT NULL CHECK (type IN (0, 1, 2))
);
CREATE INDEX idx_items_uuid ON items (uuid);
CREATE INDEX idx_items_parent ON items (parent);
CREATE TABLE roots (
    id BIGINT PRIMARY KEY NOT NULL,
    storage_used BIGINT NOT NULL DEFAULT 0,
    max_storage BIGINT NOT NULL DEFAULT 0,
    last_updated BIGINT NOT NULL DEFAULT 0,
    FOREIGN KEY (id) REFERENCES items (id) ON DELETE CASCADE
);
CREATE TABLE files (
    id BIGINT PRIMARY KEY NOT NULL,
    mime TEXT NOT NULL,
    file_key TEXT NOT NULL,
    created BIGINT NOT NULL,
    modified BIGINT NOT NULL,
    size BIGINT NOT NULL,
    chunks BIGINT NOT NULL,
    favorited BOOLEAN NOT NULL CHECK (favorited IN (0, 1)),
    region TEXT NOT NULL,
    bucket TEXT NOT NULL,
    hash BLOB,
    FOREIGN KEY (id) REFERENCES items (id) ON DELETE CASCADE
);
CREATE TABLE dirs (
    id BIGINT PRIMARY KEY NOT NULL,
    created BIGINT,
    favorited BOOLEAN NOT NULL CHECK (favorited IN (0, 1)) DEFAULT 0,
    color TEXT,
    last_listed BIGINT NOT NULL DEFAULT 0,
    FOREIGN KEY (id) REFERENCES items (id) ON DELETE CASCADE
);

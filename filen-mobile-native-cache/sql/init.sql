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
	-- Where this row sits in the change sequence: the value `change_meta.counter`
	-- held when the item last changed in a way an external replica (the file
	-- providers) can see. Maintained by the triggers at the bottom of this file,
	-- never by a statement. The DEFAULT is load-bearing — `upsert_item.sql`
	-- deliberately names only the columns it is allowed to touch, so a fresh row
	-- has to arrive with a legal value of its own before the trigger stamps it.
	change_seq INTEGER NOT NULL DEFAULT 0,
	-- Millis at which this file's bytes were last written into the local cache
	-- directory; NULL means nothing of it is stored locally. A column of its own
	-- for the same reason `pending_upload_at` is one: `local_data` is the app's
	-- to overwrite wholesale over the FFI.
	materialised_at INTEGER,
	-- A stable id is a files-only concept: every file has one, no dir (1) or
	-- root (0) may carry one.
	CHECK ((type = 2) = (stable_uuid IS NOT NULL)),
	-- So is an outstanding upload: only a file has bytes to send.
	CHECK (pending_upload_at IS NULL OR type = 2),
	-- And so are locally materialised bytes.
	CHECK (materialised_at IS NULL OR type = 2)
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

-- The change feed's only access path: every diff a replica asks for is
-- `change_seq > anchor`.
CREATE INDEX idx_items_change_seq ON items (change_seq);

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

-- Ids that used to resolve to a row and no longer do. A replica is told to drop
-- them, so what is recorded is the *provider* id, which is type-dependent: a
-- dir's own `uuid` (the server never re-mints it, so it is the dir's lifetime
-- id) and a file's `stable_uuid` (its `uuid` is re-minted on every content
-- edit). `kind` mirrors `items.type` — 1 = dir, 2 = file — and keeping the two
-- kinds apart is what lets the not-exists guards below resolve through
-- `idx_items_uuid` / `idx_items_stable_uuid`; a single polymorphic id column
-- would need a COALESCE over both, which scans.
CREATE TABLE tombstones (
	kind SMALLINT NOT NULL CHECK (kind IN (1, 2)),
	item_id BLOB NOT NULL,
	seq INTEGER NOT NULL,
	PRIMARY KEY (kind, item_id)
);

CREATE INDEX idx_tombstones_seq ON tombstones (seq);

-- The change counter, in a one-row table so a trigger can bump it with a bare
-- UPDATE. `db_instance` names this incarnation of the database: a wipe mints a
-- new one, which is how an anchor handed out before the wipe is recognised as
-- expired instead of silently under-reporting everything the wipe destroyed.
--
-- The row is seeded here, not from Rust, because the schema is created from
-- several places (both `execute_batch(INIT)` call sites plus every test
-- harness) and every trigger below reads this row — a creation path that forgot
-- to seed it would have no working writes at all.
CREATE TABLE change_meta (
	id INTEGER PRIMARY KEY CHECK (id = 0),
	db_instance BLOB NOT NULL,
	counter INTEGER NOT NULL DEFAULT 0
);

INSERT INTO change_meta (id, db_instance) VALUES (0, randomblob(16));

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

-- Change tracking. Everything below maintains `items.change_seq` and
-- `tombstones` so a replica holding sequence N can be handed exactly what
-- happened above N. Three rules run through all of it:
--
-- * The bump is always the same two statements: raise the shared counter, then
--   stamp it on the item. The stamping UPDATE re-enters `bump_seq_items_update`
--   with a guard that does not name `change_seq`, so it stops there; it names
--   no id column either, so the `AFTER UPDATE OF` triggers never fire from it.
-- * Every OLD/NEW comparison uses `IS NOT`, never `!=`. `hash`, `created`,
--   `color` and `stable_uuid` are nullable, and on a NULL -> value transition
--   `!=` yields NULL rather than TRUE, silently swallowing the change.
-- * One logical write can bump more than once (a new file is an `items` insert
--   plus a `files` insert plus a `files_meta` insert, so three). Accepted: a
--   diff returns the row once, at its latest sequence, and the only cost is
--   sequence numbers nobody looks at.

CREATE TRIGGER bump_seq_items_insert
AFTER INSERT ON items
FOR EACH ROW
WHEN new.type != 0 -- roots exist only locally; no replica ever sees one
BEGIN
	UPDATE change_meta SET counter = counter + 1;
	UPDATE items SET change_seq = (SELECT counter FROM change_meta)
	WHERE id = new.id;
	-- A row that adopts an id makes it resolve again, so its tombstone has to
	-- go: the invariant is that a tombstone exists exactly while its id resolves
	-- to nothing. A file's `uuid` is not its provider id, which is why the dir
	-- clear is type-scoped — a file reusing a retired dir's uuid must leave that
	-- dir's tombstone standing.
	DELETE FROM tombstones
	WHERE kind = 2 AND item_id = new.stable_uuid;
	DELETE FROM tombstones
	WHERE kind = 1 AND item_id = new.uuid AND new.type = 1;
END;

-- `type` is absent from the guard because it cannot change here: all three
-- tiers of `upsert_item.sql` are type-scoped, and a cross-type collision is
-- resolved by the retirement trigger above (DELETE + fresh INSERT) instead.
-- `is_stale`, `is_recent`, `local_data`, `pending_upload_at` and
-- `materialised_at` are absent because they are local state a replica is never
-- shown — and because the stale-mark half of every directory refresh would
-- otherwise bump every row in the directory.
CREATE TRIGGER bump_seq_items_update
AFTER UPDATE ON items
FOR EACH ROW
WHEN
	old.uuid IS NOT new.uuid
	OR old.stable_uuid IS NOT new.stable_uuid
	OR old.parent IS NOT new.parent
	OR old.trashed IS NOT new.trashed
BEGIN
	UPDATE change_meta SET counter = counter + 1;
	UPDATE items SET change_seq = (SELECT counter FROM change_meta)
	WHERE id = new.id;
	DELETE FROM tombstones
	WHERE kind = 2 AND item_id = new.stable_uuid;
	DELETE FROM tombstones
	WHERE kind = 1 AND item_id = new.uuid AND new.type = 1;
END;

-- Covers `delete_item`, both cascade triggers above, the cross-type retirement,
-- and the two positional stale sweeps — which delete by predicate and so have
-- no per-row visibility in Rust at all. That last one is the whole reason this
-- is a trigger.
CREATE TRIGGER tombstone_on_delete
AFTER DELETE ON items
FOR EACH ROW
WHEN
	old.type != 0
	-- Never hand a replica an authoritative delete for the only copy of bytes
	-- that have not reached the server: it would evict them. Such a row shows up
	-- as a phantom instead, until the next listing reconciles it — the accepted
	-- trade, because the other side of it is silent data loss.
	--
	-- `materialised_at` is deliberately NOT part of this guard. Those bytes are a
	-- copy of what the server holds, so evicting them along with the item is
	-- exactly right — and suppressing on them instead would leave every file the
	-- user ever opened undeletable from a replica's point of view.
	AND old.pending_upload_at IS NULL
	-- Duplicate stable ids are legal (`stable_uuid` is deliberately not UNIQUE),
	-- so an id is only retired once the last row carrying it is gone. Two
	-- type-scoped branches rather than one COALESCE over both id columns, which
	-- could use neither index.
	AND (
		(
			old.type = 2
			AND NOT EXISTS (
				SELECT 1 FROM items
				WHERE items.stable_uuid = old.stable_uuid
			)
		)
		OR (
			old.type = 1
			AND NOT EXISTS (
				SELECT 1 FROM items
				WHERE items.uuid = old.uuid
			)
		)
	)
BEGIN
	-- Counter first, then read it back: the tombstone has to carry a sequence
	-- strictly above every anchor already handed out, or a replica holding one
	-- never learns about the delete.
	UPDATE change_meta SET counter = counter + 1;
	INSERT OR REPLACE INTO tombstones (kind, item_id, seq) VALUES (
		old.type,
		CASE old.type WHEN 2 THEN old.stable_uuid ELSE old.uuid END,
		(SELECT counter FROM change_meta)
	);
END;

-- The second retirement point, and the one no DELETE ever reaches: the
-- `(parent, name)` tier of `upsert_item.sql` resolves an incoming record onto
-- an existing row and overwrites BOTH id columns in place, so the id that row
-- used to answer to is retired while the row itself lives on.
--
-- A file's `uuid` being re-minted while its `stable_uuid` stays put is NOT a
-- retirement — that is the same provider identity with new content, which is
-- exactly what a versioning-disabled edit looks like on the way in.
CREATE TRIGGER tombstone_on_id_retirement
AFTER UPDATE OF uuid, stable_uuid ON items
FOR EACH ROW
WHEN
	-- Same data-loss guard as the delete trigger, for the same reason and with
	-- the same exclusion: bytes that have not reached the server exist nowhere
	-- else, while a materialised copy of the server's own content does not make
	-- an id any less retired.
	old.pending_upload_at IS NULL
	AND (
		(
			old.type = 2
			AND old.stable_uuid IS NOT new.stable_uuid
			AND NOT EXISTS (
				SELECT 1 FROM items
				WHERE items.stable_uuid = old.stable_uuid
			)
		)
		OR (
			old.type = 1
			AND old.uuid IS NOT new.uuid
			AND NOT EXISTS (
				SELECT 1 FROM items
				WHERE items.uuid = old.uuid
			)
		)
	)
BEGIN
	UPDATE change_meta SET counter = counter + 1;
	INSERT OR REPLACE INTO tombstones (kind, item_id, seq) VALUES (
		old.type,
		CASE old.type WHEN 2 THEN old.stable_uuid ELSE old.uuid END,
		(SELECT counter FROM change_meta)
	);
END;

-- The per-type tables carry the rest of what a replica renders, so they bump
-- the item they hang off. `region`, `bucket` and `timestamp` are excluded
-- because nothing outside this crate reads them, and `file_key` because it is
-- not user-visible at all.
--
-- INSERT and DELETE are covered as well as UPDATE, because the meta upsert is
-- conditional (`sql/file.rs`, `sql/dir.rs`): metadata becoming decodable is a
-- fresh INSERT into `*_meta` and the reverse is a bare DELETE, so an
-- UPDATE-only trigger would miss a name appearing or vanishing entirely.
-- `files` / `dirs` need no delete trigger of their own: they die only with
-- their item, which `tombstone_on_delete` already answers for.

CREATE TRIGGER bump_seq_files_insert
AFTER INSERT ON files
FOR EACH ROW
BEGIN
	UPDATE change_meta SET counter = counter + 1;
	UPDATE items SET change_seq = (SELECT counter FROM change_meta)
	WHERE id = new.id;
END;

CREATE TRIGGER bump_seq_files_update
AFTER UPDATE ON files
FOR EACH ROW
WHEN
	old.size IS NOT new.size
	OR old.chunks IS NOT new.chunks
	OR old.favorite_rank IS NOT new.favorite_rank
	OR old.metadata_state IS NOT new.metadata_state
BEGIN
	UPDATE change_meta SET counter = counter + 1;
	UPDATE items SET change_seq = (SELECT counter FROM change_meta)
	WHERE id = new.id;
END;

CREATE TRIGGER bump_seq_files_meta_insert
AFTER INSERT ON files_meta
FOR EACH ROW
BEGIN
	UPDATE change_meta SET counter = counter + 1;
	UPDATE items SET change_seq = (SELECT counter FROM change_meta)
	WHERE id = new.id;
END;

CREATE TRIGGER bump_seq_files_meta_update
AFTER UPDATE ON files_meta
FOR EACH ROW
WHEN
	old.name IS NOT new.name
	OR old.mime IS NOT new.mime
	OR old.modified IS NOT new.modified
	OR old.hash IS NOT new.hash
	OR old.created IS NOT new.created
BEGIN
	UPDATE change_meta SET counter = counter + 1;
	UPDATE items SET change_seq = (SELECT counter FROM change_meta)
	WHERE id = new.id;
END;

-- The guard is what tells the two ways this row dies apart. Metadata that
-- stopped being decodable is a change to a live item and bumps it; a meta row
-- swept away by the cascade from its own deleted item is not a change to
-- anything — the item's tombstone is the whole story, and bumping here would
-- raise the counter above any sequence ever stamped on a row.
CREATE TRIGGER bump_seq_files_meta_delete
AFTER DELETE ON files_meta
FOR EACH ROW
WHEN
	EXISTS (
		SELECT 1 FROM items
		WHERE items.id = old.id
	)
BEGIN
	UPDATE change_meta SET counter = counter + 1;
	UPDATE items SET change_seq = (SELECT counter FROM change_meta)
	WHERE id = old.id;
END;

-- The dir guards read the item's type because the root is a `dirs` row too
-- (`insert_root`), and the root is never part of the feed. A missing item row
-- yields NULL, which is not TRUE either — the same cascade case the
-- `files_meta` delete guard covers.
CREATE TRIGGER bump_seq_dirs_insert
AFTER INSERT ON dirs
FOR EACH ROW
WHEN (
	SELECT items.type FROM items
	WHERE items.id = new.id
) != 0
BEGIN
	UPDATE change_meta SET counter = counter + 1;
	UPDATE items SET change_seq = (SELECT counter FROM change_meta)
	WHERE id = new.id;
END;

CREATE TRIGGER bump_seq_dirs_update
AFTER UPDATE ON dirs
FOR EACH ROW
WHEN
	(
		SELECT items.type FROM items
		WHERE items.id = new.id
	) != 0
	AND (
		old.favorite_rank IS NOT new.favorite_rank
		OR old.color IS NOT new.color
		OR old.metadata_state IS NOT new.metadata_state
	)
BEGIN
	UPDATE change_meta SET counter = counter + 1;
	UPDATE items SET change_seq = (SELECT counter FROM change_meta)
	WHERE id = new.id;
END;

CREATE TRIGGER bump_seq_dirs_meta_insert
AFTER INSERT ON dirs_meta
FOR EACH ROW
WHEN (
	SELECT items.type FROM items
	WHERE items.id = new.id
) != 0
BEGIN
	UPDATE change_meta SET counter = counter + 1;
	UPDATE items SET change_seq = (SELECT counter FROM change_meta)
	WHERE id = new.id;
END;

CREATE TRIGGER bump_seq_dirs_meta_update
AFTER UPDATE ON dirs_meta
FOR EACH ROW
WHEN
	(
		SELECT items.type FROM items
		WHERE items.id = new.id
	) != 0
	AND (old.name IS NOT new.name OR old.created IS NOT new.created)
BEGIN
	UPDATE change_meta SET counter = counter + 1;
	UPDATE items SET change_seq = (SELECT counter FROM change_meta)
	WHERE id = new.id;
END;

CREATE TRIGGER bump_seq_dirs_meta_delete
AFTER DELETE ON dirs_meta
FOR EACH ROW
WHEN (
	SELECT items.type FROM items
	WHERE items.id = old.id
) != 0
BEGIN
	UPDATE change_meta SET counter = counter + 1;
	UPDATE items SET change_seq = (SELECT counter FROM change_meta)
	WHERE id = old.id;
END;

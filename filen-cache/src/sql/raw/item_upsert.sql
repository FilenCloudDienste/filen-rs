INSERT INTO items (id, uuid, parent, type, root_id, content_hash)
VALUES (
	-- Existing id if this uuid is already cached, else NULL. NULL →
	-- SQLite assigns a fresh rowid (INTEGER PRIMARY KEY); a non-NULL
	-- id collides on the PK below and updates the row in place. This
	-- keeps the id STABLE across re-upserts — a bare INSERT OR REPLACE
	-- would assign a new rowid and break the files/dirs FK references.
	(
		SELECT id FROM items
		WHERE uuid = ?1
	),
	?1, -- uuid
	?2, -- parent
	?3, -- type
	-- root_id is always the single account-root row (the cache has
	-- exactly one `roots` row), so look it up directly instead of
	-- re-deriving it via a join on every upsert.
	(
		SELECT id FROM roots
		ORDER BY id
		LIMIT 1
	),
	?4 -- content_hash (change-detection fingerprint; NULL for the root)
)
-- Only the PK (id) conflict target is declared, never the `uuid` UNIQUE
-- constraint — they are mutually exclusive here: if the uuid already
-- exists the subquery above returns ITS id, so the PK conflict fires
-- (and the uuid is unchanged, so its UNIQUE constraint is never
-- reached); if the uuid is new, neither conflict fires.
ON CONFLICT (id) DO UPDATE SET
	parent = excluded.parent,
	type = excluded.type,
	root_id = excluded.root_id,
	content_hash = excluded.content_hash
RETURNING id;

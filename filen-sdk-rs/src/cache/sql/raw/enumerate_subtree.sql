-- Whole-subtree dump for the sync engine's remote snapshot: every descendant
-- item under one sync root with the FULL payload columns. Unlike the search
-- window queries this has no needle/order/window/path — it returns the entire
-- subtree so the engine can reconcile it against the local tree + baseline.
--
-- COLUMN ORDER IS A CONTRACT: it must match `search::hydrate::row_to_result`
-- (indices 0-21), the same hydration the search windows use. The recursive
-- `subtree` CTE mirrors diff_subtree_absent.sql / search_window_subtree.sql
-- (UNION dedups, so a corrupt parent cycle terminates); the anchor itself is
-- never returned (the engine syncs the root's CONTENTS, not the root node).
-- ?1 = anchor (sync root) uuid.
WITH RECURSIVE
subtree (uuid) AS (
	SELECT uuid FROM items
	WHERE parent = ?1
	UNION
	SELECT i.uuid
	FROM items AS i
	INNER JOIN subtree AS s ON i.parent = s.uuid
)

SELECT
	i.uuid,
	i.parent,
	i.type,
	f.chunks_size,
	f.chunks,
	f.favorite AS file_favorite,
	f.region,
	f.bucket,
	f.timestamp AS file_timestamp,
	f.size,
	f.name AS file_name,
	f.mime,
	f.file_key,
	f.file_key_version,
	f.created AS file_created,
	f.modified,
	f.hash,
	d.favorite AS dir_favorite,
	d.color,
	d.timestamp AS dir_timestamp,
	d.name AS dir_name,
	d.created AS dir_created
FROM items AS i
INNER JOIN subtree AS s ON i.uuid = s.uuid
LEFT JOIN files AS f ON i.id = f.id
LEFT JOIN dirs AS d ON i.id = d.id;

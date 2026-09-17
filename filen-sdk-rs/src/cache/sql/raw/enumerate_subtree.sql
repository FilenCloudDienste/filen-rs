-- Whole-subtree dump for the sync engine's remote snapshot: every descendant
-- item under one sync root, in the SEVEN-odd columns its view actually reads
-- (`plan::place_remote_items`). The full payload of an item is read back by
-- uuid, for the handful a pass acts on, through hydrate_by_uuids.sql — a pass
-- would otherwise parse a file key, decode a hash and allocate four strings per
-- row of the whole tree to answer a few dozen questions.
--
-- COLUMN ORDER IS THE CONTRACT here (not the names, as in the full projection):
-- `enumerate::slim_item` reads by index, and this statement has one reader.
-- The recursive `subtree` CTE mirrors diff_subtree_absent.sql /
-- search_window_subtree.sql (UNION dedups, so a corrupt parent cycle
-- terminates); the anchor itself is never returned (the engine syncs the root's
-- CONTENTS, not the root node).
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
	f.stable_uuid,
	f.hash,
	-- The linter wants every plain column before the expressions, so the name
	-- and the two defaulted values come last; `slim_item` reads by index.
	coalesce(f.name, d.name) AS name,
	coalesce(f.size, 0) AS size,
	-- What the view calls `modified_millis`: a file's own modified stamp, a
	-- directory's creation stamp (0 where it has none), both already millis.
	coalesce(f.modified, d.created, 0) AS modified
FROM items AS i
INNER JOIN subtree AS s ON i.uuid = s.uuid
LEFT JOIN files AS f ON i.id = f.id
LEFT JOIN dirs AS d ON i.id = d.id
-- A row mid-supersede carries the PREDECESSOR's content under the successor's
-- uuid, so handing it to the engine would schedule an undownloadable pull (see
-- files.superseded). Dirs have no such row, hence the LEFT JOIN's NULL passing.
WHERE coalesce(f.superseded, FALSE) = FALSE;

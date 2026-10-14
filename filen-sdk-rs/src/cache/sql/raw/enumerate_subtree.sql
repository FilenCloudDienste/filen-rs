-- Whole-subtree dump for the sync engine's remote snapshot: every descendant
-- item under one sync root, in the SEVEN-odd columns its view actually reads
-- (`plan::place_remote_items`). The full payload of an item is read back by
-- uuid, for the handful a pass acts on, through hydrate_by_uuids.sql — a pass
-- would otherwise parse a file key, decode a hash and allocate four strings per
-- row of the whole tree to answer a few dozen questions.
--
-- COLUMN ORDER IS THE CONTRACT here (not the names, as in the full projection):
-- `enumerate::slim_item` reads by index, and this statement has one reader.
-- The recursive `subtree` CTE carries each item's own columns, so the final
-- SELECT reads them from it instead of looking every row up again by uuid,
-- and it joins with UNION ALL, so SQLite keeps no b-tree of every row seen.
-- Each item still comes out exactly once: an item has one parent and
-- items.uuid is unique, so it is emitted once per emission of its parent, and
-- the only parent cycle reachable from the anchor runs through the anchor,
-- which is never descended into. An anchor a corrupt cache lists under its own
-- child is therefore returned once, as that child's child, as it was with
-- UNION; a cycle elsewhere is never reached. diff_subtree_absent.sql and
-- search_window_subtree.sql still dedup with UNION. The anchor's own row is
-- otherwise never returned (the engine syncs the root's CONTENTS, not the
-- root node).
-- ?1 = anchor (sync root) uuid.
WITH RECURSIVE
subtree (id, uuid, parent, type) AS (
	SELECT
		id,
		uuid,
		parent,
		type
	FROM items
	WHERE parent = ?1
	UNION ALL
	SELECT
		i.id,
		i.uuid,
		i.parent,
		i.type
	FROM items AS i
	INNER JOIN subtree AS s ON i.parent = s.uuid
	WHERE s.uuid != ?1
)

SELECT
	s.uuid,
	s.parent,
	s.type,
	f.stable_uuid,
	f.hash,
	-- The linter wants every plain column before the expressions, so the name
	-- and the two defaulted values come last; `slim_item` reads by index.
	coalesce(f.name, d.name) AS name,
	coalesce(f.size, 0) AS size,
	-- What the view calls `modified_millis`: a file's own modified stamp, a
	-- directory's creation stamp (0 where it has none), both already millis.
	coalesce(f.modified, d.created, 0) AS modified
FROM subtree AS s
LEFT JOIN files AS f ON s.id = f.id
LEFT JOIN dirs AS d ON s.id = d.id
-- A row mid-supersede carries the PREDECESSOR's content under the successor's
-- uuid, so handing it to the engine would schedule an undownloadable pull (see
-- files.superseded). Dirs have no such row, hence the LEFT JOIN's NULL passing.
WHERE coalesce(f.superseded, FALSE) = FALSE;

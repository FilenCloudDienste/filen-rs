-- Resync delete detection: cached items under the sync root that are
-- ABSENT from the fresh listing → they were deleted remotely. The
-- recursive CTE walks the cached subtree from the root's direct children
-- downward; UNION (not UNION ALL) dedups, so a corrupt parent cycle
-- terminates instead of looping. The `uuid != ?1` guard keeps the
-- sync-root node itself from ever being emitted as a deletion even if a
-- stray row points at it. `?1` is the sync-root uuid.
WITH RECURSIVE subtree (uuid, type) AS (
	SELECT
		uuid,
		type
	FROM items
	WHERE parent = ?1
	UNION
	SELECT
		i.uuid,
		i.type
	FROM items AS i
	INNER JOIN subtree AS s ON i.parent = s.uuid
)

SELECT
	s.uuid,
	s.type
FROM subtree AS s
WHERE
	s.uuid != ?1
	AND s.uuid NOT IN (SELECT d.uuid FROM diff_incoming AS d);

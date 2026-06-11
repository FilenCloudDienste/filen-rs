-- Total match count for a subtree-scoped search; the WHERE clause
-- mirrors search_window_subtree.sql exactly. ?1 = anchor uuid,
-- ?2 = type filter (0/1/2), ?3 = needle, ?4 = case-insensitive flag.
WITH RECURSIVE subtree (uuid) AS (
	SELECT uuid FROM items
	WHERE parent = ?1
	UNION
	SELECT i.uuid
	FROM items AS i
	INNER JOIN subtree AS s ON i.parent = s.uuid
)

SELECT count(*)
FROM items AS i
INNER JOIN subtree AS s ON i.uuid = s.uuid
LEFT JOIN files AS f ON i.id = f.id
LEFT JOIN dirs AS d ON i.id = d.id
WHERE
	(?2 = 0 OR i.type = ?2)
	AND filen_name_matches(coalesce(f.name, d.name), ?3, ?4);

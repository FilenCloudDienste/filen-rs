-- Total match count for a non-recursive (direct children) search; the
-- WHERE clause mirrors search_window_children.sql exactly. ?1 = parent
-- uuid, ?2 = type filter (0/1/2), ?3 = needle, ?4 = case-insensitive
-- flag.
SELECT count(*)
FROM items AS i
LEFT JOIN files AS f ON i.id = f.id
LEFT JOIN dirs AS d ON i.id = d.id
WHERE
	i.parent = ?1
	AND (?2 = 0 OR i.type = ?2)
	AND filen_name_matches(coalesce(f.name, d.name), ?3, ?4);

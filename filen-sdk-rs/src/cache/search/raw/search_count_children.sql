-- Total match count for a non-recursive (direct children) search; the
-- WHERE clause mirrors search_window_children.sql exactly. ?1 = parent
-- uuid, ?2 = type filter (0/1/2), ?3 = needle, ?4 = case-insensitive
-- flag.
SELECT count(*) AS count
FROM items AS i
LEFT JOIN files AS f ON i.id = f.id
LEFT JOIN dirs AS d ON i.id = d.id
WHERE
	i.parent = ?1
	AND (?2 = 0 OR i.type = ?2)
	AND filen_name_matches(coalesce(f.name, d.name), ?3, ?4)
	-- A row mid-supersede carries the PREDECESSOR's content under the
	-- successor's uuid, so handing it out would hand out an undownloadable
	-- file (see files.superseded). Dirs have no such row, hence the LEFT
	-- JOIN's NULL passing.
	AND coalesce(f.superseded, FALSE) = FALSE;

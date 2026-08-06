-- Total match count for an account-root-scoped search; the WHERE clause
-- mirrors search_window_account.sql exactly. ?1 = type filter (0/1/2),
-- ?2 = needle, ?3 = case-insensitive flag.
SELECT count(*) AS count
FROM items AS i
LEFT JOIN files AS f ON i.id = f.id
LEFT JOIN dirs AS d ON i.id = d.id
WHERE
	i.type != 0
	AND (?1 = 0 OR i.type = ?1)
	AND filen_name_matches(coalesce(f.name, d.name), ?2, ?3)
	-- A row mid-supersede carries the PREDECESSOR's content under the
	-- successor's uuid, so handing it out would hand out an undownloadable
	-- file (see files.superseded). Dirs have no such row, hence the LEFT
	-- JOIN's NULL passing.
	AND coalesce(f.superseded, FALSE) = FALSE;

-- Total match count for an account-root-scoped search; the WHERE clause
-- mirrors search_window_account.sql exactly. ?1 = type filter (0/1/2),
-- ?2 = needle, ?3 = case-insensitive flag.
SELECT count(*)
FROM items AS i
LEFT JOIN files AS f ON i.id = f.id
LEFT JOIN dirs AS d ON i.id = d.id
WHERE
	i.type != 0
	AND (?1 = 0 OR i.type = ?1)
	AND filen_name_matches(coalesce(f.name, d.name), ?2, ?3);

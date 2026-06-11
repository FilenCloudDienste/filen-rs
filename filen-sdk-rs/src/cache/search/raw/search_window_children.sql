-- One window of a non-recursive search: the DIRECT children of one dir
-- (a live, sorted directory listing). See search_window_account.sql for
-- the matcher/ordering notes. ?1 = parent uuid, ?2 = type filter
-- (0/1/2), ?3 = needle, ?4 = case-insensitive flag, ?5 = limit,
-- ?6 = offset.
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
LEFT JOIN files AS f ON i.id = f.id
LEFT JOIN dirs AS d ON i.id = d.id
WHERE
	i.parent = ?1
	AND (?2 = 0 OR i.type = ?2)
	AND filen_name_matches(coalesce(f.name, d.name), ?3, ?4)
-- lower() = the same ASCII case-fold ordering as COLLATE NOCASE
-- (which sqlfluff cannot parse here); non-ASCII names order by their
-- (NFC-assumed) bytes.
ORDER BY i.type, lower(coalesce(f.name, d.name)), i.uuid
LIMIT ?5 OFFSET ?6;

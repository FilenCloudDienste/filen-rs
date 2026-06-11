-- One window of a SUBTREE-scoped search (anchored below one dir). The
-- recursive CTE mirrors diff_subtree_absent.sql (UNION dedups, so a
-- corrupt parent cycle terminates); the anchor itself is never returned.
-- See search_window_account.sql for the matcher/ordering notes.
-- ?1 = anchor uuid, ?2 = type filter (0/1/2), ?3 = needle,
-- ?4 = case-insensitive flag, ?5 = limit, ?6 = offset.
WITH RECURSIVE subtree (uuid) AS (
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
LEFT JOIN dirs AS d ON i.id = d.id
WHERE
	(?2 = 0 OR i.type = ?2)
	AND filen_name_matches(coalesce(f.name, d.name), ?3, ?4)
-- lower() = the same ASCII case-fold ordering as COLLATE NOCASE
-- (which sqlfluff cannot parse here); non-ASCII names order by their
-- (NFC-assumed) bytes.
ORDER BY i.type, lower(coalesce(f.name, d.name)), i.uuid
LIMIT ?5 OFFSET ?6;

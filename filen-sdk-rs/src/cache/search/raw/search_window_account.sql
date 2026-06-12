-- One window of an ACCOUNT-ROOT-scoped search: every cached item belongs
-- (no scope clause needed), filtered, ordered, and hydrated in one
-- statement. `filen_name_matches` is the engine-registered Rust matcher
-- (Unicode case folding; SQLite LIKE/lower are ASCII-only) — an empty
-- needle matches everything. Ordering: dirs first (type 1 < 2), then
-- name (NOCASE), then uuid as the deterministic pagination tiebreaker.
-- ?1 = type filter (0 = all, 1 = dirs, 2 = files), ?2 = needle,
-- ?3 = case-insensitive flag, ?4 = limit, ?5 = offset.
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
	d.created AS dir_created,
	-- the pre-LIMIT match total, piggybacked on every row so one scan
	-- serves both the window and the count (see hydrate::window_and_count)
	count(*) OVER () AS total
FROM items AS i
LEFT JOIN files AS f ON i.id = f.id
LEFT JOIN dirs AS d ON i.id = d.id
WHERE
	i.type != 0
	AND (?1 = 0 OR i.type = ?1)
	AND filen_name_matches(coalesce(f.name, d.name), ?2, ?3)
-- lower() = the same ASCII case-fold ordering as COLLATE NOCASE
-- (which sqlfluff cannot parse here); non-ASCII names order by their
-- (NFC-assumed) bytes.
ORDER BY i.type, lower(coalesce(f.name, d.name)), i.uuid
LIMIT ?4 OFFSET ?5;

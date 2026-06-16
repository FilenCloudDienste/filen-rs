-- One window of an ACCOUNT-ROOT-scoped search: every cached item belongs
-- (no scope clause needed), filtered, ordered, hydrated, and tagged with its
-- PARENT PATH in one statement. `filen_name_matches` is the
-- engine-registered Rust matcher (Unicode case folding; SQLite LIKE/lower are
-- ASCII-only) — an empty needle matches everything. Ordering: dirs first
-- (type 1 < 2), then name (NOCASE), then uuid as the deterministic pagination
-- tiebreaker.
--
-- The window is materialized ONCE as `page` (count(*) OVER () still rides
-- every row), then `climb` walks each page row's parent chain UP to the
-- account root, prepending each ancestor dir's name, so the path costs W·D
-- index point-lookups — independent of drive size. Intermediate path
-- components are always dirs, so the climb reads dirs.name only; the account
-- root (type 0, no dirs row) is the stop sentinel and is never read. A direct
-- child of the root, a broken chain, or a corrupt cycle (bounded by the depth
-- cap) all fall through the terminal LEFT JOIN miss to an empty parent path.
-- ?1 = account-root uuid (climb stop-anchor), ?2 = type filter (0 = all,
-- 1 = dirs, 2 = files), ?3 = needle, ?4 = case-insensitive flag, ?5 = limit,
-- ?6 = offset.
WITH RECURSIVE
page AS (
	SELECT
		i.id,
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
		count(*) OVER () AS total
	FROM items AS i
	LEFT JOIN files AS f ON i.id = f.id
	LEFT JOIN dirs AS d ON i.id = d.id
	WHERE
		i.type != 0
		AND (?2 = 0 OR i.type = ?2)
		AND filen_name_matches(coalesce(f.name, d.name), ?3, ?4)
	-- lower() = the same ASCII case-fold ordering as COLLATE NOCASE
	-- (which sqlfluff cannot parse here); non-ASCII names order by their
	-- (NFC-assumed) bytes.
	ORDER BY i.type, lower(coalesce(f.name, d.name)), i.uuid
	LIMIT ?5 OFFSET ?6
),

climb AS (
	-- Seed: each page row's direct parent dir, unless that parent IS the
	-- anchor (a direct child of the root keeps the '' parent path via the
	-- terminal LEFT JOIN miss below).
	SELECT
		p.id AS page_id,
		par.parent AS cur_parent,
		pd.name AS loc,
		0 AS depth
	FROM page AS p
	INNER JOIN items AS par ON p.parent = par.uuid
	INNER JOIN dirs AS pd ON par.id = pd.id
	WHERE p.parent != ?1
	UNION ALL
	-- Step: prepend the next ancestor dir's name; stop before the anchor.
	-- UNION ALL has no built-in dedup, so the depth cap is the SOLE cycle
	-- guard — a corrupt parent cycle terminates here instead of spinning.
	SELECT
		c.page_id,
		anc.parent AS cur_parent,
		ad.name || '/' || c.loc AS loc,
		c.depth + 1 AS depth
	FROM climb AS c
	INNER JOIN items AS anc ON c.cur_parent = anc.uuid
	INNER JOIN dirs AS ad ON anc.id = ad.id
	WHERE c.cur_parent != ?1 AND c.depth < 64
)

SELECT
	p.uuid,
	p.parent,
	p.type,
	p.chunks_size,
	p.chunks,
	p.file_favorite,
	p.region,
	p.bucket,
	p.file_timestamp,
	p.size,
	p.file_name,
	p.mime,
	p.file_key,
	p.file_key_version,
	p.file_created,
	p.modified,
	p.hash,
	p.dir_favorite,
	p.color,
	p.dir_timestamp,
	p.dir_name,
	p.dir_created,
	p.total,
	coalesce(term.loc, '') AS parent_path
FROM page AS p
LEFT JOIN climb AS term ON p.id = term.page_id AND term.cur_parent = ?1
ORDER BY p.type, lower(coalesce(p.file_name, p.dir_name)), p.uuid;

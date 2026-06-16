-- One window of a SUBTREE-scoped search (anchored below one dir), with each
-- result's PARENT PATH RELATIVE TO THE ANCHOR. The recursive `subtree` CTE
-- mirrors diff_subtree_absent.sql (UNION dedups, so a corrupt parent cycle
-- terminates); the anchor itself is never returned. See
-- search_window_account.sql for the matcher/ordering notes and the `climb`
-- path-build (here it stops at the anchor, so the path is anchor-relative;
-- the anchor's own direct children get '').
-- ?1 = anchor uuid (reused as the climb stop-anchor), ?2 = type filter
-- (0/1/2), ?3 = needle, ?4 = case-insensitive flag, ?5 = limit, ?6 = offset.
WITH RECURSIVE
subtree (uuid) AS (
	SELECT uuid FROM items
	WHERE parent = ?1
	UNION
	SELECT i.uuid
	FROM items AS i
	INNER JOIN subtree AS s ON i.parent = s.uuid
),

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
	INNER JOIN subtree AS s ON i.uuid = s.uuid
	LEFT JOIN files AS f ON i.id = f.id
	LEFT JOIN dirs AS d ON i.id = d.id
	WHERE
		(?2 = 0 OR i.type = ?2)
		AND filen_name_matches(coalesce(f.name, d.name), ?3, ?4)
	ORDER BY i.type, lower(coalesce(f.name, d.name)), i.uuid
	LIMIT ?5 OFFSET ?6
),

climb AS (
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

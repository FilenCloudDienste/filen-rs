SELECT
	id,
	uuid,
	parent,
	trashed,
	local_data,
	type
FROM items
WHERE stable_uuid = ?
-- duplicate stables are reachable via same-account uuid-reuse abuse; prefer
-- the live row, then the oldest, deterministically
ORDER BY trashed ASC, id ASC
LIMIT 1;

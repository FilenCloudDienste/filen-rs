SELECT
	created,
	favorite_rank,
	color,
	last_listed
FROM dirs
WHERE id = ? LIMIT 1;

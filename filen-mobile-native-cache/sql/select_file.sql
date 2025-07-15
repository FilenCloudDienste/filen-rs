SELECT
	mime,
	file_key,
	created,
	modified,
	size,
	chunks,
	favorite_rank,
	region,
	bucket,
	hash,
	version
FROM files
WHERE id = ? LIMIT 1;

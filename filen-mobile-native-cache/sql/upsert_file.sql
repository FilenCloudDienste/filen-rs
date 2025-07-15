INSERT INTO files (
	id,
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
) VALUES (
	?,
	?,
	?,
	?,
	?,
	?,
	?,
	?,
	?,
	?,
	?,
	?
) ON CONFLICT (id) DO UPDATE SET
mime = excluded.mime,
file_key = excluded.file_key,
created = excluded.created,
modified = excluded.modified,
size = excluded.size,
chunks = excluded.chunks,
-- we use the remote favorite IF favorite is being unset
-- OR if the local favorite wasn't set
favorite_rank
= CASE
	WHEN
		files.favorite_rank = 0 OR excluded.favorite_rank = 0
		THEN excluded.favorite_rank
	ELSE files.favorite_rank
END,
region = excluded.region,
bucket = excluded.bucket,
hash = excluded.hash,
version = excluded.version
RETURNING favorite_rank;

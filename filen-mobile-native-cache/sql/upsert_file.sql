INSERT INTO files (
	id,
	size,
	chunks,
	favorite_rank,
	region,
	bucket,
	metadata_state,
	raw_metadata
) VALUES (
	?,
	?,
	?,
	?,
	?,
	?,
	?,
	?
) ON CONFLICT (id) DO UPDATE SET
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
metadata_state = excluded.metadata_state,
raw_metadata = excluded.raw_metadata
RETURNING favorite_rank;

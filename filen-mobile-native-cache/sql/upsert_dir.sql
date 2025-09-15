INSERT INTO dirs (
	id,
	favorite_rank,
	color,
	timestamp,
	metadata_state,
	raw_metadata
) VALUES (
	?,
	?,
	?,
	?,
	?,
	?
) ON CONFLICT (id) DO UPDATE SET
-- we use the remote favorite IF favorite is being unset
-- OR if the local favorite wasn't set
favorite_rank
= CASE
	WHEN
		dirs.favorite_rank = 0 OR excluded.favorite_rank = 0
		THEN excluded.favorite_rank
	ELSE dirs.favorite_rank
END,
color = excluded.color,
timestamp = excluded.timestamp,
metadata_state = excluded.metadata_state,
raw_metadata = excluded.raw_metadata
RETURNING last_listed, favorite_rank;

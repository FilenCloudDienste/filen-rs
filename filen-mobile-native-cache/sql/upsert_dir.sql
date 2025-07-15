INSERT INTO dirs (
	id,
	created,
	favorite_rank,
	color
) VALUES (
	?,
	?,
	?,
	?
) ON CONFLICT (id) DO UPDATE SET
created = excluded.created,
-- we use the remote favorite IF favorite is being unset
-- OR if the local favorite wasn't set
favorite_rank
= CASE
	WHEN
		dirs.favorite_rank = 0 OR excluded.favorite_rank = 0
		THEN excluded.favorite_rank
	ELSE dirs.favorite_rank
END,
color = excluded.color
RETURNING last_listed, favorite_rank;

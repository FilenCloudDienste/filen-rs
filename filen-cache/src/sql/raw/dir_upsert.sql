INSERT INTO dirs (
	id,
	favorite,
	color,
	timestamp,
	name,
	created
) VALUES (
	?,
	?,
	?,
	?,
	?,
	?
) ON CONFLICT (id) DO UPDATE SET
	favorite = excluded.favorite,
	color = excluded.color,
	timestamp = excluded.timestamp,
	name = excluded.name,
	created = excluded.created;

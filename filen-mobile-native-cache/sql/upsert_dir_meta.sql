INSERT INTO dirs_meta (
	id,
	name,
	created
) VALUES (
	?,
	?,
	?
) ON CONFLICT (id) DO UPDATE SET
name = excluded.name,
created = excluded.created;

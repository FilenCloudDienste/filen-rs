INSERT INTO files_meta (
	id,
	name,
	mime,
	file_key,
	file_key_version,
	created,
	modified,
	hash
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
name = excluded.name,
mime = excluded.mime,
file_key = excluded.file_key,
file_key_version = excluded.file_key_version,
created = excluded.created,
modified = excluded.modified,
hash = excluded.hash;

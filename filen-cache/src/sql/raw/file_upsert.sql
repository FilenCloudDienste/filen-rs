INSERT INTO files (
	id,
	chunks_size,
	chunks,
	favorite,
	region,
	bucket,
	timestamp,

	size,
	name,
	mime,
	file_key,
	file_key_version,
	created,
	modified,
	hash
) VALUES (
	?, -- id
	?, -- chunks_size
	?, -- chunks
	?, -- favorite
	?, -- region
	?, -- bucket
	?, -- timestamp

	?, -- size
	?, -- name
	?, -- mime
	?, -- file_key
	?, -- file_key_version
	?, -- created
	?, -- modified
	? -- hash
) ON CONFLICT (id) DO UPDATE SET
	chunks_size = excluded.chunks_size,
	chunks = excluded.chunks,
	favorite = excluded.favorite,
	region = excluded.region,
	bucket = excluded.bucket,
	timestamp = excluded.timestamp,

	size = excluded.size,
	name = excluded.name,
	mime = excluded.mime,
	file_key = excluded.file_key,
	file_key_version = excluded.file_key_version,
	created = excluded.created,
	modified = excluded.modified,
	hash = excluded.hash;

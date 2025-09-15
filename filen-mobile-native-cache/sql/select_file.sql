SELECT
	files.size,
	files.chunks,
	files.favorite_rank,
	files.region,
	files.bucket,
	files.timestamp,
	files.metadata_state,
	files.raw_metadata,
	files_meta.name,
	files_meta.mime,
	files_meta.file_key,
	files_meta.file_key_version,
	files_meta.created,
	files_meta.modified,
	files_meta.hash
FROM files LEFT JOIN files_meta ON files.id = files_meta.id
WHERE files.id = ? LIMIT 1;

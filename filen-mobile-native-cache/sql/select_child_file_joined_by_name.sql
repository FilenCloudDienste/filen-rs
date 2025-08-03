SELECT
	items.id,
	items.uuid,
	items.parent,
	items.local_data,
	files.size,
	files.chunks,
	files.favorite_rank,
	files.region,
	files.bucket,
	files.metadata_state,
	files.raw_metadata,
	files_meta.name,
	files_meta.mime,
	files_meta.file_key,
	files_meta.file_key_version,
	files_meta.created,
	files_meta.modified,
	files_meta.hash
FROM items
INNER JOIN files ON items.id = files.id
LEFT JOIN files_meta ON items.id = files_meta.id
WHERE items.parent = ? AND files.name = ? LIMIT 1;

SELECT
	items.id,
	items.uuid,
	items.parent,
	items.local_data,
	items.type,
	dirs.favorite_rank AS dir_favorite_rank,
	dirs.color,
	dirs.last_listed,
	dirs.metadata_state AS dir_metadata_state,
	dirs.raw_metadata AS dir_raw_metadata,
	dirs_meta.name AS dir_name,
	dirs_meta.created AS dir_created,
	files.size,
	files.chunks,
	files.favorite_rank AS file_favorite_rank,
	files.region,
	files.bucket,
	files.metadata_state AS file_metadata_state,
	files.raw_metadata AS file_raw_metadata,
	files_meta.name AS file_name,
	files_meta.mime,
	files_meta.file_key,
	files_meta.file_key_version,
	files_meta.created AS file_created,
	files_meta.modified,
	files_meta.hash
FROM items
LEFT JOIN dirs ON items.id = dirs.id
LEFT JOIN dirs_meta ON items.id = dirs_meta.id
LEFT JOIN files ON items.id = files.id
LEFT JOIN files_meta ON items.id = files_meta.id
WHERE items.is_recent = TRUE

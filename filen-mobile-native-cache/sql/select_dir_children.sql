SELECT
	items.id,
	items.uuid,
	items.parent,
	items.name,
	items.local_data,
	items.type,
	dirs.created AS dir_created,
	dirs.favorite_rank AS dir_favorite_rank,
	dirs.color,
	dirs.last_listed,
	files.mime,
	files.file_key,
	files.created AS file_created,
	files.modified,
	files.size,
	files.chunks,
	files.favorite_rank AS file_favorite_rank,
	files.region,
	files.bucket,
	files.hash,
	files.version
FROM items
LEFT JOIN dirs ON items.id = dirs.id
LEFT JOIN files ON items.id = files.id
WHERE items.parent = ?

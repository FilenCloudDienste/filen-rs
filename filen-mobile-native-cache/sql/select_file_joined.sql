SELECT
	items.id,
	items.uuid,
	items.parent,
	items.name,
	items.local_data,
	files.mime,
	files.file_key,
	files.created,
	files.modified,
	files.size,
	files.chunks,
	files.favorite_rank,
	files.region,
	files.bucket,
	files.hash,
	files.version
FROM items INNER JOIN files ON items.id = files.id
WHERE items.uuid = ? LIMIT 1;

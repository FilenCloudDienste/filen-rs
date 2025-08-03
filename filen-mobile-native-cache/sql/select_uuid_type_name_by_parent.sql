SELECT
	items.uuid,
	items.type,
	coalesce(files_meta.name, dirs_meta.name, items.uuid) AS name
FROM items
LEFT JOIN files_meta ON items.id = files_meta.id
LEFT JOIN dirs_meta ON items.id = dirs_meta.id
WHERE items.parent = ?;

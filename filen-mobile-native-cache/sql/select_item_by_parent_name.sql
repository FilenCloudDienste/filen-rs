SELECT
	items.id,
	items.uuid,
	items.parent,
	items.local_data,
	items.type
FROM items
LEFT JOIN dirs_meta ON items.id = dirs_meta.id
LEFT JOIN files_meta ON items.id = files_meta.id
WHERE
	items.parent = ?1
	AND (
		?2 = files_meta.name OR ?2 = dirs_meta.name
		OR ?2 = items.uuid
	)
ORDER BY
	CASE
		WHEN ?2 = files_meta.name OR ?2 = dirs_meta.name THEN 0
		WHEN ?2 = items.uuid THEN 1
	END
LIMIT 1;

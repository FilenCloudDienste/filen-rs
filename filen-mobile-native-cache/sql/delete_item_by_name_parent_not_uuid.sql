DELETE FROM items
WHERE id IN (
	SELECT items.id FROM items
	LEFT JOIN files_meta ON items.id = files_meta.id
	LEFT JOIN dirs_meta ON items.id = dirs_meta.id
	WHERE
		(?1 IS NULL OR ?1 = files_meta.name OR ?1 = dirs_meta.name)
		AND items.parent = ?2
		AND items.uuid != ?3
);

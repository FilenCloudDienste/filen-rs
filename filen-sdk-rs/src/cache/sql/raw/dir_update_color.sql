UPDATE dirs SET
	color = ?
WHERE id = (
	SELECT id FROM items
	WHERE uuid = ?
);

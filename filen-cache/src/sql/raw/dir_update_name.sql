UPDATE dirs SET
	name = ?,
	created = ?
WHERE id = (
	SELECT id FROM items
	WHERE uuid = ?
);

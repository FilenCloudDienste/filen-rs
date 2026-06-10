UPDATE files SET
	size = ?,
	name = ?,
	mime = ?,
	file_key = ?,
	file_key_version = ?,
	created = ?,
	modified = ?,
	hash = ?
WHERE id = (
	SELECT id FROM items
	WHERE uuid = ?
);

SELECT
	id,
	uuid,
	parent,
	trashed,
	local_data,
	type
FROM items
WHERE uuid = ?;

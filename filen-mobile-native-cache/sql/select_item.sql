SELECT
	id,
	uuid,
	parent,
	local_data,
	type
FROM items
WHERE uuid = ?;

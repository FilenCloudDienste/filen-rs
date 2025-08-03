SELECT
	id,
	uuid,
	parent,
	local_data,
	type
FROM items
WHERE uuid = ? LIMIT 1;

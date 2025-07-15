SELECT
	id,
	uuid,
	parent,
	name,
	local_data,
	type
FROM items
WHERE uuid = ? LIMIT 1;

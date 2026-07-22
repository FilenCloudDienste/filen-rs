SELECT
	id,
	uuid,
	stable_uuid,
	parent,
	trashed,
	local_data,
	type
FROM items
WHERE uuid = ?;

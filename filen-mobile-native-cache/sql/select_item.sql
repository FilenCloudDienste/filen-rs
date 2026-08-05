SELECT
	id,
	uuid,
	parent,
	trashed,
	local_data,
	type,
	change_seq
FROM items
WHERE uuid = ?;

SELECT
	i.uuid,
	COALESCE(f.name, d.name, i.uuid) AS name,
	i.parent,
	i.is_recent,
	i.local_data
FROM items AS i
LEFT JOIN files_meta AS f ON i.id = f.id
LEFT JOIN dirs_meta AS d ON i.id = d.id;

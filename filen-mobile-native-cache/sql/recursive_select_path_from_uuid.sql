WITH RECURSIVE
path (uuid, name, parent, level) AS (
	SELECT
		i.uuid,
		COALESCE(f.name, d.name, i.uuid) AS name,
		i.parent,
		0
	FROM items AS i
	LEFT JOIN files_meta AS f ON i.id = f.id
	LEFT JOIN dirs_meta AS d ON i.id = d.id
	WHERE i.uuid = ?
	UNION ALL
	SELECT
		i.uuid,
		COALESCE(f.name, d.name, i.uuid) AS name,
		i.parent,
		p.level + 1
	FROM items AS i
	INNER JOIN path AS p ON i.uuid = p.parent
	LEFT JOIN files_meta AS f ON i.id = f.id
	LEFT JOIN dirs_meta AS d ON i.id = d.id
),

ordered_path AS (
	SELECT name FROM path
	WHERE name != ''
	ORDER BY level DESC
)

SELECT CASE
	WHEN GROUP_CONCAT(name, '/') IS NULL THEN NULL
	ELSE GROUP_CONCAT(name, '/')
END FROM ordered_path
HAVING COUNT(*) > 0;

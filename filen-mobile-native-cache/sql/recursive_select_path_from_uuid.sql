WITH RECURSIVE 
path(uuid, name, parent, level) AS (
    SELECT uuid, name, parent, 0 FROM items WHERE uuid = ?
    UNION ALL
    SELECT i.uuid, i.name, i.parent, p.level + 1 FROM items i
    JOIN path p ON i.uuid = p.parent
),
ordered_path AS (
    SELECT name FROM path WHERE name != '' ORDER BY level DESC
)
SELECT CASE 
    WHEN GROUP_CONCAT(name, '/') IS NULL THEN NULL 
    ELSE '/' || GROUP_CONCAT(name, '/') 
END FROM ordered_path
HAVING COUNT(*) > 0;
SELECT
    items.uuid,
    items.name,
    files.mime,
    files.created,
    files.modified,
    files.size,
    files.chunks,
    files.favorited,
    files.region
FROM items INNER JOIN files ON items.id = files.id
WHERE items.parent = ?

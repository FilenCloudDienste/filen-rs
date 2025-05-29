SELECT
    items.uuid,
    items.name,
    dirs.created,
    dirs.favorited,
    dirs.color,
    dirs.last_listed
FROM items INNER JOIN dirs ON items.id = dirs.id
WHERE items.parent = ?

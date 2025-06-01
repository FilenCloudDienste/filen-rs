SELECT
    items.uuid,
    items.name,
    items.type,
    dirs.created AS dir_created,
    dirs.favorited AS dir_favorited,
    dirs.color,
    dirs.last_listed,
    files.mime,
    files.created AS file_created,
    files.modified,
    files.size,
    files.chunks,
    files.favorited AS file_favorited
FROM items
LEFT JOIN dirs ON items.id = dirs.id
LEFT JOIN files ON items.id = files.id
WHERE items.parent = ?

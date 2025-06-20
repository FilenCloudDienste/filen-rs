SELECT
    items.id,
    items.uuid,
    items.parent,
    items.name,
    items.type,
    dirs.created AS dir_created,
    dirs.favorited AS dir_favorited,
    dirs.color,
    dirs.last_listed,
    files.mime,
    files.file_key,
    files.created AS file_created,
    files.modified,
    files.size,
    files.chunks,
    files.favorited AS file_favorited,
    files.region,
    files.bucket,
    files.hash,
    files.version
FROM items
LEFT JOIN dirs ON items.id = dirs.id
LEFT JOIN files ON items.id = files.id
WHERE items.parent = ?

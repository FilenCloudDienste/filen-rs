SELECT
    items.id,
    items.uuid,
    items.parent,
    items.name,
    files.mime,
    files.file_key,
    files.created,
    files.modified,
    files.size,
    files.chunks,
    files.favorited,
    files.region,
    files.bucket,
    files.hash,
    files.version
FROM items INNER JOIN files ON items.id = files.id
WHERE items.parent = ? AND items.name = ? LIMIT 1;

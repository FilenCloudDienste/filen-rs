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
    roots.storage_used,
    roots.max_storage,
    roots.last_updated,
    dirs.last_listed AS root_last_listed
FROM items
LEFT JOIN dirs ON items.id = dirs.id
LEFT JOIN files ON items.id = files.id
LEFT JOIN roots ON items.id = roots.id
WHERE items.uuid = ?
LIMIT 1;

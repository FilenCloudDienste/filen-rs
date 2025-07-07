SELECT
    items.id,
    items.uuid,
    items.parent,
    items.name,
    items.local_data,
    items.type,
    dirs.created AS dir_created,
    dirs.favorite_rank AS dir_favorite_rank,
    dirs.color,
    dirs.last_listed,
    files.mime,
    files.file_key,
    files.created AS file_created,
    files.modified,
    files.size,
    files.chunks,
    files.favorite_rank AS file_favorite_rank,
    files.region,
    files.bucket,
    files.hash,
    files.version,
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

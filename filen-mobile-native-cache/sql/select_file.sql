SELECT
    mime,
    file_key,
    created,
    modified,
    size,
    chunks,
    favorited,
    region,
    bucket,
    hash
FROM files
WHERE id = ? LIMIT 1;

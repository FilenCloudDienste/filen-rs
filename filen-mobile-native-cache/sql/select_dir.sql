SELECT
    created,
    favorited,
    color,
    last_listed
FROM dirs
WHERE id = ? LIMIT 1;

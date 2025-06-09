SELECT
    id,
    uuid,
    parent,
    name,
    type
FROM items
WHERE uuid = ? LIMIT 1;

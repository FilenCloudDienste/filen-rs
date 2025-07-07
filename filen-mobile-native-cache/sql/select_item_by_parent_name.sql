SELECT
    id,
    uuid,
    parent,
    name,
    local_data,
    type
FROM items
WHERE parent = ? AND name = ?
LIMIT 1;

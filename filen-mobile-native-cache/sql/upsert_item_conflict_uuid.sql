INSERT INTO items (
    uuid,
    parent,
    name,
    type
) VALUES (
    ?,
    ?,
    ?,
    ?
)
ON CONFLICT (uuid) DO UPDATE SET
parent = excluded.parent,
name = excluded.name,
is_stale = FALSE
RETURNING id;

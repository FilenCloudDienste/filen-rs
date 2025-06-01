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
type = excluded.type,
is_stale = FALSE
RETURNING id;

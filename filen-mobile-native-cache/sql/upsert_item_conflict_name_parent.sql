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
ON CONFLICT (name, parent, is_stale) WHERE parent != 'trash' DO UPDATE SET
uuid = excluded.uuid,
type = excluded.type,
is_stale = FALSE
RETURNING id;

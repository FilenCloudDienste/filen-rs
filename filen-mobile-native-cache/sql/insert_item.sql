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
RETURNING id;

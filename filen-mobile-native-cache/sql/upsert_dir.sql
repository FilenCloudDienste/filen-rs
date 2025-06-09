INSERT INTO dirs (
    id,
    created,
    favorited,
    color
) VALUES (
    ?,
    ?,
    ?,
    ?
) ON CONFLICT (id) DO UPDATE SET
created = excluded.created,
favorited = excluded.favorited,
color = excluded.color
RETURNING last_listed;

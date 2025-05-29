INSERT INTO dirs (
    id,
    created,
    favorited,
    color,
    last_listed
) VALUES (
    ?,
    ?,
    ?,
    ?,
    ?
) ON CONFLICT (id) DO UPDATE SET
created = excluded.created,
favorited = excluded.favorited,
color = excluded.color;

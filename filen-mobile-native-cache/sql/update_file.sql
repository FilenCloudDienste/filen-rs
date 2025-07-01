UPDATE files SET
    mime = ?1,
    file_key = ?2,
    created = ?3,
    modified = ?4,
    size = ?5,
    chunks = ?6,
    favorite_rank = CASE
        WHEN files.favorite_rank = 0 OR ?7 = 0 THEN ?7
        ELSE files.favorite_rank
    END,
    region = ?8,
    bucket = ?9,
    hash = ?10
WHERE id = ?11
RETURNING favorite_rank;

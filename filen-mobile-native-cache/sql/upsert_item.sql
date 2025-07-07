WITH local_source AS ( -- noqa: ST05
    SELECT
        COALESCE(
            moving.id,
            existing.id
        ) AS id
    FROM (
    -- we select nothing here to make sure we always have a row
        SELECT NULL AS id
    )
    -- The ambiguous cross join sqlfluff here is actually a bug
    -- this is a left join
    LEFT JOIN ( -- noqa: AM08
        SELECT id
        FROM items
        WHERE uuid = ?1
    ) AS moving
    LEFT JOIN (
        SELECT id
        FROM items
        WHERE parent = ?2 AND name = ?3
    ) AS existing
        ON 1 = 1
)

INSERT OR REPLACE INTO items (
    id,
    uuid,
    parent,
    name,
    type
)
SELECT
    local_source.id,
    ?1 AS uuid,
    ?2 AS parent,
    ?3 AS name,
    ?4 AS type
FROM local_source
RETURNING id;

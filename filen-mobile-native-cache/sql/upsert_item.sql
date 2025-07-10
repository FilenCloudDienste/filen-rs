WITH local_source AS ( -- noqa: ST05
    SELECT
        COALESCE(
            moving.id,
            existing.id
        ) AS id,
        COALESCE(
            ?4,
            moving.local_data,
            existing.local_data
        ) AS local_data,
        moving.is_recent AS is_recent
    FROM (
    -- we select nothing here to make sure we always have a row
        SELECT
            NULL AS id,
            NULL AS local_data
    )
    -- The ambiguous cross join sqlfluff here is actually a bug
    -- this is a left join
    LEFT JOIN ( -- noqa: AM08
        SELECT
            id,
            local_data,
            is_recent
        FROM items
        WHERE uuid = ?1
    ) AS moving
    LEFT JOIN (
        SELECT
            id,
            local_data
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
    local_data,
    type,
    is_recent
)
SELECT
    local_source.id,
    ?1 AS uuid,
    ?2 AS parent,
    ?3 AS name,
    local_source.local_data,
    ?5 AS type,
    local_source.is_recent
FROM local_source
RETURNING id, local_data;

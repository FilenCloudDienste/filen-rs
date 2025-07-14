WITH RECURSIVE filtered_items AS (
    SELECT
        items.id,
        items.uuid,
        items.parent,
        items.name,
        items.local_data,
        items.type,
        dirs.created AS dir_created,
        dirs.favorite_rank AS dir_favorite_rank,
        dirs.color,
        dirs.last_listed,
        files.mime,
        files.file_key,
        files.created AS file_created,
        files.modified,
        files.size,
        files.chunks,
        files.favorite_rank AS file_favorite_rank,
        files.region,
        files.bucket,
        files.hash,
        files.version,
        CASE
            WHEN items.parent_path IS NULL THEN NULL
            ELSE items.parent_path || '/' || items.name
        END AS parent_path
    FROM items
    LEFT JOIN dirs ON items.id = dirs.id
    LEFT JOIN files ON items.id = files.id
    WHERE
        (?1 IS NULL OR lower(items.name) LIKE '%' || lower(?1) || '%')
        AND (?5 IS NULL OR items.type = ?5)
        AND (((
            ?2 IS NULL
            OR EXISTS (
                SELECT 1 FROM json_each(?2)
                WHERE files.mime LIKE json_each.value
            )
        ) AND (
            ?3 IS NULL
            OR files.size >= ?3
        ) AND (
            ?4 IS NULL
            OR files.modified >= ?4
        ))
        OR (
            ?2 IS NULL
            AND ?3 IS NULL
            AND (
                dirs.created >= ?4
                OR ?4 IS NULL
            )
        )
        )
),

path_builder (original_id, id, uuid, name, parent, level, path_components) AS (
    SELECT
        id AS original_id,
        id,
        uuid,
        name,
        parent,
        0,
        CASE WHEN name != '' THEN name END
    FROM filtered_items
    WHERE parent_path IS NULL
    UNION ALL
    SELECT
        p.original_id,
        i.id,
        i.uuid,
        i.name,
        i.parent,
        p.level + 1,
        CASE
            WHEN
                i.name != '' AND p.path_components IS NOT NULL
                THEN i.name || '/' || p.path_components
            WHEN i.name != '' AND p.path_components IS NULL THEN i.name
            ELSE p.path_components
        END
    FROM items AS i
    INNER JOIN path_builder AS p ON i.uuid = p.parent
),

computed_paths AS (
    SELECT
        original_id AS item_id,
        CASE
            WHEN max(path_components) IS NULL THEN NULL
            ELSE '/' || max(path_components)
        END AS computed_path
    FROM path_builder
    GROUP BY original_id
    HAVING max(path_components) IS NOT NULL
)

SELECT
    fi.id,
    fi.uuid,
    fi.parent,
    fi.name,
    fi.local_data,
    fi.type,
    fi.dir_created,
    fi.dir_favorite_rank,
    fi.color,
    fi.last_listed,
    fi.mime,
    fi.file_key,
    fi.file_created,
    fi.modified,
    fi.size,
    fi.chunks,
    fi.file_favorite_rank,
    fi.region,
    fi.bucket,
    fi.hash,
    fi.version,
    coalesce(fi.parent_path, cp.computed_path) AS search_path
FROM filtered_items AS fi
LEFT JOIN computed_paths AS cp ON fi.id = cp.item_id;

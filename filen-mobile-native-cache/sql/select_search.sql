WITH RECURSIVE filtered_items AS (
	SELECT
		items.id,
		items.uuid,
		items.parent,
		items.local_data,
		items.type,
		dirs.favorite_rank AS dir_favorite_rank,
		dirs.color,
		dirs.timestamp AS dir_timestamp,
		dirs.last_listed,
		dirs.metadata_state AS dir_metadata_state,
		dirs.raw_metadata AS dir_raw_metadata,
		dirs_meta.name AS dir_name,
		dirs_meta.created AS dir_created,
		files.size,
		files.chunks,
		files.favorite_rank AS file_favorite_rank,
		files.region,
		files.bucket,
		files.timestamp AS file_timestamp,
		files.metadata_state AS file_metadata_state,
		files.raw_metadata AS file_raw_metadata,
		files_meta.name AS file_name,
		files_meta.mime,
		files_meta.file_key,
		files_meta.file_key_version,
		files_meta.created AS file_created,
		files_meta.modified,
		files_meta.hash,
		CASE
			WHEN items.parent_path IS NULL THEN NULL
			ELSE
				items.parent_path
				|| '/'
				|| coalesce(files_meta.name, dirs_meta.name, items.uuid)
		END AS parent_path
	FROM items
	LEFT JOIN dirs ON items.id = dirs.id
	LEFT JOIN dirs_meta ON items.id = dirs_meta.id
	LEFT JOIN files ON items.id = files.id
	LEFT JOIN files_meta ON items.id = files_meta.id
	WHERE
		(
			?1 IS NULL
			OR (
				dirs_meta.name IS NOT NULL
				AND lower(dirs_meta.name) LIKE '%' || lower(?1) || '%'
			)
			OR (
				files_meta.name IS NOT NULL
				AND lower(files_meta.name) LIKE '%' || lower(?1) || '%'
			)
		)
		AND (?5 IS NULL OR items.type = ?5)
		AND (((
			?2 IS NULL
			OR EXISTS (
				SELECT 1 FROM json_each(?2)
				-- json_each is a thing in sqlite, sqlfluff is not aware of this
				WHERE files_meta.mime LIKE json_each.value --noqa: RF01
			)
		) AND (
			?3 IS NULL
			OR files.size >= ?3
		) AND (
			?4 IS NULL
			OR files_meta.modified >= ?4
		))
		OR (
			?2 IS NULL
			AND ?3 IS NULL
			AND (
				dirs_meta.created >= ?4
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
		coalesce(file_name, dir_name, uuid) AS name,
		parent,
		0,
		CASE
			WHEN coalesce(file_name, dir_name, uuid) != ''
				THEN coalesce(file_name, dir_name, uuid)
		END
	FROM filtered_items
	WHERE parent_path IS NULL
	UNION ALL
	SELECT
		p.original_id,
		i.id,
		i.uuid,
		coalesce(f.name, d.name, i.uuid) AS name,
		i.parent,
		p.level + 1,
		CASE
			WHEN
				coalesce(f.name, d.name, i.uuid) != '' AND p.path_components IS NOT NULL
				THEN coalesce(f.name, d.name, i.uuid) || '/' || p.path_components
			WHEN
				coalesce(f.name, d.name, i.uuid) != '' AND p.path_components IS NULL
				THEN coalesce(f.name, d.name, i.uuid)
			ELSE p.path_components
		END
	FROM items AS i
	LEFT JOIN files_meta AS f ON i.id = f.id
	LEFT JOIN dirs_meta AS d ON i.id = d.id
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
	fi.local_data,
	fi.type,
	fi.dir_favorite_rank,
	fi.color,
	fi.dir_timestamp,
	fi.last_listed,
	fi.dir_metadata_state,
	fi.dir_raw_metadata,
	fi.dir_name,
	fi.dir_created,
	fi.size,
	fi.chunks,
	fi.file_favorite_rank,
	fi.region,
	fi.bucket,
	fi.file_timestamp,
	fi.file_metadata_state,
	fi.file_raw_metadata,
	fi.file_name,
	fi.mime,
	fi.file_key,
	fi.file_key_version,
	fi.file_created,
	fi.modified,
	fi.hash,
	coalesce(fi.parent_path, cp.computed_path) AS search_path
FROM filtered_items AS fi
LEFT JOIN computed_paths AS cp ON fi.id = cp.item_id;

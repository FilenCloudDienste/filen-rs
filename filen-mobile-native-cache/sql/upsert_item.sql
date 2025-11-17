INSERT INTO items (id, uuid, parent, local_data, type, is_recent)
VALUES (
	-- Get existing id if item exists at target location
	COALESCE(
		(
			SELECT id FROM items
			WHERE uuid = ?1
		),
		(
			SELECT items.id
			FROM items
			LEFT JOIN files_meta ON items.id = files_meta.id
			LEFT JOIN dirs_meta ON items.id = dirs_meta.id
			WHERE
				items.parent = ?2
				AND
				(files_meta.name = ?3 OR dirs_meta.name = ?3)
		)
	),
	?1, -- uuid
	?2, -- parent
	COALESCE(
		?4,
		(
			SELECT local_data FROM items
			WHERE uuid = ?1
		),
		(
			SELECT items.local_data
			FROM items
			LEFT JOIN files_meta ON items.id = files_meta.id
			LEFT JOIN dirs_meta ON items.id = dirs_meta.id
			WHERE
				items.parent = ?2
				AND
				(files_meta.name = ?3 OR dirs_meta.name = ?3)
		)
	), -- local_data
	?5, -- type
	COALESCE(
		(
			SELECT is_recent FROM items
			WHERE uuid = ?1
		),
		(
			SELECT items.is_recent
			FROM items
			LEFT JOIN files_meta ON items.id = files_meta.id
			LEFT JOIN dirs_meta ON items.id = dirs_meta.id
			WHERE
				items.parent = ?2
				AND
				(files_meta.name = ?3 OR dirs_meta.name = ?3)
		),
		FALSE
	) -- is_recent
)
ON CONFLICT (id) DO UPDATE SET
uuid = excluded.uuid,
parent = excluded.parent,
local_data = excluded.local_data,
type = excluded.type,
is_recent = excluded.is_recent,
is_stale = FALSE
RETURNING id, local_data;

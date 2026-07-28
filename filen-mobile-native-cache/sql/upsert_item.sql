INSERT INTO items (
	id,
	uuid,
	stable_uuid,
	parent,
	trashed,
	local_data,
	type,
	is_recent
) VALUES (
	-- Get existing id if item exists at target location. The stable_uuid match
	-- is what lets a file survive a server-side uuid re-mint (content edit or
	-- version restore): the new head arrives with a fresh uuid but the same
	-- lifetime id, and must update the existing row instead of creating a new
	-- one. The (parent, name) match remains strictly a last resort for rows
	-- the server never told us a stable id for — identity is never inferred
	-- from names when a stable id is available.
	COALESCE(
		(
			SELECT id FROM items
			WHERE uuid = ?1
		),
		(
			SELECT id FROM items
			WHERE stable_uuid = ?7 AND type = ?5
			ORDER BY trashed ASC, id ASC
			LIMIT 1
		),
		(
			SELECT items.id
			FROM items
			LEFT JOIN files_meta ON items.id = files_meta.id
			LEFT JOIN dirs_meta ON items.id = dirs_meta.id
			WHERE
				items.parent = ?2
				AND items.trashed = FALSE
				AND
				(files_meta.name = ?3 OR dirs_meta.name = ?3)
		)
	),
	?1, -- uuid
	?7, -- stable_uuid
	?2, -- parent
	?6, -- trashed
	COALESCE(
		?4,
		(
			SELECT local_data FROM items
			WHERE uuid = ?1
		),
		(
			SELECT local_data FROM items
			WHERE stable_uuid = ?7 AND type = ?5
			ORDER BY trashed ASC, id ASC
			LIMIT 1
		),
		(
			SELECT items.local_data
			FROM items
			LEFT JOIN files_meta ON items.id = files_meta.id
			LEFT JOIN dirs_meta ON items.id = dirs_meta.id
			WHERE
				items.parent = ?2
				AND items.trashed = FALSE
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
			SELECT is_recent FROM items
			WHERE stable_uuid = ?7 AND type = ?5
			ORDER BY trashed ASC, id ASC
			LIMIT 1
		),
		(
			SELECT items.is_recent
			FROM items
			LEFT JOIN files_meta ON items.id = files_meta.id
			LEFT JOIN dirs_meta ON items.id = dirs_meta.id
			WHERE
				items.parent = ?2
				AND items.trashed = FALSE
				AND
				(files_meta.name = ?3 OR dirs_meta.name = ?3)
		),
		FALSE
	) -- is_recent
)
ON CONFLICT (id) DO UPDATE SET
	uuid = excluded.uuid,
	stable_uuid = excluded.stable_uuid,
	parent = excluded.parent,
	trashed = excluded.trashed,
	local_data = excluded.local_data,
	type = excluded.type,
	is_recent = excluded.is_recent,
	is_stale = FALSE
RETURNING id, local_data;

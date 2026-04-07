INSERT INTO items (id, uuid, parent, type, root_id)
VALUES (
	-- Get existing id if item exists at target location
	(
		SELECT id FROM items
		WHERE uuid = ?1
	),
	?1, -- uuid
	?2, -- parent
	?3, -- type
	(
		SELECT roots.id FROM roots INNER JOIN items ON roots.id = items.root_id
		WHERE items.uuid = ?4
	)
)
ON CONFLICT (id) DO UPDATE SET
	parent = excluded.parent,
	type = excluded.type,
	root_id = excluded.root_id
RETURNING id;

SELECT
	items.id,
	items.uuid,
	items.stable_uuid,
	items.parent,
	items.trashed,
	items.local_data,
	items.type
FROM items
LEFT JOIN dirs_meta ON items.id = dirs_meta.id
LEFT JOIN files_meta ON items.id = files_meta.id
WHERE
	items.parent = ?1
	AND items.trashed = FALSE
	AND (
		?2 = files_meta.name OR ?2 = dirs_meta.name
		OR ?2 = uuid_text(items.uuid)
	)
ORDER BY
	CASE
		WHEN ?2 = files_meta.name OR ?2 = dirs_meta.name THEN 0
		WHEN ?2 = uuid_text(items.uuid) THEN 1
	END,
	-- Deterministic tie-break when two rows share (parent, name) — a genuine name collision the
	-- server permits (names are not unique, uuids are). Prefer the lowest items.id (first-inserted),
	-- so path resolution is stable regardless of which index the planner picks; the change-tracking
	-- indexes on (parent, seq) would otherwise order the scan by seq and pick arbitrarily.
	items.id
LIMIT 1;

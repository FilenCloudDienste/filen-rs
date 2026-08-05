SELECT
	items.id,
	items.uuid,
	items.stable_uuid,
	items.pending_upload_at,
	items.change_seq,
	items.parent,
	items.trashed,
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
	files_meta.hash
FROM items
LEFT JOIN dirs ON items.id = dirs.id
LEFT JOIN dirs_meta ON items.id = dirs_meta.id
LEFT JOIN files ON items.id = files.id
LEFT JOIN files_meta ON items.id = files_meta.id
-- Everything a replica holding sequence ?1 has not been shown yet. Trashed rows
-- are deliberately in: they moved into the trash container, which is a change
-- like any other, and they keep their original `parent` for the restore.
WHERE
	items.change_seq > ?1
	-- Roots are local scaffolding; no replica ever sees one.
	AND items.type != 0
	-- A row whose per-type half has not landed yet cannot be rendered: the
	-- `items` row is stamped the moment it is inserted, the `files`/`dirs` row
	-- that carries the rest of it a statement later. Skipping is safe — landing
	-- it stamps the item again, so it arrives with the next diff.
	AND COALESCE(files.id, dirs.id) IS NOT NULL
ORDER BY items.change_seq ASC;

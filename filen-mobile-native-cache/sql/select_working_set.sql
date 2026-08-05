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
-- The working set: the items this device has a stake in, and so the only ones
-- kept up to date incrementally. Everything else is reconciled when it is
-- presented. Trashed rows stay in: a file that was materialised or
-- favourited is still the user's business once it is in the trash.
WHERE
	-- Same two exclusions as the change feed: no roots, and no half-written row.
	items.type != 0
	AND COALESCE(files.id, dirs.id) IS NOT NULL
	AND (
		-- Its bytes are on the device...
		items.materialised_at IS NOT NULL
		-- ...or an edit of them has not reached the server...
		OR items.pending_upload_at IS NOT NULL
		-- ...or the user asked for it by name. `favorite_rank` lives on the
		-- per-type table, so a file can only ever satisfy the first of these two
		-- and a dir only the second.
		OR files.favorite_rank > 0
		OR dirs.favorite_rank > 0
	);

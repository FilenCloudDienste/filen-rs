SELECT
	dirs.favorite_rank,
	dirs.color,
	dirs.last_listed,
	dirs.metadata_state,
	dirs.raw_metadata,
	dirs_meta.name,
	dirs_meta.created
FROM dirs LEFT JOIN dirs_meta ON dirs.id = dirs_meta.id
WHERE dirs.id = ? LIMIT 1;

-- Column names are aliased to match the wide joins (select_object,
-- select_dir_children, select_trash_children, select_recents) exactly, so
-- `DBDir::from_inner_and_row` reads the same names whichever query produced
-- the row.
SELECT
	dirs.favorite_rank AS dir_favorite_rank,
	dirs.color,
	dirs.timestamp AS dir_timestamp,
	dirs.last_listed,
	dirs.metadata_state AS dir_metadata_state,
	dirs.raw_metadata AS dir_raw_metadata,
	dirs_meta.name AS dir_name,
	dirs_meta.created AS dir_created
FROM dirs LEFT JOIN dirs_meta ON dirs.id = dirs_meta.id
WHERE dirs.id = ?;

-- Column names are aliased to match the wide joins exactly, so
-- `DBFile::from_inner_and_row` reads the same names whichever query produced
-- the row.
SELECT
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
FROM files LEFT JOIN files_meta ON files.id = files_meta.id
WHERE files.id = ?;

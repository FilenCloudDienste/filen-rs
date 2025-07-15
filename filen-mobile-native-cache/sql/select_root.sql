SELECT
	roots.storage_used,
	roots.max_storage,
	roots.last_updated,
	dirs.last_listed
FROM roots INNER JOIN dirs ON roots.id = dirs.id
WHERE roots.id = ?;

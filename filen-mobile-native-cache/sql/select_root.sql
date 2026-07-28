-- `dirs.last_listed` is aliased to match select_object.sql, where the plain
-- `last_listed` name is already taken by the dir block.
SELECT
	roots.storage_used,
	roots.max_storage,
	roots.last_updated,
	dirs.last_listed AS root_last_listed
FROM roots INNER JOIN dirs ON roots.id = dirs.id
WHERE roots.id = ?;

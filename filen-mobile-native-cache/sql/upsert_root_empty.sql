INSERT INTO roots (
	id
) VALUES (
	?
) ON CONFLICT (id) DO UPDATE SET
id = id
RETURNING storage_used, max_storage, last_updated;

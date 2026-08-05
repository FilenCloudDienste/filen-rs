-- Whether anything below this directory still holds an edit the server has not
-- got. Deleting a directory row cascades to its descendants, and that takes
-- their pending-upload markers with it — the only record that those bytes
-- exist nowhere else.
--
-- The walk mirrors what the cascade would actually delete: children by
-- `parent`, generation after generation, with trashed rows excluded at every
-- level because `cascade_on_delete_delete_children` leaves them alone (they
-- hang off the trash listing, not off their parent).
--
-- UNION rather than UNION ALL: a parent chain that loops back on itself is not
-- something this cache can rule out, and deduplicating the frontier is what
-- makes such a tree terminate here instead of recursing until SQLite gives up.
WITH RECURSIVE descendants (uuid, pending_upload_at) AS (
	SELECT
		i.uuid,
		i.pending_upload_at
	FROM items AS i
	WHERE i.parent = ? AND i.trashed = FALSE
	UNION
	SELECT
		i.uuid,
		i.pending_upload_at
	FROM items AS i
	INNER JOIN descendants AS d ON i.parent = d.uuid
	WHERE i.trashed = FALSE
)

-- No LIMIT: the caller steps this exactly once (`Statement::exists`), so the
-- walk stops at the first marker it finds without one.
SELECT 1 FROM descendants
WHERE pending_upload_at IS NOT NULL;

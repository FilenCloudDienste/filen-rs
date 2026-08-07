-- Drops the pending-upload marker from a row and from everything deleting that
-- row takes with it.
--
-- Paired with the DELETE in `sql::delete_item`, in one transaction, because
-- `tombstone_on_delete` reads `pending_upload_at` off the row it is deleting: a
-- row that still claims an edit the server has not got is deleted without a
-- tombstone, and every replica keeps it forever. That suppression is right when
-- a listing stops mentioning an item — those bytes exist nowhere else — and
-- wrong for a permanent delete, which runs after the server has dropped the
-- item and the local copy with it.
--
-- The walk is the one `select_descendant_pending_upload.sql` documents, because
-- it has to reach exactly what the cascade will: children by `parent`,
-- generation after generation, trashed rows excluded because
-- `cascade_on_delete_delete_children` leaves them alone, and UNION rather than
-- UNION ALL so a parent chain that loops back on itself still terminates.
WITH RECURSIVE doomed (uuid) AS (
	SELECT ?1 AS uuid
	UNION
	SELECT i.uuid
	FROM items AS i
	INNER JOIN doomed AS d ON i.parent = d.uuid
	WHERE i.trashed = FALSE
)

UPDATE items
SET pending_upload_at = NULL
WHERE items.uuid IN (SELECT uuid FROM doomed);

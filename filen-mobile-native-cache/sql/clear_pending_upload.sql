-- Drops the pending-upload marker once the local changes have reached the
-- server. Same scoping as mark_pending_upload, so the two always agree on
-- which of a set of duplicate stables they act on.
UPDATE items
SET pending_upload_at = NULL
WHERE items.id = (
	SELECT items.id FROM items
	WHERE items.stable_uuid = ?1
	ORDER BY items.trashed ASC, items.id ASC
	LIMIT 1
);

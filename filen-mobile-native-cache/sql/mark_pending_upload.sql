-- Records that a file has local changes that are not on the server yet, as the
-- millis the edit was marked (?2).
--
-- Keyed on `stable_uuid`, not `uuid`: an upload re-mints the file's uuid, and
-- the marker belongs to the file's whole life, not to one version of it.
--
-- Its own column rather than a key in `local_data`: that column is the app's
-- to overwrite wholesale over the FFI, and `upsert_item` leaves this one out
-- of its column list, so a directory refresh cannot drop the marker either.
--
-- Scoped to the single row the read paths resolve (same trashed/id
-- tie-break as select_item_by_stable_uuid): duplicate stables are reachable
-- via same-account uuid reuse, and a bare stable_uuid match would mark or
-- clear every sibling at once.
UPDATE items
SET pending_upload_at = ?2
WHERE items.id = (
	SELECT items.id FROM items
	WHERE items.stable_uuid = ?1
	ORDER BY items.trashed ASC, items.id ASC
	LIMIT 1
);

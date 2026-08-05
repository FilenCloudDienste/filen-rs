-- Records that this file's bytes are in the local cache directory, as the
-- millis they landed there (?2).
--
-- Keyed on `uuid`, not on the `stable_uuid` the pending-upload marker uses: a
-- cache slot is NAMED after the uuid (`cache_dir/<uuid>/<name>`), so every site
-- that creates or destroys one already holds it, and `items.uuid` is UNIQUE, so
-- unlike the stable id there is no duplicate tie-break for two statements to
-- keep in step. An upload that re-mints the uuid carries the column along
-- on the row it updates, so the marker survives that too.
--
-- Its own column rather than a key in `local_data`, for the same reason
-- `pending_upload_at` is one: that column is the app's to overwrite wholesale
-- over the FFI. `upsert_item` names neither, so a directory refresh cannot drop
-- this either.
UPDATE items
SET materialised_at = ?2
WHERE uuid = ?1;

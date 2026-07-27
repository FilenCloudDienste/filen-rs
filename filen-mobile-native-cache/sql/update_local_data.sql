-- Replaces the caller's local data outright.
--
-- `local_data` is the app's alone: the pending-upload marker that used to
-- share it lives in `items.pending_upload_at`, so there is nothing internal
-- here for a plain replace to destroy.
UPDATE items
SET local_data = ?1
WHERE uuid = ?2;

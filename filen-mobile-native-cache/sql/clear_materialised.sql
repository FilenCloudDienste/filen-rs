-- Drops the record once the bytes have left the cache directory. Same key as
-- mark_materialised, so the two always agree on the row they act on.
--
-- Scoped to rows that carry a marker, which makes clearing one that has none a
-- no-op rather than a write: every deletion path funnels through
-- `io_delete_local`, dirs included, and only files can ever be materialised.
UPDATE items
SET materialised_at = NULL
WHERE uuid = ?1 AND materialised_at IS NOT NULL;

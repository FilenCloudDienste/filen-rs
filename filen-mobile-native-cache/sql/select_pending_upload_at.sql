-- The pending-upload marker of the row a stable id resolves to.
--
-- Same trashed/id tie-break as mark/clear, so a read reports on exactly the
-- row those two write. Callers act on this under the per-item lock, where a
-- marker written since they last read their row would be invisible.
SELECT pending_upload_at
FROM items
WHERE stable_uuid = ?1
ORDER BY trashed ASC, id ASC
LIMIT 1;

UPDATE items
SET is_stale = TRUE
WHERE parent = ?;

DELETE FROM items
WHERE parent = ? AND is_stale = TRUE;

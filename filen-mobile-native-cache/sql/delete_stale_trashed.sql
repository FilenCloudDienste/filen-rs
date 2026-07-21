DELETE FROM items
WHERE trashed = TRUE AND is_stale = TRUE;

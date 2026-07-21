UPDATE items
SET is_stale = TRUE
WHERE trashed = TRUE;

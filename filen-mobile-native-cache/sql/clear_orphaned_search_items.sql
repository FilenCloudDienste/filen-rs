DELETE FROM items
WHERE
	parent_path IS NOT NULL
	AND parent NOT IN (
		SELECT uuid FROM items
		WHERE uuid IS NOT NULL
	);

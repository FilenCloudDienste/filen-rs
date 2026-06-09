-- Resync create detection: a listed item that is not cached at all → emit
-- a synthetic New. The caller orders these parent-before-child by tree depth
-- before persisting, so a child never lands before its parent exists. NOTE:
-- there is no anchor guard here (unlike the move/content queries) because the
-- caller resets `diff_incoming` and re-stages it from THIS root's listing
-- alone before each call — reusing a stale `diff_incoming` across roots would
-- emit spurious New events.
SELECT
	d.uuid,
	d.type
FROM diff_incoming AS d
WHERE d.uuid NOT IN (SELECT i.uuid FROM items AS i);

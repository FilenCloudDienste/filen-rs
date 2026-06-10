-- The upward ancestor chain of one item: walk `items.parent` from the
-- seed uuid to the account root, returning the seed itself plus every
-- ancestor uuid. UNION (not UNION ALL) guards against a corrupt parent
-- cycle — it terminates instead of spinning forever. The set of
-- sync-root uuids lives in memory (not a table), so the caller
-- intersects these rows against it in Rust to decide membership ("the
-- seed, or any ancestor, is a sync root"). `?1` is the seed uuid.
WITH RECURSIVE ancestry (uuid, parent) AS (
	SELECT
		uuid,
		parent
	FROM items
	WHERE uuid = ?1
	UNION
	SELECT
		i.uuid,
		i.parent
	FROM items AS i
	INNER JOIN ancestry AS a ON i.uuid = a.parent
)

SELECT uuid FROM ancestry;

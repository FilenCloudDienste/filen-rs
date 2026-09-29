-- The upward ancestor chain of one item: walk `items.parent` from the
-- seed uuid to the account root, returning the seed itself plus every
-- ancestor uuid. UNION (not UNION ALL) guards against a corrupt parent
-- cycle — it terminates instead of spinning forever. The set of
-- sync-root uuids lives in memory (not a table), so the caller
-- intersects these rows against it in Rust to decide membership ("the
-- seed, or any ancestor, is a sync root"). `?1` is the seed uuid.
--
-- The parents are returned too, which adds exactly one uuid: the parent
-- of the top-most cached ancestor when that parent is NOT cached itself.
-- That is a sync root whose own node is not materialized yet (its first
-- listing has not committed), and an item an event put under it is still
-- in it — the membership gate admits the event by that key, so dispatch
-- must find the same owner or the event is applied and announced to
-- nobody.
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

SELECT uuid FROM ancestry
UNION
SELECT parent AS uuid FROM ancestry
WHERE parent IS NOT NULL;

-- Resync move detection: a listed item that is cached but under a
-- DIFFERENT parent → emit a synthetic Move (the Move upsert rewrites
-- every column, so a simultaneous rename or content change rides along).
-- `IS NOT` is the null-safe distinct comparison, so a NULL parent on
-- either side compares correctly. An item whose parent is unchanged is
-- left for content-change detection instead, which keeps moves and
-- content changes mutually exclusive — a moved item is never emitted
-- twice.
SELECT
	d.uuid,
	d.type
FROM diff_incoming AS d
INNER JOIN items AS i ON d.uuid = i.uuid
-- Never treat the sync root as a move, even if a malformed listing
-- stages it with a non-NULL parent: the root's cached parent is NULL, so
-- `IS NOT` would otherwise match here and then be silently dropped at
-- build time, churning every resync. `?1` is the sync-root uuid.
WHERE d.uuid != ?1 AND i.parent IS NOT d.parent;

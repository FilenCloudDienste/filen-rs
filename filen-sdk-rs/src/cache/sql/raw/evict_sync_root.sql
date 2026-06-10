-- Evict one sync root: delete its subtree, but PROTECT every still-active
-- nested root — both its own subtree (protected_down) AND its ancestor path
-- back up to the account root (protected_up). The ancestor path has to be
-- protected too because the `cascade_on_delete` trigger deletes a dir's
-- children unconditionally: deleting an intermediate dir on the path to a
-- still-active nested root would make the cascade wipe that root. Excluding
-- the whole protected set (down + up) guarantees no deleted item is ever the
-- parent of a protected one, so the cascade only ever touches victim rows.
--
-- `?1` = the evicted root uuid. `evict_protected_roots` (TEMP) = the
-- remaining active root uuids. The victim seed is the evicted root's
-- CHILDREN, not the root node itself (the root node is kept), and
-- `type != 0` keeps the account-root item.
WITH RECURSIVE
victim (uuid) AS (
	SELECT uuid FROM items
	WHERE parent = ?1
	UNION
	SELECT i.uuid FROM items AS i INNER JOIN victim AS v ON i.parent = v.uuid
),

protected_down (uuid) AS (
	SELECT uuid FROM evict_protected_roots
	UNION
	SELECT i.uuid
	FROM items AS i
	INNER JOIN protected_down AS pd ON i.parent = pd.uuid
),

protected_up (uuid, parent) AS (
	SELECT
		i.uuid,
		i.parent
	FROM items AS i
	WHERE i.uuid IN (SELECT epr.uuid FROM evict_protected_roots AS epr)
	UNION
	-- Terminates at the account root, whose parent IS NULL: no items row
	-- has uuid = NULL, so the join `i.uuid = pu.parent` yields nothing and
	-- the recursion stops.
	SELECT
		i.uuid,
		i.parent
	FROM items AS i
	INNER JOIN protected_up AS pu ON i.uuid = pu.parent
)

DELETE FROM items
WHERE
	uuid IN (SELECT uuid FROM victim)
	AND uuid NOT IN (SELECT uuid FROM protected_down)
	AND uuid NOT IN (SELECT uuid FROM protected_up)
	AND uuid != ?1
	AND type != 0;

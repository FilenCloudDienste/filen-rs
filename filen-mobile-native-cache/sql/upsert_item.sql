-- Resolve the row this item belongs to ONCE, then carry over that row's
-- values. Deriving the id and the carried-over columns from three independent
-- COALESCE chains lets them disagree: COALESCE skips a tier whose matched row
-- simply has a NULL value, so a row matched by stable id but holding no
-- local_data would fall through to the (parent, name) tier and adopt an
-- unrelated file's local_data — pending-upload marker included.
WITH target AS (
	SELECT
		COALESCE(
			-- Type-scoped like the stable tier below: on a cross-type uuid
			-- collision (uuid-reuse abuse) adopting the row would silently flip
			-- its type, destroy a file's stable id and orphan its files/files_meta
			-- rows — a fresh insert is the only safe answer.
			(
				SELECT items.id FROM items
				WHERE items.uuid = ?1 AND items.type = ?5
			),
			-- The stable_uuid match is what lets a file survive a server-side
			-- uuid re-mint (content edit or version restore): the new head
			-- arrives with a fresh uuid but the same lifetime id, and must
			-- update the existing row instead of creating a new one. Stable
			-- ids are files-only, so ?7 is NULL for a dir or root upsert and
			-- this tier can never match one (`x = NULL` is NULL, not TRUE).
			(
				SELECT items.id FROM items
				WHERE items.stable_uuid = ?7 AND items.type = ?5
				ORDER BY items.trashed ASC, items.id ASC
				LIMIT 1
			),
			-- The only tier a dir or root can resolve through, and a last
			-- resort for files the server never told us a stable id for —
			-- identity is never inferred from names when a stable id is
			-- available.
			(
				SELECT items.id
				FROM items
				LEFT JOIN files_meta ON items.id = files_meta.id
				LEFT JOIN dirs_meta ON items.id = dirs_meta.id
				WHERE
					items.parent = ?2
					AND items.trashed = FALSE
					-- Type-scoped like the other tiers: a same-named item of a
					-- different type is a different object, never this one.
					AND items.type = ?5
					AND
					(files_meta.name = ?3 OR dirs_meta.name = ?3)
			)
		) AS id
)

-- `pending_upload_at` is deliberately absent from both this list and the
-- ON CONFLICT SET below: a column the upsert never names is a column it never
-- touches, so a marker survives the identity reconciliation that re-mints a
-- file's uuid, and the stale sweep of a directory refresh. A fresh row simply
-- gets the NULL default, which is what a newly seen file should have.
INSERT INTO items (
	id,
	uuid,
	stable_uuid,
	parent,
	trashed,
	local_data,
	type,
	is_recent
)
SELECT
	target.id,
	?1 AS uuid,
	?7 AS stable_uuid,
	?2 AS parent,
	?6 AS trashed,
	COALESCE(
		?4,
		(
			SELECT items.local_data
			FROM items
			WHERE items.id = target.id
		)
	) AS local_data,
	?5 AS type,
	COALESCE(
		(
			SELECT items.is_recent
			FROM items
			WHERE items.id = target.id
		),
		FALSE
	) AS is_recent
FROM target
-- `WHERE TRUE` disambiguates the upsert clause from a join on the SELECT,
-- which SQLite would otherwise fail to parse (sqlite.org/lang_upsert.html §2.2)
WHERE TRUE
ON CONFLICT (id) DO UPDATE SET
	uuid = excluded.uuid,
	stable_uuid = excluded.stable_uuid,
	parent = excluded.parent,
	trashed = excluded.trashed,
	local_data = excluded.local_data,
	type = excluded.type,
	is_recent = excluded.is_recent,
	is_stale = FALSE
RETURNING id, local_data, pending_upload_at;

WITH moving AS (
	SELECT
		id,
		local_data,
		is_recent
	FROM items
	WHERE uuid = ?1
),

existing AS (
	SELECT
		items.id,
		items.local_data
	FROM items
	LEFT JOIN files_meta ON items.id = files_meta.id
	LEFT JOIN dirs_meta ON items.id = dirs_meta.id
	WHERE
		items.parent = ?2
		AND (?3 IS NULL OR files_meta.name = ?3 OR dirs_meta.name = ?3)
),

local_source AS ( -- noqa: ST05
	SELECT
		moving.is_recent,
		COALESCE(
			moving.id,
			existing.id
		) AS id,
		COALESCE(
			?4,
			moving.local_data,
			existing.local_data
		) AS local_data
	FROM (
		-- we select nothing here to make sure we always have a row
		SELECT
			NULL AS id,
			NULL AS local_data
	)
-- The ambiguous cross join sqlfluff here is actually a bug
	-- this is a left join
	LEFT JOIN moving -- noqa: AM08
	LEFT JOIN existing
		ON 1 = 1
)

INSERT OR REPLACE INTO items (
	id,
	uuid,
	parent,
	local_data,
	type,
	is_recent
)
SELECT
	local_source.id,
	?1 AS uuid,
	?2 AS parent,
	local_source.local_data,
	?5 AS type,
	local_source.is_recent
FROM local_source
RETURNING id, local_data;

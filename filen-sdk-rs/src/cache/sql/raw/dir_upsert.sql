-- Type-specific metadata upsert. MUST be called AFTER ITEM_UPSERT has
-- written the `items` row and returned its stable id (passed as the first
-- `?` here). `content_hash` lives on `items`, not here, so it is
-- deliberately absent.
INSERT INTO dirs (
	id,
	favorite,
	color,
	timestamp,
	name,
	created
) VALUES (
	?,
	?,
	?,
	?,
	?,
	?
) ON CONFLICT (id) DO UPDATE SET
	favorite = excluded.favorite,
	color = excluded.color,
	timestamp = excluded.timestamp,
	name = excluded.name,
	created = excluded.created;

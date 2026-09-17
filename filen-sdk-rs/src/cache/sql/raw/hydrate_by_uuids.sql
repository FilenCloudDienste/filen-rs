-- Full-payload hydration of named items BY UUID, for the sync engine's apply
-- and confirmation paths. A pass reads the whole subtree through the SLIM
-- projection (enumerate_subtree.sql) because that is all its view needs; the
-- handful of items it actually acts on — the file it downloads, trashes or
-- moves, the directory it moves or trashes, the parent it writes into, the
-- foreign version it dates a push against — need the whole payload, so they
-- are read back here by uuid.
--
-- COLUMN NAMES ARE A CONTRACT: `search::hydrate::row_to_result` reads them by
-- name, so the `AS` aliases below must match the search windows' projection.
--
-- The single `?1` below stands in for the chunk: `enumerate::hydrate_by_uuids`
-- expands it to one numbered parameter per uuid before preparing, which is why
-- this one statement is never bound as it is written.
SELECT
	i.uuid,
	i.parent,
	i.type,
	f.chunks_size,
	f.chunks,
	f.favorite AS file_favorite,
	f.region,
	f.bucket,
	f.timestamp AS file_timestamp,
	f.size,
	f.name AS file_name,
	f.mime,
	f.file_key,
	f.file_key_version,
	f.created AS file_created,
	f.modified,
	f.hash,
	f.stable_uuid,
	d.favorite AS dir_favorite,
	d.color,
	d.timestamp AS dir_timestamp,
	d.name AS dir_name,
	d.created AS dir_created
FROM items AS i
LEFT JOIN files AS f ON i.id = f.id
LEFT JOIN dirs AS d ON i.id = d.id
WHERE i.uuid IN (?1)
-- A row mid-supersede carries the PREDECESSOR's content under the successor's
-- uuid (see files.superseded), so handing it over would schedule an
-- undownloadable transfer; the caller asks the server for what it misses.
AND coalesce(f.superseded, FALSE) = FALSE;

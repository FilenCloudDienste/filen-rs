-- Resync staging table: the freshly-listed remote subtree projected to
-- just the columns the diff compares against the cached `items`. TEMP so
-- it is per-connection and non-durable — it lives only on the worker's
-- long-lived connection and is rebuilt from scratch every resync, so a
-- crash can never leave it half-populated for the next run. WITHOUT ROWID
-- because the BLOB uuid is the natural primary key that every diff query
-- joins on. No FK to `items` (TEMP tables cannot reference the main
-- schema), and none is wanted — this is throwaway staging, not cache
-- state.
CREATE TEMP TABLE IF NOT EXISTS diff_incoming (
	uuid BLOB PRIMARY KEY NOT NULL,
	parent BLOB,
	type SMALLINT NOT NULL CHECK (type IN (0, 1, 2)),
	content_hash BLOB NOT NULL
) WITHOUT ROWID;

-- Empty any rows left from a previous resync so each run starts from a
-- clean snapshot.
DELETE FROM diff_incoming;

-- Set a singleton metadata value. The row is normally seeded by
-- init.sql, but UPSERT (rather than a
-- silent UPDATE that no-ops on a missing row) self-heals and makes the
-- write unconditionally durable.
INSERT INTO cache_meta (meta_key, value) VALUES (?1, ?2)
ON CONFLICT (meta_key) DO UPDATE SET value = excluded.value;

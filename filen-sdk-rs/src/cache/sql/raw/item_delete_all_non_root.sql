-- Wipe every non-root item (`type != 0` keeps the account-root row). The
-- cascade_on_delete trigger fires per deleted dir and re-attempts to delete
-- its children, but those are already gone in this same bulk DELETE, so the
-- trigger firings are no-ops. For targeted eviction of a SINGLE sync root use
-- evict_sync_root.sql instead — this statement clears the whole item cache.
DELETE FROM items
WHERE type != 0;

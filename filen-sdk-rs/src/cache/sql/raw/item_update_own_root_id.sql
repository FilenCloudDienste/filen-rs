-- Bootstrap step 3 of 3 (see `root::insert_root`): patch the
-- account-root item's root_id from the placeholder 0 to its own rowid
-- (?1). ROOT_INSERT has already inserted ?1 into `roots`, so the
-- DEFERRABLE FK is now satisfiable and passes the deferred check at
-- commit. The `SET root_id = ?1 WHERE id = ?1` is self-referential by
-- design (the root is its own root), not a no-op.
UPDATE items SET root_id = ?1
WHERE id = ?1;

-- Bootstrap step 1 of 3 (see `root::insert_root`): insert the
-- account-root item with root_id = 0 as a placeholder — the root cannot
-- know its own rowid before insertion, and the `roots` row does not
-- exist yet. The root_id FK is DEFERRABLE INITIALLY DEFERRED (see
-- init.sql), so the violation is tolerated until ROOT_INSERT creates the
-- `roots` row and ITEM_UPDATE_OWN_ROOT_ID patches root_id within the
-- same transaction (the FK is only checked at commit).
INSERT INTO items (root_id, uuid, parent, type)
VALUES (0, ?, NULL, 0)
RETURNING id;

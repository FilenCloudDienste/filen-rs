INSERT INTO items (root_id, uuid, parent, type)
VALUES (0, ?, NULL, 0)
RETURNING id;

DELETE FROM items
WHERE name = ? AND parent = ? AND uuid != ?;

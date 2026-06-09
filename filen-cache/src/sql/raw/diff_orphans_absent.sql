-- MUST only be called for the account-root resync (where `diff_incoming`
-- spans the WHOLE account). With a subdir anchor, `diff_incoming` would not
-- contain items from other roots, so a still-live item would look absent and
-- be wrongly emitted as an orphan deletion. (The Rust caller enforces this.)
--
-- Resync orphan sweep: a cached non-root item whose parent row is gone
-- (broken ancestry — e.g. an intermediate create event was lost) is
-- UNREACHABLE by the subtree walk, so the normal subtree-absent query would
-- never sweep it. Catch any such orphan that is also absent from the fresh
-- listing and emit a synthetic deletion (the cascade trigger removes its
-- descendants). This is safe because the listing is an ancestor-closed
-- subtree: an orphan absent from the listing cannot have a listed (still-live)
-- descendant, so cascading its deletion never removes a live item.
SELECT
	i.uuid,
	i.type
FROM items AS i
WHERE
	i.type != 0
	AND i.parent IS NOT NULL
	AND i.parent NOT IN (SELECT p.uuid FROM items AS p)
	AND i.uuid NOT IN (SELECT d.uuid FROM diff_incoming AS d);

-- Files whose local changes have not reached the server yet, oldest marker
-- first so a drain retries edits in roughly the order they were made.
--
-- Grouped rather than DISTINCT: duplicate stables are reachable via
-- same-account uuid reuse, and each stable id may only be drained once —
-- while ORDER BY needs one marker per group to sort on, which a DISTINCT
-- over a column the select does not emit cannot give it.
SELECT stable_uuid
FROM items
WHERE
	pending_upload_at IS NOT NULL
	-- Markers belong to files, which the CHECK on `items` already enforces;
	-- this keeps the statement honest if that ever loosens.
	AND stable_uuid IS NOT NULL
	AND trashed = FALSE
GROUP BY stable_uuid
ORDER BY MIN(pending_upload_at) ASC;

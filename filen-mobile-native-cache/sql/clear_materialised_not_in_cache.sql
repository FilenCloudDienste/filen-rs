-- Reconciles the materialisation markers against what the cache directory
-- actually holds: ?1 is the JSON array of uuid directories found in it, ?2 the
-- millis that listing was taken at.
--
-- The sweeps delete slots by path — the budget sweep by age, the unknown-uuid
-- sweep by identity, `process_subdir` by malformed shape — and none of them is
-- in a position to write to the database. Reconciling once, after they have all
-- run, keeps the column honest whatever removed the bytes, including whatever
-- removes them next.
--
-- The `?2` bound is what makes reconciling from a snapshot safe: a file
-- materialised after the listing was taken is absent from it only because
-- it did not exist yet, and clearing it would report bytes gone that are
-- right there.
UPDATE items
SET materialised_at = NULL
WHERE
	materialised_at IS NOT NULL
	AND materialised_at < ?2
	-- `items.uuid` is a 16-byte BLOB; the JSON carries hyphenated UUID text, so
	-- strip the hyphens and decode to bytes before matching.
	AND uuid NOT IN (
		SELECT UNHEX(REPLACE(value, '-', '')) FROM JSON_EACH(?1)
	);

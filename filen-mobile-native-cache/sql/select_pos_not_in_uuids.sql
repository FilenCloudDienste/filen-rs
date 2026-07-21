WITH positions AS (
	SELECT
		value AS uuid,
		key AS position
	FROM JSON_EACH(?)
)

-- `items.uuid` is a 16-byte BLOB; the JSON carries hyphenated UUID text, so
-- strip the hyphens and decode to bytes before matching.
SELECT p.position
FROM positions AS p
LEFT JOIN items AS i ON UNHEX(REPLACE(p.uuid, '-', '')) = i.uuid
WHERE i.uuid IS NULL;

WITH positions AS (
	SELECT
		value AS uuid,
		key AS position
	FROM JSON_EACH(?)
)

SELECT p.position
FROM positions AS p
LEFT JOIN items AS i ON p.uuid = i.uuid
WHERE i.uuid IS NULL;

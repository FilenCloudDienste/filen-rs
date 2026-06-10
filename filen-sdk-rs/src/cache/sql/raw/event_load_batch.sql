-- Load the next drain batch from the durable store in apply order:
-- synthetic diff events (NULL drive_message_id) first, then by
-- drive_message_id ascending, then by insertion order. Applying
-- synthetics first lets a resync's converged state land before any live
-- events stacked behind it.
SELECT
	seq,
	drive_message_id,
	payload
FROM events
ORDER BY synthetic DESC, drive_message_id ASC, seq ASC
LIMIT ?1;

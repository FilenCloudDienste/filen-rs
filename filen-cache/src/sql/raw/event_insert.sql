-- Persist one drive event to the durable events store.
-- `OR IGNORE` drops an at-least-once redelivery of the same
-- drive_message_id via the partial unique index idx_events_unique_id;
-- synthetic events (NULL drive_message_id) are exempt and always insert.
-- seq (rowid) is assigned automatically and preserves insertion order.
INSERT OR IGNORE INTO events (drive_message_id, synthetic, payload) VALUES (
	?1, ?2, ?3
);

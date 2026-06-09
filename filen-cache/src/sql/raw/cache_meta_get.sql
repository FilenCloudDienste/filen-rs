-- Read a singleton metadata value (e.g. the watermark `last_drive_message_id`).
SELECT value FROM cache_meta
WHERE meta_key = ?1;

-- Resync content-change detection: a listed item that is cached under
-- the SAME parent but whose fingerprint differs → emit a synthetic
-- Changed. `i.parent IS d.parent` excludes moves (those are emitted
-- separately), so a moved item is never also emitted as a content
-- change. `IS NOT` is the null-safe distinct comparison; a re-listed
-- unchanged item compares equal and is skipped — that is what makes a
-- second resync over the same listing emit zero synthetics (it
-- converges).
SELECT
	d.uuid,
	d.type
FROM diff_incoming AS d
INNER JOIN items AS i ON d.uuid = i.uuid
-- Exclude the sync root: its cached `content_hash` is NULL, so a
-- malformed listing that staged the root would match `IS NOT` here and
-- then be silently dropped at build time, churning every resync.
-- `?1` is the sync-root uuid.
WHERE
	d.uuid != ?1
	AND i.parent IS d.parent
	AND d.content_hash IS NOT i.content_hash;

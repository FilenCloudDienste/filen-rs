//! Caps that keep an untrusted archive from costing unbounded memory, time or storage. Every one
//! is checked before the work it bounds, never only after it.

/// Longest entry path, in bytes of its UTF-8 name, an archive may carry. Longer paths are
/// skipped on extract and refused on compress, so an archive the SDK writes can be extracted by
/// it again. 4096 is `PATH_MAX` on Linux and more than any real tool writes.
pub(crate) const MAX_ARCHIVE_PATH_BYTES: usize = 4096;

/// Deepest entry path, in directory levels, for the same reasons as [`MAX_ARCHIVE_PATH_BYTES`].
pub(crate) const MAX_ARCHIVE_PATH_DEPTH: usize = 256;

/// Most skipped, renamed or failed entries a report keeps one by one; beyond that they are only
/// counted, so an archive of a million symlinks cannot build a million-record report.
pub(crate) const MAX_REPORT_RECORDS: usize = 1000;

/// `path` cut at a character boundary to at most [`MAX_ARCHIVE_PATH_BYTES`], for showing and
/// reporting an entry whose path was refused (possibly for its length), with whether it was cut.
pub(crate) fn display_path(path: &str) -> (&str, bool) {
	if path.len() <= MAX_ARCHIVE_PATH_BYTES {
		return (path, false);
	}
	let shown = path
		.get(..path.floor_char_boundary(MAX_ARCHIVE_PATH_BYTES))
		.expect("floor_char_boundary returns a char boundary (should be impossible)");
	(shown, true)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn display_paths_are_cut_at_a_char_boundary() {
		assert_eq!(display_path("a/b.txt"), ("a/b.txt", false));
		let long = "é".repeat(3000); // 6000 bytes
		let (shown, cut) = display_path(&long);
		assert!(cut);
		assert_eq!(shown.len(), MAX_ARCHIVE_PATH_BYTES);
		assert_eq!(shown, "é".repeat(2048));
	}
}

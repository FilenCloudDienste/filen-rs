//! Caps that keep an untrusted archive from costing unbounded memory, time or storage. Every one
//! is checked before the work it bounds, never only after it.

/// Longest entry path, in bytes of its UTF-8 name, an archive may carry. Longer paths are
/// skipped on extract and refused on compress, so an archive the SDK writes can be extracted by
/// it again. 4096 is `PATH_MAX` on Linux and more than any real tool writes.
pub(crate) const MAX_ARCHIVE_PATH_BYTES: usize = 4096;

/// Deepest entry path, in directory levels, for the same reasons as [`MAX_ARCHIVE_PATH_BYTES`].
pub(crate) const MAX_ARCHIVE_PATH_DEPTH: usize = 256;

/// Heap a zip's or 7z's parsed index may take, per byte of its index budget.
pub(crate) const HEAP_PER_INDEX_BYTE: u64 = 3;

/// Most skipped, renamed or failed entries a report keeps one by one; beyond that they are only
/// counted, so an archive of a million symlinks cannot build a million-record report. The public
/// report docs and the bindings' say 1000 in words; change them with it.
pub(crate) const MAX_REPORT_RECORDS: usize = 1000;

/// Adds a copy of `record` to `list` unless it holds [`MAX_REPORT_RECORDS`] already, then only
/// counting it in `omitted`; whether it was kept. The caller keeps `record` for its event, so a
/// record past the cap is never copied.
pub(crate) fn keep<T: Clone>(list: &mut Vec<T>, omitted: &mut u64, record: &T) -> bool {
	if list.len() < MAX_REPORT_RECORDS {
		list.push(record.clone());
		true
	} else {
		*omitted += 1;
		false
	}
}

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
	fn records_past_the_cap_are_only_counted() {
		let (mut list, mut omitted) = (Vec::new(), 0);
		for record in 0..MAX_REPORT_RECORDS + 2 {
			let kept = keep(&mut list, &mut omitted, &record);
			assert_eq!(kept, record < MAX_REPORT_RECORDS, "{record}");
		}
		assert_eq!((list.len(), omitted), (MAX_REPORT_RECORDS, 2));
	}

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

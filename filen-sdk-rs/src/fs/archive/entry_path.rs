//! An archive entry's stored path made into drive names: never anything that climbs out of the
//! folder it is extracted into, and every segment a name the drive accepts.
//!
//! Archives are untrusted input. Their paths may be absolute, climb with `..`, carry Windows
//! drive or UNC prefixes, mix `/` and `\`, or hold names today's rules reject. A path that climbs
//! is refused; everything else is made safe and reported as rewritten.

use crate::fs::name::{ValidatedName, keep_both::SourceName};

use super::limits::{MAX_ARCHIVE_PATH_BYTES, MAX_ARCHIVE_PATH_DEPTH};

/// An entry's path as drive names, parent directories first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ArchivePath {
	pub(crate) segments: Vec<ValidatedName>,
	/// Whether any segment differs from what the archive stored: a stripped absolute or drive
	/// prefix, or a name encoded or shortened to be valid.
	pub(crate) rewritten: bool,
	/// Whether the path holds characters that make it read as something it is not (see
	/// [`is_suspicious`]), such as a right-to-left override showing `invoice\u{202E}fdp.exe` as
	/// `invoiceexe.pdf`. The drive forbids C0 controls and DEL, so a segment holding one is
	/// encoded as well (and the path counts as rewritten); C1 controls and the format
	/// characters are allowed there and kept as they are.
	pub(crate) suspicious: bool,
}

/// Why an entry's path cannot be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PathRejection {
	/// Longer than [`MAX_ARCHIVE_PATH_BYTES`].
	TooLong,
	/// Deeper than [`MAX_ARCHIVE_PATH_DEPTH`].
	TooDeep,
	/// Climbs out of the extraction folder, or a segment cannot be made into a name.
	Unsafe,
	/// Nothing is left once `.` and empty segments are dropped: the archive's root.
	Empty,
}

/// Makes `raw`, an entry path as the archive stored it, into drive names.
pub(crate) fn entry_path(raw: &str) -> Result<ArchivePath, PathRejection> {
	// the caps come first, so no per-segment work is spent on a hostile path
	if raw.len() > MAX_ARCHIVE_PATH_BYTES {
		return Err(PathRejection::TooLong);
	}
	let mut rewritten = false;
	let mut parts: Vec<&str> = raw.split(['/', '\\']).collect();
	let leading_separators = parts.iter().take_while(|part| part.is_empty()).count();
	if leading_separators > 0 && parts.len() > leading_separators {
		// absolute; two separators make a UNC path, whose server and share are dropped too
		rewritten = true;
		let drop = if leading_separators >= 2 {
			leading_separators + 2
		} else {
			leading_separators
		};
		parts.drain(..drop.min(parts.len()));
	} else if parts.first().is_some_and(|first| is_drive(first)) {
		rewritten = true;
		parts.remove(0);
	}
	let parts: Vec<&str> = parts
		.into_iter()
		.filter(|part| !part.is_empty() && *part != ".")
		.collect();
	if parts.len() > MAX_ARCHIVE_PATH_DEPTH {
		return Err(PathRejection::TooDeep);
	}
	if parts.contains(&"..") {
		return Err(PathRejection::Unsafe);
	}
	if parts.is_empty() {
		return Err(PathRejection::Empty);
	}
	let mut suspicious = false;
	let mut segments = Vec::with_capacity(parts.len());
	for part in parts {
		suspicious |= part.chars().any(is_suspicious);
		let (name, segment_rewritten) = segment_name(part).ok_or(PathRejection::Unsafe)?;
		rewritten |= segment_rewritten;
		segments.push(name);
	}
	Ok(ArchivePath {
		segments,
		rewritten,
		suspicious,
	})
}

/// A Windows drive prefix such as `C:`.
fn is_drive(segment: &str) -> bool {
	matches!(segment.as_bytes(), [letter, b':'] if letter.is_ascii_alphabetic())
}

/// `segment` as a drive name: itself when valid, its reversible encoding when not, and for a
/// segment too long even for that, the longest prefix that encodes within the limit. The flag
/// says whether it differs from the stored segment.
fn segment_name(segment: &str) -> Option<(ValidatedName, bool)> {
	match SourceName::parse(segment) {
		Ok(SourceName::Valid(name)) => return Some((name, false)),
		Ok(SourceName::Encoded(name)) => return Some((name, true)),
		Err(_) => {}
	}
	// Only a segment whose encoding is too long fails both. Encodings grow with the prefix, so
	// the prefixes that fit come first and a binary search over the char boundaries finds the
	// longest in O(log n) tries, however long a hostile segment is.
	let cuts: Vec<usize> = segment.char_indices().map(|(i, _)| i).skip(1).collect();
	let fits = |cut: usize| SourceName::parse(&segment[..cut]).ok();
	let fitting = cuts.partition_point(|&cut| fits(cut).is_some());
	let cut = *cuts.get(fitting.checked_sub(1)?)?;
	fits(cut).map(|name| (name.into_name(), true))
}

/// Characters that make a name read differently from what it is: controls (C0, DEL, C1) and the
/// format characters used for bidi overrides and invisible joins.
fn is_suspicious(c: char) -> bool {
	matches!(c,
		'\u{0}'..='\u{1F}'
			| '\u{7F}'..='\u{9F}'
			| '\u{AD}'
			| '\u{61C}'
			| '\u{180E}'
			| '\u{200B}'..='\u{200F}'
			| '\u{202A}'..='\u{202E}'
			| '\u{2060}'..='\u{2064}'
			| '\u{2066}'..='\u{206F}'
			| '\u{FEFF}'
			| '\u{FFF9}'..='\u{FFFB}'
			| '\u{E0001}'
			| '\u{E0020}'..='\u{E007F}')
}

#[cfg(test)]
mod tests {
	use super::*;

	fn names(path: &ArchivePath) -> Vec<&str> {
		path.segments.iter().map(|s| s.as_ref()).collect()
	}

	#[test]
	fn plain_paths_are_kept() {
		let path = entry_path("docs/2024/report.pdf").unwrap();
		assert_eq!(names(&path), ["docs", "2024", "report.pdf"]);
		assert!(!path.rewritten);
		assert!(!path.suspicious);
		// `.` and empty segments are dropped without counting as a rewrite
		let path = entry_path("./docs//report.pdf").unwrap();
		assert_eq!(names(&path), ["docs", "report.pdf"]);
		assert!(!path.rewritten);
		// both separators split, as Windows tools write them
		assert_eq!(
			names(&entry_path("docs\\report.pdf").unwrap()),
			["docs", "report.pdf"]
		);
	}

	#[test]
	fn climbing_paths_are_refused() {
		assert_eq!(entry_path("../etc/passwd"), Err(PathRejection::Unsafe));
		assert_eq!(entry_path("docs/../../x"), Err(PathRejection::Unsafe));
		assert_eq!(entry_path("docs\\..\\x"), Err(PathRejection::Unsafe));
	}

	#[test]
	fn absolute_drive_and_unc_prefixes_are_stripped() {
		let path = entry_path("/etc/passwd").unwrap();
		assert_eq!(names(&path), ["etc", "passwd"]);
		assert!(path.rewritten);
		let path = entry_path("C:\\Users\\a.txt").unwrap();
		assert_eq!(names(&path), ["Users", "a.txt"]);
		assert!(path.rewritten);
		let path = entry_path("\\\\server\\share\\dir\\a.txt").unwrap();
		assert_eq!(names(&path), ["dir", "a.txt"]);
		assert!(path.rewritten);
	}

	#[test]
	fn nothing_left_is_the_root() {
		assert_eq!(entry_path("./"), Err(PathRejection::Empty));
		assert_eq!(entry_path(""), Err(PathRejection::Empty));
		assert_eq!(entry_path("/"), Err(PathRejection::Empty));
	}

	#[test]
	fn caps_are_checked_first() {
		let long = "a".repeat(MAX_ARCHIVE_PATH_BYTES + 1);
		assert_eq!(entry_path(&long), Err(PathRejection::TooLong));
		let deep = vec!["d"; MAX_ARCHIVE_PATH_DEPTH + 1].join("/");
		assert_eq!(entry_path(&deep), Err(PathRejection::TooDeep));
		let deep_enough = vec!["d"; MAX_ARCHIVE_PATH_DEPTH].join("/");
		assert_eq!(
			entry_path(&deep_enough).unwrap().segments.len(),
			MAX_ARCHIVE_PATH_DEPTH
		);
	}

	#[test]
	fn invalid_names_are_encoded_and_fullwidth_forms_kept() {
		let path = entry_path("a:b.txt").unwrap();
		assert!(path.rewritten);
		assert_eq!(
			path.segments[0],
			crate::fs::name::encode_name("a:b.txt").unwrap()
		);
		// full-width `／` and `＼` are ordinary characters in a drive name
		let path = entry_path("資料／2024.pdf").unwrap();
		assert_eq!(names(&path), ["資料／2024.pdf"]);
		assert!(!path.rewritten);
		assert!(!path.suspicious);
		assert_eq!(names(&entry_path("a＼b.txt").unwrap()), ["a＼b.txt"]);
	}

	#[test]
	fn overlong_segments_are_shortened() {
		let segment = "é".repeat(200); // 400 bytes
		let path = entry_path(&format!("dir/{segment}")).unwrap();
		assert!(path.rewritten);
		let shortened: &str = path.segments[1].as_ref();
		assert!(shortened.len() <= 255, "{} bytes", shortened.len());
		assert!(segment.starts_with(shortened));
		// one that needs encoding as well stays within the limit
		let colons = ":".repeat(300);
		let path = entry_path(&colons).unwrap();
		assert!(path.segments[0].as_ref().len() <= 255);
	}

	#[test]
	fn misleading_characters_are_flagged_but_kept() {
		let path = entry_path("invoice\u{202E}fdp.exe").unwrap();
		assert!(path.suspicious);
		assert!(!path.rewritten);
		assert_eq!(names(&path), ["invoice\u{202E}fdp.exe"]);
	}
}

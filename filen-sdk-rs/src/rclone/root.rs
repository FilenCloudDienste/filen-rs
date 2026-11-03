//! The directory of the drive a mount or serve exposes: the drive's root, or a directory in it.

use crate::{
	Error, ErrorKind,
	rclone::daemon::{FILEN_READ_ONLY_REMOTE, FILEN_REMOTE},
};

/// The rclone fs a mount or serve uses: `Filen:`, or `FilenRO:` when read-only, followed by
/// `root`, a path from the drive's root such as `Photos/2024`.
///
/// `root` is split on `/`, whose empty segments (a leading, trailing or doubled `/`) are dropped,
/// so `/`, an empty one and none all name the drive's root. Refused as [`ErrorKind::InvalidState`]
/// when a segment is `.` or `..`, or the path holds a backslash or a control character: rclone
/// would resolve the former outside the directory asked for, and read a backslash as a separator
/// on Windows.
pub(crate) fn role_fs(read_only: bool, root: Option<&str>) -> Result<String, Error> {
	let remote = if read_only {
		FILEN_READ_ONLY_REMOTE
	} else {
		FILEN_REMOTE
	};
	let mut fs = format!("{remote}:");
	let Some(root) = root else {
		return Ok(fs);
	};
	let invalid = |why: &str| {
		Error::custom(
			ErrorKind::InvalidState,
			format!("invalid root directory {root:?}: {why}"),
		)
	};
	if root.chars().any(char::is_control) {
		return Err(invalid("it holds a control character"));
	}
	if root.contains('\\') {
		return Err(invalid("it holds a backslash"));
	}
	let mut first = true;
	for segment in root.split('/').filter(|segment| !segment.is_empty()) {
		if segment == "." || segment == ".." {
			return Err(invalid("it holds a `.` or `..` segment"));
		}
		if !first {
			fs.push('/');
		}
		fs.push_str(segment);
		first = false;
	}
	Ok(fs)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn refused(root: &str) -> Error {
		role_fs(false, Some(root)).unwrap_err()
	}

	#[test]
	fn without_a_root_the_whole_drive_is_used() {
		assert_eq!(role_fs(false, None).unwrap(), "Filen:");
		assert_eq!(role_fs(true, None).unwrap(), "FilenRO:");
	}

	#[test]
	fn the_root_follows_the_remote() {
		assert_eq!(
			role_fs(false, Some("Photos/2024")).unwrap(),
			"Filen:Photos/2024"
		);
		assert_eq!(role_fs(true, Some("Photos")).unwrap(), "FilenRO:Photos");
	}

	#[test]
	fn empty_segments_are_dropped() {
		assert_eq!(
			role_fs(false, Some("/Photos//2024/")).unwrap(),
			"Filen:Photos/2024"
		);
		for root in ["", "/", "//"] {
			assert_eq!(role_fs(false, Some(root)).unwrap(), "Filen:", "{root:?}");
		}
	}

	#[test]
	fn other_characters_stay_in_the_path() {
		assert_eq!(
			role_fs(false, Some("a b/:local:/c,d")).unwrap(),
			"Filen:a b/:local:/c,d"
		);
	}

	#[test]
	fn dot_segments_are_refused() {
		for root in ["..", "../etc", "Photos/../..", "Photos/./2024", "."] {
			let error = refused(root);

			assert_eq!(error.kind(), ErrorKind::InvalidState, "{root:?}");
		}
	}

	#[test]
	fn a_dot_inside_a_name_is_kept() {
		assert_eq!(
			role_fs(false, Some("..hidden/a..b")).unwrap(),
			"Filen:..hidden/a..b"
		);
	}

	#[test]
	fn control_characters_and_backslashes_are_refused() {
		for root in [
			"Photos\n2024",
			"Photos\0",
			"\u{7f}",
			"Photos\\2024",
			"\\\\server",
		] {
			let error = refused(root);

			assert_eq!(error.kind(), ErrorKind::InvalidState, "{root:?}");
		}
	}

	#[test]
	fn the_refusal_names_the_root() {
		assert_eq!(
			refused("../etc").to_string(),
			Error::custom(
				ErrorKind::InvalidState,
				"invalid root directory \"../etc\": it holds a `.` or `..` segment"
			)
			.to_string()
		);
	}
}

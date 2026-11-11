//! Which names Windows can hold under their exact spelling ([`windows_rejection`]), checked on every
//! host so the rules are tested everywhere.

use std::fmt;

/// The longest name, in UTF-16 code units, NTFS and the Win32 API hold.
const WINDOWS_MAX_UNITS: usize = 255;

/// The characters Win32 refuses in a name, besides the control characters below U+0020.
const WINDOWS_FORBIDDEN: [char; 9] = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// Why this device's file system cannot hold a remote name under its exact spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeviceRejection {
	/// Longer than the file system holds: `len` bytes, or UTF-16 code units on Windows, of at most
	/// `max`.
	TooLong { len: usize, max: usize },
	/// A character Windows refuses in a name.
	ForbiddenChar(char),
	/// A trailing dot or space, which Win32 strips, so the item would land under another name, and
	/// on top of the item already holding that one.
	TrailingDotOrSpace,
	/// A name Windows reserves for a device (`CON`, `NUL`, `COM1`, ... with or without an
	/// extension), which opens the device instead of a file.
	ReservedName,
}

impl fmt::Display for DeviceRejection {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::TooLong { len, max } => write!(
				f,
				"the name is {len} long, more than the {max} this device's file system holds"
			),
			Self::ForbiddenChar(ch) => {
				write!(f, "Windows does not allow the character {ch:?} in a name")
			}
			Self::TrailingDotOrSpace => f.write_str(
				"Windows drops a trailing dot or space, so the item would be stored under another name",
			),
			Self::ReservedName => f.write_str("Windows reserves the name for a device"),
		}
	}
}

/// Why Windows cannot hold `name` under that exact spelling, or `None` when it can: the length, the
/// forbidden characters, a trailing dot or space, and the device names. Compiled on every host, so
/// its rules are tested everywhere. Allocates nothing: the sync engine asks it of every name it
/// places, and of every directory above one, and a directory download on Windows of every name it
/// joins.
pub(crate) fn windows_rejection(name: &str) -> Option<DeviceRejection> {
	// A UTF-16 unit never takes fewer than one UTF-8 byte, so only a name past the limit in bytes
	// is counted.
	if name.len() > WINDOWS_MAX_UNITS {
		let units = name.encode_utf16().count();
		if units > WINDOWS_MAX_UNITS {
			return Some(DeviceRejection::TooLong {
				len: units,
				max: WINDOWS_MAX_UNITS,
			});
		}
	}
	if let Some(ch) = name
		.chars()
		.find(|ch| *ch < ' ' || WINDOWS_FORBIDDEN.contains(ch))
	{
		return Some(DeviceRejection::ForbiddenChar(ch));
	}
	if name.ends_with(['.', ' ']) {
		return Some(DeviceRejection::TrailingDotOrSpace);
	}
	is_windows_device(name).then_some(DeviceRejection::ReservedName)
}

/// The names Win32 opens a device for, besides `COM` and `LPT` with a digit.
const WINDOWS_DEVICES: [&str; 6] = ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"];

/// The digits that make `COM` and `LPT` a device name: 0 to 9 and the superscripts 1 to 3.
const WINDOWS_PORT_DIGITS: [&str; 13] = [
	"0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "\u{B9}", "\u{B2}", "\u{B3}",
];

/// Whether Win32 opens a device for `name`: its part before the first dot, trailing spaces dropped,
/// is a device name in any case. `CON.txt` and `nul.tar.gz` are devices too, unlike what the name
/// validator checks; Windows 11 stopped reserving some of these with an extension, and refusing them
/// costs nothing but a pull that could not happen on older Windows.
fn is_windows_device(name: &str) -> bool {
	let stem = name
		.split_once('.')
		.map_or(name, |(stem, _)| stem)
		.trim_end_matches(' ');
	// Three to seven bytes: `CON` to `CONOUT$`, and `COM` with a two-byte superscript digit.
	if !(3..=7).contains(&stem.len()) {
		return false;
	}
	if WINDOWS_DEVICES
		.iter()
		.any(|device| stem.eq_ignore_ascii_case(device))
	{
		return true;
	}
	// `split_at_checked` refuses a cut inside a character, which no ASCII prefix ends in.
	stem.split_at_checked(3).is_some_and(|(prefix, digit)| {
		(prefix.eq_ignore_ascii_case("COM") || prefix.eq_ignore_ascii_case("LPT"))
			&& WINDOWS_PORT_DIGITS.contains(&digit)
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn windows_refuses_a_trailing_dot_or_space() {
		assert_eq!(
			windows_rejection("foo."),
			Some(DeviceRejection::TrailingDotOrSpace)
		);
		assert_eq!(
			windows_rejection("foo "),
			Some(DeviceRejection::TrailingDotOrSpace)
		);
		assert_eq!(windows_rejection("foo.txt"), None);
		assert_eq!(windows_rejection(".hidden"), None);
	}

	#[test]
	fn windows_refuses_its_forbidden_characters_and_control_characters() {
		for ch in WINDOWS_FORBIDDEN {
			let name = format!("a{ch}b");
			assert_eq!(
				windows_rejection(&name),
				Some(DeviceRejection::ForbiddenChar(ch)),
				"{name:?}"
			);
		}
		assert_eq!(
			windows_rejection("a\u{1}b"),
			Some(DeviceRejection::ForbiddenChar('\u{1}'))
		);
		// DEL and a fullwidth colon are fine on Windows, whatever the SDK's validator says.
		assert_eq!(windows_rejection("a\u{7f}b"), None);
		assert_eq!(windows_rejection("a\u{ff1a}b"), None);
	}

	#[test]
	fn windows_reserves_device_names_with_and_without_an_extension() {
		for name in [
			"CON",
			"con",
			"Nul",
			"PRN",
			"aux",
			"COM1",
			"lpt9",
			"COM0",
			"COM\u{b9}",
			"LPT\u{b3}",
			"CONIN$",
			"conout$",
			"CON.txt",
			"nul.tar.gz",
			"Aux .txt",
		] {
			assert_eq!(
				windows_rejection(name),
				Some(DeviceRejection::ReservedName),
				"{name:?}"
			);
		}
		for name in [
			"CONSOLE",
			"CON1",
			"COM10",
			"LPT",
			"xCON",
			"NULL.txt",
			"COM\u{b4}",
			"con_.txt",
			"CONOUT$$",
			"\u{e9}CON",
			"\u{ff23}\u{ff2f}\u{ff2e}",
			"CO\u{e9}",
		] {
			assert_eq!(windows_rejection(name), None, "{name:?}");
		}
	}

	#[test]
	fn windows_counts_a_names_length_in_utf16_units() {
		// 255 three-byte characters: 765 bytes, 255 units.
		let at_limit = "\u{20ac}".repeat(255);
		assert_eq!(windows_rejection(&at_limit), None);
		let over = "\u{20ac}".repeat(256);
		assert_eq!(
			windows_rejection(&over),
			Some(DeviceRejection::TooLong { len: 256, max: 255 })
		);
		// A character outside the BMP takes two units.
		let astral = "\u{1f600}".repeat(128);
		assert_eq!(
			windows_rejection(&astral),
			Some(DeviceRejection::TooLong { len: 256, max: 255 })
		);
	}
}

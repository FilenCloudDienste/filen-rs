use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

const FORBIDDEN: [bool; 128] = {
	let mut table = [false; 128];
	let mut i = 0u8;
	// Control characters 0x00–0x1F
	while i < 0x20 {
		table[i as usize] = true;
		i += 1;
	}
	table[0x7F] = true; // DEL
	table[b'/' as usize] = true;
	table[b'\\' as usize] = true;
	table[b':' as usize] = true;
	table[b'*' as usize] = true;
	table[b'?' as usize] = true;
	table[b'"' as usize] = true;
	table[b'<' as usize] = true;
	table[b'>' as usize] = true;
	table[b'|' as usize] = true;
	table
};

const MAX_BYTES: usize = 255;

#[derive(thiserror::Error, Debug, PartialEq)]
// todo expose
pub enum EntryNameError {
	#[error("filename is empty")]
	Empty,
	#[error("filename is too long: {bytes} bytes (max {MAX_BYTES})")]
	TooLong { bytes: usize },
	#[error("filename contains forbidden character '{ch}' at position {pos}")]
	ForbiddenChar { ch: char, pos: usize },
	#[error("filename is a reserved device name on windows")]
	ReservedName,
	#[error("filename cannot end with a dot or space")]
	TrailingDotOrSpace,
	#[error("filename cannot start with a space")]
	LeadingSpace,
	#[error("filename cannot be . or ..")]
	DotEntry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(any(feature = "uniffi", feature = "wasm-full"))]
#[cfg_attr(
	feature = "wasm-full",
	derive(serde::Serialize, serde::Deserialize, tsify::Tsify),
	tsify(into_wasm_abi)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum EntryNameErrorKindJS {
	Empty,
	TooLong,
	ForbiddenChar,
	ReservedName,
	TrailingDotOrSpace,
	LeadingSpace,
	DotEntry,
}

#[derive(Debug)]
#[cfg_attr(feature = "wasm-full", wasm_bindgen::prelude::wasm_bindgen)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
#[cfg(any(feature = "uniffi", feature = "wasm-full"))]
pub struct EntryNameErrorJS {
	kind: EntryNameErrorKindJS,
	message: String,
}

#[cfg_attr(feature = "wasm-full", wasm_bindgen::prelude::wasm_bindgen)]
#[cfg_attr(feature = "uniffi", uniffi::export)]
#[cfg(any(feature = "uniffi", feature = "wasm-full"))]
impl EntryNameErrorJS {
	pub fn kind(&self) -> EntryNameErrorKindJS {
		self.kind
	}

	pub fn message(&self) -> String {
		self.message.clone()
	}
}

#[cfg(any(feature = "uniffi", feature = "wasm-full"))]
impl std::fmt::Display for EntryNameErrorJS {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{:?}", self.message)
	}
}

#[cfg(any(feature = "uniffi", feature = "wasm-full"))]
impl std::error::Error for EntryNameErrorJS {}

#[cfg(any(feature = "uniffi", feature = "wasm-full"))]
impl From<EntryNameError> for EntryNameErrorJS {
	fn from(err: EntryNameError) -> Self {
		let kind = match err {
			EntryNameError::Empty => EntryNameErrorKindJS::Empty,
			EntryNameError::TooLong { .. } => EntryNameErrorKindJS::TooLong,
			EntryNameError::ForbiddenChar { .. } => EntryNameErrorKindJS::ForbiddenChar,
			EntryNameError::ReservedName => EntryNameErrorKindJS::ReservedName,
			EntryNameError::TrailingDotOrSpace => EntryNameErrorKindJS::TrailingDotOrSpace,
			EntryNameError::LeadingSpace => EntryNameErrorKindJS::LeadingSpace,
			EntryNameError::DotEntry => EntryNameErrorKindJS::DotEntry,
		};
		Self {
			kind,
			message: err.to_string(),
		}
	}
}

fn is_reserved_name_on_windows(name: &str) -> bool {
	let bytes = name.as_bytes();
	match bytes {
		[b0, b1, b2] => {
			let b = [
				b0.to_ascii_uppercase(),
				b1.to_ascii_uppercase(),
				b2.to_ascii_uppercase(),
			];
			matches!(&b, b"CON" | b"PRN" | b"AUX" | b"NUL")
		}
		[b0, b1, b2, digit] => {
			let prefix = [
				b0.to_ascii_uppercase(),
				b1.to_ascii_uppercase(),
				b2.to_ascii_uppercase(),
			];
			match &prefix {
				b"COM" | b"LPT" => matches!(digit, b'1'..=b'9'),
				_ => false,
			}
		}
		_ => false,
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[cfg_attr(feature = "wasm-full", derive(tsify::Tsify), tsify(into_wasm_abi))]
pub struct ValidatedName(String);

impl<'de> Deserialize<'de> for ValidatedName {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let s = filen_types::serde::cow::deserialize(deserializer)?;
		parse_name(&s).map_err(serde::de::Error::custom)
	}
}

impl AsRef<str> for ValidatedName {
	fn as_ref(&self) -> &str {
		&self.0
	}
}

impl From<ValidatedName> for String {
	fn from(val: ValidatedName) -> Self {
		val.0
	}
}

impl TryFrom<&str> for ValidatedName {
	type Error = EntryNameError;

	fn try_from(value: &str) -> Result<Self, Self::Error> {
		parse_name(value)
	}
}

#[cfg(feature = "uniffi")]
uniffi::custom_type!(ValidatedName, String, {
	remote,
	lower: |uuid: &UuidStr| uuid.as_ref().to_string(),
	try_lift: |s: String| {
		ValidatedName::try_from(s.as_ref()).map_err(|_| uniffi::deps::anyhow::anyhow!("invalid NormalizedName string: {}", s))
	},
});

/// Validate a filename according to unix + windows rules.
/// Returns the normalized name if valid, or an error describing the first violation found.
fn parse_name(name: &str) -> Result<ValidatedName, EntryNameError> {
	// 1. NFC normalize
	let name: String = name.nfc().collect();

	// 2. Empty check
	if name.is_empty() {
		return Err(EntryNameError::Empty);
	}

	// 3. Dot entries
	if name == "." || name == ".." {
		return Err(EntryNameError::DotEntry);
	}

	// 4. Byte length
	if name.len() > MAX_BYTES {
		return Err(EntryNameError::TooLong { bytes: name.len() });
	}

	// 5. Leading space
	if name.starts_with(' ') {
		return Err(EntryNameError::LeadingSpace);
	}

	// 6. Trailing dot or space
	if name.ends_with('.') || name.ends_with(' ') {
		return Err(EntryNameError::TrailingDotOrSpace);
	}

	// 7. Forbidden characters
	for (pos, ch) in name.char_indices() {
		if let Some(ascii) = ch.as_ascii()
			&& FORBIDDEN
				.get(ascii.to_u8() as usize)
				.copied()
				.unwrap_or(false)
		{
			return Err(EntryNameError::ForbiddenChar { ch, pos });
		}
		// Non-ASCII UTF-8 is fine — no filesystem forbids it
	}

	// 8. Reserved names
	if is_reserved_name_on_windows(&name) {
		return Err(EntryNameError::ReservedName);
	}

	Ok(ValidatedName(name))
}

#[cfg(any(feature = "uniffi", feature = "wasm-full"))]
#[cfg_attr(feature = "uniffi", uniffi::export(name = "parseName"))]
#[cfg_attr(
	feature = "wasm-full",
	wasm_bindgen::prelude::wasm_bindgen(js_name = "parseName")
)]
pub fn parse_name_uniffi(name: String) -> Result<ValidatedName, EntryNameErrorJS> {
	Ok(parse_name(&name)?)
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Generate all 2^n case combinations for an ASCII string.
	fn all_case_combinations(s: &str) -> Vec<String> {
		let chars: Vec<char> = s.chars().collect();
		let n = chars.len();
		(0..(1 << n))
			.map(|mask| {
				chars
					.iter()
					.enumerate()
					.map(|(i, &ch)| {
						if mask & (1 << i) != 0 {
							ch.to_ascii_uppercase()
						} else {
							ch.to_ascii_lowercase()
						}
					})
					.collect()
			})
			.collect()
	}

	// ── Valid names ──────────────────────────────────────────────

	#[test]
	fn valid_simple_names() {
		for name in [
			"hello",
			"file.txt",
			"my-document.pdf",
			"image_001.png",
			"a",
			"ab",
		] {
			assert!(parse_name(name).is_ok(), "expected {name:?} to be valid");
		}
	}

	#[test]
	fn valid_unicode_names() {
		for name in ["日本語.txt", "über.doc", "café", "файл.txt", "🎉"] {
			assert!(parse_name(name).is_ok(), "expected {name:?} to be valid");
		}
	}

	#[test]
	fn valid_names_with_dots() {
		for name in ["file.tar.gz", ".hidden", ".gitignore", "a.b.c.d"] {
			assert!(parse_name(name).is_ok(), "expected {name:?} to be valid");
		}
	}

	#[test]
	fn valid_at_max_length() {
		let name = "a".repeat(MAX_BYTES);
		assert!(parse_name(&name).is_ok());
	}

	// ── Empty ───────────────────────────────────────────────────

	#[test]
	fn empty_name() {
		assert_eq!(parse_name(""), Err(EntryNameError::Empty));
	}

	// ── Dot entries ─────────────────────────────────────────────

	#[test]
	fn dot_entries() {
		assert_eq!(parse_name("."), Err(EntryNameError::DotEntry));
		assert_eq!(parse_name(".."), Err(EntryNameError::DotEntry));
	}

	// ── Too long ────────────────────────────────────────────────

	#[test]
	fn too_long_by_one() {
		let name = "a".repeat(MAX_BYTES + 1);
		assert_eq!(
			parse_name(&name),
			Err(EntryNameError::TooLong {
				bytes: MAX_BYTES + 1
			})
		);
	}

	#[test]
	fn too_long_multibyte() {
		// Each '🎉' is 4 bytes, so 64 of them = 256 bytes > 255
		let name = "🎉".repeat(64);
		assert_eq!(
			parse_name(&name),
			Err(EntryNameError::TooLong { bytes: 256 })
		);
	}

	// ── Leading space ───────────────────────────────────────────

	#[test]
	fn leading_space() {
		assert_eq!(parse_name(" foo"), Err(EntryNameError::LeadingSpace));
		assert_eq!(parse_name("  bar"), Err(EntryNameError::LeadingSpace));
		assert_eq!(parse_name(" "), Err(EntryNameError::LeadingSpace));
	}

	// ── Trailing dot or space ───────────────────────────────────

	#[test]
	fn trailing_dot() {
		assert_eq!(parse_name("foo."), Err(EntryNameError::TrailingDotOrSpace));
		assert_eq!(parse_name("foo.."), Err(EntryNameError::TrailingDotOrSpace));
	}

	#[test]
	fn trailing_space() {
		assert_eq!(parse_name("foo "), Err(EntryNameError::TrailingDotOrSpace));
		assert_eq!(parse_name("foo  "), Err(EntryNameError::TrailingDotOrSpace));
	}

	// ── Forbidden characters ────────────────────────────────────

	#[test]
	fn forbidden_special_chars() {
		for ch in ['/', '\\', ':', '*', '?', '"', '<', '>', '|'] {
			let name = format!("file{ch}name");
			let result = parse_name(&name);
			assert!(
				matches!(result, Err(EntryNameError::ForbiddenChar { .. })),
				"expected {name:?} to be rejected for forbidden char, got {result:?}"
			);
		}
	}

	#[test]
	fn forbidden_control_chars() {
		// 0x01–0x1F (skip 0x00 since it terminates strings on Windows)
		for byte in 1u8..=0x1F {
			let ch = byte as char;
			let name = format!("file{ch}name");
			assert!(
				matches!(parse_name(&name), Err(EntryNameError::ForbiddenChar { .. })),
				"expected control char 0x{byte:02X} to be rejected"
			);
		}
	}

	#[test]
	fn forbidden_del() {
		let name = "file\x7Fname";
		assert!(matches!(
			parse_name(name),
			Err(EntryNameError::ForbiddenChar { .. })
		));
	}

	#[test]
	fn forbidden_char_reports_correct_position() {
		assert_eq!(
			parse_name("abc*def"),
			Err(EntryNameError::ForbiddenChar { ch: '*', pos: 3 })
		);
	}

	// ── Reserved names — all case combinations ──────────────────

	#[test]
	fn reserved_3char_all_cases() {
		for base in ["con", "prn", "aux", "nul"] {
			for variant in all_case_combinations(base) {
				assert_eq!(
					parse_name(&variant),
					Err(EntryNameError::ReservedName),
					"expected {variant:?} to be reserved"
				);
			}
		}
	}

	#[test]
	fn reserved_com_all_digits_all_cases() {
		for digit in b'1'..=b'9' {
			let base = format!("com{}", digit as char);
			for variant in all_case_combinations(&base) {
				assert_eq!(
					parse_name(&variant),
					Err(EntryNameError::ReservedName),
					"expected {variant:?} to be reserved"
				);
			}
		}
	}

	#[test]
	fn reserved_lpt_all_digits_all_cases() {
		for digit in b'1'..=b'9' {
			let base = format!("lpt{}", digit as char);
			for variant in all_case_combinations(&base) {
				assert_eq!(
					parse_name(&variant),
					Err(EntryNameError::ReservedName),
					"expected {variant:?} to be reserved"
				);
			}
		}
	}

	// ── Reserved names with extensions (should be accepted) ─────

	#[test]
	fn reserved_with_extension_accepted() {
		for name in [
			"CON.txt", "con.txt", "Con.log", "PRN.txt", "prn.doc", "AUX.dat", "aux.bin", "NUL.txt",
			"nul.csv", "COM1.txt", "com1.log", "COM9.txt", "LPT1.txt", "lpt1.dat", "LPT9.bin",
		] {
			assert!(
				parse_name(name).is_ok(),
				"expected {name:?} to be valid (reserved name with extension)"
			);
		}
	}

	// ── Not-reserved lookalikes ─────────────────────────────────

	#[test]
	fn not_reserved_lookalikes() {
		for name in [
			"CONSOLE",
			"PRINT",
			"AUXILIARY",
			"NULL",
			"COMA",
			"LPTA",
			"COM",
			"LPT",
			"COM0",
			"LPT0",
			"CO",
			"LP",
			"CONX",
			"PRNX",
			"AUXX",
			"NULX",
		] {
			assert!(
				parse_name(name).is_ok(),
				"expected {name:?} to NOT be reserved"
			);
		}
	}

	// ── NFC normalization ───────────────────────────────────────

	#[test]
	fn nfc_normalization() {
		// é as e + combining acute (NFD) normalizes to single codepoint (NFC)
		let nfd = "e\u{0301}";
		let nfc = "\u{00E9}";
		assert_eq!(parse_name(nfd).unwrap().as_ref(), nfc);
	}

	#[test]
	fn nfc_normalization_does_not_change_length_for_already_nfc() {
		let name = "café";
		let result = parse_name(name).unwrap();
		assert_eq!(result.as_ref(), name);
	}

	// ── Windows filesystem cross-validation ─────────────────────
	//
	// These tests actually create files on Windows to confirm our
	// validator agrees with the OS. They are skipped on other targets.

	#[cfg(target_os = "windows")]
	mod windows_fs {
		use super::super::*;
		use std::fs;
		use std::path::{Path, PathBuf};

		fn test_dir(suffix: &str) -> PathBuf {
			let dir = std::env::temp_dir()
				.join(format!("filen_name_test_{suffix}_{}", std::process::id()));
			fs::create_dir_all(&dir).unwrap();
			dir
		}

		/// Try to create a file and verify it actually persists on disk
		/// with the exact name we requested. Returns false if creation
		/// fails or Windows silently renamed/stripped the name.
		fn windows_accepts(dir: &Path, name: &str) -> bool {
			let path = dir.join(name);
			let file = match fs::File::create(&path) {
				Ok(f) => f,
				Err(_) => return false,
			};
			drop(file);

			// Scan the directory to confirm the file exists with the
			// exact name (guards against device-name aliasing and
			// silent trailing-dot/space stripping).
			let found = fs::read_dir(dir)
				.unwrap()
				.filter_map(Result::ok)
				.any(|e| e.file_name().to_str() == Some(name));

			if found {
				let _ = fs::remove_file(&path);
			}
			found
		}

		#[test]
		fn win_forbidden_chars_rejected() {
			let dir = test_dir("forbidden_chars");
			for ch in ['/', '\\', ':', '*', '?', '"', '<', '>', '|'] {
				let name = format!("test{ch}file");
				assert!(
					!windows_accepts(&dir, &name),
					"Windows should reject {name:?}"
				);
				assert!(
					parse_name(&name).is_err(),
					"Our validator should also reject {name:?}"
				);
			}
			let _ = fs::remove_dir_all(&dir);
		}

		#[test]
		fn win_control_chars_rejected() {
			let dir = test_dir("control_chars");
			for byte in 1u8..=0x1F {
				let name = format!("f{}\x61", byte as char);
				assert!(
					!windows_accepts(&dir, &name),
					"Windows should reject ctrl 0x{byte:02X}"
				);
				assert!(
					parse_name(&name).is_err(),
					"Our validator should also reject ctrl 0x{byte:02X}"
				);
			}
			let _ = fs::remove_dir_all(&dir);
		}

		#[test]
		fn win_reserved_bare_names_rejected() {
			let dir = test_dir("reserved_bare");
			for base in ["con", "prn", "aux", "nul"] {
				for variant in super::all_case_combinations(base) {
					assert!(
						!windows_accepts(&dir, &variant),
						"Windows should reject reserved name {variant:?}"
					);
					assert!(
						parse_name(&variant).is_err(),
						"Our validator should also reject {variant:?}"
					);
				}
			}
			let _ = fs::remove_dir_all(&dir);
		}

		#[test]
		fn win_reserved_com_lpt_match_validator() {
			let dir = test_dir("com_lpt");
			for digit in b'0'..=b'9' {
				for prefix in ["COM", "com", "LPT", "lpt"] {
					let name = format!("{prefix}{}", digit as char);
					let win = windows_accepts(&dir, &name);
					let us = parse_name(&name).is_ok();
					assert_eq!(
						win, us,
						"Mismatch for {name:?}: Windows accepts={win}, we accept={us}"
					);
				}
			}
			let _ = fs::remove_dir_all(&dir);
		}

		#[test]
		fn win_reserved_with_extension_match_validator() {
			let dir = test_dir("reserved_ext");
			for name in [
				"CON.txt", "con.txt", "PRN.log", "AUX.dat", "NUL.bin", "COM1.txt", "com1.txt",
				"LPT1.txt", "lpt1.txt",
			] {
				let win = windows_accepts(&dir, name);
				let us = parse_name(name).is_ok();
				assert_eq!(
					win, us,
					"Mismatch for {name:?}: Windows accepts={win}, we accept={us}"
				);
			}
			let _ = fs::remove_dir_all(&dir);
		}

		#[test]
		fn win_trailing_dot_space_not_preserved() {
			let dir = test_dir("trailing");
			// Windows silently strips trailing dots and spaces,
			// so the file name doesn't match what was requested.
			// Our validator rejects these proactively.
			for name in ["file.", "file ", "file..", "file  "] {
				assert!(
					!windows_accepts(&dir, name),
					"Windows should not preserve {name:?} as-is"
				);
				assert!(
					parse_name(name).is_err(),
					"Our validator should reject {name:?}"
				);
			}
			let _ = fs::remove_dir_all(&dir);
		}

		#[test]
		fn win_valid_names_accepted() {
			let dir = test_dir("valid");
			for name in [
				"hello.txt",
				"my-file",
				"document.pdf",
				".hidden",
				".gitignore",
				"file.tar.gz",
			] {
				assert!(
					windows_accepts(&dir, name),
					"Windows should accept {name:?}"
				);
				assert!(
					parse_name(name).is_ok(),
					"Our validator should also accept {name:?}"
				);
			}
			let _ = fs::remove_dir_all(&dir);
		}

		#[test]
		fn win_unicode_names_accepted() {
			let dir = test_dir("unicode");
			for name in ["日本語.txt", "über.doc", "café"] {
				assert!(
					windows_accepts(&dir, name),
					"Windows should accept {name:?}"
				);
				assert!(
					parse_name(name).is_ok(),
					"Our validator should also accept {name:?}"
				);
			}
			let _ = fs::remove_dir_all(&dir);
		}
	}
}

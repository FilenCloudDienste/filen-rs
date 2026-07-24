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
#[error("invalid filename {name:?}: {kind}")]
pub struct EntryNameError {
	/// The name as passed by the caller (before NFC normalization).
	pub name: String,
	pub kind: EntryNameErrorKind,
}

#[derive(thiserror::Error, Debug, PartialEq)]
pub enum EntryNameErrorKind {
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
	name: String,
	message: String,
}

#[cfg_attr(feature = "wasm-full", wasm_bindgen::prelude::wasm_bindgen)]
#[cfg_attr(feature = "uniffi", uniffi::export)]
#[cfg(any(feature = "uniffi", feature = "wasm-full"))]
impl EntryNameErrorJS {
	pub fn kind(&self) -> EntryNameErrorKindJS {
		self.kind
	}

	pub fn name(&self) -> String {
		self.name.clone()
	}

	pub fn message(&self) -> String {
		self.message.clone()
	}
}

#[cfg(any(feature = "uniffi", feature = "wasm-full"))]
impl std::fmt::Display for EntryNameErrorJS {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.message)
	}
}

#[cfg(any(feature = "uniffi", feature = "wasm-full"))]
impl std::error::Error for EntryNameErrorJS {}

#[cfg(any(feature = "uniffi", feature = "wasm-full"))]
impl From<EntryNameError> for EntryNameErrorJS {
	fn from(err: EntryNameError) -> Self {
		let kind = match err.kind {
			EntryNameErrorKind::Empty => EntryNameErrorKindJS::Empty,
			EntryNameErrorKind::TooLong { .. } => EntryNameErrorKindJS::TooLong,
			EntryNameErrorKind::ForbiddenChar { .. } => EntryNameErrorKindJS::ForbiddenChar,
			EntryNameErrorKind::ReservedName => EntryNameErrorKindJS::ReservedName,
			EntryNameErrorKind::TrailingDotOrSpace => EntryNameErrorKindJS::TrailingDotOrSpace,
			EntryNameErrorKind::LeadingSpace => EntryNameErrorKindJS::LeadingSpace,
			EntryNameErrorKind::DotEntry => EntryNameErrorKindJS::DotEntry,
		};
		Self {
			kind,
			message: err.to_string(),
			name: err.name,
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
	// The macro only keeps the closure's parameter name and body; the
	// parameter is a by-value ValidatedName, so move the inner String out.
	lower: |name| String::from(name),
	try_lift: |s: String| {
		ValidatedName::try_from(s.as_ref()).map_err(|e| uniffi::deps::anyhow::anyhow!("{e}"))
	},
});

/// Validate a filename according to unix + windows rules.
/// Returns the normalized name if valid, or an error describing the first violation found,
/// carrying the name exactly as the caller passed it.
fn parse_name(name: &str) -> Result<ValidatedName, EntryNameError> {
	parse_name_kind(name).map_err(|kind| EntryNameError {
		name: name.to_string(),
		kind,
	})
}

fn parse_name_kind(name: &str) -> Result<ValidatedName, EntryNameErrorKind> {
	// 1. NFC normalize
	let name: String = name.nfc().collect();

	// 2. Empty check
	if name.is_empty() {
		return Err(EntryNameErrorKind::Empty);
	}

	// 3. Dot entries
	if name == "." || name == ".." {
		return Err(EntryNameErrorKind::DotEntry);
	}

	// 4. Byte length
	if name.len() > MAX_BYTES {
		return Err(EntryNameErrorKind::TooLong { bytes: name.len() });
	}

	// 5. Leading space
	if name.starts_with(' ') {
		return Err(EntryNameErrorKind::LeadingSpace);
	}

	// 6. Trailing dot or space
	if name.ends_with('.') || name.ends_with(' ') {
		return Err(EntryNameErrorKind::TrailingDotOrSpace);
	}

	// 7. Forbidden characters
	for (pos, ch) in name.char_indices() {
		if let Some(ascii) = ch.as_ascii()
			&& FORBIDDEN
				.get(ascii.to_u8() as usize)
				.copied()
				.unwrap_or(false)
		{
			return Err(EntryNameErrorKind::ForbiddenChar { ch, pos });
		}
		// Non-ASCII UTF-8 is fine — no filesystem forbids it
	}

	// 8. Reserved names
	if is_reserved_name_on_windows(&name) {
		return Err(EntryNameErrorKind::ReservedName);
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

// ── Name encoding ───────────────────────────────────────────────────
//
// Reversible encoding of arbitrary names into names that pass validation,
// modeled on rclone's lib/encoder: characters the validator rejects are
// replaced with visually similar Unicode characters (mostly FULLWIDTH
// variants), and literal occurrences of those replacement characters are
// prefixed with a quote rune so decoding is unambiguous.
// See: https://rclone.org/overview/#restricted-filenames

/// Adding this to a printable ASCII character yields its FULLWIDTH variant.
const FULLWIDTH_OFFSET: u32 = 0xFEE0;
/// U+2400 SYMBOL FOR NULL — start of the Control Pictures block; control
/// character `c` (0x00–0x1F) is encoded as `SYMBOL_FOR_NULL + c`.
const SYMBOL_FOR_NULL: char = '\u{2400}';
/// U+2420 SYMBOL FOR SPACE — replaces leading and trailing spaces.
const SYMBOL_FOR_SPACE: char = '\u{2420}';
/// U+2421 SYMBOL FOR DELETE — replaces DEL (0x7F).
const SYMBOL_FOR_DELETE: char = '\u{2421}';
/// U+FF0E FULLWIDTH FULL STOP — replaces trailing dots and the dots of the
/// "." and ".." names.
const FULLWIDTH_FULL_STOP: char = '\u{FF0E}';
/// U+201B SINGLE HIGH-REVERSED-9 QUOTATION MARK — marks a literal occurrence
/// of a character the encoder uses as a replacement, so decoding stays
/// unambiguous. A literal quote rune is encoded as two quote runes.
const QUOTE_RUNE: char = '\u{201B}';

/// Map a character the validator forbids anywhere in a name to its encoded
/// replacement.
fn encode_forbidden(c: char) -> Option<char> {
	let ascii = c.as_ascii()?.to_u8();
	if !FORBIDDEN[usize::from(ascii)] {
		return None;
	}
	Some(match ascii {
		0x00..=0x1F => char::from_u32(SYMBOL_FOR_NULL as u32 + u32::from(ascii)).unwrap(),
		0x7F => SYMBOL_FOR_DELETE,
		_ => char::from_u32(u32::from(ascii) + FULLWIDTH_OFFSET).unwrap(),
	})
}

/// Inverse of [`encode_forbidden`]: map an encoded replacement back to the
/// forbidden character it stands for.
fn decode_forbidden(c: char) -> Option<char> {
	let code = c as u32;
	if let Some(ctrl) = code.checked_sub(SYMBOL_FOR_NULL as u32)
		&& ctrl <= 0x1F
	{
		return char::from_u32(ctrl);
	}
	if c == SYMBOL_FOR_DELETE {
		return Some('\u{7F}');
	}
	// Only printable ASCII gets fullwidth replacements (control characters use
	// the Control Pictures block above). Without the range check, unrelated
	// characters whose codepoint happens to sit FULLWIDTH_OFFSET above a
	// forbidden control character — Arabic Presentation Forms-B (U+FEE0–FEFF,
	// including the BOM U+FEFF) and `｟` (U+FF5F) — would decode to garbage.
	if let Some(ascii) = code.checked_sub(FULLWIDTH_OFFSET)
		&& (0x20..0x7F).contains(&ascii)
		&& FORBIDDEN[ascii as usize]
	{
		return char::from_u32(ascii);
	}
	None
}

/// If `c` is a fullwidth ASCII letter, return the plain ASCII letter.
fn fullwidth_to_ascii_letter(c: char) -> Option<char> {
	let ascii = u8::try_from((c as u32).checked_sub(FULLWIDTH_OFFSET)?).ok()?;
	ascii.is_ascii_alphabetic().then_some(char::from(ascii))
}

/// Replace the first character of `name` with `first`.
fn with_first_char(name: &str, first: char) -> String {
	let mut out = String::with_capacity(name.len() + first.len_utf8());
	out.push(first);
	out.push_str(&name[name.chars().next().map(char::len_utf8).unwrap_or_default()..]);
	out
}

/// If `name` starts with a fullwidth letter and replacing it with its plain
/// ASCII form yields a reserved Windows device name, return that name.
fn decodes_to_reserved(name: &str) -> Option<String> {
	let ascii = fullwidth_to_ascii_letter(name.chars().next()?)?;
	let candidate = with_first_char(name, ascii);
	is_reserved_name_on_windows(&candidate).then_some(candidate)
}

/// Reversibly encode `name` so that [`parse_name`] accepts it (aside from the
/// empty name and names whose encoding exceeds the length limit):
///
/// - forbidden characters become their fullwidth variants (`:` → `：`),
///   control characters become Control Pictures symbols (0x01 → `␁`)
/// - leading/trailing spaces become `␠`, trailing dots become `．`,
///   `.` and `..` become `．` and `．．`
/// - a reserved Windows device name gets a fullwidth first letter
///   (`CON` → `ＣON`)
/// - literal occurrences of any replacement character are prefixed with the
///   quote rune `‛` (itself doubled) so [`decode_name`] is exact
fn encode_name_raw(name: &str) -> String {
	// Whole-name special cases: dot entries and their literal lookalikes.
	match name {
		"." => return FULLWIDTH_FULL_STOP.to_string(),
		".." => return format!("{FULLWIDTH_FULL_STOP}{FULLWIDTH_FULL_STOP}"),
		"．" => return format!("{QUOTE_RUNE}{FULLWIDTH_FULL_STOP}"),
		"．．" => {
			return format!("{QUOTE_RUNE}{FULLWIDTH_FULL_STOP}{QUOTE_RUNE}{FULLWIDTH_FULL_STOP}");
		}
		// "．." would otherwise encode to "．．" (its trailing dot becomes a
		// fullwidth stop), colliding with the encoding of "..". Quote the
		// leading literal stop instead; the general decode path reverses this.
		"．." => return format!("{QUOTE_RUNE}{FULLWIDTH_FULL_STOP}{FULLWIDTH_FULL_STOP}"),
		_ => {}
	}

	// Reserved device names: replace the first letter with its fullwidth
	// variant. A name that would *decode* to a reserved name gets its first
	// character quoted instead. Reserved names are pure ASCII alphanumerics,
	// so no other encoding rule can apply to either shape.
	if is_reserved_name_on_windows(name) {
		let first = name.chars().next().unwrap();
		let fullwidth = char::from_u32(first as u32 + FULLWIDTH_OFFSET).unwrap();
		return with_first_char(name, fullwidth);
	}
	if decodes_to_reserved(name).is_some() {
		return format!("{QUOTE_RUNE}{name}");
	}

	let last_index = name
		.char_indices()
		.next_back()
		.map(|(i, _)| i)
		.unwrap_or_default();
	let mut out = String::with_capacity(name.len());
	for (i, c) in name.char_indices() {
		let first = i == 0;
		let last = i == last_index;
		if c == QUOTE_RUNE {
			out.push(QUOTE_RUNE);
			out.push(QUOTE_RUNE);
		} else if let Some(encoded) = encode_forbidden(c) {
			out.push(encoded);
		} else if (first || last) && c == ' ' {
			out.push(SYMBOL_FOR_SPACE);
		} else if last && c == '.' {
			out.push(FULLWIDTH_FULL_STOP);
		} else if ((first || last) && c == SYMBOL_FOR_SPACE)
			|| (last && c == FULLWIDTH_FULL_STOP)
			|| decode_forbidden(c).is_some()
		{
			out.push(QUOTE_RUNE);
			out.push(c);
		} else {
			out.push(c);
		}
	}
	out
}

/// Encode `name` rclone-style so it passes name validation, returning the
/// validated result. See [`encode_name_raw`] for the scheme.
///
/// The input is NFC-normalized before encoding (mirroring what validation
/// does to names that need no encoding), so [`decode_name`] applied to the
/// result returns exactly the NFC-normalized input. Fails only for the empty
/// name and for names whose encoding exceeds the byte-length limit; the
/// error carries `name` exactly as passed.
pub fn encode_name(name: &str) -> Result<ValidatedName, EntryNameError> {
	let normalized: String = name.nfc().collect();
	parse_name(&encode_name_raw(&normalized)).map_err(|err| EntryNameError {
		name: name.to_string(),
		kind: err.kind,
	})
}

/// Decode a name encoded by [`encode_name`], recovering the original
/// (NFC-normalized) name. Total and lossless over encoded names; on strings
/// that never went through [`encode_name`] it may still substitute
/// replacement characters with their plain forms or drop a dangling
/// quote rune.
pub fn decode_name(name: &str) -> String {
	// Whole-name special cases, mirroring encode_name_raw.
	match name {
		"．" => return ".".to_string(),
		"．．" => return "..".to_string(),
		"‛．" => return "．".to_string(),
		"‛．‛．" => return "．．".to_string(),
		_ => {}
	}

	// Reserved device names encoded via a fullwidth first letter.
	if let Some(decoded) = decodes_to_reserved(name) {
		return decoded;
	}

	let last_index = name
		.char_indices()
		.next_back()
		.map(|(i, _)| i)
		.unwrap_or_default();
	let mut out = String::with_capacity(name.len());
	let mut quoted = false;
	for (i, c) in name.char_indices() {
		if quoted {
			out.push(c);
			quoted = false;
		} else if c == QUOTE_RUNE {
			quoted = true;
		} else if let Some(decoded) = decode_forbidden(c) {
			out.push(decoded);
		} else if (i == 0 || i == last_index) && c == SYMBOL_FOR_SPACE {
			out.push(' ');
		} else if i == last_index && c == FULLWIDTH_FULL_STOP {
			out.push('.');
		} else {
			out.push(c);
		}
	}
	out
}

#[cfg(any(feature = "uniffi", feature = "wasm-full"))]
#[cfg_attr(feature = "uniffi", uniffi::export(name = "encodeName"))]
#[cfg_attr(
	feature = "wasm-full",
	wasm_bindgen::prelude::wasm_bindgen(js_name = "encodeName")
)]
pub fn encode_name_uniffi(name: String) -> Result<ValidatedName, EntryNameErrorJS> {
	Ok(encode_name(&name)?)
}

#[cfg(any(feature = "uniffi", feature = "wasm-full"))]
#[cfg_attr(feature = "uniffi", uniffi::export(name = "decodeName"))]
#[cfg_attr(
	feature = "wasm-full",
	wasm_bindgen::prelude::wasm_bindgen(js_name = "decodeName")
)]
pub fn decode_name_uniffi(name: String) -> String {
	decode_name(&name)
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Build the error `parse_name(name)` is expected to return.
	fn name_err(name: &str, kind: EntryNameErrorKind) -> Result<ValidatedName, EntryNameError> {
		Err(EntryNameError {
			name: name.to_string(),
			kind,
		})
	}

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
		assert_eq!(parse_name(""), name_err("", EntryNameErrorKind::Empty));
	}

	// ── Dot entries ─────────────────────────────────────────────

	#[test]
	fn dot_entries() {
		assert_eq!(parse_name("."), name_err(".", EntryNameErrorKind::DotEntry));
		assert_eq!(
			parse_name(".."),
			name_err("..", EntryNameErrorKind::DotEntry)
		);
	}

	// ── Too long ────────────────────────────────────────────────

	#[test]
	fn too_long_by_one() {
		let name = "a".repeat(MAX_BYTES + 1);
		assert_eq!(
			parse_name(&name),
			name_err(
				&name,
				EntryNameErrorKind::TooLong {
					bytes: MAX_BYTES + 1
				}
			)
		);
	}

	#[test]
	fn too_long_multibyte() {
		// Each '🎉' is 4 bytes, so 64 of them = 256 bytes > 255
		let name = "🎉".repeat(64);
		assert_eq!(
			parse_name(&name),
			name_err(&name, EntryNameErrorKind::TooLong { bytes: 256 })
		);
	}

	// ── Leading space ───────────────────────────────────────────

	#[test]
	fn leading_space() {
		assert_eq!(
			parse_name(" foo"),
			name_err(" foo", EntryNameErrorKind::LeadingSpace)
		);
		assert_eq!(
			parse_name("  bar"),
			name_err("  bar", EntryNameErrorKind::LeadingSpace)
		);
		assert_eq!(
			parse_name(" "),
			name_err(" ", EntryNameErrorKind::LeadingSpace)
		);
	}

	// ── Trailing dot or space ───────────────────────────────────

	#[test]
	fn trailing_dot() {
		assert_eq!(
			parse_name("foo."),
			name_err("foo.", EntryNameErrorKind::TrailingDotOrSpace)
		);
		assert_eq!(
			parse_name("foo.."),
			name_err("foo..", EntryNameErrorKind::TrailingDotOrSpace)
		);
	}

	#[test]
	fn trailing_space() {
		assert_eq!(
			parse_name("foo "),
			name_err("foo ", EntryNameErrorKind::TrailingDotOrSpace)
		);
		assert_eq!(
			parse_name("foo  "),
			name_err("foo  ", EntryNameErrorKind::TrailingDotOrSpace)
		);
	}

	// ── Forbidden characters ────────────────────────────────────

	#[test]
	fn forbidden_special_chars() {
		for ch in ['/', '\\', ':', '*', '?', '"', '<', '>', '|'] {
			let name = format!("file{ch}name");
			let result = parse_name(&name);
			assert!(
				matches!(
					result,
					Err(EntryNameError {
						kind: EntryNameErrorKind::ForbiddenChar { .. },
						..
					})
				),
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
				matches!(
					parse_name(&name),
					Err(EntryNameError {
						kind: EntryNameErrorKind::ForbiddenChar { .. },
						..
					})
				),
				"expected control char 0x{byte:02X} to be rejected"
			);
		}
	}

	#[test]
	fn forbidden_del() {
		let name = "file\x7Fname";
		assert!(matches!(
			parse_name(name),
			Err(EntryNameError {
				kind: EntryNameErrorKind::ForbiddenChar { .. },
				..
			})
		));
	}

	#[test]
	fn forbidden_char_reports_correct_position() {
		assert_eq!(
			parse_name("abc*def"),
			name_err(
				"abc*def",
				EntryNameErrorKind::ForbiddenChar { ch: '*', pos: 3 }
			)
		);
	}

	#[cfg(any(feature = "uniffi", feature = "wasm-full"))]
	#[test]
	fn entry_name_error_js_display_is_plain() {
		// Display must render the message as-is, not debug-quoted — this is
		// what uniffi surfaces as the exception message on mobile.
		let err = EntryNameErrorJS::from(parse_name("").unwrap_err());
		assert_eq!(err.to_string(), r#"invalid filename "": filename is empty"#);
		assert_eq!(err.to_string(), err.message());
	}

	#[test]
	fn error_reports_original_input_name() {
		// NFD input: the error should echo the caller's exact string, not the
		// NFC-normalized form that validation ran on.
		let nfd = "e\u{0301}/x";
		let err = parse_name(nfd).unwrap_err();
		assert_eq!(err.name, nfd);
		assert!(matches!(
			err.kind,
			EntryNameErrorKind::ForbiddenChar { ch: '/', .. }
		));
	}

	// ── Reserved names — all case combinations ──────────────────

	#[test]
	fn reserved_3char_all_cases() {
		for base in ["con", "prn", "aux", "nul"] {
			for variant in all_case_combinations(base) {
				assert_eq!(
					parse_name(&variant),
					name_err(&variant, EntryNameErrorKind::ReservedName),
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
					name_err(&variant, EntryNameErrorKind::ReservedName),
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
					name_err(&variant, EntryNameErrorKind::ReservedName),
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

	// ── Name encoding ───────────────────────────────────────────

	/// Assert the encoding of `input` decodes back exactly, and that the
	/// validated [`encode_name`] result decodes to the NFC-normalized input.
	fn assert_encode_round_trip(input: &str) {
		let encoded = encode_name_raw(input);
		let decoded = decode_name(&encoded);
		assert_eq!(
			decoded, input,
			"round trip failed: {input:?} -> {encoded:?} -> {decoded:?}"
		);
		if input.is_empty() || encoded.len() > MAX_BYTES {
			return;
		}
		let validated = encode_name(input)
			.unwrap_or_else(|e| panic!("encode_name({input:?}) should succeed, got {e}"));
		let nfc: String = input.nfc().collect();
		assert_eq!(
			decode_name(validated.as_ref()),
			nfc,
			"decoding validated encoding of {input:?} should yield its NFC form"
		);
	}

	#[test]
	fn encode_forbidden_chars_to_fullwidth() {
		assert_eq!(encode_name_raw("a/b"), "a／b");
		assert_eq!(encode_name_raw("a\\b"), "a＼b");
		assert_eq!(encode_name_raw("a:b"), "a：b");
		assert_eq!(encode_name_raw("a*b"), "a＊b");
		assert_eq!(encode_name_raw("a?b"), "a？b");
		assert_eq!(encode_name_raw("a\"b"), "a＂b");
		assert_eq!(encode_name_raw("a<b"), "a＜b");
		assert_eq!(encode_name_raw("a>b"), "a＞b");
		assert_eq!(encode_name_raw("a|b"), "a｜b");
	}

	#[test]
	fn encode_control_chars_to_symbols() {
		assert_eq!(encode_name_raw("a\u{0}b"), "a␀b");
		assert_eq!(encode_name_raw("a\u{1}b"), "a␁b");
		assert_eq!(encode_name_raw("a\u{1F}b"), "a␟b");
		assert_eq!(encode_name_raw("a\u{7F}b"), "a␡b");
	}

	#[test]
	fn encode_positional_space_and_dot() {
		assert_eq!(encode_name_raw(" a"), "␠a");
		assert_eq!(encode_name_raw("a "), "a␠");
		assert_eq!(encode_name_raw("a."), "a．");
		assert_eq!(encode_name_raw(" "), "␠");
		assert_eq!(encode_name_raw("  "), "␠␠");
		// Interior spaces and dots are valid and stay untouched.
		assert_eq!(encode_name_raw("a b"), "a b");
		assert_eq!(encode_name_raw("a.b"), "a.b");
	}

	#[test]
	fn encode_dot_entries() {
		assert_eq!(encode_name_raw("."), "．");
		assert_eq!(encode_name_raw(".."), "．．");
		assert_eq!(encode_name_raw("．"), "‛．");
		assert_eq!(encode_name_raw("．．"), "‛．‛．");
		assert_eq!(encode_name_raw("．."), "‛．．");
		assert_eq!(decode_name("．"), ".");
		assert_eq!(decode_name("．．"), "..");
		assert_eq!(decode_name("‛．"), "．");
		assert_eq!(decode_name("‛．‛．"), "．．");
		assert_eq!(decode_name("‛．．"), "．.");
	}

	#[test]
	fn encode_reserved_names() {
		assert_eq!(encode_name_raw("CON"), "ＣON");
		assert_eq!(encode_name_raw("con"), "ｃon");
		assert_eq!(encode_name_raw("Com1"), "Ｃom1");
		assert_eq!(encode_name_raw("LPT9"), "ＬPT9");
		assert_eq!(decode_name("ＣON"), "CON");
		// A name that would decode to a reserved name gets quoted…
		assert_eq!(encode_name_raw("ＣON"), "‛ＣON");
		assert_eq!(decode_name("‛ＣON"), "ＣON");
		// …while innocent fullwidth-letter names are untouched.
		assert_eq!(encode_name_raw("Ｃat"), "Ｃat");
		assert_eq!(decode_name("Ｃat"), "Ｃat");
		// Reserved names with extensions are valid and stay untouched.
		assert_eq!(encode_name_raw("CON.txt"), "CON.txt");
	}

	#[test]
	fn encode_quote_rune_literals() {
		assert_eq!(encode_name_raw("‛"), "‛‛");
		assert_eq!(encode_name_raw("a‛b"), "a‛‛b");
		assert_eq!(encode_name_raw("＊"), "‛＊");
		assert_eq!(encode_name_raw("␀"), "‛␀");
		assert_eq!(encode_name_raw("␡"), "‛␡");
		assert_eq!(encode_name_raw("␠"), "‛␠");
		// An interior literal symbol-for-space needs no quoting: only
		// first/last position decodes back to a space.
		assert_eq!(encode_name_raw("x␠y"), "x␠y");
	}

	#[test]
	fn encode_name_validates_output() {
		for (name, expected) in [
			("a:b", "a：b"),
			(" CON", "␠CON"),
			("CON", "ＣON"),
			("..", "．．"),
			("nul\u{7F}", "nul␡"),
		] {
			assert_eq!(encode_name(name).unwrap().as_ref(), expected);
		}
	}

	#[test]
	fn encode_name_empty() {
		assert_eq!(encode_name(""), name_err("", EntryNameErrorKind::Empty));
	}

	#[test]
	fn encode_name_too_long() {
		// 85 colons encode to 255 bytes — exactly at the limit.
		let name = ":".repeat(85);
		assert_eq!(encode_name(&name).unwrap().as_ref(), "：".repeat(85));

		// 86 colons encode to 258 bytes — over it. The error reports the
		// original name and the encoded byte length.
		let name = ":".repeat(86);
		assert_eq!(
			encode_name(&name).unwrap_err(),
			EntryNameError {
				name: name.clone(),
				kind: EntryNameErrorKind::TooLong { bytes: 258 },
			}
		);

		// A name valid on its own can still fail once encoding expands it.
		let name = format!("{}:", "a".repeat(254));
		assert!(matches!(
			encode_name(&name).unwrap_err().kind,
			EntryNameErrorKind::TooLong { bytes: 257 }
		));
	}

	#[test]
	fn encode_name_leaves_valid_names_unchanged() {
		for name in ["hello.txt", ".hidden", "file.tar.gz", "日本語.txt", "café"] {
			assert_eq!(encode_name(name).unwrap().as_ref(), name);
			assert_eq!(decode_name(name), name);
		}
	}

	#[test]
	fn decode_leaves_non_replacement_chars_alone() {
		// Arabic Presentation Forms-B (U+FEE0–FEFF, including the BOM) and
		// `｟` (U+FF5F) sit FULLWIDTH_OFFSET above forbidden control
		// characters, but the encoder never produces them — both directions
		// must leave them untouched.
		for name in [
			"a\u{FEE4}b",
			"\u{FEE0}\u{FEE1}",
			"a\u{FEFF}b",
			"a\u{FF5F}b",
			"\u{FF5F}x\u{FF60}",
			"\u{FEFF}",
		] {
			assert_eq!(encode_name_raw(name), name);
			assert_eq!(decode_name(name), name);
		}
	}

	#[test]
	fn encode_name_normalizes_before_encoding() {
		// '<' + U+0338 composes to '≮' under NFC; normalizing first means
		// the composed character needs no encoding at all.
		assert_eq!(encode_name("<\u{338}").unwrap().as_ref(), "≮");
		assert_eq!(encode_name(">\u{338}").unwrap().as_ref(), "≯");
		assert_eq!(decode_name("≮"), "≮");
		// Without a composition partner the '<' is still encoded.
		assert_eq!(encode_name("<\u{301}").unwrap().as_ref(), "＜\u{301}");
		assert_eq!(decode_name("＜\u{301}"), "<\u{301}");
	}

	#[test]
	fn encode_hand_picked_round_trips() {
		let cases = [
			"",
			"a",
			" ",
			".",
			"..",
			"...",
			"．",
			"．．",
			".．",
			"．.",
			"．..",
			"‛",
			"‛‛",
			"‛．",
			"a‛ ",
			"a‛␠",
			"a‛‛␠",
			" ‛␠",
			"‛␠x",
			"␠x",
			"‛ x",
			" ‛x",
			"CON",
			"con",
			"COM1",
			"lpt9",
			"ＣON",
			"ｃon",
			"Ｃat",
			"‛ＣON",
			"CON.txt",
			"CON ",
			" CON",
			"AUX.",
			"a:b",
			"a／b",
			"＊＊",
			"file.txt",
			"日本語.txt",
			"café",
			"e\u{301}x",
			"🎉",
			"a\u{0}b",
			"\u{1F}",
			"a b‛c．",
			"．mid．",
			"␠",
			"␠␠",
			"‛‛‛",
			"a.",
			"a..",
			"..a",
			" . ",
			" .",
			"con.",
			"con ",
			"NUL\u{7F}",
			"|",
			":",
			"?",
			"aux",
			"auxx",
			"co",
			"lpt",
			"lpt0",
			"ＡUX",
			"ｌpt5",
			"Ｎul",
			"‛ｃon",
			"Ｃon1",
			"ＣOM1",
			"\u{301}",
			"e\u{301}",
			":\u{301}",
			" \u{301}",
			"e\u{301}.",
			"\u{958}",
			"가",
			"\u{1100}\u{1161}",
			"<\u{338}",
			">\u{338}",
			"=\u{338}",
			"a\u{FEE4}b",
			"\u{FEFF}",
			"\u{FF5F}x\u{FF60}",
		];
		for case in cases {
			assert_encode_round_trip(case);
		}
	}

	#[test]
	fn encode_exhaustive_short_strings() {
		// Every string of length 0..=4 over a hostile alphabet.
		let alphabet = [
			' ', '.', '‛', '␠', '．', '＊', '*', '/', 'C', 'c', 'O', 'N', '1', '␀', '\u{0}',
		];
		let mut cases = vec![String::new()];
		for len in 1..=4 {
			let mut next = Vec::new();
			for base in cases.iter().filter(|s| s.chars().count() == len - 1) {
				for c in alphabet {
					let mut s = base.clone();
					s.push(c);
					next.push(s);
				}
			}
			cases.extend(next);
		}
		for case in &cases {
			assert_encode_round_trip(case);
		}
	}

	#[test]
	fn encode_exhaustive_reserved_all_cases() {
		let mut names = Vec::new();
		for base in ["con", "prn", "aux", "nul"] {
			names.extend(all_case_combinations(base));
		}
		for digit in b'1'..=b'9' {
			names.extend(all_case_combinations(&format!("com{}", digit as char)));
			names.extend(all_case_combinations(&format!("lpt{}", digit as char)));
		}
		for name in &names {
			assert_encode_round_trip(name);
			let encoded = encode_name_raw(name);
			assert_ne!(&encoded, name, "reserved {name:?} must be altered");
			// The fullwidth-first-letter lookalike must round-trip too.
			let first = name.chars().next().unwrap();
			let fullwidth = char::from_u32(first as u32 + FULLWIDTH_OFFSET).unwrap();
			assert_encode_round_trip(&with_first_char(name, fullwidth));
		}
	}

	#[test]
	fn encode_pseudo_random_fuzz() {
		// Deterministic xorshift so failures are reproducible.
		let alphabet = [
			'a', 'Z', '/', '\\', ':', '*', '?', '"', '<', '>', '|', '\u{0}', '\u{1}', '\u{1F}',
			'\u{7F}', ' ', '.', '‛', '␀', '␁', '␟', '␠', '␡', '．', '＊', '／', 'Ｃ', 'ｃ', 'Ａ',
			'é', '\u{301}', '日', '🎉', '~', 'C', 'O', 'N', 'P', 'L', '1', '9', '\u{338}',
			'\u{FEE4}', '\u{FEFF}', '\u{FF5F}',
		];
		let mut state = 0x9E3779B97F4A7C15u64;
		let mut next = move || {
			state ^= state << 13;
			state ^= state >> 7;
			state ^= state << 17;
			state
		};
		for _ in 0..20_000 {
			let len = (next() % 12) as usize;
			let name: String = (0..len)
				.map(|_| alphabet[(next() % alphabet.len() as u64) as usize])
				.collect();
			assert_encode_round_trip(&name);
		}
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

		/// Our validator is at least as strict as Windows: anything we accept,
		/// Windows must also accept. We may additionally reject names that
		/// Windows allows — this is intentional for cross-platform safety.
		///
		/// Windows 11 relaxed legacy device name restrictions (CON, PRN, COM1,
		/// etc. are now usable as regular filenames). Our validator still
		/// rejects them to ensure compatibility with older Windows versions.
		/// See: https://learn.microsoft.com/en-us/dotnet/standard/io/file-path-formats#handle-legacy-devices
		#[test]
		fn win_validator_is_superset_of_os_restrictions() {
			let dir = test_dir("superset");
			let names: Vec<String> = [
				// Reserved bare names (all case combos)
				"con", "prn", "aux", "nul",
			]
			.into_iter()
			.flat_map(super::all_case_combinations)
			// COM/LPT with digits 0-9
			.chain((b'0'..=b'9').flat_map(|d| {
				["COM", "com", "LPT", "lpt"]
					.into_iter()
					.map(move |p| format!("{p}{}", d as char))
			}))
			// Reserved names with extensions
			.chain(
				[
					"CON.txt", "con.txt", "PRN.log", "AUX.dat", "NUL.bin", "COM1.txt", "com1.txt",
					"LPT1.txt", "lpt1.txt",
				]
				.into_iter()
				.map(String::from),
			)
			// Not-reserved lookalikes
			.chain(
				["CONSOLE", "NULL", "COMA", "LPTA", "COM", "LPT", "CONX"]
					.into_iter()
					.map(String::from),
			)
			.collect();

			for name in &names {
				let win = windows_accepts(&dir, name);
				let us = parse_name(name).is_ok();
				assert!(
					!us || win,
					"Validator accepts {name:?} but Windows rejects it — \
					 our validator must never be more permissive than the OS"
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

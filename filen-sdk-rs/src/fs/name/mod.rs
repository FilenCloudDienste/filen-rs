pub(crate) mod keep_both;

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

pub(crate) const MAX_BYTES: usize = 255;

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
	let mut rest = name.chars();
	rest.next();
	out.push_str(rest.as_str());
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
mod tests;

//! Recovery for decrypted metadata plaintext produced by buggy sharer clients.

/// Reinterprets bytes as Latin-1, mapping every byte to the char with the
/// same code point.
///
/// The TS SDK's react-native sharer path RSA-encrypts metadata without
/// UTF-8-encoding it first (node-forge takes one byte per UTF-16 code unit),
/// so names containing U+0080..=U+00FF arrive as Latin-1 instead of UTF-8.
/// Because that bug truncates each code point to one byte, this mapping is
/// its exact inverse and losslessly restores the original string.
pub(crate) fn latin1_to_string(bytes: &[u8]) -> String {
	bytes.iter().map(|&b| char::from(b)).collect()
}

/// Replaces `\uXXXX` escapes encoding unpaired UTF-16 surrogates with the
/// escaped replacement character, returning `None` when nothing was replaced.
///
/// JS clients `JSON.stringify` names containing unpaired surrogates (legal in
/// Windows filenames) as lone `\udXXX` escapes, which `JSON.parse` accepts
/// but serde_json rejects. Substituting U+FFFD matches what TS recipients
/// effectively render once such a name leaves the JS string domain.
pub(crate) fn replace_unpaired_surrogate_escapes(s: &str) -> Option<String> {
	const REPLACEMENT: &str = "\\uFFFD";
	let bytes = s.as_bytes();
	let mut out = String::new();
	let mut last_copied = 0;
	let mut changed = false;
	let mut i = 0;
	while i < bytes.len() {
		if bytes[i] != b'\\' {
			i += 1;
			continue;
		}
		let Some(code) = unicode_escape_code(bytes, i) else {
			// any other escape (\\, \", \n, ...): skip the escaped char so a
			// following backslash is not misread as an escape introducer
			i += 2;
			continue;
		};
		if (0xD800..=0xDBFF).contains(&code) {
			if let Some(next) = unicode_escape_code(bytes, i + 6)
				&& (0xDC00..=0xDFFF).contains(&next)
			{
				// valid surrogate pair
				i += 12;
				continue;
			}
		} else if !(0xDC00..=0xDFFF).contains(&code) {
			// ordinary escape, not a surrogate
			i += 6;
			continue;
		}
		out.push_str(&s[last_copied..i]);
		out.push_str(REPLACEMENT);
		changed = true;
		i += 6;
		last_copied = i;
	}
	if !changed {
		return None;
	}
	out.push_str(&s[last_copied..]);
	Some(out)
}

/// Parses a JSON `\uXXXX` escape starting at byte `i`, returning its code
/// unit.
fn unicode_escape_code(bytes: &[u8], i: usize) -> Option<u32> {
	if bytes.len() < i + 6 || bytes[i] != b'\\' || bytes[i + 1] != b'u' {
		return None;
	}
	let hex = &bytes[i + 2..i + 6];
	if !hex.iter().all(|b| b.is_ascii_hexdigit()) {
		return None;
	}
	// SAFETY-adjacent: all-hexdigit bytes are ASCII, so from_utf8 cannot fail
	u32::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn latin1_maps_bytes_to_code_points() {
		assert_eq!(latin1_to_string(&[0x52, 0xe9]), "Ré");
	}

	#[test]
	fn surrogate_replaces_lone_high() {
		assert_eq!(
			replace_unpaired_surrogate_escapes(r#"a\ud800b"#).as_deref(),
			Some("a\\uFFFDb")
		);
	}

	#[test]
	fn surrogate_replaces_lone_low() {
		assert_eq!(
			replace_unpaired_surrogate_escapes(r#"a\udc00b"#).as_deref(),
			Some("a\\uFFFDb")
		);
	}

	#[test]
	fn surrogate_keeps_valid_pair() {
		assert_eq!(replace_unpaired_surrogate_escapes("\\ud83d\\ude00"), None);
	}

	#[test]
	fn surrogate_ignores_escaped_backslash() {
		assert_eq!(replace_unpaired_surrogate_escapes(r#"a\\ud800b"#), None);
	}

	#[test]
	fn surrogate_handles_uppercase_hex() {
		assert_eq!(
			replace_unpaired_surrogate_escapes(r#"\uD800"#).as_deref(),
			Some("\\uFFFD")
		);
	}

	#[test]
	fn surrogate_returns_none_when_clean() {
		assert_eq!(
			replace_unpaired_surrogate_escapes(r#"{"name":"plain"}"#),
			None
		);
	}

	#[test]
	fn surrogate_replaces_lone_low_but_keeps_following_pair() {
		assert_eq!(
			replace_unpaired_surrogate_escapes("\\udc00\\ud83d\\ude00").as_deref(),
			Some("\\uFFFD\\ud83d\\ude00")
		);
	}

	#[test]
	fn surrogate_trailing_backslash_is_ignored() {
		assert_eq!(replace_unpaired_surrogate_escapes(r"abc\"), None);
	}
}

#[cfg(test)]
pub(crate) mod test_support {
	use std::sync::LazyLock;

	use filen_types::crypto::rsa::RSAEncryptedString;
	use rsa::RsaPrivateKey;

	use crate::crypto::rsa::blocking_encrypt_with_public_key;

	/// Shared across test modules because RSA key generation is slow.
	/// 4096 bits matches production Filen account keys and gives OAEP-SHA512
	/// a 382-byte plaintext budget for realistic metadata payloads.
	pub(crate) static TEST_RSA_KEY: LazyLock<RsaPrivateKey> = LazyLock::new(|| {
		RsaPrivateKey::new(&mut old_rng::thread_rng(), 4096)
			.expect("failed to generate test RSA key")
	});

	pub(crate) fn rsa_encrypt(plaintext: &[u8]) -> RSAEncryptedString<'static> {
		blocking_encrypt_with_public_key(TEST_RSA_KEY.as_ref(), plaintext)
			.expect("failed to RSA-encrypt test metadata")
	}

	/// Encodes a string the way the TS SDK's react-native sharer path does:
	/// node-forge takes one byte per UTF-16 code unit (no UTF-8 encoding), so
	/// every code point is truncated to a single byte.
	pub(crate) fn latin1_bytes(s: &str) -> Vec<u8> {
		s.chars()
			.map(|c| {
				let cp = c as u32;
				assert!(cp <= 0xFF, "test string must be Latin-1 representable");
				cp as u8
			})
			.collect()
	}
}

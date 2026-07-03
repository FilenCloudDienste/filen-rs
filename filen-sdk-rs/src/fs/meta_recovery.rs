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

#[cfg(test)]
pub(crate) mod test_support {
	use std::sync::LazyLock;

	use filen_types::crypto::rsa::RSAEncryptedString;
	use rsa::RsaPrivateKey;

	use crate::crypto::rsa::blocking_encrypt_with_public_key;

	/// Shared across test modules because RSA key generation is slow in debug
	/// builds; OAEP-SHA512 needs at least a 2048-bit modulus.
	pub(crate) static TEST_RSA_KEY: LazyLock<RsaPrivateKey> = LazyLock::new(|| {
		RsaPrivateKey::new(&mut old_rng::thread_rng(), 2048)
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

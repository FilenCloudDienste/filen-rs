/// Edge-case unit tests for the crypto module.
///
/// Each test asserts the *correct, fixed* behavior.
/// Tests that cover confirmed bugs are intentionally RED until the bug is fixed.
/// Do not change a failing test to make it pass — fix the underlying code instead.
#[cfg(test)]
mod tests {
	use std::borrow::Cow;

	use filen_types::crypto::EncryptedString;

	use crate::crypto::{
		error::ConversionError,
		shared::{DataCrypter, MetaCrypter, NONCE_SIZE, TAG_SIZE},
		v2::{self, MasterKey, MasterKeys},
		v3::EncryptionKey,
	};

	#[test]
	fn test_v1_decrypt_data_too_short_for_salt_extraction() {
		use std::str::FromStr;

		let short_but_salted = b"Salted__1234567"; // 15 bytes — passes header check, underflows salt slice
		assert_eq!(short_but_salted.len(), 15);

		let key = crate::crypto::v1::FileKey::from_str("12345678901234567890123456789012")
			.expect("32-char key is valid");
		let mut data = short_but_salted.to_vec();

		// Correct behavior: returns Err, does NOT panic.
		let result = key.blocking_decrypt_data(&mut data);
		assert!(
			result.is_err(),
			"blocking_decrypt_data must return Err for a < 16-byte ciphertext, not panic"
		);
	}

	#[test]
	fn test_v3_encryption_key_from_str_non_hex_returns_err() {
		use std::str::FromStr;

		// 64 chars, all 'z' — correct length, but 'z' is not a hex digit.
		let non_hex_64 = "z".repeat(64);

		// Correct behavior: returns Err, does NOT panic.
		let result = EncryptionKey::from_str(&non_hex_64);
		assert!(
			result.is_err(),
			"EncryptionKey::from_str must return Err for a 64-char non-hex string, not panic"
		);
	}

	#[test]
	fn test_v2_master_keys_from_empty_string_returns_error() {
		let result = MasterKeys::from_decrypted_string("");
		assert!(
			result.is_err(),
			"from_decrypted_string(\"\") must return Err — an empty key string is invalid"
		);
	}

	#[test]
	fn test_v1_hash_password_determinism() {
		use crate::crypto::v1::derive_password_and_mk;

		let (mk1, pwd1) = derive_password_and_mk(b"password123").unwrap();
		let (mk2, pwd2) = derive_password_and_mk(b"password123").unwrap();
		assert_eq!(pwd1.0, pwd2.0, "V1 password hashing must be deterministic");
		assert_eq!(mk1, mk2, "V1 master key derivation must be deterministic");
	}

	#[test]
	fn test_v2_decrypt_meta_minimum_length_empty_ciphertext_returns_error() {
		use std::str::FromStr;

		let key = MasterKey::from_str(&"a".repeat(32)).expect("valid 32-char key");
		// "002" + 12 ASCII nonce bytes + zero base64 payload = minimum passing length
		let meta_str = format!("002{}", "a".repeat(NONCE_SIZE));
		assert_eq!(meta_str.len(), 3 + NONCE_SIZE);

		let meta = EncryptedString(Cow::Borrowed(&meta_str));
		let result = key.blocking_decrypt_meta(&meta);
		assert!(
			result.is_err(),
			"decrypting a zero-length ciphertext (no auth tag) must fail, not panic"
		);
	}

	#[test]
	fn test_v3_decrypt_meta_wrong_version_tag_returns_error() {
		let key = EncryptionKey::new([0xABu8; 32]);

		let fake_meta = format!("002{}", "a".repeat(NONCE_SIZE * 2 + 20));
		let meta = EncryptedString(Cow::Borrowed(&fake_meta));
		let result = key.blocking_decrypt_meta(&meta);
		assert!(
			result.is_err(),
			"V3 key must reject a '002'-tagged ciphertext with an error"
		);
		if let Err(e) = result {
			assert!(
				matches!(e, ConversionError::InvalidVersion(..)),
				"expected InvalidVersion error, got: {e:?}"
			);
		}
	}

	#[test]
	fn test_shared_encrypt_decrypt_empty_plaintext_roundtrip() {
		let key = EncryptionKey::new([0x42u8; 32]);

		let mut data: Vec<u8> = Vec::new();
		key.blocking_encrypt_data(&mut data)
			.expect("encrypting empty plaintext must succeed");

		assert_eq!(
			data.len(),
			NONCE_SIZE + TAG_SIZE,
			"empty-plaintext ciphertext must be exactly nonce({NONCE_SIZE}) + tag({TAG_SIZE}) bytes"
		);

		key.blocking_decrypt_data(&mut data)
			.expect("decrypting empty-plaintext ciphertext must succeed");

		assert!(
			data.is_empty(),
			"round-trip of empty plaintext must produce empty plaintext"
		);
	}

	#[test]
	fn test_shared_decrypt_data_too_short_returns_error() {
		let key = EncryptionKey::new([0x42u8; 32]);

		let mut data = vec![0u8; NONCE_SIZE + TAG_SIZE - 1];
		let result = key.blocking_decrypt_data(&mut data);
		assert!(
			result.is_err(),
			"decrypt_data on data shorter than nonce+tag must return Err"
		);
	}

	#[test]
	fn test_v3_derive_password_and_kek_invalid_salt_returns_error() {
		use crate::crypto::v3::derive_password_and_kek;

		let invalid_salt = "z".repeat(512);
		let result = derive_password_and_kek(b"any_password", invalid_salt.as_bytes());
		assert!(
			result.is_err(),
			"derive_password_and_kek must return Err for non-hex salt"
		);
	}

	#[test]
	fn test_v3_derive_password_and_kek_short_salt_returns_error() {
		use crate::crypto::v3::derive_password_and_kek;

		let short_salt = "ab".repeat(32); // 64 chars, not 512
		let result = derive_password_and_kek(b"any_password", short_salt.as_bytes());
		assert!(
			result.is_err(),
			"derive_password_and_kek must return Err for a salt shorter than 512 hex chars"
		);
	}

	#[test]
	fn test_v2_encrypt_decrypt_empty_meta_roundtrip() {
		use std::str::FromStr;

		let key = MasterKey::from_str(&"b".repeat(32)).expect("valid 32-char key");
		let encrypted = key.blocking_encrypt_meta("");
		let decrypted = key
			.blocking_decrypt_meta(&encrypted)
			.expect("round-trip of empty meta must succeed");
		assert_eq!(
			decrypted, "",
			"round-trip of empty meta must produce empty string"
		);
	}

	#[test]
	fn test_v3_encrypt_decrypt_empty_meta_roundtrip() {
		let key = EncryptionKey::new([0x77u8; 32]);
		let encrypted = key.blocking_encrypt_meta("");
		let decrypted = key
			.blocking_decrypt_meta(&encrypted)
			.expect("round-trip of empty V3 meta must succeed");
		assert_eq!(
			decrypted, "",
			"round-trip of empty V3 meta must produce empty string"
		);
	}

	#[test]
	fn test_v2_file_key_wrong_length_returns_error() {
		assert!(
			v2::FileKey::try_from("a".repeat(31)).is_err(),
			"31-byte key must be rejected"
		);
		assert!(
			v2::FileKey::try_from("a".repeat(33)).is_err(),
			"33-byte key must be rejected"
		);
	}
}

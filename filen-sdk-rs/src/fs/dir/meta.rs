use std::borrow::Cow;

use chrono::{DateTime, SubsecRound, Utc};
use filen_macros::js_type;
use filen_types::{
	crypto::{EncryptedString, rsa::RSAEncryptedString},
	rkyv::date_time::DateTimeUtcDef,
	traits::CowHelpers,
};
use rkyv::with::{AsOwned, Map};
use rsa::RsaPrivateKey;
use serde::{Deserialize, Serialize};

use crate::{
	crypto::{self, shared::MetaCrypter},
	error::{Error, MetadataWasNotDecryptedError},
	fs::{
		meta_recovery,
		name::{EntryNameError, ValidatedName},
	},
};
// Reuse the file-side CreatedTime so the two twins share one uniffi enum (a second
// enum of the same name would collide in the flattened uniffi namespace).
#[cfg(feature = "uniffi")]
use crate::fs::file::meta::CreatedTime;

#[derive(Debug, PartialEq, Eq, Clone, CowHelpers)]
pub enum DirectoryMeta<'a> {
	Decoded(DecryptedDirectoryMeta<'a>),
	DecryptedRaw(Cow<'a, [u8]>),
	DecryptedUTF8(Cow<'a, str>),
	Encrypted(EncryptedString<'a>),
	RSAEncrypted(RSAEncryptedString<'a>),
}

impl<'a> DirectoryMeta<'a> {
	pub(crate) fn blocking_from_encrypted(
		encrypted: EncryptedString<'a>,
		decrypter: &impl MetaCrypter,
	) -> Self {
		let Ok(decrypted) = decrypter.blocking_decrypt_meta(&encrypted) else {
			return Self::Encrypted(encrypted);
		};
		let Ok(meta) = serde_json::from_str::<DecryptedDirectoryMeta>(&decrypted) else {
			if let Some(meta) = Self::retry_with_sanitized_surrogates(&decrypted) {
				return meta;
			}
			return Self::DecryptedUTF8(Cow::Owned(decrypted));
		};
		Self::Decoded(meta.into_owned_cow())
	}

	pub(crate) fn blocking_from_rsa_encrypted(
		encrypted: RSAEncryptedString<'a>,
		decrypter: &RsaPrivateKey,
	) -> Self {
		let Ok(decrypted) = crypto::rsa::blocking_decrypt_with_private_key(decrypter, &encrypted)
		else {
			return Self::RSAEncrypted(encrypted);
		};
		let Ok(meta) = serde_json::from_slice::<DecryptedDirectoryMeta>(decrypted.as_ref()) else {
			match String::from_utf8(decrypted) {
				Ok(decrypted) => {
					if let Some(meta) = Self::retry_with_sanitized_surrogates(&decrypted) {
						return meta;
					}
					return Self::DecryptedUTF8(Cow::Owned(decrypted));
				}
				Err(err) => {
					let latin1 = meta_recovery::latin1_to_string(err.as_bytes());
					return match serde_json::from_str::<DecryptedDirectoryMeta>(&latin1) {
						Ok(meta) => Self::Decoded(meta.into_owned_cow()),
						Err(_) => Self::retry_with_sanitized_surrogates(&latin1)
							.unwrap_or_else(|| Self::DecryptedRaw(Cow::Owned(err.into_bytes()))),
					};
				}
			}
		};
		Self::Decoded(meta.into_owned_cow())
	}

	/// Retries a failed metadata JSON parse after replacing unpaired UTF-16
	/// surrogate escapes, which JS clients emit for malformed names and
	/// serde_json rejects.
	fn retry_with_sanitized_surrogates(json: &str) -> Option<DirectoryMeta<'static>> {
		let sanitized = meta_recovery::replace_unpaired_surrogate_escapes(json)?;
		let meta = serde_json::from_str::<DecryptedDirectoryMeta>(&sanitized).ok()?;
		Some(DirectoryMeta::Decoded(meta.into_owned_cow()))
	}
}

impl<'a> DirectoryMeta<'a> {
	pub fn try_to_string(&'a self) -> Option<Cow<'a, str>> {
		match self {
			// SAFETY: serializing a DecryptedDirectoryMeta always succeeds
			// - filen_types::serde::time::truncating_seconds_or_millis_opt::serialize
			//   (which delegates to time::optional::serialize) cannot fail
			// - serializing a String cannot fail
			// - serde_json::to_string always suceeds if we have string keys and serialization cannot fail
			Self::Decoded(meta) => Some(Cow::Owned(meta.to_json_string())),
			Self::DecryptedUTF8(utf8) => Some(Cow::Borrowed(utf8)),
			Self::DecryptedRaw(_) | Self::Encrypted(_) | Self::RSAEncrypted(_) => None,
		}
	}

	pub fn name(&self) -> Option<&str> {
		match self {
			Self::Decoded(meta) => Some(meta.name()),
			Self::DecryptedRaw(_)
			| Self::DecryptedUTF8(_)
			| Self::Encrypted(_)
			| Self::RSAEncrypted(_) => None,
		}
	}

	pub fn created(&self) -> Option<DateTime<Utc>> {
		match self {
			Self::Decoded(meta) => meta.created(),
			Self::DecryptedRaw(_)
			| Self::DecryptedUTF8(_)
			| Self::Encrypted(_)
			| Self::RSAEncrypted(_) => None,
		}
	}

	pub(crate) fn apply_changes(&mut self, changes: DirectoryMetaChanges) -> Result<(), Error> {
		match self {
			Self::Decoded(meta) => meta.apply_changes(changes),
			Self::DecryptedRaw(_)
			| Self::DecryptedUTF8(_)
			| Self::Encrypted(_)
			| Self::RSAEncrypted(_) => {
				// if all the metadata is being applied, we can convert to Decoded
				#[cfg(not(feature = "uniffi"))]
				let created = changes.created.ok_or(MetadataWasNotDecryptedError)?;
				#[cfg(feature = "uniffi")]
				let created = match changes.created {
					CreatedTime::Keep => return Err(MetadataWasNotDecryptedError.into()),
					CreatedTime::Unset => None,
					CreatedTime::Set(t) => Some(t),
				};
				*self = Self::Decoded(DecryptedDirectoryMeta {
					name: changes
						.name
						.map(|v| Cow::Owned(v.into()))
						.ok_or(MetadataWasNotDecryptedError)?,
					created,
				})
			}
		}
		Ok(())
	}

	pub(crate) fn borrow_with_changes(
		&'a self,
		changes: &'a DirectoryMetaChanges,
	) -> Result<Self, Error> {
		Ok(match self {
			Self::Decoded(meta) => Self::Decoded(meta.borrowed_with_changes(changes)),
			Self::DecryptedRaw(_)
			| Self::DecryptedUTF8(_)
			| Self::Encrypted(_)
			| Self::RSAEncrypted(_) => {
				let name = if let Some(name) = &changes.name {
					Cow::Borrowed(name.as_ref())
				} else {
					return Err(MetadataWasNotDecryptedError.into());
				};
				#[cfg(not(feature = "uniffi"))]
				let created = changes.created.ok_or(MetadataWasNotDecryptedError)?;
				#[cfg(feature = "uniffi")]
				let created = match &changes.created {
					CreatedTime::Keep => return Err(MetadataWasNotDecryptedError.into()),
					CreatedTime::Unset => None,
					CreatedTime::Set(t) => Some(*t),
				};
				Self::Decoded(DecryptedDirectoryMeta { name, created })
			}
		})
	}
}

#[derive(
	Debug,
	PartialEq,
	Eq,
	Clone,
	Serialize,
	Deserialize,
	CowHelpers,
	rkyv::Serialize,
	rkyv::Deserialize,
	rkyv::Archive,
)]
pub struct DecryptedDirectoryMeta<'a> {
	#[serde(borrow)]
	#[rkyv(with = AsOwned)]
	pub name: Cow<'a, str>,
	#[serde(
		with = "filen_types::serde::time::truncating_seconds_or_millis_opt",
		rename = "creation",
		default
	)]
	#[rkyv(with = Map<DateTimeUtcDef>)]
	pub created: Option<DateTime<Utc>>,
}

impl<'a> DecryptedDirectoryMeta<'a> {
	pub fn name(&self) -> &str {
		&self.name
	}

	pub fn created(&self) -> Option<DateTime<Utc>> {
		self.created
	}

	fn apply_changes(&mut self, changes: DirectoryMetaChanges) {
		if let Some(name) = changes.name {
			self.name = Cow::Owned(name.into());
		}
		#[cfg(not(feature = "uniffi"))]
		{
			if let Some(created) = changes.created {
				self.created = created;
			}
		}
		#[cfg(feature = "uniffi")]
		{
			self.created = match changes.created {
				CreatedTime::Keep => self.created,
				CreatedTime::Unset => None,
				CreatedTime::Set(t) => Some(t),
			};
		}
	}

	pub fn borrowed_with_changes(&'a self, changes: &'a DirectoryMetaChanges) -> Self {
		let created = {
			#[cfg(feature = "uniffi")]
			{
				match &changes.created {
					CreatedTime::Keep => self.created,
					CreatedTime::Unset => None,
					CreatedTime::Set(t) => Some(*t),
				}
			}
			#[cfg(not(feature = "uniffi"))]
			{
				changes.created.unwrap_or(self.created)
			}
		};
		Self {
			name: if let Some(name) = &changes.name {
				Cow::Borrowed(name.as_ref())
			} else {
				Cow::Borrowed(&self.name)
			},
			created,
		}
	}

	pub(crate) fn to_json_string(&self) -> String {
		// SAFETY: serializing a DecryptedDirectoryMeta always succeeds
		// - filen_types::serde::time::truncating_seconds_or_millis_opt::serialize
		//   (which delegates to time::optional::serialize) cannot fail
		// - serializing a String cannot fail
		// - serde_json::to_string always suceeds if we have string keys and serialization cannot fail
		serde_json::to_string(self)
			.expect("Failed to serialize directory meta (should be impossible)")
	}
}

#[derive(Default)]
#[js_type(import)]
pub struct DirectoryMetaChanges {
	#[cfg_attr(feature = "wasm-full", tsify(type = "string"), serde(default))]
	#[cfg_attr(feature = "uniffi", uniffi(default = None))]
	name: Option<ValidatedName>,
	// double option because we need to distinguish between "not set" and "set to
	// None". uniffi collapses nested nullability (T?? == T?), so it uses the
	// CreatedTime enum instead — matching the FileMetaChanges twin.
	#[cfg(not(feature = "uniffi"))]
	#[cfg_attr(
		feature = "wasm-full",
		tsify(type = "bigint | null"),
		serde(
			default,
			deserialize_with = "crate::serde::deserialize_double_option_timestamp"
		)
	)]
	created: Option<Option<DateTime<Utc>>>,
	#[cfg(feature = "uniffi")]
	created: CreatedTime,
}

impl DirectoryMetaChanges {
	pub fn name(mut self, name: &str) -> Result<Self, EntryNameError> {
		self.name = Some(ValidatedName::try_from(name)?);
		Ok(self)
	}

	pub fn created(mut self, created: Option<DateTime<Utc>>) -> Self {
		#[cfg(feature = "uniffi")]
		{
			self.created = match created {
				Some(t) => CreatedTime::Set(t.round_subsecs(3)),
				None => CreatedTime::Unset,
			};
		}
		#[cfg(not(feature = "uniffi"))]
		{
			self.created = Some(created.map(|t| t.round_subsecs(3)));
		}
		self
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::fs::meta_recovery::test_support::{TEST_RSA_KEY, latin1_bytes, rsa_encrypt};

	#[test]
	fn rsa_metadata_valid_utf8_decodes() {
		let meta = DirectoryMeta::blocking_from_rsa_encrypted(
			rsa_encrypt(r#"{"name":"Résumé"}"#.as_bytes()),
			&TEST_RSA_KEY,
		);
		assert_eq!(meta.name(), Some("Résumé"));
	}

	// The TS SDK's react-native sharer path RSA-encrypts metadata without
	// UTF-8-encoding it first, so names containing U+0080..=U+00FF arrive as
	// Latin-1. The original name must be recovered, not discarded.
	#[test]
	fn rsa_metadata_latin1_recovers_original_name() {
		let meta = DirectoryMeta::blocking_from_rsa_encrypted(
			rsa_encrypt(&latin1_bytes(r#"{"name":"Résumé"}"#)),
			&TEST_RSA_KEY,
		);
		assert_eq!(meta.name(), Some("Résumé"));
		assert_eq!(meta.created(), None);
	}

	#[test]
	fn rsa_metadata_multibyte_name_decodes() {
		let meta = DirectoryMeta::blocking_from_rsa_encrypted(
			rsa_encrypt(r#"{"name":"😀"}"#.as_bytes()),
			&TEST_RSA_KEY,
		);
		assert_eq!(meta.name(), Some("😀"));
	}

	#[test]
	fn rsa_metadata_paired_surrogate_escape_decodes() {
		let json = format!(r#"{{"name":"{}"}}"#, "\\ud83d\\ude00");
		let meta =
			DirectoryMeta::blocking_from_rsa_encrypted(rsa_encrypt(json.as_bytes()), &TEST_RSA_KEY);
		assert_eq!(meta.name(), Some("😀"));
	}

	// JS clients JSON.stringify names containing unpaired UTF-16 surrogates
	// (legal in Windows filenames) as \udXXX escapes, which JSON.parse accepts
	// but serde_json rejects. TS recipients effectively render U+FFFD, so the
	// name must survive with a replacement char instead of being discarded.
	#[test]
	fn rsa_metadata_lone_surrogate_escape_decodes_with_replacement() {
		let meta = DirectoryMeta::blocking_from_rsa_encrypted(
			rsa_encrypt(br#"{"name":"a\ud800b"}"#),
			&TEST_RSA_KEY,
		);
		assert_eq!(meta.name(), Some("a\u{FFFD}b"));
	}

	// A literal backslash-u sequence in the name (escaped backslash in JSON)
	// is not a surrogate escape and must be preserved verbatim.
	#[test]
	fn rsa_metadata_escaped_backslash_u_sequence_is_preserved() {
		let meta = DirectoryMeta::blocking_from_rsa_encrypted(
			rsa_encrypt(br#"{"name":"a\\ud800b"}"#),
			&TEST_RSA_KEY,
		);
		assert_eq!(meta.name(), Some(r"a\ud800b"));
	}

	#[test]
	fn aes_metadata_lone_surrogate_escape_decodes_with_replacement() {
		let key = crate::crypto::v3::EncryptionKey::new([0x77u8; 32]);
		let encrypted = key.blocking_encrypt_meta(r#"{"name":"a\ud800b"}"#);
		let meta = DirectoryMeta::blocking_from_encrypted(encrypted, &key);
		assert_eq!(meta.name(), Some("a\u{FFFD}b"));
	}

	// A creation timestamp written as a float or numeric string by another
	// client must not destroy the folder name; TS never even reads creation
	// for folders.
	#[test]
	fn rsa_metadata_float_creation_decodes() {
		let meta = DirectoryMeta::blocking_from_rsa_encrypted(
			rsa_encrypt(br#"{"name":"d","creation":1718999999999.4321}"#),
			&TEST_RSA_KEY,
		);
		assert_eq!(meta.name(), Some("d"));
		assert_eq!(
			meta.created(),
			DateTime::<Utc>::from_timestamp_millis(1718999999999)
		);
	}

	// Only floats (cast) and numeric strings (converted) are tolerated; a
	// non-numeric creation keeps failing the parse so bad data is not
	// silently accepted as valid.
	#[test]
	fn rsa_metadata_non_numeric_creation_fails_to_decode() {
		let meta = DirectoryMeta::blocking_from_rsa_encrypted(
			rsa_encrypt(br#"{"name":"d","creation":"soon"}"#),
			&TEST_RSA_KEY,
		);
		assert_eq!(meta.name(), None);
		assert!(matches!(meta, DirectoryMeta::DecryptedUTF8(_)));
	}

	#[test]
	fn rsa_metadata_numeric_string_creation_decodes() {
		let meta = DirectoryMeta::blocking_from_rsa_encrypted(
			rsa_encrypt(br#"{"name":"d","creation":"1718999999999"}"#),
			&TEST_RSA_KEY,
		);
		assert_eq!(meta.name(), Some("d"));
		assert_eq!(
			meta.created(),
			DateTime::<Utc>::from_timestamp_millis(1718999999999)
		);
	}

	// null for an OPTIONAL timestamp is a legitimate "no value", matching the
	// strict time::optional semantics — not a lossy fallback.
	#[test]
	fn rsa_metadata_null_creation_decodes_as_none() {
		let meta = DirectoryMeta::blocking_from_rsa_encrypted(
			rsa_encrypt(br#"{"name":"d","creation":null}"#),
			&TEST_RSA_KEY,
		);
		assert_eq!(meta.name(), Some("d"));
		assert_eq!(meta.created(), None);
	}
}

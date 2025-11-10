use std::{borrow::Cow, str::FromStr};

use filen_types::serde::rsa::RsaDerPublicKey;
use rsa::RsaPublicKey;

#[cfg(feature = "uniffi")]
uniffi::use_remote_type!(filen_types::filen_types::fs::UuidStr);

#[cfg(feature = "uniffi")]
uniffi::custom_type!(RsaPublicKey, String, {
	remote,
	lower: |key: &RsaPublicKey| RsaDerPublicKey(Cow::Borrowed(&key)).to_string(),
	try_lift: |s: &str| {
		RsaDerPublicKey::from_str(&s).map(|k| k.0.into_owned()).map_err(|e| uniffi::deps::anyhow::anyhow!("failed to parse RSA public key from string: {}", e))
	},
});

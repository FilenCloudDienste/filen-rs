pub mod rsa;
use base64::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct DerivedPassword(pub String);

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct EncodedString(pub String);

impl TryFrom<&EncodedString> for Vec<u8> {
	type Error = base64::DecodeError;
	fn try_from(value: &EncodedString) -> Result<Self, Self::Error> {
		BASE64_STANDARD.decode(&value.0)
	}
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct EncryptedString(pub String);

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct EncryptedMasterKeys(pub EncryptedString);

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct EncryptedDEK(pub EncryptedString);

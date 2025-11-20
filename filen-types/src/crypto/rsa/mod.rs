use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::traits::CowHelpers;

use super::EncryptedString;

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq, CowHelpers)]
#[serde(transparent)]
pub struct EncryptedPrivateKey<'a>(pub EncryptedString<'a>);

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq, CowHelpers)]
#[serde(transparent)]
pub struct RSAEncryptedString<'a>(pub Cow<'a, str>);

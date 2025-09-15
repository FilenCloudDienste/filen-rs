use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::impl_cow_helpers_for_newtype;

use super::EncryptedString;

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(transparent)]
pub struct EncryptedPrivateKey<'a>(pub EncryptedString<'a>);
impl_cow_helpers_for_newtype!(EncryptedPrivateKey);

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(transparent)]
pub struct RSAEncryptedString<'a>(pub Cow<'a, str>);
impl_cow_helpers_for_newtype!(RSAEncryptedString);

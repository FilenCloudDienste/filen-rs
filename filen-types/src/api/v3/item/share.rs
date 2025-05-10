use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{crypto::rsa::RSAEncryptedString, fs::ObjectType};

pub const ENDPOINT: &str = "v3/item/share";

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Request<'a> {
	pub uuid: Uuid,
	#[serde(with = "crate::serde::uuid::none")]
	pub parent: Option<Uuid>,
	pub email: Cow<'a, str>,
	pub r#type: ObjectType,
	pub metadata: Cow<'a, RSAEncryptedString>,
}

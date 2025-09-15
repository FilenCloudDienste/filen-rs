use std::borrow::Cow;

use rsa::RsaPublicKey;
use serde::{Deserialize, Serialize};

use crate::{api::v3::contacts::Contact, fs::UuidStr, traits::CowHelpers};

pub mod r#in;
pub mod out;
pub mod rename;

pub const ENDPOINT: &str = "v3/item/shared";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub uuid: UuidStr,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	pub sharing: bool,
	pub users: Vec<SharedUser<'a>>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SharedUser<'a> {
	pub id: u64,
	pub email: Cow<'a, str>,
	#[serde(with = "crate::serde::rsa::public_key_der")]
	pub public_key: RsaPublicKey,
}

impl<'a> From<&'a Contact<'a>> for SharedUser<'a> {
	fn from(contact: &'a Contact<'a>) -> Self {
		Self {
			id: contact.user_id,
			email: contact.email.as_borrowed_cow(),
			public_key: contact.public_key.clone(),
		}
	}
}

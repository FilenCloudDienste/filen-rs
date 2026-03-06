use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::{auth::AuthVersion, crypto::DerivedPassword};

pub const ENDPOINT: &str = "v3/register";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub email: Cow<'a, str>,
	pub password: DerivedPassword<'a>,
	pub salt: Cow<'a, str>, // this is not base64 or hex encoded, so probably bad practice, we should take a look at this
	pub auth_version: AuthVersion,
	pub ref_id: Option<Cow<'a, str>>,
	pub aff_id: Option<Cow<'a, str>>,
}

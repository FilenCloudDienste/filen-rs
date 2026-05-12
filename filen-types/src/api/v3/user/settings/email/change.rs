use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::{auth::AuthVersion, crypto::DerivedPassword};

pub const ENDPOINT: &str = "v3/user/settings/email/change";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub email: Cow<'a, str>,
	pub password: DerivedPassword<'a>,
	pub auth_version: AuthVersion,
}

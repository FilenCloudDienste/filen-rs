use serde::{Deserialize, Serialize};

use crate::crypto::EncryptedDEK;

pub const ENDPOINT: &str = "v3/user/dek";

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	/// `None` for accounts that have never completed a v3 login (no DEK set yet).
	pub dek: Option<EncryptedDEK<'a>>,
}

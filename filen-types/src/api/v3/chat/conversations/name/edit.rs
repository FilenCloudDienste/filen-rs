use serde::{Deserialize, Serialize};

use crate::{crypto::EncryptedString, fs::Uuid};

pub const ENDPOINT: &str = "v3/chat/conversations/name/edit";

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Request<'a> {
	pub uuid: Uuid,
	pub name: EncryptedString<'a>,
}

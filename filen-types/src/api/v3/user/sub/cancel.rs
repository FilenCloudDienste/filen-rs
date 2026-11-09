use serde::{Deserialize, Serialize};

use crate::fs::Uuid;

pub const ENDPOINT: &str = "v3/user/sub/cancel";

/// Cancels a subscription.
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Request {
	/// The subscription's id, `UserAccountSubs::id`.
	pub uuid: Uuid,
}

use serde::{Deserialize, Serialize};

use crate::api::v3::chat::last_focus_update::ChatLastFocusValues;

pub const ENDPOINT: &str = "/v3/chat/lastFocus";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response(Vec<ChatLastFocusValues>);

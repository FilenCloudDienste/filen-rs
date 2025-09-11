use serde::{Deserialize, Serialize};

pub const ENDPOINT: &str = "v3/contacts/requests/in/count";

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Response(pub u64);

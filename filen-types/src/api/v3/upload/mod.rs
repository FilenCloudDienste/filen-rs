use serde::{Deserialize, Serialize};

pub mod done;
pub mod empty;

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Response {
	pub bucket: String,
	pub region: String,
}

use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Request {
	pub uuid: uuid::Uuid,
}

impl From<Request> for reqwest::Body {
	fn from(val: Request) -> Self {
		serde_json::to_string(&val).unwrap().into()
	}
}

impl From<Request> for reqwest::blocking::Body {
	fn from(val: Request) -> Self {
		serde_json::to_string(&val).unwrap().into()
	}
}

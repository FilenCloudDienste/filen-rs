use crate::auth::{Client, unauth::UnauthClient};

pub(crate) trait SharedClient {
	fn get_unauth_client(&self) -> &UnauthClient;
}

impl SharedClient for UnauthClient {
	fn get_unauth_client(&self) -> &UnauthClient {
		self
	}
}

impl SharedClient for Client {
	fn get_unauth_client(&self) -> &UnauthClient {
		self.unauthed()
	}
}

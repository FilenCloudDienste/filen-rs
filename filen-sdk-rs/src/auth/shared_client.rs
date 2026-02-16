use crate::auth::{Client, http::AuthClient, unauth::UnauthClient};

pub(crate) trait SharedClient<C> {
	fn get_unauth_client(&self) -> &C;
}

impl SharedClient<UnauthClient> for UnauthClient {
	fn get_unauth_client(&self) -> &UnauthClient {
		self
	}
}

impl SharedClient<AuthClient> for Client {
	fn get_unauth_client(&self) -> &AuthClient {
		self.client()
	}
}

use filen_types::auth::APIKey;
use reqwest::IntoUrl;

#[derive(Default)]
pub struct UnauthClient {
	client: reqwest::Client,
}

impl UnauthorizedClient for &UnauthClient {
	fn get_client(&self) -> &reqwest::Client {
		&self.client
	}
}

pub struct AuthClient {
	client: reqwest::Client,
	api_key: APIKey,
}

impl AuthClient {
	pub fn new(api_key: APIKey) -> Self {
		Self {
			client: reqwest::Client::default(),
			api_key,
		}
	}

	pub fn new_from_client(api_key: APIKey, client: UnauthClient) -> Self {
		Self {
			client: client.client,
			api_key,
		}
	}
}

impl UnauthorizedClient for &AuthClient {
	fn get_client(&self) -> &reqwest::Client {
		&self.client
	}
}

impl AuthorizedClient for &AuthClient {
	fn get_api_key(&self) -> &APIKey {
		&self.api_key
	}
}

pub trait UnauthorizedClient {
	fn get_client(&self) -> &reqwest::Client;

	fn get_request(&self, url: impl IntoUrl) -> reqwest::RequestBuilder {
		self.get_client().get(url)
	}

	fn post_request(&self, url: impl IntoUrl) -> reqwest::RequestBuilder {
		self.get_client().post(url)
	}

	fn post_request_json(&self, url: impl IntoUrl) -> reqwest::RequestBuilder {
		self.post_request(url)
			.header("Content-Type", "application/json")
	}
}

pub trait AuthorizedClient: UnauthorizedClient {
	fn get_api_key(&self) -> &APIKey;

	fn get_auth_request(&self, url: impl IntoUrl) -> reqwest::RequestBuilder {
		self.get_request(url)
			.header("Authorization", format!("Bearer {}", self.get_api_key()))
	}

	fn post_auth_request(&self, url: impl IntoUrl) -> reqwest::RequestBuilder {
		self.post_request(url)
			.header("Authorization", format!("Bearer {}", self.get_api_key()))
	}

	fn post_auth_request_json(&self, url: impl IntoUrl) -> reqwest::RequestBuilder {
		self.post_auth_request(url)
			.header("Content-Type", "application/json")
	}
}

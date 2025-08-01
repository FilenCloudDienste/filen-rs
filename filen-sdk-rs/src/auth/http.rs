use digest::Digest;
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
	pub(crate) api_key: APIKey,
}

impl PartialEq for AuthClient {
	fn eq(&self, other: &Self) -> bool {
		self.api_key == other.api_key
	}
}
impl Eq for AuthClient {}

impl std::fmt::Debug for AuthClient {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		let api_key_str =
			faster_hex::hex_string(sha2::Sha512::digest(self.api_key.0.as_bytes()).as_ref());
		f.debug_struct("AuthClient")
			.field("api_key", &api_key_str)
			.finish()
	}
}

// not sure if this is the best way to do this
// this is to work around the fact that in tests
// this will be used across runtimes, which it doesn't like
impl Clone for AuthClient {
	fn clone(&self) -> Self {
		Self {
			client: reqwest::Client::new(),
			api_key: self.api_key.clone(),
		}
	}
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

	pub(crate) fn get_api_key(&self) -> &APIKey {
		&self.api_key
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

	async fn get_semaphore_permit(&self) -> Option<tokio::sync::SemaphorePermit> {
		None
	}
}

impl UnauthorizedClient for crate::auth::Client {
	fn get_client(&self) -> &reqwest::Client {
		&self.client().client
	}
}

impl AuthorizedClient for crate::auth::Client {
	fn get_api_key(&self) -> &APIKey {
		self.client().get_api_key()
	}

	async fn get_semaphore_permit(&self) -> Option<tokio::sync::SemaphorePermit> {
		Some(
			self.api_semaphore
				.acquire()
				.await
				.expect("Semaphore acquisition failed"),
		)
	}
}

pub(crate) trait UnauthorizedClient {
	fn get_client(&self) -> &reqwest::Client;

	fn get_request(&self, url: impl IntoUrl) -> reqwest::RequestBuilder {
		self.get_client().get(url)
	}

	fn post_request(&self, url: impl IntoUrl) -> reqwest::RequestBuilder {
		self.get_client().post(url)
	}
}

pub(crate) trait AuthorizedClient: UnauthorizedClient {
	fn get_api_key(&self) -> &APIKey;

	fn get_auth_request(&self, url: impl IntoUrl) -> reqwest::RequestBuilder {
		self.get_request(url)
			.bearer_auth(self.get_api_key().0.as_str())
	}

	fn post_auth_request(&self, url: impl IntoUrl) -> reqwest::RequestBuilder {
		self.post_request(url)
			.bearer_auth(self.get_api_key().0.as_str())
	}

	async fn get_semaphore_permit(&self) -> Option<tokio::sync::SemaphorePermit>;
}

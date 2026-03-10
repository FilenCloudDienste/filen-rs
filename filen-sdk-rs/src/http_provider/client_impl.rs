use std::sync::Arc;

use crate::{Error, auth::shared_client::SharedClient};

use super::HttpProviderHandle;

#[allow(private_bounds)]
#[allow(async_fn_in_trait)]
pub trait HttpProviderSharedClientExt: SharedClient {
	async fn start_http_provider(
		&self,
		port: Option<u16>,
	) -> Result<Arc<HttpProviderHandle>, Error> {
		let unauth_client = self.get_unauth_client();
		// ideally this wouldn't clone the client and we would get an Arc<Self> from the SharedClient
		let client = Arc::new(unauth_client.clone());

		let mut guard = unauth_client.http_provider.lock().await;
		if let Some(handle) = guard.upgrade() {
			return Ok(handle);
		}

		let handle = Arc::new(HttpProviderHandle::new(port, client).await?);

		*guard = Arc::downgrade(&handle);
		Ok(handle)
	}
}

impl<T: SharedClient> HttpProviderSharedClientExt for T {}

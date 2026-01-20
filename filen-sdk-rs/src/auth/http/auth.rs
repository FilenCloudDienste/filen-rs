use std::sync::{Arc, RwLock};

use filen_types::auth::APIKey;
use reqwest::RequestBuilder;
use tower::{Layer, Service};

trait AuthedRequest {}

#[derive(Clone, Debug)]
pub(crate) struct AuthService<'a, S> {
	inner: S,
	pub(crate) api_key: &'a Arc<RwLock<APIKey<'static>>>,
}

impl<'a, S> Service<RequestBuilder> for AuthService<'a, S>
where
	S: Service<RequestBuilder>,
{
	type Response = S::Response;
	type Error = S::Error;
	type Future = S::Future;

	fn poll_ready(
		&mut self,
		cx: &mut std::task::Context<'_>,
	) -> std::task::Poll<Result<(), Self::Error>> {
		self.inner.poll_ready(cx)
	}

	fn call(&mut self, req: RequestBuilder) -> Self::Future {
		self.inner
			.call(req.bearer_auth(self.api_key.read().unwrap_or_else(|v| v.into_inner())))
	}
}

#[derive(Clone, Debug)]
pub(crate) struct AuthLayer<'a> {
	api_key: &'a Arc<RwLock<APIKey<'static>>>,
}

impl<'a> AuthLayer<'a> {
	pub fn new(api_key: &'a Arc<RwLock<APIKey<'static>>>) -> Self {
		Self { api_key }
	}
}

impl<'a, S> Layer<S> for AuthLayer<'a> {
	type Service = AuthService<'a, S>;

	fn layer(&self, inner: S) -> Self::Service {
		AuthService {
			inner,
			api_key: self.api_key,
		}
	}
}

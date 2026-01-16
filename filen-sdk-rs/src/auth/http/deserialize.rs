use std::{
	pin::Pin,
	task::{Context, Poll},
};

use filen_types::{
	api::response::{FilenResponse, ResponseIntoData},
	error::ResponseError,
};
use serde::de::DeserializeOwned;
use tower::Service;

use crate::{Error, ErrorKind, auth::http::ResponseType};

use super::{Request, retry::RetryError};

pub(crate) struct DeserializeLayer<T> {
	_phantom: std::marker::PhantomData<T>,
}

impl<T> Clone for DeserializeLayer<T> {
	fn clone(&self) -> Self {
		Self {
			_phantom: std::marker::PhantomData,
		}
	}
}

impl<T> DeserializeLayer<T> {
	pub(crate) fn new() -> Self {
		Self {
			_phantom: std::marker::PhantomData,
		}
	}
}

impl<S, T> tower::Layer<S> for DeserializeLayer<T> {
	type Service = DeserializeService<S, T>;

	fn layer(&self, inner: S) -> Self::Service {
		DeserializeService {
			inner,
			_phantom: std::marker::PhantomData,
		}
	}
}

pub(crate) struct DeserializeService<S, T> {
	inner: S,
	_phantom: std::marker::PhantomData<T>,
}

impl<S, T> Clone for DeserializeService<S, T>
where
	S: Clone,
{
	fn clone(&self) -> Self {
		Self {
			inner: self.inner.clone(),
			_phantom: std::marker::PhantomData,
		}
	}
}

impl<S, T, Body, Url> Service<Request<Body, Url>> for DeserializeService<S, T>
where
	S: Service<Request<Body, Url>, Error = super::retry::RetryError<Error>>,
	S::Response: AsRef<[u8]>,
	S::Future: Send + 'static,
	T: DeserializeOwned,
{
	type Response = T;
	type Error = S::Error;
	// can avoid the box here with TAITs https://github.com/rust-lang/rust/issues/63063
	// once they are stable
	type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

	fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		self.inner.poll_ready(cx)
	}

	fn call(&mut self, req: Request<Body, Url>) -> Self::Future {
		let response_type = req.response_type;
		let fut = self.inner.call(req);

		Box::pin(async move {
			let resp = fut.await?;
			let bytes = resp.as_ref();

			let response: FilenResponse<T> = if response_type == ResponseType::Large {
				rmp_serde::from_slice(bytes).map_err(|e| {
					RetryError::NoRetry(Error::custom_with_source(
						ErrorKind::Response,
						e,
						Some("msgpack deserialization"),
					))
				})?
			} else {
				serde_json::from_slice(bytes).map_err(|e| {
					RetryError::NoRetry(Error::custom_with_source(
						ErrorKind::Response,
						e,
						Some("json deserialization"),
					))
				})?
			};

			match ResponseIntoData::into_data(response) {
				Ok(data) => Ok(data),
				Err(ResponseError::ApiError { message, code })
					if code.as_deref() == Some("internal_error") =>
				{
					Err(RetryError::Retry(
						ResponseError::ApiError { message, code }.into(),
					))
				}
				Err(e) => Err(RetryError::NoRetry(e.into())),
			}
		})
	}
}

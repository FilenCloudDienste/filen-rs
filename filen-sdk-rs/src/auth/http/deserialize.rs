use std::{
	marker::PhantomData,
	task::{Context, Poll},
};

use filen_types::{
	api::response::{FilenResponse, ResponseIntoData},
	error::ResponseError,
};
use serde::{Deserialize, de::DeserializeOwned};
use tower::Service;

use crate::{Error, ErrorKind, auth::http::ResponseType};

use super::{Request, retry::RetryError};

/// The gateway msgpack-encodes the same string-keyed structures as its JSON responses (uuids as
/// hyphenated strings, not 16-byte arrays), so format-sensitive types like [`uuid::Uuid`] must be
/// decoded in human-readable mode.
fn from_msgpack_slice<'de, T>(bytes: &'de [u8]) -> Result<T, rmp_serde::decode::Error>
where
	T: Deserialize<'de>,
{
	let mut deserializer = rmp_serde::Deserializer::from_read_ref(bytes).with_human_readable();
	T::deserialize(&mut deserializer)
}

pub(crate) struct DeserializeLayer<Res> {
	_phantom: std::marker::PhantomData<Res>,
}

impl<Res> Clone for DeserializeLayer<Res> {
	fn clone(&self) -> Self {
		Self {
			_phantom: self._phantom,
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

impl<S, Res> tower::Layer<S> for DeserializeLayer<Res> {
	type Service = DeserializeService<S, Res>;

	fn layer(&self, inner: S) -> Self::Service {
		DeserializeService {
			inner,
			_phantom: self._phantom,
		}
	}
}

pub(crate) struct DeserializeService<S, Res> {
	inner: S,
	_phantom: std::marker::PhantomData<Res>,
}

impl<S, Res> Clone for DeserializeService<S, Res>
where
	S: Clone,
{
	fn clone(&self) -> Self {
		Self {
			inner: self.inner.clone(),
			_phantom: self._phantom,
		}
	}
}

impl<S, Res, Body, Url> Service<Request<Body, Url>> for DeserializeService<S, Res>
where
	S: Service<Request<Body, Url>, Error = super::retry::RetryError<Error>>,
	S::Response: AsRef<[u8]>,
	Res: DeserializeOwned,
{
	type Response = Res;
	type Error = S::Error;
	type Future = DeserializeFuture<S::Future, Res>;

	fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		self.inner.poll_ready(cx)
	}

	fn call(&mut self, req: Request<Body, Url>) -> Self::Future {
		let response_type = req.response_type;
		DeserializeFuture::new(self.inner.call(req), response_type)
	}
}

#[pin_project::pin_project]
pub(crate) struct DeserializeFuture<F, Res> {
	response_type: ResponseType,
	#[pin]
	fut: Option<F>,
	_phantom: PhantomData<Res>,
}

impl<F, Res> DeserializeFuture<F, Res> {
	fn new(fut: F, response_type: ResponseType) -> Self {
		Self {
			response_type,
			fut: Some(fut),
			_phantom: PhantomData,
		}
	}
}

impl<F, Res, InRes> Future for DeserializeFuture<F, Res>
where
	F: Future<Output = Result<InRes, RetryError<Error>>>,
	InRes: AsRef<[u8]>,
	Res: DeserializeOwned,
{
	type Output = Result<Res, RetryError<Error>>;

	fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		let this = self.project();
		if let Some(fut) = this.fut.as_pin_mut() {
			match fut.poll(cx) {
				Poll::Ready(Ok(res)) => {
					let response: FilenResponse<Res> = if *this.response_type == ResponseType::Large
					{
						match from_msgpack_slice(res.as_ref()) {
							Ok(resp) => resp,
							Err(e) => {
								return Poll::Ready(Err(RetryError::NoRetry(
									Error::custom_with_source(
										ErrorKind::Response,
										e,
										Some(format!(
											"Failed to deserialize msgpack response ({} bytes)",
											res.as_ref().len()
										)),
									),
								)));
							}
						}
					} else {
						match serde_json::from_slice(res.as_ref()) {
							Ok(resp) => resp,
							Err(e) => {
								return Poll::Ready(Err(RetryError::NoRetry(
									Error::custom_with_source(
										ErrorKind::Response,
										e,
										Some(format!(
											"Failed to deserialize json response ({} bytes)",
											res.as_ref().len()
										)),
									),
								)));
							}
						}
					};

					match ResponseIntoData::into_data(response) {
						Ok(data) => Poll::Ready(Ok(data)),
						Err(ResponseError::ApiError { message, code })
							if code.as_deref() == Some("internal_error") =>
						{
							Poll::Ready(Err(RetryError::Retry(
								ResponseError::ApiError { message, code }.into(),
							)))
						}
						Err(e) => Poll::Ready(Err(RetryError::NoRetry(e.into()))),
					}
				}
				Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
				Poll::Pending => Poll::Pending,
			}
		} else {
			panic!("future already completed")
		}
	}
}

#[cfg(test)]
mod tests {
	use filen_types::{api::response::FilenResponse, fs::Uuid};
	use serde::Deserialize;

	#[derive(Deserialize, Debug)]
	struct UuidHolder {
		uuid: Uuid,
	}

	/// The live gateway sends uuids as hyphenated strings inside msgpack responses; decoding them
	/// as [`Uuid`] requires the human-readable deserializer (the default binary mode expects
	/// 16-byte arrays and rejects every real response).
	#[test]
	fn msgpack_response_decodes_string_uuids() {
		let uuid = Uuid::new_v4();
		let bytes = rmp_serde::to_vec(&serde_json::json!({
			"status": true,
			"message": "ok",
			"code": "ok",
			"data": { "uuid": uuid.to_string() },
		}))
		.unwrap();

		let response: FilenResponse<UuidHolder> = super::from_msgpack_slice(&bytes).unwrap();
		assert_eq!(response.into_data().unwrap().uuid, uuid);

		rmp_serde::from_slice::<FilenResponse<UuidHolder>>(&bytes)
			.expect_err("binary-mode decode should reject string uuids; if this now passes, the human-readable workaround may be removable");
	}
}

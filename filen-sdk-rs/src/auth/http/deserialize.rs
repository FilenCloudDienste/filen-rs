use std::{
	marker::PhantomData,
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
						match rmp_serde::from_slice(res.as_ref()) {
							Ok(resp) => resp,
							Err(e) => {
								return Poll::Ready(Err(RetryError::NoRetry(
									Error::custom_with_source(
										ErrorKind::Response,
										e,
										Some(format!(
											"Failed to deserialize msgpack response: {}",
											String::from_utf8_lossy(res.as_ref())
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
											"Failed to deserialize json response: {}",
											String::from_utf8_lossy(res.as_ref())
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

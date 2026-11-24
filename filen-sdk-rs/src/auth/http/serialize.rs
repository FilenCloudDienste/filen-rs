use std::task::{Context, Poll};

use bytes::Bytes;
use tower::Service;

use crate::Error;

use super::{Request, RequestMethod};

/// The body a stack sends for `data`: its JSON, or nothing for `()`. Serialized before the stack,
/// so the stack is the same whatever the request's type; [`SerializeLayer`] fails the request
/// with the error if it did not serialize.
pub(crate) fn serialize_body<Req: serde::Serialize>(data: &Req) -> Result<Bytes, Error> {
	Req::call(data)
}

/// Sends the body [`serialize_body`] made, or fails the request with its error, from inside the
/// stack's log layer, so the failure is logged as the request's own.
#[derive(Clone)]
pub(crate) struct SerializeLayer;

impl<S> tower::Layer<S> for SerializeLayer {
	type Service = SerializeService<S>;

	fn layer(&self, inner: S) -> Self::Service {
		SerializeService { inner }
	}
}

/// See [`SerializeLayer`].
#[derive(Clone)]
pub(crate) struct SerializeService<S> {
	inner: S,
}

impl<S, Url> Service<Request<Result<Bytes, Error>, Url>> for SerializeService<S>
where
	S: Service<Request<Bytes, Url>, Error = Error>,
{
	type Response = S::Response;
	type Error = S::Error;
	type Future = SerializeFuture<S::Future>;

	fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		self.inner.poll_ready(cx)
	}

	fn call(&mut self, req: Request<Result<Bytes, Error>, Url>) -> Self::Future {
		let method = match req.method {
			RequestMethod::Get => RequestMethod::Get,
			RequestMethod::Post(Ok(body)) => RequestMethod::Post(body),
			RequestMethod::Post(Err(e)) => return SerializeFuture::Error(Some(e)),
		};
		SerializeFuture::Inner(self.inner.call(Request {
			method,
			response_type: req.response_type,
			url: req.url,
			client: req.client,
		}))
	}
}

trait RequestCallTrait<Req> {
	fn call(data: &Req) -> Result<Bytes, Error>;
}

impl<Req> RequestCallTrait<Req> for Req
where
	Req: serde::Serialize,
{
	default fn call(data: &Req) -> Result<Bytes, Error> {
		let body = serde_json::to_vec(data)?;
		Ok(Bytes::from_owner(body))
	}
}

impl RequestCallTrait<()> for () {
	fn call(_data: &()) -> Result<Bytes, Error> {
		Ok(Bytes::new())
	}
}

#[pin_project::pin_project(project = SerializeFutureProj)]
pub(crate) enum SerializeFuture<F> {
	Inner(#[pin] F),
	Error(Option<Error>),
}

impl<F, Resp> std::future::Future for SerializeFuture<F>
where
	F: std::future::Future<Output = Result<Resp, Error>>,
{
	type Output = F::Output;

	fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		let this = self.project();
		match this {
			SerializeFutureProj::Inner(fut) => fut.poll(cx),
			SerializeFutureProj::Error(err_opt) => {
				Poll::Ready(Err(err_opt.take().expect("polled after ready")))
			}
		}
	}
}

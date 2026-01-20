use std::task::{Context, Poll};

use tower::Service;

use crate::Error;

use super::Request;

#[derive(Clone)]
pub(crate) struct SerializeLayer<'a, T> {
	data: &'a T,
}

impl<'a, T> SerializeLayer<'a, T> {
	pub(crate) fn new(data: &'a T) -> Self {
		Self { data }
	}
}

impl<'a, S, T> tower::Layer<S> for SerializeLayer<'a, T> {
	type Service = SerializeService<'a, S, T>;

	fn layer(&self, inner: S) -> Self::Service {
		SerializeService {
			inner,
			data: self.data,
		}
	}
}

#[derive(Clone)]
pub(crate) struct SerializeService<'a, S, T> {
	inner: S,
	data: &'a T,
}

impl<S, Req, Url> Service<Request<(), Url>> for SerializeService<'_, S, Req>
where
	S: Service<Request<bytes::Bytes, Url>, Error = Error>,
	Req: serde::Serialize,
{
	type Response = S::Response;
	type Error = S::Error;
	type Future = SerializeFuture<S::Future>;

	default fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		self.inner.poll_ready(cx)
	}

	default fn call(&mut self, req: Request<(), Url>) -> Self::Future {
		let body = Req::call(self.data);
		match body {
			Ok(body_bytes) => {
				let req_with_body = Request {
					method: super::RequestMethod::Post(body_bytes),
					response_type: req.response_type,
					url: req.url,
					client: req.client,
				};
				SerializeFuture::Inner(self.inner.call(req_with_body))
			}
			Err(e) => SerializeFuture::Error(Some(Error::from(e))),
		}
	}
}

trait RequestCallTrait<Req> {
	fn call(data: &Req) -> Result<bytes::Bytes, Error>;
}

impl<Req> RequestCallTrait<Req> for Req
where
	Req: serde::Serialize,
{
	default fn call(data: &Req) -> Result<bytes::Bytes, Error> {
		let body = serde_json::to_vec(data)?;
		Ok(bytes::Bytes::from_owner(body))
	}
}

impl RequestCallTrait<()> for () {
	fn call(_data: &()) -> Result<bytes::Bytes, Error> {
		Ok(bytes::Bytes::new())
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

use std::task::{Context, Poll};

use tower::Service;

use crate::{Error, ErrorKind};

use super::Request;

#[derive(Clone)]
pub(crate) struct UrlParseLayer;

impl<S> tower::Layer<S> for UrlParseLayer {
	type Service = UrlParseService<S>;

	fn layer(&self, inner: S) -> Self::Service {
		UrlParseService { inner }
	}
}

#[derive(Clone)]
pub(crate) struct UrlParseService<S> {
	inner: S,
}

impl<S, Body, Url> Service<Request<Body, Url>> for UrlParseService<S>
where
	S: Service<Request<Body, reqwest::Url>, Error = Error>,
	Url: AsRef<str>,
{
	type Response = S::Response;
	type Error = S::Error;
	type Future = UrlParseFuture<S::Future>;

	fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		self.inner.poll_ready(cx)
	}

	fn call(&mut self, req: Request<Body, Url>) -> Self::Future {
		let url_str = req.url.as_ref();
		match reqwest::Url::parse(url_str) {
			Ok(url) => {
				let req_with_url = Request {
					method: req.method,
					response_type: req.response_type,
					url,
					client: req.client,
				};
				UrlParseFuture::Inner(self.inner.call(req_with_url))
			}
			Err(e) => UrlParseFuture::Error(Some(Error::custom_with_source(
				ErrorKind::Reqwest,
				e,
				Some(format!("parsing URL '{url_str}'")),
			))),
		}
	}
}

#[pin_project::pin_project(project = UrlParseFutureProj)]
pub(crate) enum UrlParseFuture<F> {
	Inner(#[pin] F),
	Error(Option<Error>),
}

impl<F, Resp> std::future::Future for UrlParseFuture<F>
where
	F: std::future::Future<Output = Result<Resp, Error>>,
{
	type Output = F::Output;

	fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		let this = self.project();
		match this {
			UrlParseFutureProj::Inner(fut) => fut.poll(cx),
			UrlParseFutureProj::Error(err_opt) => {
				Poll::Ready(Err(err_opt.take().expect("polled after ready")))
			}
		}
	}
}

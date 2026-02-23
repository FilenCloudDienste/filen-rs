use std::{
	borrow::Cow,
	pin::Pin,
	task::{Context, Poll},
};

use tower::Service;

use crate::Error;

// future improvement here would be using a Rc/'static str for endpoint to avoid allocations for Strings
#[derive(Clone)]
pub(crate) struct LogLayer {
	level_filter: log::LevelFilter,
	endpoint: Cow<'static, str>,
}

impl LogLayer {
	pub fn new(level_filter: log::LevelFilter, endpoint: impl Into<Cow<'static, str>>) -> Self {
		Self {
			level_filter,
			endpoint: endpoint.into(),
		}
	}
}

impl<S> tower::Layer<S> for LogLayer {
	type Service = LogService<S>;

	fn layer(&self, inner: S) -> Self::Service {
		LogService {
			inner,
			level_filter: self.level_filter,
			endpoint: self.endpoint.clone(),
		}
	}
}

#[derive(Clone)]
pub(crate) struct LogService<S> {
	inner: S,
	level_filter: log::LevelFilter,
	endpoint: Cow<'static, str>,
}

impl<S, Req> Service<Req> for LogService<S>
where
	S: Service<Req, Error = Error>,
	S::Response: std::fmt::Debug,
	Req: std::fmt::Debug,
{
	type Response = S::Response;
	type Error = S::Error;
	type Future = LoggedFuture<S::Future>;

	fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		match self.inner.poll_ready(cx) {
			Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
			Poll::Ready(Err(e)) => {
				let e = e.with_context(self.endpoint.clone());
				if self.level_filter >= log::LevelFilter::Error {
					log::error!("call to {} poll_ready error: {}", self.endpoint, e);
				}
				Poll::Ready(Err(e))
			}
			Poll::Pending => Poll::Pending,
		}
	}

	fn call(&mut self, req: Req) -> Self::Future {
		if self.level_filter >= log::LevelFilter::Trace {
			log::debug!("calling {} with ", self.endpoint);
		}
		LoggedFuture {
			inner: self.inner.call(req),
			filter: self.level_filter,
			endpoint: self.endpoint.clone(),
		}
	}
}

#[pin_project::pin_project]
pub struct LoggedFuture<F> {
	#[pin]
	inner: F,
	filter: log::LevelFilter,
	endpoint: Cow<'static, str>,
}

impl<F, Res> Future for LoggedFuture<F>
where
	F: Future<Output = Result<Res, Error>>,
	Res: std::fmt::Debug,
{
	type Output = F::Output;

	fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		let this = self.project();
		match this.inner.poll(cx) {
			Poll::Ready(output) => match output {
				Ok(res) => {
					if *this.filter >= log::LevelFilter::Trace {
						log::trace!(
							"call to {} succeeded with response: {:?}",
							this.endpoint,
							res
						);
					}
					Poll::Ready(Ok(res))
				}
				Err(e) => {
					let e = e.with_context(this.endpoint.clone());
					if *this.filter >= log::LevelFilter::Error {
						log::error!("call to {} error: {}", this.endpoint, e);
					}
					Poll::Ready(Err(e))
				}
			},
			Poll::Pending => Poll::Pending,
		}
	}
}

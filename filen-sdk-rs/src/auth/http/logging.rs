use std::{
	borrow::Cow,
	pin::Pin,
	sync::atomic::{AtomicU64, Ordering},
	task::{Context, Poll},
};

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use std::time::Instant;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasmtimer::std::Instant;

use tower::Service;

use crate::Error;

/// Monotonic per-request id, used to correlate the (possibly retried) log records of a single
/// HTTP request as it crosses async boundaries and the retry/rate-limit layers.
static REQUEST_ID: AtomicU64 = AtomicU64::new(0);

// future improvement here would be using a Rc/'static str for endpoint to avoid allocations for Strings
#[derive(Clone)]
pub(crate) struct LogLayer {
	endpoint: Cow<'static, str>,
}

impl LogLayer {
	pub fn new(endpoint: impl Into<Cow<'static, str>>) -> Self {
		Self {
			endpoint: endpoint.into(),
		}
	}
}

impl<S> tower::Layer<S> for LogLayer {
	type Service = LogService<S>;

	fn layer(&self, inner: S) -> Self::Service {
		LogService {
			inner,
			endpoint: self.endpoint.clone(),
		}
	}
}

#[derive(Clone)]
pub(crate) struct LogService<S> {
	inner: S,
	endpoint: Cow<'static, str>,
}

impl<S, Req> Service<Req> for LogService<S>
where
	S: Service<Req, Error = Error>,
{
	type Response = S::Response;
	type Error = S::Error;
	type Future = LoggedFuture<S::Future>;

	fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		match self.inner.poll_ready(cx) {
			Poll::Ready(Err(e)) => {
				let e = e.with_context(self.endpoint.clone());
				tracing::warn!(endpoint = %self.endpoint, error = %e, "poll_ready failed");
				Poll::Ready(Err(e))
			}
			other => other,
		}
	}

	fn call(&mut self, req: Req) -> Self::Future {
		// An info-level span so the in-flight watchdog tracks the request at the default level
		// (this is the "still running after Ns" hang signal). The span emits nothing on its own:
		// success latency is the fmt layer's busy/idle on close, failures are the warn! below.
		let span = tracing::info_span!(
			"http_request",
			endpoint = %self.endpoint,
			request_id = REQUEST_ID.fetch_add(1, Ordering::Relaxed),
		);
		LoggedFuture {
			inner: self.inner.call(req),
			span,
			endpoint: self.endpoint.clone(),
			started: Instant::now(),
		}
	}
}

#[pin_project::pin_project]
pub struct LoggedFuture<F> {
	#[pin]
	inner: F,
	span: tracing::Span,
	endpoint: Cow<'static, str>,
	started: Instant,
}

impl<F, Res> Future for LoggedFuture<F>
where
	F: Future<Output = Result<Res, Error>>,
{
	type Output = F::Output;

	fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		let this = self.project();
		// Make the request span current so events from the inner layers nest under it, and so the
		// fmt layer accumulates busy (poll) vs idle (await) time for this request.
		let _enter = this.span.enter();
		match this.inner.poll(cx) {
			Poll::Ready(Ok(res)) => Poll::Ready(Ok(res)),
			Poll::Ready(Err(e)) => {
				let e = e.with_context(this.endpoint.clone());
				tracing::warn!(
					elapsed_ms = this.started.elapsed().as_millis() as u64,
					error = %e,
					"request failed",
				);
				Poll::Ready(Err(e))
			}
			Poll::Pending => Poll::Pending,
		}
	}
}

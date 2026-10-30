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

use crate::{Error, ErrorKind};

/// Whether a failed request's error is the server's not-found answer about an item. Probes expect
/// it (`ResultExt::optional`), and a caller for whom it is a failure reports it itself, so it is
/// logged at debug; every other failure stays at warn.
pub(crate) fn is_not_found_answer(e: &Error) -> bool {
	matches!(
		e.kind(),
		ErrorKind::FileNotFound | ErrorKind::FolderNotFound
	)
}

/// A failed request's log line: `warn!`, or `debug!` for a not-found answer (see
/// [`is_not_found_answer`]).
macro_rules! log_request_failure {
	($error:expr, $($arg:tt)+) => {
		if $crate::auth::http::logging::is_not_found_answer($error) {
			tracing::debug!($($arg)+)
		} else {
			tracing::warn!($($arg)+)
		}
	};
}
pub(crate) use log_request_failure;

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
		// success latency is the fmt layer's busy/idle on close, failures are logged below.
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
				log_request_failure!(
					&e,
					elapsed_ms = u64::try_from(this.started.elapsed().as_millis()).unwrap_or(u64::MAX),
					error = %e,
					"request failed",
				);
				Poll::Ready(Err(e))
			}
			Poll::Pending => Poll::Pending,
		}
	}
}

#[cfg(test)]
mod tests {
	use std::sync::{Arc, Mutex};

	use filen_types::error::ResponseError;
	use tracing::{Level, Subscriber};
	use tracing_subscriber::{
		layer::{Context, Layer, SubscriberExt},
		registry,
	};

	use super::*;

	/// Records the level of every event.
	struct Levels(Arc<Mutex<Vec<Level>>>);

	impl<S: Subscriber> Layer<S> for Levels {
		fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
			self.0
				.lock()
				.expect("no test thread panics holding it")
				.push(*event.metadata().level());
		}
	}

	/// The levels a request failing with the server's `code` is logged at.
	fn logged_levels(code: &str) -> Vec<Level> {
		let levels = Arc::new(Mutex::new(Vec::new()));
		let subscriber = registry().with(Levels(Arc::clone(&levels)));
		let error = Error::from(ResponseError::ApiError {
			message: None,
			code: Some(code.to_owned()),
		});
		tracing::subscriber::with_default(subscriber, || {
			let failed = LoggedFuture {
				inner: std::future::ready(Err::<(), _>(error)),
				span: tracing::Span::none(),
				endpoint: "v3/dir".into(),
				started: Instant::now(),
			};
			assert!(futures::executor::block_on(failed).is_err());
		});
		Arc::into_inner(levels)
			.expect("the subscriber is gone")
			.into_inner()
			.expect("no test thread panics holding it")
	}

	#[test]
	fn a_not_found_answer_is_logged_at_debug() {
		assert_eq!(logged_levels("folder_not_found"), [Level::DEBUG]);
		assert_eq!(logged_levels("invalid_folder"), [Level::DEBUG]);
		assert_eq!(logged_levels("file_not_found"), [Level::DEBUG]);
	}

	#[test]
	fn every_other_failure_is_still_logged_at_warn() {
		assert_eq!(logged_levels("internal_error"), [Level::WARN]);
		assert_eq!(logged_levels("api_key_not_found"), [Level::WARN]);
	}
}

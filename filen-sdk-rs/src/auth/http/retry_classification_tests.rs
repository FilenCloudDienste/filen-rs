#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use std::io;

use reqwest::StatusCode;

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use super::dead_connection_in_chain;
use super::{error_chain_mentions, is_attempt_retryable};

/// A permanent 4xx (the egest `404 Not Found` for a missing chunk) must NOT be retried — this
/// is the regression that hung the nightly test job forever.
#[test]
fn permanent_4xx_status_is_not_retried() {
	for status in [
		StatusCode::NOT_FOUND,
		StatusCode::FORBIDDEN,
		StatusCode::BAD_REQUEST,
		StatusCode::UNAUTHORIZED,
		StatusCode::GONE,
	] {
		assert!(
			!is_attempt_retryable(Some(status), false, false, false),
			"{status} should not be retried"
		);
	}
}

/// Transient statuses stay retryable.
#[test]
fn transient_statuses_are_retried() {
	for status in [
		StatusCode::INTERNAL_SERVER_ERROR,
		StatusCode::BAD_GATEWAY,
		StatusCode::SERVICE_UNAVAILABLE,
		StatusCode::GATEWAY_TIMEOUT,
		StatusCode::REQUEST_TIMEOUT,
		StatusCode::TOO_MANY_REQUESTS,
	] {
		assert!(
			is_attempt_retryable(Some(status), false, false, false),
			"{status} should be retried"
		);
	}
}

/// Transport errors (no HTTP status) keep the prior behavior: a builder or request error may
/// have been partially sent and is not retryable; any other transport error is.
#[test]
fn transport_errors_without_status_keep_prior_behavior() {
	assert!(!is_attempt_retryable(None, true, false, false)); // builder error
	assert!(!is_attempt_retryable(None, false, true, false)); // request error (maybe sent)
	assert!(is_attempt_retryable(None, false, false, false)); // connect/decode error
}

/// A dead pooled connection (`DispatchGone`) failed before the request was written, so it is
/// retryable even though it surfaces as a request error.
#[test]
fn dispatch_gone_is_always_retryable() {
	assert!(is_attempt_retryable(None, false, true, true));
}

#[derive(Debug)]
struct Layered {
	msg: &'static str,
	source: Option<Box<Layered>>,
}

impl std::fmt::Display for Layered {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(self.msg)
	}
}

impl std::error::Error for Layered {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		self.source
			.as_ref()
			.map(|s| s.as_ref() as &(dyn std::error::Error + 'static))
	}
}

/// Mirrors the reqwest → hyper-util → hyper layering: the `DispatchGone` marker is only on the
/// innermost error, so the walk must descend the whole `source` chain.
#[test]
fn matches_needle_deep_in_the_source_chain() {
	let err = Layered {
		msg: "error sending request for url (https://example/v3/dir/create)",
		source: Some(Box::new(Layered {
			msg: "client error (SendRequest)",
			source: Some(Box::new(Layered {
				msg: "dispatch task is gone",
				source: None,
			})),
		})),
	};
	assert!(error_chain_mentions(
		&err,
		&["dispatch task is gone", "runtime dropped the dispatch task"]
	));
	assert!(!error_chain_mentions(&err, &["connection refused"]));
}

/// The kinds that mark a connection dead during the request, found directly and
/// below an outer error the way hyper nests the socket error. `AddrNotAvailable` is the
/// EADDRNOTAVAIL the macOS runners' lock polls failed with; `TimedOut` stays out so a stalled
/// host keeps failing fast.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[test]
fn dead_connection_kinds_are_found_through_the_chain() {
	#[derive(Debug)]
	struct Outer(io::Error);

	impl std::fmt::Display for Outer {
		fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
			f.write_str("client error (SendRequest)")
		}
	}

	impl std::error::Error for Outer {
		fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
			Some(&self.0)
		}
	}

	for kind in [
		io::ErrorKind::ConnectionReset,
		io::ErrorKind::BrokenPipe,
		io::ErrorKind::ConnectionAborted,
		io::ErrorKind::NotConnected,
		io::ErrorKind::AddrNotAvailable,
		io::ErrorKind::UnexpectedEof,
	] {
		assert_eq!(
			dead_connection_in_chain(&io::Error::from(kind)).map(io::Error::kind),
			Some(kind),
			"{kind:?} must mark the connection dead"
		);
		assert_eq!(
			dead_connection_in_chain(&Outer(io::Error::from(kind))).map(io::Error::kind),
			Some(kind),
			"{kind:?} must be found below an outer error"
		);
	}
	for kind in [
		io::ErrorKind::TimedOut,
		io::ErrorKind::ConnectionRefused,
		io::ErrorKind::Other,
	] {
		assert!(
			dead_connection_in_chain(&io::Error::from(kind)).is_none(),
			"{kind:?} must not mark the connection dead"
		);
	}
}

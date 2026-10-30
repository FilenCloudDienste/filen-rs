use tokio::net::TcpListener;

use super::{ClientConfig, is_attempt_retryable, is_pre_send_connect_failure, retry};

/// A connect that fails before any byte is sent must reach the retry layer as the backoff
/// class. Modelled by a refused connect rather than the failed DNS lookup that motivated the
/// class (2026-09-17 nightly, three tests panicked on `EAI_NONAME`): both produce a
/// connect-kind error with no timeout, but only this one is deterministic offline — a test
/// that depends on a name failing to resolve depends on whatever resolver CI happens to have.
#[tokio::test]
async fn refused_connect_is_classified_as_pre_send_connect() {
	let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let addr = listener.local_addr().unwrap();
	// Free the port again: nothing is listening, so the connect is refused immediately.
	drop(listener);

	let client = ClientConfig::default().build_reqwest_client().unwrap();
	let err = client
		.get(format!("http://{addr}/"))
		.send()
		.await
		.expect_err("a connect to a closed port must fail");

	assert!(err.is_connect(), "expected a connect error, got {err:?}");
	assert!(!err.is_timeout(), "it is refused, not stalled");
	assert!(is_pre_send_connect_failure(&err));
	assert!(
		!is_attempt_retryable(err.status(), err.is_builder(), err.is_request(), false),
		"the general classifier still refuses it — which is exactly why this class exists"
	);

	match super::execute_request(
		client.get(format!("http://{addr}/")),
		std::sync::Arc::default(),
	)
	.await
	{
		Err(retry::RetryError::RetryAfterBackoff(_)) => {}
		Err(other) => panic!("must classify as RetryAfterBackoff, got {other:?}"),
		Ok(_) => panic!("a connect to a closed port must not succeed"),
	}
}

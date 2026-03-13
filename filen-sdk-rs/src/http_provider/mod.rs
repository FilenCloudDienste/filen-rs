use std::{ops::Bound, sync::Arc};

use axum::{
	extract::{Query, State},
	response::{IntoResponse, Response},
	routing::get,
};
use axum_extra::{TypedHeader, headers::Range};
use base64::{Engine, prelude::BASE64_URL_SAFE_NO_PAD};
use futures::AsyncReadExt;
use http::{
	StatusCode,
	header::{CONTENT_LENGTH, CONTENT_TYPE},
	response,
};
use serde::Deserialize;

use crate::{
	Error,
	auth::unauth::UnauthClient,
	fs::file::{enums::RemoteFileType, read::FileReaderBuilder},
	io::HasFileInfo,
};

pub mod client_impl;
#[cfg(feature = "uniffi")]
mod js_impl;

#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct HttpProviderHandle {
	task: Option<tokio::task::JoinHandle<()>>,
	cancel_sender: Option<tokio::sync::oneshot::Sender<()>>,
	port: u16,
}

impl Drop for HttpProviderHandle {
	fn drop(&mut self) {
		if let Some(cancel_sender) = self.cancel_sender.take() {
			let _ = cancel_sender.send(());
		}

		if let Some(task) = self.task.take() {
			match tokio::runtime::Handle::try_current() {
				Ok(runtime_handle) => {
					runtime_handle.spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                        if !task.is_finished() {
                            log::error!("HTTPProviderCanceller was dropped but the task is still running after 10 seconds. Forcing abort.");
                            task.abort();
                        }
                    });
				}
				Err(_) => {
					std::thread::spawn(move || {
						std::thread::sleep(std::time::Duration::from_secs(10));
						if !task.is_finished() {
							log::error!(
								"HTTPProviderCanceller was dropped but the task is still running after 10 seconds. Forcing abort."
							);
							task.abort();
						}
					});
				}
			}
		}
	}
}

impl HttpProviderHandle {
	pub(crate) async fn new(port: Option<u16>, client: Arc<UnauthClient>) -> Result<Self, Error> {
		let router = axum::Router::new()
			.route("/file", get(file_handler))
			.with_state(ProviderState { client });

		let (port_sender, port_receiver) = tokio::sync::oneshot::channel();
		let (cancel_sender, cancel_receiver) = tokio::sync::oneshot::channel();

		let task =
			tokio::task::spawn(async move {
				let listener =
					match tokio::net::TcpListener::bind(("127.0.0.1", port.unwrap_or_default()))
						.await
					{
						Ok(listener) => listener,
						Err(e) => {
							let _ = port_sender.send(Err(e));
							return;
						}
					};

				let _ = port_sender.send(listener.local_addr().map(|addr| addr.port()));
				axum::serve(listener, router)
					.with_graceful_shutdown(async {
						let _ = cancel_receiver.await;
					})
					.await
					.expect("Failed to start HTTP server");
			});
		let port = port_receiver.await.unwrap()?;

		Ok(Self {
			task: Some(task),
			cancel_sender: Some(cancel_sender),
			port,
		})
	}
}

#[cfg_attr(feature = "uniffi", uniffi::export)]
impl HttpProviderHandle {
	pub fn port(&self) -> u16 {
		self.port
	}
}

impl HttpProviderHandle {
	/// Returns the local HTTP URL that serves the given file through this provider.
	///
	/// The URL encodes the file metadata as a msgpack+base64url query parameter so
	/// that the server can reconstruct the file reader without any additional RPC.
	pub fn get_file_url(&self, file: &RemoteFileType<'_>) -> String {
		let msgpack = rmp_serde::to_vec(&file)
			.expect("RemoteFile serialization to msgpack should never fail");
		let encoded = BASE64_URL_SAFE_NO_PAD.encode(&msgpack);

		format!("http://127.0.0.1:{}/file?file={}", self.port, encoded)
	}
}

#[cfg(feature = "uniffi")]
#[uniffi::export]
impl HttpProviderHandle {
	#[uniffi::method(name = "getFileUrl")]
	pub fn get_file_url_uniffi(&self, file: crate::js::AnyFile) -> Result<String, Error> {
		Ok(self.get_file_url(&file.try_into()?))
	}
}

#[derive(Clone)]
struct ProviderState {
	client: Arc<UnauthClient>,
}

mod custom_serde {
	use base64::{Engine, prelude::BASE64_URL_SAFE_NO_PAD};
	use serde::Deserializer;

	use crate::fs::file::enums::RemoteFileType;

	pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<RemoteFileType<'static>, D::Error>
	where
		D: Deserializer<'de>,
	{
		let string = filen_types::serde::cow::deserialize(deserializer)?;
		let bytes = BASE64_URL_SAFE_NO_PAD
			.decode(string.as_ref())
			.map_err(serde::de::Error::custom)?;
		rmp_serde::from_slice::<RemoteFileType<'static>>(&bytes).map_err(serde::de::Error::custom)
	}
}

#[derive(Deserialize)]
struct FileQuery {
	#[serde(deserialize_with = "custom_serde::deserialize")]
	file: RemoteFileType<'static>,
}

fn get_real_bounds(file: &RemoteFileType, (start, end): (Bound<u64>, Bound<u64>)) -> (u64, u64) {
	let start = match start {
		Bound::Included(start) => start,
		Bound::Excluded(start) => start + 1,
		Bound::Unbounded => 0,
	};
	let end = match end {
		Bound::Included(end) => end + 1,
		Bound::Excluded(end) => end,
		Bound::Unbounded => u64::MAX,
	}
	.min(file.size());
	(start, end)
}

fn single_range_response_builder(
	file: RemoteFileType<'static>,
	(start, end): (u64, u64),
	client: Arc<UnauthClient>,
	mut builder: response::Builder,
) -> Result<Response, http::Error> {
	let size = file.size();

	let status = if start == 0 && end == size {
		StatusCode::OK
	} else {
		StatusCode::PARTIAL_CONTENT
	};

	builder = builder
		.status(status)
		.header(CONTENT_LENGTH, end.saturating_sub(start).to_string())
		.header(
			CONTENT_TYPE,
			file.mime().unwrap_or("application/octet-stream"),
		);

	let stream = async_stream::stream! {
		let mut reader = FileReaderBuilder::new(&client, &file)
			.with_start(start)
			.with_end(end)
			.build();
		let mut buf = vec![0; 8192];
		loop {
			match reader.read(&mut buf).await {
				Ok(0) => break, // EOF
				Ok(n) => yield Ok(buf[..n].to_vec()),
				Err(e) => {
					yield Err(e);
				}
			}
		}
	};

	if status == StatusCode::PARTIAL_CONTENT {
		let content_range_value = format!("bytes {}-{}/{}", start, end - 1, size);
		builder = builder.header(http::header::CONTENT_RANGE, content_range_value);
	}
	builder.body(axum::body::Body::from_stream(stream))
}

fn multiple_range_response_builder(
	file: RemoteFileType<'static>,
	ranges: Vec<(u64, u64)>,
	client: Arc<UnauthClient>,
	mut builder: response::Builder,
) -> Result<Response, http::Error> {
	let boundary = BASE64_URL_SAFE_NO_PAD.encode(rand::random::<[u8; 16]>());
	builder = builder.status(StatusCode::PARTIAL_CONTENT).header(
		CONTENT_TYPE,
		format!("multipart/byteranges; boundary={}", boundary),
	);

	let stream = async_stream::stream! {
		let mime = file.mime().unwrap_or("application/octet-stream");
		let mut first = true;
		for (start, end) in ranges {
			// RFC 2046 §5.1: encapsulation boundary is CRLF + "--" + boundary.
			// The first boundary has no leading CRLF (it is the dash-boundary itself).
			let prefix = if first { "" } else { "\r\n" };
			first = false;
			let header = format!(
				"{prefix}--{boundary}\r\nContent-Type: {mime}\r\nContent-Range: bytes {start}-{end_incl}/{size}\r\n\r\n",
				end_incl = end.saturating_sub(1),
				size = file.size(),
			);
			yield Ok(header.into_bytes());
			let mut reader = FileReaderBuilder::new(&client, &file)
				.with_start(start)
				.with_end(end)
				.build();
			let mut buf = vec![0; 8192];
			loop {
				match reader.read(&mut buf).await {
					Ok(0) => break, // EOF
					Ok(n) => yield Ok(buf[..n].to_vec()),
					Err(e) => {
						yield Err(e);
					}
				}
			}
		}
		// RFC 2046 §5.1: close-delimiter is CRLF + "--" + boundary + "--".
		yield Ok(format!("\r\n--{boundary}--\r\n").into_bytes());
	};

	builder.body(axum::body::Body::from_stream(stream))
}

async fn file_handler(
	Query(params): Query<FileQuery>,
	State(state): State<ProviderState>,
	range: Option<TypedHeader<Range>>,
) -> impl IntoResponse {
	let ranges = if let Some(TypedHeader(range)) = range {
		range
			.satisfiable_ranges(params.file.size())
			.filter_map(|r| {
				let (start, end) = get_real_bounds(&params.file, r);
				if start < end {
					Some((start, end))
				} else {
					None
				}
			})
			.collect::<Vec<_>>()
	} else {
		vec![(0, params.file.size())]
	};

	let response_builder = http::Response::builder().header(http::header::ACCEPT_RANGES, "bytes");

	match ranges {
		ranges if let [range] = *ranges => single_range_response_builder(
			params.file,
			range,
			state.client.clone(),
			response_builder,
		),
		ranges if ranges.is_empty() => response_builder
			.status(StatusCode::RANGE_NOT_SATISFIABLE)
			.header(
				http::header::CONTENT_RANGE,
				format!("bytes */{}", params.file.size()),
			)
			.body(axum::body::Body::empty()),

		ranges => multiple_range_response_builder(
			params.file,
			ranges,
			state.client.clone(),
			response_builder,
		),
	}
	.unwrap_or_else(|e| {
		http::Response::builder()
			.status(StatusCode::INTERNAL_SERVER_ERROR)
			.body(axum::body::Body::from(format!(
				"Error building response: {e}"
			)))
			.expect("should always be able to build a response")
	})
}

/// Unit tests for http_provider logic that do not require network credentials.
///
/// These tests use the `http-provider` feature flag and a local Tokio runtime.
/// They document and guard behavioral contracts of the provider lifecycle.
#[cfg(all(test, feature = "http-provider"))]
mod tests {
	use crate::{
		auth::{http::ClientConfig, unauth::UnauthClient},
		http_provider::client_impl::HttpProviderSharedClientExt,
	};

	/// When `start_http_provider` is called a second time while an existing provider is
	/// still live (its `Arc` has not been dropped), the second call silently discards
	/// the `port` argument and returns a handle to the already-running provider.
	///
	/// Hypothesis: the returned handle from both calls has the same port number, and that
	/// port is the port chosen when the *first* provider was started — not the port
	/// explicitly requested in the second call.
	///
	/// This is not a bug; it is the documented "only one provider at a time" contract.
	/// The test exists to:
	/// 1. Make the silent-port-discard contract explicit and visible in CI.
	/// 2. Catch any future regression where the second call spawns a second provider
	///    on the requested port instead of reusing the first.
	#[tokio::test]
	async fn test_start_http_provider_second_call_discards_port_arg() {
		let client = UnauthClient::from_config(ClientConfig::default())
			.expect("default ClientConfig should always succeed");

		// Start a provider on an OS-assigned ephemeral port.
		let handle1 = client
			.start_http_provider(None)
			.await
			.expect("first start_http_provider call should succeed");
		let port1 = handle1.port();
		assert_ne!(port1, 0, "OS should assign a non-zero ephemeral port");

		// Request a second provider on a *different* explicit port.
		// The current implementation returns the same provider as handle1.
		let handle2 = client
			.start_http_provider(Some(port1.wrapping_add(1)))
			.await
			.expect("second start_http_provider call should succeed");
		let port2 = handle2.port();

		// Both handles must refer to the same underlying server (same port).
		// If this assertion fails it means a second, independent provider was
		// created on the requested port — breaking the singleton guarantee.
		assert_eq!(
			port1, port2,
			"second call with a different port should reuse the existing provider \
			 (port1={port1}, port2={port2}); the `port` argument is silently ignored \
			 when a provider is already live"
		);

		// Verify the provider is still reachable on port1 (not the discarded port).
		let url = format!("http://127.0.0.1:{port1}/file?file=x");
		// We expect a 400 (bad `file` param) not a connection error — confirming the
		// server is alive on port1.
		let resp = reqwest::get(&url)
			.await
			.expect("server on port1 should accept connections");
		assert_eq!(
			resp.status(),
			400,
			"server should return 400 for an invalid `file` query param, not a connection error"
		);

		drop(handle1);
		drop(handle2);
	}

	/// Verifies that after all `Arc<HttpProviderHandle>` clones are dropped, the provider
	/// stops and its port no longer accepts connections.
	///
	/// Hypothesis: the graceful-shutdown path (cancel_sender + axum graceful shutdown)
	/// causes the server to stop within a reasonable time after the last handle is dropped.
	#[tokio::test]
	async fn test_start_http_provider_stops_after_all_handles_dropped() {
		let client = UnauthClient::from_config(ClientConfig::default())
			.expect("default ClientConfig should always succeed");

		let handle = client
			.start_http_provider(None)
			.await
			.expect("start_http_provider should succeed");
		let port = handle.port();

		// Confirm the server is up.
		let alive_resp = reqwest::get(format!("http://127.0.0.1:{port}/file?file=x"))
			.await
			.expect("server should be reachable while handle is live");
		assert_eq!(
			alive_resp.status(),
			400,
			"pre-drop: expect 400 for bad file param"
		);

		// Drop the only handle — this sends the cancel signal.
		drop(handle);

		// Give axum's graceful shutdown up to 1 s to complete.
		tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

		// The server should now refuse new connections.
		let dead_result = reqwest::get(format!("http://127.0.0.1:{port}/file?file=x")).await;
		assert!(
			dead_result.is_err(),
			"after all handles dropped and 1s elapsed, the server should no longer \
			 accept connections on port {port}"
		);
	}
}

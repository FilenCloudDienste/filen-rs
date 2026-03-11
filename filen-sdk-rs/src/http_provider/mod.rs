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

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
};
use serde::Deserialize;

use crate::{
	Error,
	auth::unauth::UnauthClient,
	fs::file::{RemoteFile, enums::RemoteFileType, read::FileReaderBuilder},
	io::HasFileInfo,
	js::ProviderAnyFile,
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

		let task = tokio::task::spawn(async move {
			let listener =
				match tokio::net::TcpListener::bind(("0.0.0.0", port.unwrap_or_default())).await {
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
	pub fn get_file_url(&self, file: &RemoteFile) -> String {
		let js_file = ProviderAnyFile::File(crate::js::File::from(file.clone()));

		let msgpack = rmp_serde::to_vec_named(&js_file)
			.expect("RemoteFile serialization to msgpack should never fail");
		let encoded = BASE64_URL_SAFE_NO_PAD.encode(&msgpack);

		// let json = serde_json::to_vec_pretty(&js_file)
		// 	.expect("RemoteFile serialization to msgpack should never fail");
		// let encoded = BASE64_URL_SAFE_NO_PAD.encode(&json);
		format!("http://127.0.0.1:{}/file?file={}", self.port, encoded)
	}
}

#[derive(Clone)]
struct ProviderState {
	client: Arc<UnauthClient>,
}

struct RemoteFileCustomDeser(RemoteFileType<'static>);

#[derive(Deserialize)]
struct FileQuery {
	file: RemoteFileCustomDeser,
}

impl<'de> Deserialize<'de> for RemoteFileCustomDeser {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let base_64_encoded = filen_types::serde::cow::deserialize(deserializer)?;
		let msgpack_bytes = BASE64_URL_SAFE_NO_PAD
			.decode(base_64_encoded.as_bytes())
			.map_err(serde::de::Error::custom)?;

		let js_file: ProviderAnyFile = rmp_serde::from_slice(&msgpack_bytes).map_err(|e| {
			serde::de::Error::custom(crate::Error::custom_with_source(
				crate::ErrorKind::Server,
				e,
				Some("asdf"),
			))
		})?;

		Ok(RemoteFileCustomDeser(
			js_file.try_into().map_err(serde::de::Error::custom)?,
		))
	}
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
	range: (Bound<u64>, Bound<u64>),
	client: Arc<UnauthClient>,
) -> Result<Response, http::Error> {
	let (start, end) = get_real_bounds(&file, range);

	let size = file.size();

	let status = if start == 0 && end == size {
		StatusCode::OK
	} else {
		StatusCode::PARTIAL_CONTENT
	};

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

	let mut builder = http::Response::builder()
		.status(status)
		.header(
			CONTENT_LENGTH,
			http::HeaderValue::from_str(&(end.saturating_sub(start)).to_string())
				.expect("numbers should always be valid header values"),
		)
		.header(
			CONTENT_TYPE,
			http::HeaderValue::from_static("application/octet-stream"),
		)
		.header(
			http::header::ACCEPT_RANGES,
			http::HeaderValue::from_static("bytes"),
		);

	if status == StatusCode::PARTIAL_CONTENT {
		let content_range_value = format!("bytes {}-{}/{}", start, end - 1, size);
		builder = builder.header(http::header::CONTENT_RANGE, content_range_value);
	}
	builder.body(axum::body::Body::from_stream(stream))
}

fn multiple_range_response_builder(
	_file: RemoteFileType<'static>,
	_ranges: Vec<(Bound<u64>, Bound<u64>)>,
	_client: Arc<UnauthClient>,
) -> Result<Response, http::Error> {
	todo!()
}

async fn file_handler(
	Query(params): Query<FileQuery>,
	State(state): State<ProviderState>,
	range: Option<TypedHeader<Range>>,
) -> impl IntoResponse {
	let ranges = if let Some(TypedHeader(range)) = range {
		range
			.satisfiable_ranges(params.file.0.size())
			.collect::<Vec<_>>()
	} else {
		vec![(Bound::Included(0), Bound::Unbounded)]
	};

	let resposne = if ranges.len() == 1 {
		single_range_response_builder(params.file.0, ranges[0], state.client.clone())
	} else {
		multiple_range_response_builder(params.file.0, ranges, state.client.clone())
	};
	match resposne {
		Ok(response) => response,
		Err(e) => http::Response::builder()
			.status(StatusCode::INTERNAL_SERVER_ERROR)
			.body(axum::body::Body::from(format!(
				"Error building response: {e}"
			)))
			.expect("should always be able to build a response"),
	}
}

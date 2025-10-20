use std::{cmp::min, sync::Arc};

use crate::{
	Error, ErrorKind,
	auth::JsClient,
	fs::file::{HasFileInfo, enums::RemoteFileType, meta::FileMetaChanges},
	js::{File, FileEnum, UploadFileParams},
	runtime::{self, do_on_commander},
};
use filen_types::fs::UuidStr;
use futures::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use wasm_bindgen::prelude::*;

// these are not particularly efficient and long term it would be better to replace them
pub(crate) struct StreamWriter {
	sender: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
}

impl StreamWriter {
	pub fn new(sender: tokio::sync::mpsc::Sender<Vec<u8>>) -> Self {
		Self {
			sender: Some(sender),
		}
	}
}

impl AsyncWrite for StreamWriter {
	fn poll_write(
		self: std::pin::Pin<&mut Self>,
		cx: &mut std::task::Context<'_>,
		buf: &[u8],
	) -> std::task::Poll<std::io::Result<usize>> {
		let this = self.get_mut();
		if let Some(sender) = &this.sender {
			let len = buf.len();
			let data = buf.to_vec();
			match sender.try_send(data) {
				Ok(()) => std::task::Poll::Ready(Ok(len)),
				Err(tokio::sync::mpsc::error::TrySendError::Full(data)) => {
					// channel is full, need to wait
					let waker = cx.waker().clone();
					let sender_clone = sender.clone();
					runtime::spawn_local(async move {
						let _ = sender_clone.send(data).await;
						waker.wake();
					});
					std::task::Poll::Pending
				}
				Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
					std::task::Poll::Ready(Err(std::io::Error::new(
						std::io::ErrorKind::BrokenPipe,
						"channel closed",
					)))
				}
			}
		} else {
			std::task::Poll::Ready(Err(std::io::Error::other("stream already closed")))
		}
	}

	fn poll_flush(
		self: std::pin::Pin<&mut Self>,
		_cx: &mut std::task::Context<'_>,
	) -> std::task::Poll<std::io::Result<()>> {
		// nothing to flush in this implementation
		std::task::Poll::Ready(Ok(()))
	}

	fn poll_close(
		mut self: std::pin::Pin<&mut Self>,
		_cx: &mut std::task::Context<'_>,
	) -> std::task::Poll<std::io::Result<()>> {
		if self.sender.take().is_some() {
			std::task::Poll::Ready(Ok(()))
		} else {
			std::task::Poll::Ready(Err(std::io::Error::other("stream already closed")))
		}
	}
}

struct StreamReader {
	receiver: tokio::sync::mpsc::Receiver<Vec<u8>>,
	current_chunk: Option<Vec<u8>>,
}

impl AsyncRead for StreamReader {
	fn poll_read(
		mut self: std::pin::Pin<&mut Self>,
		cx: &mut std::task::Context<'_>,
		buf: &mut [u8],
	) -> std::task::Poll<std::io::Result<usize>> {
		let current_chunk = match self.current_chunk.take() {
			Some(chunk) => chunk,
			None => match self.receiver.poll_recv(cx) {
				std::task::Poll::Ready(Some(chunk)) => chunk,
				std::task::Poll::Ready(None) => {
					// no more data
					return std::task::Poll::Ready(Ok(0));
				}
				std::task::Poll::Pending => {
					// no data available yet
					return std::task::Poll::Pending;
				}
			},
		};

		let len = std::cmp::min(buf.len(), current_chunk.len());
		buf[..len].copy_from_slice(&current_chunk[..len]);
		if len < current_chunk.len() {
			// still have data left in the chunk
			self.current_chunk = Some(current_chunk[len..].to_vec());
		}
		std::task::Poll::Ready(Ok(len))
	}
}

pub(crate) fn spawn_write_future(
	mut data_receiver: tokio::sync::mpsc::Receiver<Vec<u8>>,
	mut writer: wasm_streams::writable::IntoAsyncWrite<'static>,
	progress_callback: Option<impl Fn(u64) + 'static>,
	result_sender: tokio::sync::oneshot::Sender<Result<(), Error>>,
) {
	runtime::spawn_local(async move {
		let mut read = 0u64;

		while let Some(data) = data_receiver.recv().await {
			if let Err(e) = writer.write(&data).await {
				let _ = result_sender.send(Err(Error::custom(
					ErrorKind::IO,
					format!("error writing to stream: {:?}", e),
				)));
				return;
			}
			read += u64::try_from(data.len()).unwrap_throw();
			if let Some(callback) = &progress_callback {
				callback(read);
			}
		}

		if let Err(e) = writer.close().await {
			let _ = result_sender.send(Err(Error::custom(
				ErrorKind::IO,
				format!("error closing stream: {:?}", e),
			)));
			return;
		}
		let _ = result_sender.send(Ok(()));
	});
}

#[wasm_bindgen(js_class = "Client")]
impl JsClient {
	#[wasm_bindgen(js_name = "getFile")]
	pub async fn get_file_js(&self, uuid: UuidStr) -> Result<File, Error> {
		let this = self.inner();
		do_on_commander(move || async move { this.get_file(uuid).await.map(File::from) }).await
		// Ok(self.get_file(uuid).awaiDt?.into())
	}

	#[wasm_bindgen(js_name = "uploadFile")]
	pub async fn upload_file_js(
		&self,
		data: Vec<u8>,
		params: UploadFileParams,
	) -> Result<File, JsValue> {
		let this = self.inner();

		let file = params
			.managed_future
			.into_js_managed_future(do_on_commander(move || async move {
				let builder = params.file_builder_params.into_file_builder(&this);
				this.upload_file(Arc::new(builder.build()), &data).await
			}))?
			.await?;

		Ok(file.into())
	}

	#[wasm_bindgen(js_name = "downloadFile")]
	pub async fn download_file_js(&self, file: FileEnum) -> Result<Vec<u8>, Error> {
		let this = self.inner();
		do_on_commander(move || async move {
			this.download_file(&RemoteFileType::try_from(file)?).await
		})
		.await
	}

	#[wasm_bindgen(js_name = "trashFile")]
	pub async fn trash_file_js(&self, file: File) -> Result<File, Error> {
		let this = self.inner();
		do_on_commander(move || async move {
			let mut file = file.try_into()?;
			this.trash_file(&mut file).await?;
			Ok(file.into())
		})
		.await
	}

	#[wasm_bindgen(js_name = "deleteFilePermanently")]
	pub async fn delete_file_permanently_js(&self, file: File) -> Result<(), Error> {
		let this = self.inner();
		do_on_commander(move || async move { this.delete_file_permanently(file.try_into()?).await })
			.await
	}

	#[wasm_bindgen(js_name = "downloadFileToWriter")]
	pub async fn download_file_to_writer_js(
		&self,
		params: crate::js::DownloadFileStreamParams,
	) -> Result<(), Error> {
		let (data_sender, data_receiver) = tokio::sync::mpsc::channel::<Vec<u8>>(10);

		let writer = wasm_streams::WritableStream::from_raw(params.writer)
			.try_into_async_write()
			.map_err(|(e, _)| {
				Error::custom(
					ErrorKind::Conversion,
					format!("got error when converting to WritableStream: {:?}", e),
				)
			})?;

		// we handle the progress callback here because it's easier to not have to spawn another local task
		// to pass through the progress updates
		let progress_callback = if params.progress.is_undefined() {
			None
		} else {
			Some(move |bytes: u64| {
				let _ = params.progress.call1(&JsValue::UNDEFINED, &bytes.into());
			})
		};

		let (result_sender, result_receiver) = tokio::sync::oneshot::channel::<Result<(), Error>>();

		spawn_write_future(data_receiver, writer, progress_callback, result_sender);

		let this = self.inner();
		params
			.managed_future
			.into_js_managed_future(do_on_commander(move || async move {
				let mut writer = StreamWriter {
					sender: Some(data_sender),
				};

				let file = RemoteFileType::try_from(params.file)?;
				this.download_file_to_writer_for_range(
					&file,
					&mut writer,
					None,
					params.start.unwrap_or(0),
					params.end.unwrap_or(file.size()),
				)
				.await?;
				result_receiver.await.unwrap_or_else(|_| {
					Err(Error::custom(
						ErrorKind::Cancelled,
						"download task cancelled",
					))
				})
			}))?
			.await
	}

	#[wasm_bindgen(js_name = "uploadFileFromReader")]
	pub async fn upload_file_from_reader_js(
		&self,
		params: crate::js::UploadFileStreamParams,
	) -> Result<File, JsValue> {
		let (data_sender, data_receiver) = tokio::sync::mpsc::channel::<Vec<u8>>(10);

		let (result_sender, result_receiver) = tokio::sync::oneshot::channel::<Result<(), Error>>();

		let progress_callback = if params.progress.is_undefined() {
			None
		} else {
			Some(move |bytes: u64| {
				let _ = params.progress.call1(&JsValue::UNDEFINED, &bytes.into());
			})
		};

		let mut reader = wasm_streams::ReadableStream::from_raw(params.reader)
			.try_into_async_read()
			.map_err(|(e, _)| e)?;

		runtime::spawn_local(async move {
			let mut written = 0u64;
			let max_cache_size = const { 64usize * 1024 };
			let cache_size = min(
				params
					.known_size
					.and_then(|v| usize::try_from(v).ok())
					.unwrap_or(max_cache_size),
				max_cache_size,
			);
			let mut buffer = Vec::with_capacity(cache_size);
			loop {
				match reader.read(&mut buffer).await {
					Ok(0) => break, // EOF
					Ok(n) => {
						// should never fail to convert usize to u64
						written += u64::try_from(n).unwrap();
						let data = buffer[..n].to_vec();
						if data_sender.send(data).await.is_err() {
							let _ = result_sender.send(Err(Error::custom(
								ErrorKind::Cancelled,
								"upload task cancelled",
							)));
							return;
						}
						if let Some(callback) = &progress_callback {
							callback(written);
						}
					}
					Err(e) => {
						let _ = result_sender.send(Err(Error::custom(
							ErrorKind::IO,
							format!("error reading from stream: {:?}", e),
						)));
						return;
					}
				}
			}
			let _ = result_sender.send(Ok(()));
		});

		let this = self.inner();

		let file = params
			.file_params
			.managed_future
			.into_js_managed_future(do_on_commander(move || async move {
				let mut reader = StreamReader {
					receiver: data_receiver,
					current_chunk: None,
				};

				let builder = params
					.file_params
					.file_builder_params
					.into_file_builder(&this);
				let file = this
					.upload_file_from_reader(
						Arc::new(builder.build()),
						&mut reader,
						None,
						params.known_size,
					)
					.await?;
				result_receiver.await.unwrap_or_else(|_| {
					Err(Error::custom(
						ErrorKind::Cancelled,
						"upload task result sender dropped",
					))
				})?;
				Ok(file)
			}))?
			.await?;

		Ok(file.into())
	}

	#[wasm_bindgen(js_name = "updateFileMetadata")]
	pub async fn update_file_metadata_js(
		&self,
		file: File,
		changes: FileMetaChanges,
	) -> Result<File, Error> {
		let this = self.inner();
		do_on_commander(move || async move {
			let mut file = file.try_into()?;
			this.update_file_metadata(&mut file, changes).await?;
			Ok(file.into())
		})
		.await
	}
}

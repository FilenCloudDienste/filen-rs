#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use std::cmp::min;
use std::sync::Arc;

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use crate::{ErrorKind, fs::file::traits::HasFileInfo};

use crate::{
	Error,
	auth::{JsClient, js_impls::UnauthJsClient, shared_client::SharedClient},
	fs::{
		dir::UnsharedDirectoryType,
		file::{enums::RemoteFileType, meta::FileMetaChanges},
	},
	io::client_impl::IoSharedClientExt,
	js::{DirEnum, File, FileEnum, FileVersion, ManagedFuture, UploadFileParams},
	runtime::do_on_commander,
};
use filen_types::fs::UuidStr;

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use futures::{AsyncRead, AsyncReadExt};
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasm_bindgen::prelude::JsValue;

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
pub(crate) use super::service_worker::{MAX_BUFFER_SIZE_BEFORE_FLUSH, StreamWriter};

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
struct StreamReader {
	receiver: tokio::sync::mpsc::Receiver<Vec<u8>>,
	current_chunk: Option<Vec<u8>>,
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
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

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
#[wasm_bindgen::prelude::wasm_bindgen(js_class = "Client")]
impl JsClient {
	#[wasm_bindgen::prelude::wasm_bindgen(js_name = "uploadFile")]
	pub async fn upload_file(
		&self,
		data: Vec<u8>,
		params: UploadFileParams,
	) -> Result<File, JsValue> {
		let this = self.inner();

		let file = params
			.managed_future
			.into_js_managed_commander_future(move || async move {
				let builder = params.file_builder_params.into_file_builder(&this);
				this.upload_file(Arc::new(builder.build()), &data).await
			})?
			.await?;

		Ok(file.into())
	}

	#[wasm_bindgen::prelude::wasm_bindgen(js_name = "uploadFileFromReader")]
	pub async fn upload_file_from_reader(
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

		crate::runtime::spawn_local(async move {
			let mut written = 0u64;
			let max_cache_size = const { 64usize * 1024 };
			let cache_size = min(
				params
					.known_size
					.and_then(|v| usize::try_from(v).ok())
					.unwrap_or(max_cache_size),
				max_cache_size,
			);
			let mut buffer = vec![0u8; cache_size];
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
			.into_js_managed_commander_future(move || async move {
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
			})?
			.await?;

		Ok(file.into())
	}
}

#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	wasm_bindgen::prelude::wasm_bindgen(js_class = "Client")
)]
#[cfg_attr(feature = "uniffi", uniffi::export)]
impl JsClient {
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "getFile")
	)]
	pub async fn get_file(&self, uuid: UuidStr) -> Result<File, Error> {
		let this = self.inner();
		do_on_commander(move || async move { this.get_file(uuid).await.map(File::from) }).await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "trashFile")
	)]
	pub async fn trash_file(&self, file: File) -> Result<File, Error> {
		let this = self.inner();
		do_on_commander(move || async move {
			let mut file = file.try_into()?;
			this.trash_file(&mut file).await?;
			Ok(file.into())
		})
		.await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "restoreFile")
	)]
	pub async fn restore_file(&self, file: File) -> Result<File, Error> {
		let this = self.inner();

		do_on_commander(move || async move {
			let mut file = file.try_into()?;
			this.restore_file(&mut file).await?;
			Ok(file.into())
		})
		.await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "moveFile")
	)]
	pub async fn move_file(&self, file: File, new_parent: DirEnum) -> Result<File, Error> {
		let this = self.inner();
		do_on_commander(move || async move {
			let mut file = file.try_into()?;
			this.move_file(&mut file, &UnsharedDirectoryType::from(new_parent))
				.await?;
			Ok(file.into())
		})
		.await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "deleteFilePermanently")
	)]
	pub async fn delete_file_permanently(&self, file: File) -> Result<(), Error> {
		let this = self.inner();
		do_on_commander(move || async move { this.delete_file_permanently(file.try_into()?).await })
			.await
	}

	#[cfg(feature = "uniffi")]
	pub async fn upload_file_from_bytes(
		&self,
		data: Vec<u8>,
		params: UploadFileParams,
	) -> Result<File, Error> {
		let this = self.inner();
		params
			.managed_future
			.into_js_managed_commander_future(move || async move {
				let builder = params.file_builder_params.into_file_builder(&this);
				this.upload_file(Arc::new(builder.build()), &data).await
			})
			.await
			.map(Into::into)
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "updateFileMetadata")
	)]
	pub async fn update_file_metadata(
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

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "listFileVersions")
	)]
	pub async fn list_file_versions(&self, file: File) -> Result<Vec<FileVersion>, Error> {
		let this = self.inner();
		do_on_commander(move || async move {
			let file = file.try_into()?;
			let versions = this.list_file_versions(&file).await?;
			Ok(versions.into_iter().map(FileVersion::from).collect())
		})
		.await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "restoreFileVersion")
	)]
	pub async fn restore_file_version(
		&self,
		file: File,
		version: FileVersion,
	) -> Result<File, Error> {
		let this = self.inner();
		let mut file = file.try_into()?;
		let version = version.try_into()?;

		do_on_commander(move || async move {
			this.restore_file_version(&mut file, version).await?;
			Ok(file.into())
		})
		.await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "deleteFileVersion")
	)]
	pub async fn delete_file_version(&self, version: FileVersion) -> Result<(), Error> {
		let this = self.inner();
		let version = version.try_into()?;

		do_on_commander(move || async move { this.delete_file_version(version).await }).await
	}
}

async fn download_file_generic<T>(
	client: Arc<T>,
	file: FileEnum,
	managed_future: Option<ManagedFuture>,
) -> Result<Vec<u8>, Error>
where
	T: SharedClient + Send + Sync + 'static,
{
	let fut = move || async move { client.download_file(&RemoteFileType::try_from(file)?).await };

	if let Some(managed_future) = managed_future {
		let res = managed_future.into_js_managed_commander_future(fut);
		#[cfg(all(target_family = "wasm", target_os = "unknown"))]
		{
			res?.await
		}
		#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
		{
			res.await
		}
	} else {
		do_on_commander(fut).await
	}
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
async fn download_file_to_writer_generic<T>(
	client: Arc<T>,
	params: crate::js::DownloadFileStreamParams,
) -> Result<(), Error>
where
	T: SharedClient + Send + Sync + 'static,
{
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

	crate::js::spawn_buffered_write_future(data_receiver, writer, progress_callback, result_sender);

	params
		.managed_future
		.into_js_managed_commander_future(move || async move {
			let mut writer = StreamWriter::new(data_sender);

			let file = RemoteFileType::try_from(params.file)?;
			client
				.download_file_to_writer_for_range(
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
		})?
		.await
}

#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	wasm_bindgen::prelude::wasm_bindgen(js_class = "Client")
)]
#[cfg_attr(feature = "uniffi", uniffi::export)]
impl JsClient {
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "downloadFile")
	)]
	pub async fn download_file(
		&self,
		file: FileEnum,
		managed_future: Option<ManagedFuture>,
	) -> Result<Vec<u8>, Error> {
		download_file_generic(self.inner(), file, managed_future).await
	}
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
#[wasm_bindgen::prelude::wasm_bindgen(js_class = "Client")]
impl JsClient {
	#[wasm_bindgen::prelude::wasm_bindgen(js_name = "downloadFileToWriter")]
	pub async fn download_file_to_writer(
		&self,
		params: crate::js::DownloadFileStreamParams,
	) -> Result<(), Error> {
		download_file_to_writer_generic(self.inner(), params).await
	}
}

#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	wasm_bindgen::prelude::wasm_bindgen(js_class = "UnauthClient")
)]
#[cfg_attr(feature = "uniffi", uniffi::export)]
impl UnauthJsClient {
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "downloadFile")
	)]
	pub async fn download_file(
		&self,
		file: FileEnum,
		managed_future: Option<ManagedFuture>,
	) -> Result<Vec<u8>, Error> {
		download_file_generic(self.inner(), file, managed_future).await
	}
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
#[wasm_bindgen::prelude::wasm_bindgen(js_class = "UnauthClient")]
impl UnauthJsClient {
	#[wasm_bindgen::prelude::wasm_bindgen(js_name = "downloadFileToWriter")]
	pub async fn download_file_to_writer(
		&self,
		params: crate::js::DownloadFileStreamParams,
	) -> Result<(), Error> {
		download_file_to_writer_generic(self.inner(), params).await
	}
}

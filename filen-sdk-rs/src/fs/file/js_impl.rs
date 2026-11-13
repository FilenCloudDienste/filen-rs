#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use std::cmp::min;
use std::sync::Arc;

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use crate::{ErrorKind, fs::file::traits::HasFileInfo};

use crate::{
	Error,
	auth::{JsClient, js_impls::UnauthJsClient, shared_client::SharedClient},
	error::ResultExt,
	fs::{
		categories::{DirType, Normal},
		file::enums::RemoteFileType,
	},
	io::client_impl::IoSharedClientExt,
	js::{
		AnyFile, AnyNormalDir, File, FileMetaChanges, FileVersion, ManagedFuture, UploadFileParams,
	},
	runtime::do_on_commander,
};
use filen_types::fs::{StableUuid, UuidStr};

#[cfg(any(all(target_family = "wasm", target_os = "unknown"), test))]
use bytes::Bytes;
#[cfg(any(all(target_family = "wasm", target_os = "unknown"), test))]
use futures::AsyncRead;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use futures::AsyncReadExt;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasm_bindgen::prelude::JsValue;

/// The bytes of a JS `ReadableStream`, which a task on the calling thread pumps into `receiver`
/// because the stream cannot be moved to the commander. A read error arrives in-band as the last
/// item and is returned as one, so the upload fails rather than taking the closed channel for the
/// end of the stream and finishing a truncated file.
#[cfg(any(all(target_family = "wasm", target_os = "unknown"), test))]
struct StreamReader {
	receiver: tokio::sync::mpsc::Receiver<Result<Vec<u8>, Error>>,
	/// What `buf` had no room for of the last chunk received, handed out before the next one.
	leftover: Bytes,
}

#[cfg(any(all(target_family = "wasm", target_os = "unknown"), test))]
impl AsyncRead for StreamReader {
	fn poll_read(
		mut self: std::pin::Pin<&mut Self>,
		cx: &mut std::task::Context<'_>,
		buf: &mut [u8],
	) -> std::task::Poll<std::io::Result<usize>> {
		while self.leftover.is_empty() {
			match std::task::ready!(self.receiver.poll_recv(cx)) {
				Some(Ok(chunk)) => self.leftover = Bytes::from(chunk),
				Some(Err(e)) => return std::task::Poll::Ready(Err(std::io::Error::other(e))),
				// the pump ends the channel only at the end of the stream
				None => return std::task::Poll::Ready(Ok(0)),
			}
		}
		let len = std::cmp::min(buf.len(), self.leftover.len());
		buf[..len].copy_from_slice(&self.leftover.split_to(len));
		std::task::Poll::Ready(Ok(len))
	}
}

#[filen_macros::js_exports]
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
				let builder = params.file_builder_params.into_file_builder(&this)?;
				this.upload_file(builder, &data).await
			})?
			.await?;

		Ok(file.into())
	}

	#[wasm_bindgen::prelude::wasm_bindgen(js_name = "uploadFileFromReader")]
	pub async fn upload_file_from_reader(
		&self,
		params: crate::js::UploadFileStreamParams,
	) -> Result<File, JsValue> {
		let (data_sender, data_receiver) = tokio::sync::mpsc::channel::<Result<Vec<u8>, Error>>(10);

		let progress_callback = params.progress.map(|progress| {
			move |bytes: u64| {
				let _ = progress.call1(&JsValue::UNDEFINED, &bytes.into());
			}
		});

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
					Ok(0) => return, // EOF
					Ok(n) => {
						// should never fail to convert usize to u64
						written += u64::try_from(n).unwrap();
						// a closed channel: the upload has ended, with a result of its own
						if data_sender.send(Ok(buffer[..n].to_vec())).await.is_err() {
							return;
						}
						if let Some(callback) = &progress_callback {
							callback(written);
						}
					}
					Err(e) => {
						let _ = data_sender
							.send(Err(Error::custom(
								ErrorKind::IO,
								format!("error reading from stream: {:?}", e),
							)))
							.await;
						return;
					}
				}
			}
		});

		let this = self.inner();

		let file = params
			.managed_future
			.into_js_managed_commander_future(move || async move {
				let mut reader = StreamReader {
					receiver: data_receiver,
					leftover: Bytes::new(),
				};

				let builder = params.file_builder_params.into_file_builder(&this)?;
				this.upload_file_from_reader(builder, &mut reader, None, params.known_size)
					.await
			})?
			.await?;

		Ok(file.into())
	}
}

#[filen_macros::js_exports]
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
		do_on_commander(move || async move { this.get_file(uuid.into()).await.map(File::from) })
			.await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "getFileOptional")
	)]
	pub async fn get_file_optional(&self, uuid: UuidStr) -> Result<Option<File>, Error> {
		self.get_file(uuid).await.optional()
	}

	/// The current head of a file's lineage, which carries a new `uuid` after every content
	/// edit. `undefined` only when the server does not know the lineage (permanently deleted);
	/// a trashed head still resolves, with a `trash` parent.
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "getFileByStableUuidOptional")
	)]
	pub async fn get_file_by_stable_uuid_optional(
		&self,
		stable_uuid: StableUuid,
	) -> Result<Option<File>, Error> {
		let this = self.inner();
		do_on_commander(move || async move {
			this.get_file_by_stable_uuid(stable_uuid)
				.await
				.map(File::from)
				.optional()
		})
		.await
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
	pub async fn move_file(&self, file: File, new_parent: AnyNormalDir) -> Result<File, Error> {
		let this = self.inner();
		do_on_commander(move || async move {
			let mut file = file.try_into()?;
			this.move_file(&mut file, &DirType::<'static, Normal>::from(new_parent))
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
				let builder = params.file_builder_params.into_file_builder(&this)?;
				this.upload_file(builder, &data).await
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
			this.update_file_metadata(&mut file, changes.try_into()?)
				.await?;
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
	file: AnyFile,
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
	// we handle the progress callback here because it's easier to not have to spawn another local task
	// to pass through the progress updates
	let progress_callback = params.progress.map(|progress| {
		move |bytes: u64| {
			let _ = progress.call1(&JsValue::UNDEFINED, &bytes.into());
		}
	});

	let (mut writer, result_receiver) = crate::js::stream_writer(
		params.writer,
		progress_callback,
		"got error when converting to WritableStream",
	)?;

	params
		.managed_future
		.into_js_managed_commander_future(move || async move {
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

#[filen_macros::js_exports]
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
	pub async fn download_file_to_bytes(
		&self,
		file: AnyFile,
		managed_future: Option<ManagedFuture>,
	) -> Result<Vec<u8>, Error> {
		download_file_generic(self.inner(), file, managed_future).await
	}
}

#[filen_macros::js_exports]
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

#[filen_macros::js_exports]
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
		file: AnyFile,
		managed_future: Option<ManagedFuture>,
	) -> Result<Vec<u8>, Error> {
		download_file_generic(self.inner(), file, managed_future).await
	}
}

#[filen_macros::js_exports]
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

#[cfg(all(test, feature = "uniffi"))]
mod tests {
	use filen_types::fs::Uuid;
	use futures::AsyncReadExt;

	use super::*;
	use crate::{ErrorKind, auth::test_support::offline_client};

	/// A reader over `items`, its channel closed after the last one as the pump closes it at the
	/// end of the stream.
	fn stream_reader(items: Vec<Result<Vec<u8>, Error>>) -> StreamReader {
		let (sender, receiver) = tokio::sync::mpsc::channel(items.len());
		for item in items {
			sender
				.try_send(item)
				.expect("the channel has room for every item");
		}
		StreamReader {
			receiver,
			leftover: Bytes::new(),
		}
	}

	fn read_error() -> Error {
		Error::custom(ErrorKind::IO, "error reading from stream: broken")
	}

	#[tokio::test]
	async fn a_stream_reader_reads_every_chunk_in_order_then_ends() {
		let mut reader = stream_reader(vec![Ok(b"hello ".to_vec()), Ok(b"world".to_vec())]);
		let mut read = Vec::new();
		reader.read_to_end(&mut read).await.unwrap();
		assert_eq!(read, b"hello world");
	}

	#[tokio::test]
	async fn a_stream_reader_hands_out_a_chunk_larger_than_the_buffer_in_parts() {
		let mut reader = stream_reader(vec![Ok(b"abcdef".to_vec())]);
		let mut buf = [0u8; 4];
		assert_eq!(reader.read(&mut buf).await.unwrap(), 4);
		assert_eq!(&buf, b"abcd");
		assert_eq!(reader.read(&mut buf).await.unwrap(), 2);
		assert_eq!(&buf[..2], b"ef");
		assert_eq!(reader.read(&mut buf).await.unwrap(), 0);
	}

	#[tokio::test]
	async fn a_stream_reader_fails_on_a_read_error_instead_of_ending() {
		let mut reader = stream_reader(vec![Ok(b"partial".to_vec()), Err(read_error())]);
		let mut buf = [0u8; 16];
		assert_eq!(reader.read(&mut buf).await.unwrap(), 7);
		let error = Error::from(reader.read(&mut buf).await.unwrap_err());
		assert_eq!(error.kind(), ErrorKind::IO);
		assert_eq!(
			error.to_string(),
			"Error of kind IO: context: error reading from stream: broken"
		);
	}

	#[tokio::test]
	async fn an_upload_from_a_stream_that_fails_ends_with_the_read_error() {
		let client = offline_client();
		let builder = client
			.make_file_builder("payload.bin", Uuid::from_u128(1))
			.unwrap();
		let mut reader = stream_reader(vec![Ok(vec![7u8; 1024]), Err(read_error())]);

		let error = client
			.upload_file_from_reader(builder, &mut reader, None, None)
			.await
			.unwrap_err();

		assert_eq!(error.kind(), ErrorKind::IO);
		assert_eq!(
			error.to_string(),
			"Error of kind IO: context: error reading from stream: broken"
		);
	}
}

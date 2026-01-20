use std::rc::Rc;

use crate::{
	Error, ErrorKind,
	auth::{Client, StringifiedClient},
	crypto::error::ConversionError,
	fs::file::{enums::RemoteFileType, service_worker::StreamWriter, traits::HasFileInfo},
};

#[cfg(feature = "wasm-full")]
use futures::AsyncWriteExt;

use super::shared::*;

#[derive(Clone)]
#[wasm_bindgen::prelude::wasm_bindgen(js_name = "Client")]
pub struct ServiceWorkerClient {
	client: Rc<Client>,
}

impl ServiceWorkerClient {
	pub(crate) fn inner(&self) -> &Client {
		&self.client
	}

	pub(crate) fn new(client: Client) -> Self {
		Self {
			client: Rc::new(client),
		}
	}
}

#[wasm_bindgen::prelude::wasm_bindgen(js_class = "Client")]
impl ServiceWorkerClient {
	#[wasm_bindgen::prelude::wasm_bindgen(js_name = "downloadFileToWriter")]
	pub async fn download_file_to_writer(
		&self,
		params: DownloadFileStreamParams,
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
				let _ = params
					.progress
					.call1(&wasm_bindgen::JsValue::UNDEFINED, &bytes.into());
			})
		};

		let (result_sender, result_receiver) = tokio::sync::oneshot::channel::<Result<(), Error>>();

		super::shared::spawn_buffered_write_future(
			data_receiver,
			writer,
			progress_callback,
			result_sender,
		);

		let this = self.inner();

		params
			.managed_future
			.into_js_managed_future(async move {
				let mut writer = StreamWriter::new(data_sender);

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
			})?
			.await
	}
}

#[wasm_bindgen::prelude::wasm_bindgen(js_class = "Client")]
impl ServiceWorkerClient {
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "downloadItemsToZip")
	)]
	pub async fn download_items_to_zip(
		&self,
		params: DownloadFileToZipParams,
	) -> Result<(), Error> {
		let (data_sender, data_receiver) = tokio::sync::mpsc::channel::<Vec<u8>>(10);
		let writer = wasm_streams::WritableStream::from_raw(params.writer)
			.try_into_async_write()
			.map_err(|(e, _)| {
				Error::custom(
					ErrorKind::Conversion,
					format!("failed to convert WritableStream to AsyncWrite: {:?}", e),
				)
			})?;

		let (result_sender, result_receiver) = tokio::sync::oneshot::channel::<Result<(), Error>>();

		spawn_buffered_write_future(data_receiver, writer, None::<fn(u64)>, result_sender);

		let items = params
			.items
			.into_iter()
			.map(TryInto::try_into)
			.collect::<Result<Vec<_>, ConversionError>>()
			.map_err(Error::from)?;

		let progress_callback = params.progress.into_rust_callback();

		let this = self.inner();

		params
			.managed_future
			.into_js_managed_future(async move {
				let writer = StreamWriter::new(data_sender);

				this.download_items_to_zip(&items, writer, progress_callback.as_ref())
					.await?;
				result_receiver.await.unwrap_or_else(|e| {
					Err(Error::custom(
						ErrorKind::IO,
						format!("zip download result_sender dropped: {}", e),
					))
				})
			})?
			.await
	}
}

#[wasm_bindgen::prelude::wasm_bindgen(js_name = "fromStringified")]
pub fn from_stringified(serialized: StringifiedClient) -> Result<ServiceWorkerClient, Error> {
	Ok(ServiceWorkerClient::new(Client::from_stringified(
		serialized,
	)?))
}

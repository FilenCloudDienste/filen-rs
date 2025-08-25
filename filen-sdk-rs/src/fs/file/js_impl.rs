use std::sync::Arc;

use crate::{
	Error,
	auth::Client,
	fs::file::RemoteFile,
	js::{File, UploadFileParams},
};
#[cfg(feature = "node")]
use napi_derive::napi;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
use wasm_bindgen::prelude::*;

#[cfg_attr(all(target_arch = "wasm32", target_os = "unknown"), wasm_bindgen)]
#[cfg_attr(feature = "node", napi)]
impl Client {
	#[cfg_attr(
		all(target_arch = "wasm32", target_os = "unknown"),
		wasm_bindgen(js_name = "uploadFile")
	)]
	#[cfg_attr(feature = "node", napi(js_name = "uploadFile"))]
	pub async fn upload_file_js(
		&self,
		data: &[u8],
		params: UploadFileParams,
	) -> Result<File, Error> {
		let builder = params.into_file_builder(self);
		let file = self.upload_file(Arc::new(builder.build()), data).await?;
		Ok(file.into())
	}

	#[cfg_attr(
		all(target_arch = "wasm32", target_os = "unknown"),
		wasm_bindgen(js_name = "downloadFile")
	)]
	#[cfg_attr(feature = "node", napi(js_name = "downloadFile"))]
	pub async fn download_file_js(&self, file: File) -> Result<Vec<u8>, Error> {
		self.download_file(&RemoteFile::try_from(file)?).await
	}

	#[cfg_attr(
		all(target_arch = "wasm32", target_os = "unknown"),
		wasm_bindgen(js_name = "trashFile")
	)]
	#[cfg_attr(feature = "node", napi(js_name = "trashFile"))]
	pub async fn trash_file_js(&self, file: File) -> Result<File, Error> {
		let mut file = file.try_into()?;
		self.trash_file(&mut file).await?;
		Ok(file.into())
	}

	#[cfg_attr(
		all(target_arch = "wasm32", target_os = "unknown"),
		wasm_bindgen(js_name = "deleteFilePermanently")
	)]
	#[cfg_attr(feature = "node", napi(js_name = "deleteFilePermanently"))]
	pub async fn delete_file_permanently_js(&self, file: File) -> Result<(), Error> {
		self.delete_file_permanently(file.try_into()?).await
	}

	#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
	#[wasm_bindgen(js_name = "downloadFileToWriter")]
	pub async fn download_file_to_writer_js(
		&self,
		params: crate::js::DownloadFileStreamParams,
	) -> Result<(), JsValue> {
		let mut writer = wasm_streams::WritableStream::from_raw(params.writer)
			.try_into_async_write()
			.map_err(|(e, _)| e)?;

		let progress_callback = if params.progress.is_undefined() {
			None
		} else {
			Some(std::rc::Rc::new(move |bytes: u64| {
				let _ = params.progress.call1(&JsValue::UNDEFINED, &bytes.into());
			}) as crate::util::MaybeSendCallback<u64>)
		};

		self.download_file_to_writer(
			&RemoteFile::try_from(params.file).map_err(Error::from)?,
			&mut writer,
			progress_callback,
		)
		.await?;
		Ok(())
	}

	#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
	#[wasm_bindgen(js_name = "uploadFileFromReader")]
	pub async fn upload_file_from_reader_js(
		&self,
		params: crate::js::UploadFileStreamParams,
	) -> Result<File, JsValue> {
		let builder = params.file_params.into_file_builder(&self);
		let mut reader = wasm_streams::ReadableStream::from_raw(params.reader)
			.try_into_async_read()
			.map_err(|(e, _)| e)?;

		let progress_callback = if params.progress.is_undefined() {
			None
		} else {
			Some(std::rc::Rc::new(move |bytes: u64| {
				let _ = params.progress.call1(&JsValue::UNDEFINED, &bytes.into());
			}) as crate::util::MaybeSendCallback<u64>)
		};

		let file = self
			.upload_file_from_reader(
				Arc::new(builder.build()),
				&mut reader,
				progress_callback,
				params.known_size,
			)
			.await?;
		Ok(file.into())
	}
}

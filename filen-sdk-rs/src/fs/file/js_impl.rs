use std::sync::Arc;

use crate::{
	Error,
	auth::Client,
	fs::file::{HasFileInfo, enums::RemoteFileType, meta::FileMetaChanges},
	js::{File, FileEnum, UploadFileParams},
};
use filen_types::fs::UuidStr;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
impl Client {
	#[wasm_bindgen(js_name = "getFile")]
	pub async fn get_file_js(&self, uuid: UuidStr) -> Result<File, Error> {
		Ok(self.get_file(uuid).await?.into())
	}

	#[wasm_bindgen(js_name = "uploadFile")]
	pub async fn upload_file_js(
		&self,
		data: &[u8],
		params: UploadFileParams,
	) -> Result<File, JsValue> {
		let builder = params.file_builder_params.into_file_builder(self);

		let file = params
			.managed_future
			.into_js_managed_future(self.upload_file(Arc::new(builder.build()), data))?
			.await?;

		Ok(file.into())
	}

	#[wasm_bindgen(js_name = "downloadFile")]
	pub async fn download_file_js(&self, file: FileEnum) -> Result<Vec<u8>, Error> {
		self.download_file(&RemoteFileType::try_from(file)?).await
	}

	#[wasm_bindgen(js_name = "trashFile")]
	pub async fn trash_file_js(&self, file: File) -> Result<File, Error> {
		let mut file = file.try_into()?;
		self.trash_file(&mut file).await?;
		Ok(file.into())
	}

	#[wasm_bindgen(js_name = "deleteFilePermanently")]
	pub async fn delete_file_permanently_js(&self, file: File) -> Result<(), Error> {
		self.delete_file_permanently(file.try_into()?).await
	}

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
		let file = RemoteFileType::try_from(params.file).map_err(Error::from)?;
		params
			.managed_future
			.into_js_managed_future(self.download_file_to_writer_for_range(
				&file,
				&mut writer,
				progress_callback,
				params.start.unwrap_or(0),
				params.end.unwrap_or(file.size()),
			))?
			.await?;
		Ok(())
	}

	#[wasm_bindgen(js_name = "uploadFileFromReader")]
	pub async fn upload_file_from_reader_js(
		&self,
		params: crate::js::UploadFileStreamParams,
	) -> Result<File, JsValue> {
		let builder = params
			.file_params
			.file_builder_params
			.into_file_builder(self);
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

		let file = params
			.file_params
			.managed_future
			.into_js_managed_future(self.upload_file_from_reader(
				Arc::new(builder.build()),
				&mut reader,
				progress_callback,
				params.known_size,
			))?
			.await?;
		Ok(file.into())
	}

	#[wasm_bindgen(js_name = "updateFileMetadata")]
	pub async fn update_file_metadata_js(
		&self,
		file: File,
		changes: FileMetaChanges,
	) -> Result<File, Error> {
		let mut file = file.try_into()?;
		self.update_file_metadata(&mut file, changes).await?;
		Ok(file.into())
	}
}

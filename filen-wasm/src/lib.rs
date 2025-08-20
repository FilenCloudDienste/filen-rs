use std::sync::Arc;

use filen_sdk_rs::{Error, auth::StringifiedClient};
use wasm_bindgen::prelude::*;

pub mod ffi;
pub use ffi::*;

#[wasm_bindgen(start)]
pub fn main_js() -> Result<(), JsValue> {
	console_error_panic_hook::set_once();
	wasm_logger::init(wasm_logger::Config::default());
	Ok(())
}

#[wasm_bindgen]
pub struct FilenState {
	client: filen_sdk_rs::auth::Client,
}

impl FilenState {}

#[wasm_bindgen]
pub async fn login(
	email: String,
	password: &str,
	two_factor_code: Option<String>,
) -> Result<FilenState, JsValue> {
	let client = filen_sdk_rs::auth::Client::login(
		email,
		password,
		two_factor_code.as_deref().unwrap_or("XXXXXX"),
	)
	.await?;
	Ok(FilenState { client })
}

#[wasm_bindgen]
pub fn from_serialized(serialized: StringifiedClient) -> Result<FilenState, JsValue> {
	let client = filen_sdk_rs::auth::Client::from_stringified(serialized).map_err(Error::from)?;
	Ok(FilenState { client })
}

#[wasm_bindgen]
impl FilenState {
	#[wasm_bindgen]
	pub fn serialize(&self) -> StringifiedClient {
		self.client.to_stringified()
	}
}

/// Converts a tuple into a WASM JSValue Array
///
/// Usage: `tuple_to_jsvalue!(value1, value2, value3)`
///
/// Each tuple element must implement `Into<JsValue>`
macro_rules! tuple_to_jsvalue {
	// Handle direct tuple literals
	($($element:expr),+ $(,)?) => {{
		let elements = [$($element.into()),+];
		let array = web_sys::js_sys::Array::new_with_length(elements.len() as u32);
		for (index, element) in elements.into_iter().enumerate() {
			array.set(index as u32, element);
		}
		JsValue::from(array)
	}};
}

mod dir {
	use filen_sdk_rs::fs::NonRootFSObject;

	use super::*;
	#[wasm_bindgen]
	impl FilenState {
		#[wasm_bindgen]
		pub fn root(&self) -> Dir {
			self.client.root().clone().into()
		}

		#[wasm_bindgen(unchecked_return_type = "[Dir[], File[]]", js_name = "listDir")]
		pub async fn list_dir(&self, dir: &Dir) -> Result<JsValue, JsValue> {
			let (dirs, files) = self.client.list_dir(&dir.0).await?;
			Ok(tuple_to_jsvalue!(
				dirs.into_iter().map(Dir::from).collect::<Vec<_>>(),
				files.into_iter().map(File::from).collect::<Vec<_>>()
			))
		}

		#[wasm_bindgen(js_name = "createDir")]
		pub async fn create_dir(&self, parent: &Dir, name: String) -> Result<Dir, JsValue> {
			let dir = self.client.create_dir(&parent.0, name).await?;
			Ok(dir.into())
		}

		#[wasm_bindgen(js_name = "deleteDirPermanently")]
		pub async fn delete_dir_permanently(&self, dir: Dir) -> Result<(), JsValue> {
			log::info!("Deleting dir permanently: {:?}", dir.0);
			self.client.delete_dir_permanently(dir.try_into()?).await?;
			Ok(())
		}

		#[wasm_bindgen(js_name = "trashDir")]
		pub async fn trash_dir(&self, dir: &mut Dir) -> Result<(), JsValue> {
			self.client.trash_dir(dir.try_mut_ref()?).await?;
			Ok(())
		}

		#[wasm_bindgen(js_name = "dirExists")]
		pub async fn dir_exists(&self, parent: &Dir, name: &str) -> Result<(), JsValue> {
			self.client.dir_exists(&parent.0, name).await?;
			Ok(())
		}

		#[wasm_bindgen(
			js_name = "findItemInDir",
			unchecked_return_type = "Dir | File | undefined"
		)]
		pub async fn find_item_in_dir(
			&self,
			dir: &Dir,
			name_or_uuid: &str,
		) -> Result<Option<JsValue>, JsValue> {
			let item = self.client.find_item_in_dir(&dir.0, name_or_uuid).await?;
			Ok(item.map(|o| match o {
				NonRootFSObject::Dir(dir) => JsValue::from(Dir::from(dir.into_owned())),
				NonRootFSObject::File(file) => JsValue::from(File::from(file.into_owned())),
			}))
		}
	}
}

mod file {

	use filen_sdk_rs::util::{MaybeArc, MaybeSendCallback};

	use super::*;
	#[wasm_bindgen]
	impl FilenState {
		#[wasm_bindgen(js_name = "uploadFile")]
		pub async fn upload_file(
			&self,
			parent: &Dir,
			data: &[u8],
			params: UploadFileParams,
		) -> Result<File, JsValue> {
			let builder = params.into_file_builder(&self.client, &parent.0);
			let file = self
				.client
				.upload_file(Arc::new(builder.build()), data)
				.await?;
			Ok(file.into())
		}

		#[wasm_bindgen(js_name = "uploadFileStream")]
		pub async fn upload_file_stream(
			&self,
			parent: &Dir,
			params: UploadFileStreamParams,
		) -> Result<File, JsValue> {
			let builder = params
				.file_params
				.into_file_builder(&self.client, &parent.0);
			let mut reader = wasm_streams::ReadableStream::from_raw(params.reader)
				.try_into_async_read()
				.map_err(|(e, _)| e)?;

			let progress_callback = if params.progress.is_undefined() {
				None
			} else {
				Some(MaybeArc::new(move |bytes: u64| {
					let _ = params.progress.call1(&JsValue::UNDEFINED, &bytes.into());
				}) as MaybeSendCallback<u64>)
			};

			let file = self
				.client
				.upload_file_from_reader(
					Arc::new(builder.build()),
					&mut reader,
					progress_callback,
					params.known_size,
				)
				.await?;
			Ok(file.into())
		}

		#[wasm_bindgen(js_name = "downloadFile")]
		pub async fn download_file(&self, file: &File) -> Result<Vec<u8>, JsValue> {
			let data = self.client.download_file(&file.0).await?;
			Ok(data)
		}

		#[wasm_bindgen(js_name = "downloadFileToWriter")]
		pub async fn download_file_to_writer(
			&self,
			file: &File,
			params: DownloadFileStreamParams,
		) -> Result<(), JsValue> {
			let mut writer = wasm_streams::WritableStream::from_raw(params.writer)
				.try_into_async_write()
				.map_err(|(e, _)| e)?;

			let progress_callback = if params.progress.is_undefined() {
				None
			} else {
				Some(MaybeArc::new(move |bytes: u64| {
					let _ = params.progress.call1(&JsValue::UNDEFINED, &bytes.into());
				}) as MaybeSendCallback<u64>)
			};

			self.client
				.download_file_to_writer(&file.0, &mut writer, progress_callback)
				.await?;
			Ok(())
		}

		#[wasm_bindgen(js_name = "trashFile")]
		pub async fn trash_file(&self, file: &mut File) -> Result<(), JsValue> {
			self.client.trash_file(&mut file.0).await?;
			Ok(())
		}

		#[wasm_bindgen(js_name = "deleteFilePermanently")]
		pub async fn delete_file_permanently(&self, file: File) -> Result<(), JsValue> {
			self.client.delete_file_permanently(file.0).await?;
			Ok(())
		}
	}
}

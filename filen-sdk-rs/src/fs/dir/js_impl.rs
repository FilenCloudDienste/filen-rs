use crate::{
	Error,
	auth::Client,
	fs::dir::UnsharedDirectoryType,
	js::{Dir, DirEnum, File, NonRootObject, Root},
};
#[cfg(feature = "node")]
use napi_derive::napi;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
use wasm_bindgen::prelude::*;

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
macro_rules! tuple_to_jsvalue {
	// Handle direct tuple literals
	($($element:expr),+ $(,)?) => {{
		let elements = [$(JsValue::from($element)),+];
		let array = web_sys::js_sys::Array::new_with_length(elements.len() as u32);
		for (index, element) in elements.into_iter().enumerate() {
			array.set(index as u32, element);
		}
		JsValue::from(array)
	}};
}

#[cfg_attr(all(target_arch = "wasm32", target_os = "unknown"), wasm_bindgen)]
#[cfg_attr(feature = "node", napi)]
impl Client {
	#[cfg_attr(
		all(target_arch = "wasm32", target_os = "unknown"),
		wasm_bindgen(js_name = "root")
	)]
	#[cfg_attr(feature = "node", napi(js_name = "root"))]
	pub fn root_js(&self) -> Root {
		self.root().clone().into()
	}
	#[cfg_attr(
		all(target_arch = "wasm32", target_os = "unknown"),
		wasm_bindgen(js_name = "createDir")
	)]
	#[cfg_attr(feature = "node", napi(js_name = "createDir"))]
	pub async fn create_dir_js(&self, parent: DirEnum, name: String) -> Result<Dir, Error> {
		Ok(self
			.create_dir(&UnsharedDirectoryType::from(parent), name)
			.await?
			.into())
	}

	#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
	#[cfg_attr(
		all(target_arch = "wasm32", target_os = "unknown"),
		wasm_bindgen(unchecked_return_type = "[Dir[], File[]]", js_name = "listDir")
	)]
	pub async fn list_dir_js(&self, dir: DirEnum) -> Result<JsValue, Error> {
		let (dirs, files) = self.list_dir(&UnsharedDirectoryType::from(dir)).await?;
		Ok(tuple_to_jsvalue!(
			dirs.into_iter().map(Dir::from).collect::<Vec<_>>(),
			files.into_iter().map(File::from).collect::<Vec<_>>()
		))
	}

	#[cfg(feature = "node")]
	#[cfg_attr(feature = "node", napi(js_name = "listDir"))]
	pub async fn list_dir_js(&self, dir: DirEnum) -> Result<(Vec<Dir>, Vec<File>), Error> {
		let (dirs, files) = self.list_dir(&UnsharedDirectoryType::from(dir)).await?;
		Ok((
			dirs.into_iter().map(Dir::from).collect(),
			files.into_iter().map(File::from).collect(),
		))
	}

	#[cfg_attr(
		all(target_arch = "wasm32", target_os = "unknown"),
		wasm_bindgen(js_name = "deleteDirPermanently")
	)]
	#[cfg_attr(feature = "node", napi(js_name = "deleteDirPermanently"))]
	pub async fn delete_dir_permanently_js(&self, dir: Dir) -> Result<(), Error> {
		self.delete_dir_permanently(dir.into()).await
	}

	#[cfg_attr(
		all(target_arch = "wasm32", target_os = "unknown"),
		wasm_bindgen(js_name = "trashDir")
	)]
	#[cfg_attr(feature = "node", napi(js_name = "trashDir"))]
	pub async fn trash_dir_js(&self, dir: Dir) -> Result<Dir, Error> {
		let mut dir = dir.into();
		self.trash_dir(&mut dir).await?;
		Ok(dir.into())
	}

	#[cfg_attr(
		all(target_arch = "wasm32", target_os = "unknown"),
		wasm_bindgen(js_name = "dirExists")
	)]
	#[cfg_attr(feature = "node", napi(js_name = "dirExists"))]
	pub async fn dir_exists_js(&self, parent: DirEnum, name: String) -> Result<(), Error> {
		self.dir_exists(&UnsharedDirectoryType::from(parent), &name)
			.await?;
		Ok(())
	}

	// because wasm_bindgen doesn't automatically camelify names
	// fixing PR: https://github.com/wasm-bindgen/wasm-bindgen/pull/4215
	// we need to sometimes redefine functions
	#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
	#[cfg_attr(
		all(target_arch = "wasm32", target_os = "unknown"),
		wasm_bindgen(js_name = "findItemInDir")
	)]
	pub async fn find_item_in_dir_js(
		&self,
		dir: DirEnum,
		#[wasm_bindgen(js_name = "nameOrUuid")] name_or_uuid: String,
	) -> Result<Option<NonRootObject>, Error> {
		let item = self
			.find_item_in_dir(&UnsharedDirectoryType::from(dir), &name_or_uuid)
			.await?;
		Ok(item.map(Into::into))
	}

	#[cfg(feature = "node")]
	#[cfg_attr(feature = "node", napi(js_name = "findItemInDir"))]
	pub async fn find_item_in_dir_js(
		&self,
		dir: DirEnum,
		name_or_uuid: String,
	) -> Result<Option<NonRootObject>, Error> {
		let item = self
			.find_item_in_dir(&UnsharedDirectoryType::from(dir), &name_or_uuid)
			.await?;
		Ok(item.map(Into::into))
	}
}

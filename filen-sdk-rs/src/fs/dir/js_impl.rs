use crate::{
	Error,
	auth::Client,
	fs::dir::{DirectoryType, UnsharedDirectoryType, meta::DirectoryMetaChanges},
	js::{Dir, DirColor, DirEnum, File, NonRootItemTagged, Root},
};
use crate::{
	fs::dir::HasContents,
	js::{AnyDirEnum, AnyDirEnumWithShareInfo, DirSizeResponse},
};
use filen_types::fs::UuidStr;
use wasm_bindgen::prelude::*;

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

pub(crate) use tuple_to_jsvalue;

#[wasm_bindgen]
impl Client {
	#[wasm_bindgen(js_name = "root")]
	pub fn root_js(&self) -> Root {
		self.root().clone().into()
	}

	#[wasm_bindgen(js_name = "getDir")]
	pub async fn get_dir_js(&self, uuid: UuidStr) -> Result<Dir, Error> {
		Ok(self.get_dir(uuid).await?.into())
	}

	#[wasm_bindgen(js_name = "createDir")]
	pub async fn create_dir_js(&self, parent: DirEnum, name: String) -> Result<Dir, Error> {
		Ok(self
			.create_dir(&UnsharedDirectoryType::from(parent), name)
			.await?
			.into())
	}

	async fn list_dir_inner_wasm(&self, parent: &impl HasContents) -> Result<JsValue, Error> {
		let (dirs, files) = self.list_dir(parent).await?;
		Ok(tuple_to_jsvalue!(
			dirs.into_iter().map(Dir::from).collect::<Vec<_>>(),
			files.into_iter().map(File::from).collect::<Vec<_>>()
		))
	}

	#[wasm_bindgen(unchecked_return_type = "[Dir[], File[]]", js_name = "listDir")]
	pub async fn list_dir_js(&self, dir: DirEnum) -> Result<JsValue, Error> {
		self.list_dir_inner_wasm(&UnsharedDirectoryType::from(dir))
			.await
	}

	#[wasm_bindgen(unchecked_return_type = "[Dir[], File[]]", js_name = "listRecents")]
	pub async fn list_recents_js(&self) -> Result<JsValue, Error> {
		use filen_types::fs::ParentUuid;

		self.list_dir_inner_wasm(&ParentUuid::Recents).await
	}

	#[wasm_bindgen(unchecked_return_type = "[Dir[], File[]]", js_name = "listFavorites")]
	pub async fn list_favorites_js(&self) -> Result<JsValue, Error> {
		use filen_types::fs::ParentUuid;

		self.list_dir_inner_wasm(&ParentUuid::Favorites).await
	}

	#[wasm_bindgen(
		unchecked_return_type = "[Dir[], File[]]",
		js_name = "listDirRecursive"
	)]
	pub async fn list_dir_recursive_js(&self, dir: DirEnum) -> Result<JsValue, Error> {
		let (dirs, files) = self
			.list_dir_recursive(&UnsharedDirectoryType::from(dir))
			.await?;
		Ok(tuple_to_jsvalue!(
			dirs.into_iter().map(Dir::from).collect::<Vec<_>>(),
			files.into_iter().map(File::from).collect::<Vec<_>>()
		))
	}

	#[wasm_bindgen(js_name = "deleteDirPermanently")]
	pub async fn delete_dir_permanently_js(&self, dir: Dir) -> Result<(), Error> {
		self.delete_dir_permanently(dir.into()).await
	}

	#[wasm_bindgen(js_name = "trashDir")]
	pub async fn trash_dir_js(&self, dir: Dir) -> Result<Dir, Error> {
		let mut dir = dir.into();
		self.trash_dir(&mut dir).await?;
		Ok(dir.into())
	}

	#[wasm_bindgen(js_name = "dirExists")]
	pub async fn dir_exists_js(&self, parent: AnyDirEnum, name: String) -> Result<(), Error> {
		self.dir_exists(&DirectoryType::from(parent), &name).await?;
		Ok(())
	}

	#[wasm_bindgen(js_name = "findItemInDir")]
	pub async fn find_item_in_dir_js(
		&self,
		dir: AnyDirEnum,
		#[wasm_bindgen(js_name = "nameOrUuid")] name_or_uuid: String,
	) -> Result<Option<NonRootItemTagged>, Error> {
		let item = self
			.find_item_in_dir(&DirectoryType::from(dir), &name_or_uuid)
			.await?;
		Ok(item.map(Into::into))
	}

	#[wasm_bindgen(js_name = "getDirSize")]
	pub async fn get_dir_size_js(
		&self,
		dir: AnyDirEnumWithShareInfo,
	) -> Result<DirSizeResponse, Error> {
		use crate::fs::dir::DirectoryTypeWithShareInfo;

		let resp = self
			.get_dir_size(&DirectoryTypeWithShareInfo::from(dir))
			.await?;
		Ok(DirSizeResponse {
			size: resp.size,
			files: resp.files,
			dirs: resp.dirs,
		})
	}

	#[wasm_bindgen(js_name = "updateDirMetadata")]
	pub async fn update_dir_metadata_js(
		&self,
		dir: Dir,
		changes: DirectoryMetaChanges,
	) -> Result<Dir, Error> {
		let mut dir = dir.into();
		self.update_dir_metadata(&mut dir, changes).await?;
		Ok(dir.into())
	}

	#[wasm_bindgen(js_name = "setDirColor")]
	pub async fn set_dir_color_js(
		&self,
		dir: Dir,
		#[wasm_bindgen(unchecked_param_type = "DirColor")] color: JsValue,
	) -> Result<Dir, JsValue> {
		let mut dir = dir.into();
		let color: DirColor = serde_wasm_bindgen::from_value(color)?;
		self.set_dir_color(&mut dir, color.into()).await?;
		Ok(dir.into())
	}
}

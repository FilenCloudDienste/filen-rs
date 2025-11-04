use crate::{
	Error,
	auth::JsClient,
	fs::dir::{
		DirectoryType, DirectoryTypeWithShareInfo, UnsharedDirectoryType,
		meta::DirectoryMetaChanges,
	},
	js::{Dir, DirColor, DirEnum, DirsAndFiles, File, NonRootItemTagged, Root},
	runtime::do_on_commander,
};
use crate::{
	fs::dir::HasContents,
	js::{AnyDirEnum, AnyDirEnumWithShareInfo, DirSizeResponse},
};
use filen_types::fs::{ParentUuid, UuidStr};

impl JsClient {
	async fn list_dir_inner_wasm<T>(&self, parent: T) -> Result<DirsAndFiles, Error>
	where
		T: HasContents + Send + 'static,
	{
		let this = self.inner();
		let (dirs, files) = do_on_commander(move || async move {
			this.list_dir(&parent).await.map(|(dirs, files)| {
				(
					dirs.into_iter().map(Dir::from).collect::<Vec<_>>(),
					files.into_iter().map(File::from).collect::<Vec<_>>(),
				)
			})
		})
		.await?;
		Ok(DirsAndFiles { dirs, files })
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
		wasm_bindgen::prelude::wasm_bindgen
	)]
	pub fn root(&self) -> Root {
		self.inner_ref().root().clone().into()
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "getDir")
	)]
	pub async fn get_dir(&self, uuid: UuidStr) -> Result<Dir, Error> {
		let this = self.inner();
		do_on_commander(move || async move { this.get_dir(uuid).await.map(Dir::from) }).await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "createDir")
	)]
	pub async fn create_dir(&self, parent: DirEnum, name: String) -> Result<Dir, Error> {
		let this = self.inner();
		do_on_commander(move || async move {
			this.create_dir(&UnsharedDirectoryType::from(parent), name)
				.await
				.map(Dir::from)
		})
		.await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "listDir")
	)]
	pub async fn list_dir(&self, dir: DirEnum) -> Result<DirsAndFiles, Error> {
		self.list_dir_inner_wasm(UnsharedDirectoryType::from(dir))
			.await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "listRecents")
	)]
	pub async fn list_recents(&self) -> Result<DirsAndFiles, Error> {
		self.list_dir_inner_wasm(ParentUuid::Recents).await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "listFavorites")
	)]
	pub async fn list_favorites(&self) -> Result<DirsAndFiles, Error> {
		self.list_dir_inner_wasm(ParentUuid::Favorites).await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "listDirRecursive")
	)]
	pub async fn list_dir_recursive(&self, dir: DirEnum) -> Result<DirsAndFiles, Error> {
		let this = self.inner();
		let (dirs, files) = do_on_commander(move || async move {
			this.list_dir_recursive(&UnsharedDirectoryType::from(dir))
				.await
				.map(|(dirs, files)| {
					(
						dirs.into_iter().map(Dir::from).collect::<Vec<_>>(),
						files.into_iter().map(File::from).collect::<Vec<_>>(),
					)
				})
		})
		.await?;
		Ok(DirsAndFiles { dirs, files })
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "deleteDirPermanently")
	)]
	pub async fn delete_dir_permanently(&self, dir: Dir) -> Result<(), Error> {
		let this = self.inner();
		do_on_commander(move || async move { this.delete_dir_permanently(dir.into()).await }).await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "trashDir")
	)]
	pub async fn trash_dir(&self, dir: Dir) -> Result<Dir, Error> {
		let this = self.inner();
		do_on_commander(move || async move {
			let mut dir = dir.into();
			this.trash_dir(&mut dir).await?;
			Ok(dir.into())
		})
		.await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "dirExists")
	)]
	pub async fn dir_exists(&self, parent: AnyDirEnum, name: String) -> Result<(), Error> {
		let this = self.inner();
		do_on_commander(move || async move {
			this.dir_exists(&DirectoryType::from(parent), &name)
				.await
				.map(|_| ())
		})
		.await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "findItemInDir")
	)]
	pub async fn find_item_in_dir(
		&self,
		dir: AnyDirEnum,
		name_or_uuid: String,
	) -> Result<Option<NonRootItemTagged>, Error> {
		let this = self.inner();
		do_on_commander(move || async move {
			this.find_item_in_dir(&DirectoryType::from(dir), &name_or_uuid)
				.await
				.map(|item| item.map(Into::into))
		})
		.await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "getDirSize")
	)]
	pub async fn get_dir_size(
		&self,
		dir: AnyDirEnumWithShareInfo,
	) -> Result<DirSizeResponse, Error> {
		let this = self.inner();

		do_on_commander(move || async move {
			this.get_dir_size(&DirectoryTypeWithShareInfo::from(dir))
				.await
				.map(|resp| DirSizeResponse {
					size: resp.size,
					files: resp.files,
					dirs: resp.dirs,
				})
		})
		.await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "updateDirMetadata")
	)]
	pub async fn update_dir_metadata(
		&self,
		dir: Dir,
		changes: DirectoryMetaChanges,
	) -> Result<Dir, Error> {
		let this = self.inner();
		do_on_commander(move || async move {
			let mut dir = dir.into();
			this.update_dir_metadata(&mut dir, changes).await?;
			Ok(dir.into())
		})
		.await
	}

	pub async fn set_dir_color(&self, dir: Dir, color: DirColor) -> Result<Dir, Error> {
		let this = self.inner();
		do_on_commander(move || async move {
			let mut dir = dir.into();
			this.set_dir_color(&mut dir, color.into()).await?;
			Ok(dir.into())
		})
		.await
	}
}

#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	wasm_bindgen::prelude::wasm_bindgen(js_class = "Client")
)]
impl JsClient {
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	#[wasm_bindgen::prelude::wasm_bindgen(js_name = "setDirColor")]
	pub async fn set_dir_color_wasm(
		&self,
		dir: Dir,
		#[wasm_bindgen(unchecked_param_type = "DirColor")] color: DirColor,
	) -> Result<Dir, Error> {
		self.set_dir_color(dir, color).await
	}
}

use std::{borrow::Cow, sync::Arc};

use crate::{
	Error,
	auth::{Client, JsClient},
	fs::{
		categories::{
			DirType, Normal,
			fs::{CategoryFS, CategoryFSExt},
		},
		dir::meta::DirectoryMetaChanges,
	},
	js::{
		AnyDirWithContext, AnyNormalDir, Dir, DirByCategoryWithContext, DirColor, DirWithPath,
		DirsAndFiles, DirsAndFilesWithPaths, File, FileWithPath, NonRootDirTagged,
		NonRootItemTagged, NormalDirsAndFiles, Root,
	},
	runtime::do_on_commander,
};
use crate::{
	fs::categories::{Linked, Shared},
	js::NonRootDir,
};
use filen_types::fs::UuidStr;

impl JsClient {
	async fn list_dir_inner_wasm<Cat: CategoryFS<Client = Client>, F>(
		&self,
		parent: DirType<'static, Cat>,
		progress: Option<F>,
		context: Cat::ListDirContext<'static>,
	) -> Result<NormalDirsAndFiles, Error>
	where
		F: Fn(u64, Option<u64>) + Send + Sync + 'static,
		Cat::File: Into<File>,
		Cat::Dir: Into<Dir>,
	{
		let this = self.inner();
		let (dirs, files) = do_on_commander(move || async move {
			Cat::list_dir(&*this, &parent, progress.as_ref(), context)
				.await
				.map(|(dirs, files)| {
					(
						dirs.into_iter().map(Into::<Dir>::into).collect::<Vec<_>>(),
						files
							.into_iter()
							.map(Into::<File>::into)
							.collect::<Vec<_>>(),
					)
				})
		})
		.await?;
		Ok(NormalDirsAndFiles { dirs, files })
	}
}

#[cfg(feature = "uniffi")]
#[uniffi::export(with_foreign)]
pub trait DirContentDownloadProgressCallback: Send + Sync {
	fn on_progress(&self, bytes_downloaded: u64, total_bytes: Option<u64>);
}

#[cfg(feature = "uniffi")]
#[uniffi::export(with_foreign)]
pub trait DirContentDownloadErrorCallback: Send + Sync {
	fn on_errors(&self, errors: Vec<std::sync::Arc<Error>>);
}

#[cfg(feature = "uniffi")]
#[uniffi::export]
impl JsClient {
	pub async fn list_dir_recursive(
		&self,
		dir: AnyDirWithContext,
		callback: Option<std::sync::Arc<dyn DirContentDownloadProgressCallback>>,
	) -> Result<DirsAndFiles, Error> {
		let callback = callback.map(|cb| {
			move |downloaded, total| {
				let callback = std::sync::Arc::clone(&cb);
				tokio::task::spawn_blocking(move || {
					callback.on_progress(downloaded, total);
				});
			}
		});
		self.inner_list_dir_recursive(dir, callback).await
	}

	pub async fn list_dir_recursive_with_paths(
		&self,
		dir: AnyDirWithContext,
		list_dir_progress_callback: Option<std::sync::Arc<dyn DirContentDownloadProgressCallback>>,
		scan_error_callback: std::sync::Arc<dyn DirContentDownloadErrorCallback>,
	) -> Result<DirsAndFilesWithPaths, Error> {
		let list_dir_progress_callback = list_dir_progress_callback.map(|cb| {
			move |downloaded, total| {
				let callback = std::sync::Arc::clone(&cb);
				tokio::task::spawn_blocking(move || {
					callback.on_progress(downloaded, total);
				});
			}
		});

		self.inner_list_dir_recursive_with_paths(dir, list_dir_progress_callback, move |errors| {
			let callback = std::sync::Arc::clone(&scan_error_callback);
			let errors = errors
				.into_iter()
				.map(std::sync::Arc::new)
				.collect::<Vec<_>>();
			tokio::task::spawn_blocking(move || {
				callback.on_errors(errors);
			});
		})
		.await
	}
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
#[wasm_bindgen::prelude::wasm_bindgen(js_class = "Client")]
impl JsClient {
	#[wasm_bindgen::prelude::wasm_bindgen(js_name = "listDirRecursive")]
	pub async fn list_dir_recursive(
		&self,
		dir: AnyDirWithContext,
		#[wasm_bindgen(
			unchecked_param_type = "(downloadedBytes: number, totalBytes: number | undefined) => void | undefined"
		)]
		callback: web_sys::js_sys::Function,
	) -> Result<DirsAndFiles, Error> {
		use crate::runtime;
		use wasm_bindgen::JsValue;

		let callback = if !callback.is_undefined() {
			let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
			runtime::spawn_local(async move {
				while let Some((downloaded, total)) = receiver.recv().await {
					let _ = callback.call2(
						&JsValue::UNDEFINED,
						&JsValue::from_f64(downloaded as f64),
						&match total {
							Some(v) => JsValue::from_f64(v as f64),
							None => JsValue::UNDEFINED,
						},
					);
				}
			});
			Some(move |downloaded, total| {
				let _ = sender.send((downloaded, total));
			})
		} else {
			None
		};

		self.inner_list_dir_recursive(dir, callback).await
	}

	#[wasm_bindgen::prelude::wasm_bindgen(js_name = "listDirRecursiveWithPaths")]
	pub async fn list_dir_recursive_with_paths(
		&self,
		dir: AnyDirWithContext,
		#[wasm_bindgen(
			unchecked_param_type = "(downloadedBytes: number, totalBytes: number | undefined) => void | undefined"
		)]
		list_dir_progress_callback: web_sys::js_sys::Function,
		#[wasm_bindgen(unchecked_param_type = "(errors: [FilenSdkError]) => void")]
		scan_error_callback: web_sys::js_sys::Function,
	) -> Result<DirsAndFilesWithPaths, Error> {
		use crate::runtime;
		use wasm_bindgen::JsValue;
		let (ls_sender, mut ls_receiver) = tokio::sync::mpsc::unbounded_channel();

		let list_dir_progress_callback = if !list_dir_progress_callback.is_undefined() {
			runtime::spawn_local(async move {
				while let Some((downloaded, total)) = ls_receiver.recv().await {
					let _ = list_dir_progress_callback.call2(
						&JsValue::UNDEFINED,
						&JsValue::from_f64(downloaded as f64),
						&match total {
							Some(v) => JsValue::from_f64(v as f64),
							None => JsValue::UNDEFINED,
						},
					);
				}
			});
			Some(move |downloaded, total| {
				let _ = ls_sender.send((downloaded, total));
			})
		} else {
			None
		};

		let (err_sender, mut err_receiver) = tokio::sync::mpsc::unbounded_channel::<Vec<Error>>();
		runtime::spawn_local(async move {
			while let Some(errors) = err_receiver.recv().await {
				let js_errors = web_sys::js_sys::Array::new();
				for error in errors {
					js_errors.push(&JsValue::from(error));
				}
				let _ = scan_error_callback.call1(&JsValue::UNDEFINED, &js_errors);
			}
		});

		self.inner_list_dir_recursive_with_paths(dir, list_dir_progress_callback, move |errors| {
			let _ = err_sender.send(errors);
		})
		.await
	}
}

fn dirs_and_files_into_js<D, F>((dirs, files): (Vec<D>, Vec<F>)) -> DirsAndFiles
where
	D: Into<NonRootDir>,
	F: Into<File>,
{
	let dirs = dirs
		.into_iter()
		.map(Into::<NonRootDirTagged>::into)
		.collect::<Vec<_>>();
	let files = files
		.into_iter()
		.map(Into::<File>::into)
		.collect::<Vec<_>>();
	DirsAndFiles { dirs, files }
}

#[allow(clippy::type_complexity)]
fn dirs_and_files_into_js_with_paths<D, F>(
	(dirs, files): (Vec<(D, String)>, Vec<(F, String)>),
) -> DirsAndFilesWithPaths
where
	NonRootDir: From<D>,
	F: Into<File>,
{
	let dirs = dirs
		.into_iter()
		.map(|(d, path)| DirWithPath {
			dir: NonRootDirTagged::from(d),
			path,
		})
		.collect::<Vec<_>>();
	let files = files
		.into_iter()
		.map(|(f, path)| FileWithPath {
			file: f.into(),
			path,
		})
		.collect::<Vec<_>>();
	DirsAndFilesWithPaths { dirs, files }
}

impl JsClient {
	async fn inner_list_dir_recursive<F>(
		&self,
		dir: AnyDirWithContext,
		callback: Option<F>,
	) -> Result<DirsAndFiles, Error>
	where
		F: Fn(u64, Option<u64>) + Send + Sync + 'static,
	{
		let this = self.inner();
		do_on_commander(move || async move {
			let dir_by_category = DirByCategoryWithContext::from(dir);
			match dir_by_category {
				DirByCategoryWithContext::Normal(dir) => {
					Normal::list_dir_recursive(&this, &dir, callback.as_ref(), ())
						.await
						.map(dirs_and_files_into_js)
				}
				DirByCategoryWithContext::Shared(dir, sharing_role) => {
					Shared::list_dir_recursive(&this, &dir, callback.as_ref(), &sharing_role)
						.await
						.map(dirs_and_files_into_js)
				}
				DirByCategoryWithContext::Linked(dir, dir_public_link) => {
					Linked::list_dir_recursive(
						this.unauthed(),
						&dir,
						callback.as_ref(),
						Cow::Owned(dir_public_link),
					)
					.await
					.map(dirs_and_files_into_js)
				}
			}
		})
		.await
	}

	async fn inner_list_dir_recursive_with_paths<F, F1>(
		&self,
		dir: AnyDirWithContext,
		list_dir_callback: Option<F>,
		mut scan_errors_callback: F1,
	) -> Result<DirsAndFilesWithPaths, Error>
	where
		F: Fn(u64, Option<u64>) + Send + Sync + 'static,
		F1: FnMut(Vec<Error>) + Send + Sync + 'static,
	{
		let this = self.inner();
		do_on_commander(move || async move {
			let dir_by_category = DirByCategoryWithContext::from(dir);
			match dir_by_category {
				DirByCategoryWithContext::Normal(dir) => Normal::list_dir_recursive_with_paths(
					this,
					dir,
					list_dir_callback.as_ref(),
					&mut scan_errors_callback,
					(),
				)
				.await
				.map(dirs_and_files_into_js_with_paths),
				DirByCategoryWithContext::Shared(dir, sharing_role) => {
					Shared::list_dir_recursive_with_paths(
						this,
						dir,
						list_dir_callback.as_ref(),
						&mut scan_errors_callback,
						&sharing_role,
					)
					.await
					.map(dirs_and_files_into_js_with_paths)
				}
				DirByCategoryWithContext::Linked(dir, dir_public_link) => {
					Linked::list_dir_recursive_with_paths(
						Arc::new(this.get_unauthed()),
						dir,
						list_dir_callback.as_ref(),
						&mut scan_errors_callback,
						Cow::Owned(dir_public_link),
					)
					.await
					.map(dirs_and_files_into_js_with_paths)
				}
			}
		})
		.await
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
	pub async fn create_dir(&self, parent: AnyNormalDir, name: String) -> Result<Dir, Error> {
		let this = self.inner();
		do_on_commander(move || async move {
			this.create_dir(&DirType::<'static, Normal>::from(parent), name)
				.await
				.map(Dir::from)
		})
		.await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "listDir")
	)]
	pub async fn list_dir(&self, dir: AnyNormalDir) -> Result<NormalDirsAndFiles, Error> {
		self.list_dir_inner_wasm(
			DirType::<'static, Normal>::from(dir),
			None::<&fn(u64, Option<u64>)>,
			(),
		)
		.await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "listRecents")
	)]
	pub async fn list_recents(&self) -> Result<NormalDirsAndFiles, Error> {
		let this = self.inner();
		do_on_commander(move || async move {
			let (dirs, files) = this.list_recents(None::<&fn(u64, Option<u64>)>).await?;
			Ok(NormalDirsAndFiles {
				dirs: dirs.into_iter().map(Into::<Dir>::into).collect(),
				files: files.into_iter().map(Into::<File>::into).collect(),
			})
		})
		.await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "listFavorites")
	)]
	pub async fn list_favorites(&self) -> Result<NormalDirsAndFiles, Error> {
		let this = self.inner();
		do_on_commander(move || async move {
			let (dirs, files) = this.list_favorites(None::<&fn(u64, Option<u64>)>).await?;
			Ok(NormalDirsAndFiles {
				dirs: dirs.into_iter().map(Into::<Dir>::into).collect(),
				files: files.into_iter().map(Into::<File>::into).collect(),
			})
		})
		.await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "restoreDir")
	)]
	pub async fn restore_dir(&self, dir: Dir) -> Result<Dir, Error> {
		let this = self.inner();
		do_on_commander(move || async move {
			let mut dir = dir.into();
			this.restore_dir(&mut dir).await?;
			Ok(dir.into())
		})
		.await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "moveDir")
	)]
	pub async fn move_dir(&self, dir: Dir, new_parent: AnyNormalDir) -> Result<Dir, Error> {
		let this = self.inner();
		do_on_commander(move || async move {
			let mut dir = dir.into();
			this.move_dir(&mut dir, &DirType::<'static, Normal>::from(new_parent))
				.await?;
			Ok(dir.into())
		})
		.await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "listTrash")
	)]
	pub async fn list_trash(&self) -> Result<NormalDirsAndFiles, Error> {
		let this = self.inner();
		do_on_commander(move || async move {
			let (dirs, files) = this.list_trash(None::<&fn(u64, Option<u64>)>).await?;
			Ok(NormalDirsAndFiles {
				dirs: dirs.into_iter().map(Into::<Dir>::into).collect(),
				files: files.into_iter().map(Into::<File>::into).collect(),
			})
		})
		.await
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
	pub async fn dir_exists(&self, parent: AnyNormalDir, name: String) -> Result<(), Error> {
		let this = self.inner();
		do_on_commander(move || async move {
			this.dir_exists(&DirType::<'static, Normal>::from(parent), &name)
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
		dir: AnyDirWithContext,
		name_or_uuid: String,
	) -> Result<Option<NonRootItemTagged>, Error> {
		let this = self.inner();
		do_on_commander(move || async move {
			let dir_by_category = DirByCategoryWithContext::from(dir);
			Ok(match dir_by_category {
				DirByCategoryWithContext::Normal(dir) => Normal::find_item_in_dir(
					&*this,
					&dir,
					None::<&fn(u64, Option<u64>)>,
					&name_or_uuid,
					(),
				)
				.await?
				.map(NonRootItemTagged::from),
				DirByCategoryWithContext::Shared(dir, share_info) => Shared::find_item_in_dir(
					&*this,
					&dir,
					None::<&fn(u64, Option<u64>)>,
					&name_or_uuid,
					&share_info,
				)
				.await?
				.map(NonRootItemTagged::from),
				DirByCategoryWithContext::Linked(dir, link_info) => Linked::find_item_in_dir(
					this.unauthed(),
					&dir,
					None::<&fn(u64, Option<u64>)>,
					&name_or_uuid,
					Cow::Owned(link_info),
				)
				.await?
				.map(NonRootItemTagged::from),
			})
		})
		.await
	}

	// #[cfg_attr(
	// 	all(target_family = "wasm", target_os = "unknown"),
	// 	wasm_bindgen::prelude::wasm_bindgen(js_name = "getDirSize")
	// )]
	// pub async fn get_dir_size(&self, dir: AnyDirWithContext) -> Result<DirSizeResponse, Error> {
	// 	let this = self.inner();

	// 	do_on_commander(move || async move {
	// 		this.get_dir_size(DirectoryTypeWithShareInfo::from(dir))
	// 			.await
	// 			.map(|resp| DirSizeResponse {
	// 				size: resp.size,
	// 				files: resp.files,
	// 				dirs: resp.dirs,
	// 			})
	// 	})
	// 	.await
	// }

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

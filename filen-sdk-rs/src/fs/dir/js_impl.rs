use std::{borrow::Cow, sync::Arc};

use crate::{
	Error,
	auth::{Client, JsClient, js_impls::UnauthJsClient},
	error::ResultExt,
	fs::categories::{
		DirType, Normal,
		fs::{CategoryFS, CategoryFSExt},
	},
	js::{
		AnyDirWithContext, AnyLinkedDirWithContext, AnyNormalDir, Dir, DirByCategoryWithContext,
		DirColor, DirSizeResponse, DirWithPath, DirectoryMetaChanges, DirsAndFiles,
		DirsAndFilesWithPaths, File, FileWithPath, NonRootDirTagged, NonRootItemTagged,
		NormalDirsAndFiles, Root,
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

/// `JsClient.listDirRecursiveWithPaths` on napi, which takes callback objects the macros cannot
/// read. Each becomes one [`Listener`](crate::js::napi::Listener), read and bound on the JS thread
/// when the call is made; the listing resolves with the whole tree at once, and every callback call
/// made before it returned runs before its promise settles.
#[cfg(feature = "napi")]
mod napi_impl {
	use std::sync::Arc;

	use napi::{
		Env,
		bindgen_prelude::{BigInt, Either, FnArgs, FunctionRef, Object, PromiseRaw, Undefined},
	};

	use crate::{
		Error, auth,
		js::{
			AnyDirWithContext, DirsAndFilesWithPaths,
			napi::{
				FromWire, IntoWire, Listener, ListenerCall, OptionReturn, Wire, bound_method,
				spawn_promise,
			},
		},
	};

	/// One call of a listing's `DirContentDownloadProgressCallback`: the bytes listed so far and
	/// the total.
	pub(super) struct ListProgress {
		downloaded: BigInt,
		/// `undefined` while the total is unknown, as ubrn passes it, where napi writes `null`.
		total: Either<BigInt, Undefined>,
	}

	impl ListProgress {
		fn new(downloaded: u64, total: Option<u64>) -> Self {
			Self {
				downloaded: downloaded.into_wire(),
				total: OptionReturn(total).into_wire(),
			}
		}
	}

	impl ListenerCall for ListProgress {
		type Methods = FunctionRef<FnArgs<(BigInt, Either<BigInt, Undefined>)>, ()>;

		fn bind(callback: &Object<'_>) -> napi::Result<Self::Methods> {
			bound_method(callback, "onProgress")
		}

		fn call(self, env: &Env, on_progress: &Self::Methods) -> napi::Result<()> {
			// `call`, not `apply`: it takes a throw out of the environment and returns it, where a
			// pending one would end the process as an uncaught exception.
			on_progress
				.borrow_back(env)?
				.call(FnArgs::from((self.downloaded, self.total)))
		}
	}

	/// One call of a listing's `DirContentDownloadErrorCallback`: the errors the scan met.
	pub(super) struct ListErrors(Vec<<Arc<Error> as Wire>::Wire>);

	impl ListenerCall for ListErrors {
		type Methods = FunctionRef<Vec<<Arc<Error> as Wire>::Wire>, ()>;

		fn bind(callback: &Object<'_>) -> napi::Result<Self::Methods> {
			bound_method(callback, "onErrors")
		}

		fn call(self, env: &Env, on_errors: &Self::Methods) -> napi::Result<()> {
			// `call`, not `apply`, as in `ListProgress::call`.
			on_errors.borrow_back(env)?.call(self.0)
		}
	}

	/// `listDirRecursiveWithPaths`'s body, outside the module whose `#[napi] impl` needs the
	/// `unsafe_code` allow. A callback without its method throws napi's own error synchronously; a
	/// directory the SDK cannot read rejects.
	// napi-derive registers the method calling this only outside `cfg(test)`.
	#[cfg_attr(test, allow(dead_code))]
	fn list_dir_recursive_with_paths<'env>(
		client: &auth::JsClient,
		env: &'env Env,
		dir: <AnyDirWithContext as Wire>::Wire,
		list_dir_progress_callback: Option<Object<'env>>,
		scan_error_callback: Object<'env>,
		async_opts: Option<Object<'env>>,
	) -> napi::Result<PromiseRaw<'env, <DirsAndFilesWithPaths as Wire>::Wire>> {
		let progress = list_dir_progress_callback
			.map(|callback| Listener::<ListProgress>::new(env, callback))
			.transpose()?;
		let errors = Listener::<ListErrors>::new(env, scan_error_callback)?;
		// Read here, on the JS thread: no JS value enters the future.
		let dir = AnyDirWithContext::from_wire(dir);
		let client = client.clone();
		spawn_promise(env, async_opts, async move {
			let dir = dir?;
			let on_progress = progress.clone().map(|listener| {
				move |downloaded, total| listener.send(ListProgress::new(downloaded, total))
			});
			let on_errors = {
				let errors = errors.clone();
				move |scan_errors: Vec<Error>| {
					errors.send(ListErrors(
						scan_errors
							.into_iter()
							.map(|e| Arc::new(e).into_wire())
							.collect(),
					));
				}
			};
			let listed = client
				.inner_list_dir_recursive_with_paths(dir, on_progress, on_errors)
				.await;
			if let Some(progress) = &progress {
				progress.flushed().await;
			}
			errors.flushed().await;
			listed
		})
	}

	// napi-derive's generated FFI glue (the raw N-API calls of the method's callback) lands in
	// this module, which holds nothing but the napi impl forwarding to
	// `list_dir_recursive_with_paths`.
	#[allow(unsafe_code)]
	mod class {
		use napi::{
			Env,
			bindgen_prelude::{Object, PromiseRaw},
		};
		use napi_derive::napi;

		use super::list_dir_recursive_with_paths;
		use crate::js::{
			AnyDirWithContext, DirsAndFilesWithPaths,
			napi::{JsClient, Wire},
		};

		#[napi]
		impl JsClient {
			/// List `dir` and everything below it, in one batch. Each path is the remote names
			/// from `dir` down, joined with `/` and relative to `dir`; the names are NOT checked
			/// for components unsafe on a local disk (`..`, separators, reserved names).
			/// `listDirProgressCallback` gets the bytes of the listing downloaded so far and its
			/// total, if known; `scanErrorCallback` the errors met reading the listing.
			#[napi(
				js_name = "listDirRecursiveWithPaths",
				ts_args_type = "dir: AnyDirWithContext, listDirProgressCallback: DirContentDownloadProgressCallback | undefined, scanErrorCallback: DirContentDownloadErrorCallback, asyncOpts_?: { signal: AbortSignal }",
				ts_return_type = "Promise<DirsAndFilesWithPaths>"
			)]
			pub fn __napi_list_dir_recursive_with_paths<'env>(
				&self,
				env: &'env Env,
				dir: <AnyDirWithContext as Wire>::Wire,
				list_dir_progress_callback: Option<Object<'env>>,
				scan_error_callback: Object<'env>,
				async_opts: Option<Object<'env>>,
			) -> napi::Result<PromiseRaw<'env, <DirsAndFilesWithPaths as Wire>::Wire>> {
				list_dir_recursive_with_paths(
					&self.0,
					env,
					dir,
					list_dir_progress_callback,
					scan_error_callback,
					async_opts,
				)
			}
		}
	}

	#[cfg(test)]
	mod tests {
		use super::*;

		#[test]
		fn an_unknown_listing_total_is_written_as_undefined() {
			let ListProgress {
				downloaded: unknown_downloaded,
				total: Either::B(()),
			} = ListProgress::new(4, None)
			else {
				panic!("an unknown total was not undefined");
			};
			let ListProgress {
				downloaded,
				total: Either::A(total),
			} = ListProgress::new(6, Some(u64::MAX))
			else {
				panic!("a known total was not a bigint");
			};
			assert_eq!(u64::from_wire(unknown_downloaded).unwrap(), 4);
			assert_eq!(u64::from_wire(downloaded).unwrap(), 6);
			assert_eq!(u64::from_wire(total).unwrap(), u64::MAX);
		}
	}
}

#[filen_macros::js_exports]
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

// Only the uniffi and wasm bindings list a tree without paths.
#[cfg_attr(feature = "napi", allow(dead_code))]
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
	// Only the uniffi and wasm bindings list a tree without paths.
	#[cfg_attr(feature = "napi", allow(dead_code))]
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
						Cow::Owned(dir_public_link.try_into()?),
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
						Cow::Owned(dir_public_link.try_into()?),
					)
					.await
					.map(dirs_and_files_into_js_with_paths)
				}
			}
		})
		.await
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
		wasm_bindgen::prelude::wasm_bindgen
	)]
	#[napi_export]
	pub fn root(&self) -> Root {
		self.inner_ref().root().clone().into()
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "getDir")
	)]
	#[napi_export]
	pub async fn get_dir(&self, uuid: UuidStr) -> Result<Dir, Error> {
		let this = self.inner();
		do_on_commander(move || async move { this.get_dir(uuid.into()).await.map(Dir::from) }).await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "getDirOptional")
	)]
	pub async fn get_dir_optional(&self, uuid: UuidStr) -> Result<Option<Dir>, Error> {
		self.get_dir(uuid).await.optional()
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "createDir")
	)]
	#[napi_export]
	pub async fn create_dir(&self, parent: AnyNormalDir, name: String) -> Result<Dir, Error> {
		let this = self.inner();
		do_on_commander(move || async move {
			this.create_dir(&DirType::<'static, Normal>::from(parent), &name)
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
	pub async fn dir_exists(&self, parent: AnyNormalDir, name: String) -> Result<bool, Error> {
		let this = self.inner();
		do_on_commander(move || async move {
			this.dir_exists(&DirType::<'static, Normal>::from(parent), &name)
				.await
				.map(|o| o.is_some())
		})
		.await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "findDirUuid")
	)]
	pub async fn find_dir_uuid(
		&self,
		parent: UuidStr,
		name: String,
	) -> Result<Option<UuidStr>, Error> {
		let this = self.inner();
		do_on_commander(move || async move {
			this.find_dir_uuid(parent.into(), &name)
				.await
				.map(|uuid| uuid.map(UuidStr::from))
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
					Cow::Owned(link_info.try_into()?),
				)
				.await?
				.map(NonRootItemTagged::from),
			})
		})
		.await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "getDirSize")
	)]
	pub async fn get_dir_size(&self, dir: AnyDirWithContext) -> Result<DirSizeResponse, Error> {
		let this = self.inner();

		do_on_commander(move || async move {
			let dir_by_category = DirByCategoryWithContext::from(dir);

			let info = match dir_by_category {
				DirByCategoryWithContext::Normal(dir) => Normal::dir_size(&*this, &dir, ()).await?,
				DirByCategoryWithContext::Shared(dir, share_info) => {
					Shared::dir_size(&*this, &dir, &share_info).await?
				}
				DirByCategoryWithContext::Linked(dir, link_info) => {
					Linked::dir_size(this.unauthed(), &dir, Cow::Owned(link_info.try_into()?))
						.await?
				}
			};

			Ok(DirSizeResponse {
				size: info.size,
				files: info.files,
				dirs: info.dirs,
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
			this.update_dir_metadata(&mut dir, changes.try_into()?)
				.await?;
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

#[filen_macros::js_exports]
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

#[filen_macros::js_exports]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	wasm_bindgen::prelude::wasm_bindgen(js_class = "UnauthClient")
)]
#[cfg_attr(feature = "uniffi", uniffi::export)]
impl UnauthJsClient {
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "getDirSize")
	)]
	pub async fn get_dir_size(
		&self,
		dir: AnyLinkedDirWithContext,
	) -> Result<DirSizeResponse, Error> {
		let this = self.inner();

		do_on_commander(move || async move {
			let parsed_dir = DirType::from(dir.dir);

			Linked::dir_size(&*this, &parsed_dir, Cow::Owned(dir.link.try_into()?))
				.await
				.map(|resp| DirSizeResponse {
					size: resp.size,
					files: resp.files,
					dirs: resp.dirs,
				})
		})
		.await
	}
}

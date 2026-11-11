//! The path transfers' napi exports, which take callback objects the macros cannot read:
//! `JsClient.downloadFileToPath`, `JsClient.uploadFile` and `JsClient.downloadDirRecursively`.
//!
//! A call's callback object becomes one [`Listener`], read and bound on the JS thread when the call
//! is made; the SDK's progress reaches it from the commander runtime without waiting on JS. Every
//! call it makes before the SDK returns reaches JS before the call's promise settles (the call
//! awaits [`Listener::flushed`]), except after an abort: one through `asyncOpts_` drops the call at
//! once, and one through the managed future's `ManagedAbortSignal` rejects as soon as the transfer
//! is told to stop, while it may still make a call or two and remove its temp file just after. In
//! both cases calls already queued, or made while the transfer stops, may arrive after the
//! rejection.

use std::sync::Arc;

use napi::{
	Env,
	bindgen_prelude::{BigInt, Either, FnArgs, FunctionRef, Object, PromiseRaw, Undefined},
};

use super::DownloadError;
use crate::{
	Error, auth,
	fs::categories::{Category, NonRootItemType, fs::CategoryFS},
	io::DirDownloadCallback,
	js::{
		AnyDirWithContext, AnyFile, DirWithPath, File, FileBuilderParamsOptionalName, FileWithPath,
		NonRootDir, NonRootItem,
		napi::{
			FromWire, IntoWire, Listener, ListenerCall, OptionReturn, Wire, bound_method,
			read_managed_future, spawn_promise,
		},
	},
	util::MaybeSendCallback,
};

/// The bytes a download or an upload moved since its last call, for its `onUpdate`.
pub(crate) struct FileProgress(BigInt);

impl ListenerCall for FileProgress {
	type Methods = FunctionRef<BigInt, ()>;

	fn bind(callback: &Object<'_>) -> napi::Result<Self::Methods> {
		bound_method(callback, "onUpdate")
	}

	fn call(self, env: &Env, on_update: &Self::Methods) -> napi::Result<()> {
		// `call`, not `apply`: it takes a throw out of the environment and returns it, where a
		// pending one would end the process as an uncaught exception.
		on_update.borrow_back(env)?.call(self.0)
	}
}

/// The SDK's progress callback for `listener`.
fn file_progress(listener: Listener<FileProgress>) -> MaybeSendCallback<'static, u64> {
	Arc::new(move |bytes: u64| listener.send(FileProgress(bytes.into_wire())))
}

/// `downloadFileToPath`'s body, outside the module whose `#[napi] impl` needs the `unsafe_code`
/// allow. A callback without a callable `onUpdate` throws napi's own error synchronously; a file
/// or a managed future the SDK cannot read rejects.
// napi-derive registers the method calling this only outside `cfg(test)`.
#[cfg_attr(test, allow(dead_code))]
fn download_file_to_path<'env>(
	client: &auth::JsClient,
	env: &'env Env,
	file: <AnyFile as Wire>::Wire,
	file_path: String,
	callback: Option<Object<'env>>,
	managed_future: Object<'env>,
	async_opts: Option<Object<'env>>,
) -> napi::Result<PromiseRaw<'env, ()>> {
	let listener = callback
		.map(|callback| Listener::<FileProgress>::new(env, callback))
		.transpose()?;
	let managed_future = read_managed_future(&managed_future)?;
	// Read here, on the JS thread: no JS value enters the future.
	let file = AnyFile::from_wire(file);
	let client = client.inner();
	spawn_promise(env, async_opts, async move {
		let file = file?;
		let managed_future = managed_future?;
		let progress = listener.clone().map(file_progress);
		let downloaded = managed_future
			.into_js_managed_commander_future(move || {
				super::download_file_to_path(client, file, file_path, progress)
			})
			.await;
		if let Some(listener) = &listener {
			listener.flushed().await;
		}
		downloaded
	})
}

/// `uploadFile`'s body, as [`download_file_to_path`]'s: a callback without a callable `onUpdate`
/// throws napi's own error synchronously; params or a managed future the SDK cannot read reject.
// napi-derive registers the method calling this only outside `cfg(test)`.
#[cfg_attr(test, allow(dead_code))]
fn upload_file<'env>(
	client: &auth::JsClient,
	env: &'env Env,
	params: <FileBuilderParamsOptionalName as Wire>::Wire,
	file_path: String,
	callback: Option<Object<'env>>,
	managed_future: Object<'env>,
	async_opts: Option<Object<'env>>,
) -> napi::Result<PromiseRaw<'env, <File as Wire>::Wire>> {
	let listener = callback
		.map(|callback| Listener::<FileProgress>::new(env, callback))
		.transpose()?;
	let managed_future = read_managed_future(&managed_future)?;
	// Read here, on the JS thread: no JS value enters the future.
	let params = FileBuilderParamsOptionalName::from_wire(params);
	let client = client.inner();
	spawn_promise(env, async_opts, async move {
		let params = params?;
		let managed_future = managed_future?;
		let progress = listener.clone().map(file_progress);
		let uploaded = managed_future
			.into_js_managed_commander_future(move || {
				super::upload_file(client, params, file_path, progress)
			})
			.await;
		if let Some(listener) = &listener {
			listener.flushed().await;
		}
		uploaded
	})
}

/// One call of a directory download's `JsDirDownloadCallback`, its arguments in their wire form.
pub(crate) enum DirDownloadCall {
	QueryProgress {
		known_bytes: BigInt,
		/// `undefined` while the total is unknown, as ubrn passes it, where napi writes `null`.
		total_bytes: Either<BigInt, Undefined>,
	},
	ScanProgress(BigInt, BigInt, BigInt),
	ScanErrors(Vec<<Arc<Error> as Wire>::Wire>),
	ScanComplete(BigInt, BigInt, BigInt),
	DownloadUpdate(
		Vec<<DirWithPath as Wire>::Wire>,
		Vec<<FileWithPath as Wire>::Wire>,
		BigInt,
	),
	DownloadErrors(Vec<<DownloadError as Wire>::Wire>),
}

/// `onQueryDownloadProgress`'s arguments: the bytes listed so far and the total, if known.
type QueryProgressArgs = FnArgs<(BigInt, Either<BigInt, Undefined>)>;
/// The three counts `onScanProgress` and `onScanComplete` take.
type Counts = FnArgs<(BigInt, BigInt, BigInt)>;
/// `onDownloadUpdate`'s arguments: the directories and files downloaded, and the bytes.
type DownloadUpdateArgs = FnArgs<(
	Vec<<DirWithPath as Wire>::Wire>,
	Vec<<FileWithPath as Wire>::Wire>,
	BigInt,
)>;

/// A `JsDirDownloadCallback`'s methods, bound to it.
pub(crate) struct DirDownloadMethods {
	on_query_download_progress: FunctionRef<QueryProgressArgs, ()>,
	on_scan_progress: FunctionRef<Counts, ()>,
	on_scan_errors: FunctionRef<Vec<<Arc<Error> as Wire>::Wire>, ()>,
	on_scan_complete: FunctionRef<Counts, ()>,
	on_download_update: FunctionRef<DownloadUpdateArgs, ()>,
	on_download_errors: FunctionRef<Vec<<DownloadError as Wire>::Wire>, ()>,
}

impl ListenerCall for DirDownloadCall {
	type Methods = DirDownloadMethods;

	fn bind(callback: &Object<'_>) -> napi::Result<Self::Methods> {
		Ok(DirDownloadMethods {
			on_query_download_progress: bound_method(callback, "onQueryDownloadProgress")?,
			on_scan_progress: bound_method(callback, "onScanProgress")?,
			on_scan_errors: bound_method(callback, "onScanErrors")?,
			on_scan_complete: bound_method(callback, "onScanComplete")?,
			on_download_update: bound_method(callback, "onDownloadUpdate")?,
			on_download_errors: bound_method(callback, "onDownloadErrors")?,
		})
	}

	fn call(self, env: &Env, methods: &Self::Methods) -> napi::Result<()> {
		// `call`, not `apply`, as in `FileProgress::call`.
		match self {
			Self::QueryProgress {
				known_bytes,
				total_bytes,
			} => methods
				.on_query_download_progress
				.borrow_back(env)?
				.call(FnArgs::from((known_bytes, total_bytes))),
			Self::ScanProgress(dirs, files, bytes) => methods
				.on_scan_progress
				.borrow_back(env)?
				.call(FnArgs::from((dirs, files, bytes))),
			Self::ScanErrors(errors) => methods.on_scan_errors.borrow_back(env)?.call(errors),
			Self::ScanComplete(dirs, files, bytes) => methods
				.on_scan_complete
				.borrow_back(env)?
				.call(FnArgs::from((dirs, files, bytes))),
			Self::DownloadUpdate(dirs, files, bytes) => methods
				.on_download_update
				.borrow_back(env)?
				.call(FnArgs::from((dirs, files, bytes))),
			Self::DownloadErrors(errors) => {
				methods.on_download_errors.borrow_back(env)?.call(errors)
			}
		}
	}
}

/// The SDK's directory download callback for a `JsDirDownloadCallback`. Skipped files count as
/// downloaded and resumed bytes go unreported, as on uniffi (the trait's defaults).
struct NapiDirDownloadCallback(Listener<DirDownloadCall>);

impl<Cat: CategoryFS> DirDownloadCallback<Cat> for NapiDirDownloadCallback
where
	Cat::File: Into<File>,
	Cat::Dir: Into<NonRootDir>,
	NonRootItemType<'static, Cat>: Into<NonRootItem>,
{
	fn on_query_download_progress(&self, known_bytes: u64, total_bytes: Option<u64>) {
		self.0.send(DirDownloadCall::QueryProgress {
			known_bytes: known_bytes.into_wire(),
			total_bytes: OptionReturn(total_bytes).into_wire(),
		});
	}

	fn on_scan_progress(&self, known_dirs: u64, known_files: u64, known_bytes: u64) {
		self.0.send(DirDownloadCall::ScanProgress(
			known_dirs.into_wire(),
			known_files.into_wire(),
			known_bytes.into_wire(),
		));
	}

	fn on_scan_errors(&self, errors: Vec<Error>) {
		self.0.send(DirDownloadCall::ScanErrors(
			errors
				.into_iter()
				.map(|e| Arc::new(e).into_wire())
				.collect(),
		));
	}

	fn on_scan_complete(&self, total_dirs: u64, total_files: u64, total_bytes: u64) {
		self.0.send(DirDownloadCall::ScanComplete(
			total_dirs.into_wire(),
			total_files.into_wire(),
			total_bytes.into_wire(),
		));
	}

	fn on_download_update(
		&self,
		downloaded_dirs: Vec<(<Cat as Category>::Dir, String)>,
		downloaded_files: Vec<(<Cat as Category>::File, String)>,
		downloaded_bytes: u64,
	) {
		self.0.send(DirDownloadCall::DownloadUpdate(
			downloaded_dirs
				.into_iter()
				.map(|(dir, path)| {
					DirWithPath {
						path,
						dir: dir.into(),
					}
					.into_wire()
				})
				.collect(),
			downloaded_files
				.into_iter()
				.map(|(file, path)| {
					FileWithPath {
						path,
						file: file.into(),
					}
					.into_wire()
				})
				.collect(),
			downloaded_bytes.into_wire(),
		));
	}

	fn on_download_errors(&self, errors: Vec<(Error, String, NonRootItemType<'static, Cat>)>) {
		self.0.send(DirDownloadCall::DownloadErrors(
			errors
				.into_iter()
				.map(|(error, path, item)| {
					DownloadError {
						error: Arc::new(error),
						path,
						item: item.into(),
					}
					.into_wire()
				})
				.collect(),
		));
	}
}

/// `downloadDirRecursively`'s body, as [`download_file_to_path`]'s: a callback without one of the
/// six methods throws napi's own error synchronously; a target or a managed future the SDK cannot
/// read rejects.
// napi-derive registers the method calling this only outside `cfg(test)`.
#[cfg_attr(test, allow(dead_code))]
fn download_dir_recursively<'env>(
	client: &auth::JsClient,
	env: &'env Env,
	dir_path: String,
	callback: Object<'env>,
	target: <AnyDirWithContext as Wire>::Wire,
	managed_future: Object<'env>,
	async_opts: Option<Object<'env>>,
) -> napi::Result<PromiseRaw<'env, ()>> {
	let listener = Listener::<DirDownloadCall>::new(env, callback)?;
	let managed_future = read_managed_future(&managed_future)?;
	// Read here, on the JS thread: no JS value enters the future.
	let target = AnyDirWithContext::from_wire(target);
	let client = client.inner();
	spawn_promise(env, async_opts, async move {
		let target = target?;
		let managed_future = managed_future?;
		let callback = NapiDirDownloadCallback(listener.clone());
		let downloaded = managed_future
			.into_js_managed_commander_future(move || async move {
				super::download_dir(client, dir_path, target, &callback).await
			})
			.await;
		listener.flushed().await;
		downloaded
	})
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{fs::categories::Normal, js::napi::listener_test_support::recording};

	#[test]
	fn a_dir_download_call_keeps_its_counts_on_the_wire() {
		let (listener, sent) = recording();
		let callback = NapiDirDownloadCallback(listener);

		DirDownloadCallback::<Normal>::on_scan_progress(&callback, 1, 2, u64::MAX);
		DirDownloadCallback::<Normal>::on_scan_complete(&callback, 3, 0, 5);
		DirDownloadCallback::<Normal>::on_download_update(&callback, Vec::new(), Vec::new(), 7);

		let counts =
			|a: BigInt, b: BigInt, c: BigInt| [a, b, c].map(|count| u64::from_wire(count).unwrap());
		let sent = std::mem::take(&mut *sent.lock().unwrap());
		let [
			DirDownloadCall::ScanProgress(a, b, c),
			DirDownloadCall::ScanComplete(d, e, f),
			DirDownloadCall::DownloadUpdate(dirs, files, bytes),
		] = <[DirDownloadCall; 3]>::try_from(sent).ok().unwrap()
		else {
			panic!("the calls were not sent as made");
		};
		assert_eq!(counts(a, b, c), [1, 2, u64::MAX]);
		assert_eq!(counts(d, e, f), [3, 0, 5]);
		assert!(dirs.is_empty() && files.is_empty());
		assert_eq!(u64::from_wire(bytes).unwrap(), 7);
	}

	#[test]
	fn an_unknown_total_is_written_as_undefined() {
		let (listener, sent) = recording();
		let callback = NapiDirDownloadCallback(listener);

		DirDownloadCallback::<Normal>::on_query_download_progress(&callback, 4, None);
		DirDownloadCallback::<Normal>::on_query_download_progress(&callback, 6, Some(9));

		let sent = std::mem::take(&mut *sent.lock().unwrap());
		let [
			DirDownloadCall::QueryProgress {
				known_bytes: unknown_known,
				total_bytes: Either::B(()),
			},
			DirDownloadCall::QueryProgress {
				known_bytes,
				total_bytes: Either::A(total_bytes),
			},
		] = <[DirDownloadCall; 2]>::try_from(sent).ok().unwrap()
		else {
			panic!("an unknown total was not undefined, or a known one not a bigint");
		};
		assert_eq!(u64::from_wire(unknown_known).unwrap(), 4);
		assert_eq!(u64::from_wire(known_bytes).unwrap(), 6);
		assert_eq!(u64::from_wire(total_bytes).unwrap(), 9);
	}
}

// napi-derive's generated FFI glue (the raw N-API calls of each item's callback) lands in this
// module, which holds nothing but the napi item forwarding to plain Rust.
#[allow(unsafe_code)]
mod class {
	use napi::{
		Env,
		bindgen_prelude::{Object, PromiseRaw},
	};
	use napi_derive::napi;

	use super::{download_dir_recursively, download_file_to_path, upload_file};
	use crate::js::{
		AnyDirWithContext, AnyFile, File, FileBuilderParamsOptionalName,
		napi::{JsClient, Wire},
	};

	#[napi]
	impl JsClient {
		/// Download `file` to `filePath`, through a temp file beside it renamed into place once
		/// every byte is written. `callback.onUpdate` gets the bytes downloaded since its last
		/// call (a delta, not a running total).
		#[napi(
			js_name = "downloadFileToPath",
			ts_args_type = "file: AnyFile, filePath: string, callback: JsFileDownloadCallback | undefined, managedFuture: ManagedFuture, asyncOpts_?: { signal: AbortSignal }",
			ts_return_type = "Promise<void>"
		)]
		pub fn __napi_download_file_to_path<'env>(
			&self,
			env: &'env Env,
			file: <AnyFile as Wire>::Wire,
			file_path: String,
			callback: Option<Object<'env>>,
			managed_future: Object<'env>,
			async_opts: Option<Object<'env>>,
		) -> napi::Result<PromiseRaw<'env, ()>> {
			download_file_to_path(
				&self.0,
				env,
				file,
				file_path,
				callback,
				managed_future,
				async_opts,
			)
		}

		/// Upload the local file at `filePath` as `params` describe it, named as the local file
		/// when `params` gives no name. `callback.onUpdate` gets the bytes uploaded since its last call (a delta,
		/// not a running total).
		#[napi(
			js_name = "uploadFile",
			ts_args_type = "params: FileBuilderParamsOptionalName, filePath: string, callback: JsFileUploadCallback | undefined, managedFuture: ManagedFuture, asyncOpts_?: { signal: AbortSignal }",
			ts_return_type = "Promise<File>"
		)]
		pub fn __napi_upload_file<'env>(
			&self,
			env: &'env Env,
			params: <FileBuilderParamsOptionalName as Wire>::Wire,
			file_path: String,
			callback: Option<Object<'env>>,
			managed_future: Object<'env>,
			async_opts: Option<Object<'env>>,
		) -> napi::Result<PromiseRaw<'env, <File as Wire>::Wire>> {
			upload_file(
				&self.0,
				env,
				params,
				file_path,
				callback,
				managed_future,
				async_opts,
			)
		}

		/// Download `target` and everything below it into the local directory `dirPath`, each
		/// file through a temp file beside it renamed into place once every byte is written.
		/// `callback` gets the listing's, the scan's and the download's progress: counts and
		/// batches of what was downloaded or failed since its last call.
		#[napi(
			js_name = "downloadDirRecursively",
			ts_args_type = "dirPath: string, callback: JsDirDownloadCallback, target: AnyDirWithContext, managedFuture: ManagedFuture, asyncOpts_?: { signal: AbortSignal }",
			ts_return_type = "Promise<void>"
		)]
		pub fn __napi_download_dir_recursively<'env>(
			&self,
			env: &'env Env,
			dir_path: String,
			callback: Object<'env>,
			target: <AnyDirWithContext as Wire>::Wire,
			managed_future: Object<'env>,
			async_opts: Option<Object<'env>>,
		) -> napi::Result<PromiseRaw<'env, ()>> {
			download_dir_recursively(
				&self.0,
				env,
				dir_path,
				callback,
				target,
				managed_future,
				async_opts,
			)
		}
	}
}

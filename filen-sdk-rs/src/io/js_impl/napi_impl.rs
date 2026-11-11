//! The path transfers' napi exports, which take callback objects the macros cannot read:
//! `JsClient.downloadFileToPath` and `JsClient.uploadFile`.
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
	bindgen_prelude::{BigInt, Function, FunctionRef, JsObjectValue, Object, PromiseRaw},
};

use crate::{
	auth,
	js::{
		AnyFile, File, FileBuilderParamsOptionalName,
		napi::{
			FromWire, IntoWire, Listener, ListenerCall, Wire, read_managed_future, spawn_promise,
		},
	},
	util::MaybeSendCallback,
};

/// The bytes a download or an upload moved since its last call, for its `onUpdate`.
pub(crate) struct FileProgress(BigInt);

impl ListenerCall for FileProgress {
	type Methods = FunctionRef<BigInt, ()>;

	fn bind(callback: &Object<'_>) -> napi::Result<Self::Methods> {
		let on_update: Function<'_, BigInt, ()> = callback.get_named_property("onUpdate")?;
		// ubrn calls it as a method of the callback, so `this` is bound to it.
		on_update.bind(callback)?.create_ref()
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

// napi-derive's generated FFI glue (the raw N-API calls of each item's callback) lands in this
// module, which holds nothing but the napi item forwarding to plain Rust.
#[allow(unsafe_code)]
mod class {
	use napi::{
		Env,
		bindgen_prelude::{Object, PromiseRaw},
	};
	use napi_derive::napi;

	use super::{download_file_to_path, upload_file};
	use crate::js::{
		AnyFile, File, FileBuilderParamsOptionalName,
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
	}
}

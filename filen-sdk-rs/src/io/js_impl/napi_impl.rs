//! The path transfers' napi exports, which take callback objects the macros cannot read:
//! `JsClient.downloadFileToPath`.
//!
//! A call's callback object becomes one [`Listener`], read and bound on the JS thread when the call
//! is made; the SDK's progress reaches it from the commander runtime without waiting on JS. Every
//! call it makes before the SDK returns reaches JS before the call's promise settles (the call
//! awaits [`Listener::flushed`]), except after an abort through `asyncOpts_`, which drops the call
//! at once: its queued calls may then still arrive after it rejected.

use std::sync::Arc;

use napi::{
	Env,
	bindgen_prelude::{BigInt, Function, FunctionRef, JsObjectValue, Object, PromiseRaw},
};

use crate::{
	auth,
	js::{
		AnyFile,
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

// napi-derive's generated FFI glue (the raw N-API calls of each item's callback) lands in this
// module, which holds nothing but the napi item forwarding to plain Rust.
#[allow(unsafe_code)]
mod class {
	use napi::{
		Env,
		bindgen_prelude::{Object, PromiseRaw},
	};
	use napi_derive::napi;

	use super::download_file_to_path;
	use crate::js::{
		AnyFile,
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
	}
}

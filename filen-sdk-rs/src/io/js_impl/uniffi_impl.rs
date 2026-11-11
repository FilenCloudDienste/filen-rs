use std::{path::PathBuf, sync::Arc};

use crate::{
	Error,
	auth::{JsClient, js_impls::UnauthJsClient},
	fs::categories::{Category, NonRootItemType, fs::CategoryFS},
	io::{DirDownloadCallback, RemoteDirectory, RemoteFile, client_impl::IoSharedClientExt},
	js::{
		AnyDirWithContext, AnyFile, Dir, DirWithPath, File, FileBuilderParamsOptionalName,
		FileWithPath, LinkedFile, ManagedFuture, NonRootDir, NonRootItem,
	},
	util::MaybeSendCallback,
};

use super::{DownloadError, UploadError};

#[uniffi::export(with_foreign)]
pub trait JsDirUploadCallback: Send + Sync {
	fn on_scan_progress(&self, known_dirs: u64, known_files: u64, known_bytes: u64);
	fn on_scan_errors(&self, errors: Vec<Arc<Error>>);
	fn on_scan_complete(&self, total_dirs: u64, total_files: u64, total_bytes: u64);
	fn on_upload_update(
		&self,
		uploaded_dirs: Vec<Dir>,
		uploaded_files: Vec<File>,
		uploaded_bytes: u64,
	);
	fn on_upload_errors(&self, errors: Vec<UploadError>);
}

impl crate::io::DirUploadCallback for Arc<dyn JsDirUploadCallback> {
	fn on_scan_progress(&self, known_dirs: u64, known_files: u64, known_bytes: u64) {
		let this = self.clone();
		tokio::task::spawn_blocking(move || {
			JsDirUploadCallback::on_scan_progress(
				this.as_ref(),
				known_dirs,
				known_files,
				known_bytes,
			);
		});
	}

	fn on_scan_errors(&self, errors: Vec<Error>) {
		let this = self.clone();
		tokio::task::spawn_blocking(move || {
			JsDirUploadCallback::on_scan_errors(
				this.as_ref(),
				errors.into_iter().map(Arc::new).collect(),
			);
		});
	}

	fn on_scan_complete(&self, total_dirs: u64, total_files: u64, total_bytes: u64) {
		let this = self.clone();
		tokio::task::spawn_blocking(move || {
			JsDirUploadCallback::on_scan_complete(
				this.as_ref(),
				total_dirs,
				total_files,
				total_bytes,
			);
		});
	}

	fn on_upload_update(
		&self,
		uploaded_dirs: Vec<RemoteDirectory>,
		uploaded_files: Vec<RemoteFile>,
		uploaded_bytes: u64,
	) {
		let this = self.clone();
		tokio::task::spawn_blocking(move || {
			JsDirUploadCallback::on_upload_update(
				this.as_ref(),
				uploaded_dirs.into_iter().map(|d| d.into()).collect(),
				uploaded_files.into_iter().map(|f| f.into()).collect(),
				uploaded_bytes,
			);
		});
	}

	fn on_upload_errors(&self, errors: Vec<(PathBuf, Error)>) {
		let this = self.clone();
		tokio::task::spawn_blocking(move || {
			JsDirUploadCallback::on_upload_errors(
				this.as_ref(),
				errors
					.into_iter()
					.map(|(path, e)| UploadError {
						path: path.into_string_lossy(),
						error: Arc::new(e),
					})
					.collect(),
			);
		});
	}
}

#[uniffi::export(with_foreign)]
pub trait JsDirDownloadCallback: Send + Sync {
	/// Called periodically while /dir/download is listing the directory contents
	fn on_query_download_progress(&self, known_bytes: u64, total_bytes: Option<u64>);
	/// Called during tree building
	fn on_scan_progress(&self, known_dirs: u64, known_files: u64, known_bytes: u64);
	/// Called when errors occur during tree building
	fn on_scan_errors(&self, errors: Vec<Arc<Error>>);
	/// Called when tree building is complete
	fn on_scan_complete(&self, total_dirs: u64, total_files: u64, total_bytes: u64);
	/// Called periodically during the download process
	fn on_download_update(
		&self,
		downloaded_dirs: Vec<DirWithPath>,
		downloaded_files: Vec<FileWithPath>,
		downloaded_bytes: u64,
	);
	/// Called when errors occur during the download process
	fn on_download_errors(&self, errors: Vec<DownloadError>);
}

// because uniffi
struct DownloadCallbackWrapper(Arc<dyn JsDirDownloadCallback>);

impl<Cat: CategoryFS> DirDownloadCallback<Cat> for DownloadCallbackWrapper
where
	Cat::File: Into<File>,
	Cat::Dir: Into<NonRootDir>,
	NonRootItemType<'static, Cat>: Into<NonRootItem>,
{
	fn on_query_download_progress(&self, known_bytes: u64, total_bytes: Option<u64>) {
		let this = self.0.clone();
		tokio::task::spawn_blocking(move || {
			JsDirDownloadCallback::on_query_download_progress(
				this.as_ref(),
				known_bytes,
				total_bytes,
			);
		});
	}

	fn on_scan_progress(&self, known_dirs: u64, known_files: u64, known_bytes: u64) {
		let this = self.0.clone();
		tokio::task::spawn_blocking(move || {
			JsDirDownloadCallback::on_scan_progress(
				this.as_ref(),
				known_dirs,
				known_files,
				known_bytes,
			);
		});
	}

	fn on_scan_errors(&self, errors: Vec<Error>) {
		let this = self.0.clone();
		tokio::task::spawn_blocking(move || {
			JsDirDownloadCallback::on_scan_errors(
				this.as_ref(),
				errors.into_iter().map(Arc::new).collect(),
			);
		});
	}

	fn on_scan_complete(&self, total_dirs: u64, total_files: u64, total_bytes: u64) {
		let this = self.0.clone();
		tokio::task::spawn_blocking(move || {
			JsDirDownloadCallback::on_scan_complete(
				this.as_ref(),
				total_dirs,
				total_files,
				total_bytes,
			);
		});
	}

	fn on_download_update(
		&self,
		downloaded_dirs: Vec<(<Cat as Category>::Dir, String)>,
		downloaded_files: Vec<(<Cat as Category>::File, String)>,
		downloaded_bytes: u64,
	) {
		let this = self.0.clone();
		tokio::task::spawn_blocking(move || {
			JsDirDownloadCallback::on_download_update(
				this.as_ref(),
				downloaded_dirs
					.into_iter()
					.map(|(dir, path)| DirWithPath {
						path,
						dir: dir.into(),
					})
					.collect(),
				downloaded_files
					.into_iter()
					.map(|(file, path)| FileWithPath {
						path,
						file: file.into(),
					})
					.collect(),
				downloaded_bytes,
			);
		});
	}

	fn on_download_errors(&self, errors: Vec<(Error, String, NonRootItemType<'static, Cat>)>) {
		let this = self.0.clone();
		tokio::task::spawn_blocking(move || {
			JsDirDownloadCallback::on_download_errors(
				this.as_ref(),
				errors
					.into_iter()
					.map(|(e, path, object)| DownloadError {
						error: Arc::new(e),
						path,
						item: object.into(),
					})
					.collect(),
			);
		});
	}
}

#[uniffi::export(with_foreign)]
pub trait JsFileDownloadCallback: Send + Sync {
	/// `downloaded_bytes` is a **delta** — the number of plaintext bytes downloaded *since the
	/// previous call*, not a running total. Accumulate the values to obtain overall progress
	/// (`sum == file size` once the transfer completes).
	///
	/// This differs from the wasm/service-worker stream download callbacks, which report the
	/// **cumulative** total transferred so far. Treating this delta as a cumulative total leaves
	/// progress stuck near zero.
	fn on_update(&self, downloaded_bytes: u64);
}

#[uniffi::export(with_foreign)]
pub trait JsFileUploadCallback: Send + Sync {
	/// `uploaded_bytes` is a **delta** — the number of plaintext bytes uploaded *since the
	/// previous call*, not a running total. Accumulate the values to obtain overall progress
	/// (`sum == file size` once the transfer completes).
	///
	/// This differs from the wasm/service-worker stream upload callbacks, which report the
	/// **cumulative** total transferred so far. Treating this delta as a cumulative total leaves
	/// progress stuck near zero.
	fn on_update(&self, uploaded_bytes: u64);
}

/// The progress callback of a file download, calling `callback` off the runtime's threads.
fn download_progress(callback: Arc<dyn JsFileDownloadCallback>) -> MaybeSendCallback<'static, u64> {
	Arc::new(move |downloaded_bytes| {
		let inner_cb = Arc::clone(&callback);
		tokio::task::spawn_blocking(move || {
			JsFileDownloadCallback::on_update(inner_cb.as_ref(), downloaded_bytes);
		});
	})
}

#[uniffi::export]
impl JsClient {
	pub async fn upload_dir_recursively(
		&self,
		dir_path: String,
		callback: Arc<dyn JsDirUploadCallback>,
		target: Dir,
		managed_future: ManagedFuture,
	) -> Result<(), Error> {
		let this = self.inner();
		managed_future
			.into_js_managed_commander_future(move || async move {
				super::upload_dir(this, dir_path, target, &callback).await
			})
			.await
	}

	pub async fn download_dir_recursively(
		&self,
		dir_path: String,
		callback: Arc<dyn JsDirDownloadCallback>,
		target: AnyDirWithContext,
		managed_future: ManagedFuture,
	) -> Result<(), Error> {
		let this = self.inner();
		managed_future
			.into_js_managed_commander_future(move || async move {
				super::download_dir(this, dir_path, target, &DownloadCallbackWrapper(callback))
					.await
			})
			.await
	}

	pub async fn upload_file(
		&self,
		params: FileBuilderParamsOptionalName,
		file_path: String,
		callback: Option<Arc<dyn JsFileUploadCallback>>,
		managed_future: ManagedFuture,
	) -> Result<File, Error> {
		let this = self.inner();
		managed_future
			.into_js_managed_commander_future(move || async move {
				let callback = callback.map(|cb| {
					Arc::new(move |uploaded_bytes| {
						let inner_cb = Arc::clone(&cb);
						tokio::task::spawn_blocking(move || {
							JsFileUploadCallback::on_update(inner_cb.as_ref(), uploaded_bytes);
						});
					}) as MaybeSendCallback<'static, u64>
				});
				super::upload_file(this, params, file_path, callback).await
			})
			.await
	}

	pub async fn download_file_to_path(
		&self,
		file: AnyFile,
		file_path: String,
		callback: Option<Arc<dyn JsFileDownloadCallback>>,
		managed_future: ManagedFuture,
	) -> Result<(), Error> {
		let this = self.inner();
		managed_future
			.into_js_managed_commander_future(move || async move {
				super::download_file_to_path(this, file, file_path, callback.map(download_progress))
					.await
			})
			.await
	}
}

trait PathToStringExt {
	fn into_string_lossy(self) -> String;
}

// replace with string_from_utf8_lossy_owned
// https://github.com/rust-lang/rust/issues/129436 when stabilized
impl PathToStringExt for PathBuf {
	fn into_string_lossy(self) -> String {
		let os_string = self.into_os_string();
		match os_string.into_string() {
			Ok(s) => s,
			Err(s) => s.to_string_lossy().into_owned(),
		}
	}
}

#[uniffi::export]
impl UnauthJsClient {
	pub async fn download_file_to_path(
		&self,
		file: LinkedFile,
		file_path: String,
		callback: Option<Arc<dyn JsFileDownloadCallback>>,
		managed_future: ManagedFuture,
	) -> Result<(), Error> {
		let this = self.inner();
		managed_future
			.into_js_managed_commander_future(move || async move {
				let file: crate::fs::file::LinkedFile = file.try_into()?;
				this.download_file_to_path(
					&file,
					file_path.as_ref(),
					callback.map(download_progress),
				)
				.await
			})
			.await
	}
}

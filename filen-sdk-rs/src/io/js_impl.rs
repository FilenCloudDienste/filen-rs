use std::{path::PathBuf, sync::Arc};

use crate::{
	Error,
	auth::JsClient,
	error::FilenSdkError,
	fs::dir::UnsharedDirectoryType,
	js::{AnyDirEnum, Dir, DirEnum, DirWithPath, File, FileWithPath, NonRootItemTagged},
	util::MaybeSendCallback,
};

#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct UploadError {
	pub path: String,
	pub error: Arc<FilenSdkError>,
}

#[cfg_attr(feature = "uniffi", uniffi::export(with_foreign))]
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
		uploaded_dirs: Vec<super::RemoteDirectory>,
		uploaded_files: Vec<super::RemoteFile>,
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

	fn on_upload_errors(&self, errors: Vec<(std::path::PathBuf, Error)>) {
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

#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct DownloadError {
	pub error: Arc<Error>,
	pub path: String,
	pub item: NonRootItemTagged,
}

#[cfg_attr(feature = "uniffi", uniffi::export(with_foreign))]
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

impl crate::io::DirDownloadCallback for Arc<dyn JsDirDownloadCallback> {
	fn on_query_download_progress(&self, known_bytes: u64, total_bytes: Option<u64>) {
		let this = self.clone();
		tokio::task::spawn_blocking(move || {
			JsDirDownloadCallback::on_query_download_progress(
				this.as_ref(),
				known_bytes,
				total_bytes,
			);
		});
	}

	fn on_scan_progress(&self, known_dirs: u64, known_files: u64, known_bytes: u64) {
		let this = self.clone();
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
		let this = self.clone();
		tokio::task::spawn_blocking(move || {
			JsDirDownloadCallback::on_scan_errors(
				this.as_ref(),
				errors.into_iter().map(Arc::new).collect(),
			);
		});
	}

	fn on_scan_complete(&self, total_dirs: u64, total_files: u64, total_bytes: u64) {
		let this = self.clone();
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
		downloaded_dirs: Vec<(super::RemoteDirectory, String)>,
		downloaded_files: Vec<(super::RemoteFile, String)>,
		downloaded_bytes: u64,
	) {
		let this = self.clone();
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

	fn on_download_errors(
		&self,
		errors: Vec<(Error, String, crate::fs::NonRootFSObject<'static>)>,
	) {
		let this = self.clone();
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

#[cfg_attr(feature = "uniffi", uniffi::export(with_foreign))]
pub trait JsFileDownloadCallback: Send + Sync {
	fn on_update(&self, downloaded_bytes: u64);
}

#[cfg_attr(feature = "uniffi", uniffi::export(with_foreign))]
pub trait JsFileUploadCallback: Send + Sync {
	fn on_update(&self, uploaded_bytes: u64);
}

#[cfg_attr(feature = "uniffi", uniffi::export)]
impl JsClient {
	pub async fn upload_dir_recursively(
		&self,
		dir_path: String,
		callback: Arc<dyn JsDirUploadCallback>,
		target: Dir,
		managed_future: crate::js::ManagedFuture,
	) -> Result<(), Error> {
		let this = self.inner();
		managed_future
			.into_js_managed_commander_future(move || async move {
				let target = target.into();
				let dir_path = PathBuf::from(dir_path);
				this.upload_dir_recursively(dir_path, &callback, &target)
					.await
			})
			.await
	}

	pub async fn download_dir_recursively(
		&self,
		dir_path: String,
		callback: Arc<dyn JsDirDownloadCallback>,
		target: AnyDirEnum,
		managed_future: crate::js::ManagedFuture,
	) -> Result<(), Error> {
		let this = self.inner();
		managed_future
			.into_js_managed_commander_future(move || async move {
				let target = target.into();
				this.download_dir_recursively(dir_path, &callback, target)
					.await
			})
			.await
	}

	pub async fn upload_file(
		&self,
		parent_dir: DirEnum,
		file_path: String,
		callback: Option<Arc<dyn JsFileUploadCallback>>,
		managed_future: crate::js::ManagedFuture,
	) -> Result<File, Error> {
		let this = self.inner();
		managed_future
			.into_js_managed_commander_future(move || async move {
				let callback = callback.as_ref().map(|cb| {
					Arc::new(|downloaded_bytes| {
						let inner_cb = Arc::clone(cb);
						tokio::task::spawn_blocking(move || {
							JsFileUploadCallback::on_update(inner_cb.as_ref(), downloaded_bytes);
						});
					}) as MaybeSendCallback<u64>
				});

				let parent_dir = UnsharedDirectoryType::from(parent_dir);
				let file_path = PathBuf::from(file_path);
				this.upload_file_from_path(&parent_dir, file_path, callback)
					.await
					.map(|(file, _)| File::from(file))
			})
			.await
	}

	pub async fn download_file_to_path(
		&self,
		file: File,
		file_path: String,
		callback: Option<Arc<dyn JsFileDownloadCallback>>,
		managed_future: crate::js::ManagedFuture,
	) -> Result<(), Error> {
		let this = self.inner();
		managed_future
			.into_js_managed_commander_future(move || async move {
				let callback = callback.as_ref().map(|cb| {
					Arc::new(|downloaded_bytes| {
						let inner_cb = Arc::clone(cb);
						tokio::task::spawn_blocking(move || {
							JsFileDownloadCallback::on_update(inner_cb.as_ref(), downloaded_bytes);
						});
					}) as MaybeSendCallback<u64>
				});

				let file: crate::io::RemoteFile = file.try_into()?;
				let target_path = PathBuf::from(file_path);
				super::client_impl::IoSharedClientExt::download_file_to_path(
					this.as_ref(),
					&file,
					target_path,
					callback,
				)
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

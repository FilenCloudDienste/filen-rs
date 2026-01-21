use std::{path::PathBuf, sync::Arc};

use crate::{
	Error,
	auth::JsClient,
	error::FilenSDKError,
	io::{DirUploadCallback, dir_download::DirDownloadCallback},
	js::{Dir, DirEnum, File, NonRootItemTagged},
	runtime::do_on_commander,
};

#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct UploadError {
	pub path: String,
	pub error: Arc<FilenSDKError>,
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

impl DirUploadCallback for dyn JsDirUploadCallback {
	fn on_scan_progress(&self, known_dirs: u64, known_files: u64, known_bytes: u64) {
		JsDirUploadCallback::on_scan_progress(self, known_dirs, known_files, known_bytes);
	}

	fn on_scan_errors(&self, errors: Vec<Error>) {
		JsDirUploadCallback::on_scan_errors(self, errors.into_iter().map(Arc::new).collect());
	}

	fn on_scan_complete(&self, total_dirs: u64, total_files: u64, total_bytes: u64) {
		JsDirUploadCallback::on_scan_complete(self, total_dirs, total_files, total_bytes);
	}

	fn on_upload_update(
		&self,
		uploaded_dirs: Vec<super::RemoteDirectory>,
		uploaded_files: Vec<super::RemoteFile>,
		uploaded_bytes: u64,
	) {
		JsDirUploadCallback::on_upload_update(
			self,
			uploaded_dirs.into_iter().map(|d| d.into()).collect(),
			uploaded_files.into_iter().map(|f| f.into()).collect(),
			uploaded_bytes,
		);
	}

	fn on_upload_errors(&self, errors: Vec<(std::path::PathBuf, Error)>) {
		JsDirUploadCallback::on_upload_errors(
			self,
			errors
				.into_iter()
				.map(|(path, e)| UploadError {
					path: path.into_string_lossy(),
					error: Arc::new(e),
				})
				.collect(),
		);
	}
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct DirWithPath {
	pub path: String,
	pub dir: Dir,
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct FileWithPath {
	pub path: String,
	pub file: File,
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

impl DirDownloadCallback for dyn JsDirDownloadCallback {
	fn on_query_download_progress(&self, known_bytes: u64, total_bytes: Option<u64>) {
		JsDirDownloadCallback::on_query_download_progress(self, known_bytes, total_bytes);
	}

	fn on_scan_progress(&self, known_dirs: u64, known_files: u64, known_bytes: u64) {
		JsDirDownloadCallback::on_scan_progress(self, known_dirs, known_files, known_bytes);
	}

	fn on_scan_errors(&self, errors: Vec<Error>) {
		JsDirDownloadCallback::on_scan_errors(self, errors.into_iter().map(Arc::new).collect());
	}

	fn on_scan_complete(&self, total_dirs: u64, total_files: u64, total_bytes: u64) {
		JsDirDownloadCallback::on_scan_complete(self, total_dirs, total_files, total_bytes);
	}

	fn on_download_update(
		&self,
		downloaded_dirs: Vec<(super::RemoteDirectory, PathBuf)>,
		downloaded_files: Vec<(super::RemoteFile, PathBuf)>,
		downloaded_bytes: u64,
	) {
		JsDirDownloadCallback::on_download_update(
			self,
			downloaded_dirs
				.into_iter()
				.map(|(dir, path)| DirWithPath {
					path: path.into_string_lossy(),
					dir: dir.into(),
				})
				.collect(),
			downloaded_files
				.into_iter()
				.map(|(file, path)| FileWithPath {
					path: path.into_string_lossy(),
					file: file.into(),
				})
				.collect(),
			downloaded_bytes,
		);
	}

	fn on_download_errors(
		&self,
		errors: Vec<(Error, PathBuf, crate::fs::NonRootFSObject<'static>)>,
	) {
		JsDirDownloadCallback::on_download_errors(
			self,
			errors
				.into_iter()
				.map(|(e, path, object)| DownloadError {
					error: Arc::new(e),
					path: path.into_string_lossy(),
					item: object.into(),
				})
				.collect(),
		);
	}
}

#[cfg_attr(feature = "uniffi", uniffi::export)]
impl JsClient {
	pub async fn upload_dir_recursively(
		&self,
		dir_path: String,
		callback: Arc<dyn JsDirUploadCallback>,
		target: Dir,
	) -> Result<(), Error> {
		let this = self.inner();
		do_on_commander(move || async move {
			let target = target.into();
			let dir_path = PathBuf::from(dir_path);
			this.upload_dir_recursively(dir_path, callback, &target)
				.await
		})
		.await
	}

	pub async fn download_dir_recursively(
		&self,
		dir_path: String,
		callback: Arc<dyn JsDirDownloadCallback>,
		target: DirEnum,
	) -> Result<(), Error> {
		let this = self.inner();
		do_on_commander(move || async move {
			let target = target.into();
			let dir_path = PathBuf::from(dir_path);

			this.download_dir_recursively(dir_path, callback, target)
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

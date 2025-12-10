use std::{
	borrow::Cow,
	io::Read,
	path::{self, Path, PathBuf},
	sync::{Arc, atomic::AtomicU64},
};

use md5::Digest;
use tokio_util::compat::TokioAsyncWriteCompatExt;

use crate::{
	Error, ErrorKind,
	auth::Client,
	consts::CALLBACK_INTERVAL,
	error::ErrorExt,
	fs::{
		NonRootFSObject,
		dir::RemoteDirectory,
		file::{RemoteFile, traits::HasRemoteFileInfo},
	},
	io::{FilenMetaExt, HasFileInfo},
};

use super::fs_tree::Entry;

impl Client {
	pub(crate) async fn recursive_download_dir(
		self: Arc<Self>,
		callback: Arc<dyn DirDownloadCallback>,
		path: PathBuf,
		target_folder: &RemoteDirectory,
	) -> Result<(), Error> {
		tokio::fs::create_dir_all(&path).await.map_err(|e| {
			Error::custom(ErrorKind::Conversion, "Failed to create target directory")
		})?;

		todo!()
	}

	pub(crate) async fn download_fs_tree_from_target_into_path(
		self: Arc<Self>,
		callback: Arc<dyn DirDownloadCallback>,
		path: PathBuf,
		tree: super::fs_tree::FSTree<RemoteDirectory, RemoteFile>,
		target_folder: &RemoteDirectory,
	) -> Result<(), Error> {
		let (file_download_result_sender, mut file_download_result_receiver) =
			tokio::sync::mpsc::channel::<FileDownloadResult>(16);

		let (file_download_request_sender, mut file_download_request_receiver) =
			tokio::sync::mpsc::channel::<(RemoteFile, PathBuf)>(self.max_parallel_requests);

		let (entry_complete_sender, mut entry_complete_receiver) =
			tokio::sync::mpsc::channel::<(Result<NonRootFSObject, (Error, PathBuf)>)>(16);

		let update_interval = tokio::time::interval(CALLBACK_INTERVAL);

		todo!()
	}

	async fn spawn_folder_maker_task(
		self: Arc<Self>,
		tree: super::fs_tree::FSTree<RemoteDirectory, RemoteFile>,
		entry_complete_sender: tokio::sync::mpsc::Sender<(
			Result<(), Error>,
			PathBuf,
			NonRootFSObject<'static>,
		)>,
		target_folder: &RemoteDirectory,
		downloaded_bytes: Arc<AtomicU64>,
		root_path: PathBuf,
	) -> Result<(tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>), Error> {
		let (file_download_request_sender, mut file_download_request_receiver) =
			tokio::sync::mpsc::channel::<(RemoteFile, PathBuf)>(self.max_parallel_requests);

		if let Err(e) = tokio::fs::create_dir_all(&root_path).await
			&& e.kind() != std::io::ErrorKind::AlreadyExists
		{
			log::error!("Failed to create root download dir {:?}: {}", root_path, e);
			return Err(e.with_context(format!(
				"failed to create root download dir at path {root_path:?}"
			)));
		}
		let dir_handle = {
			let entry_complete_sender = entry_complete_sender.clone();
			tokio::task::spawn_blocking(move || {
				for (entry, path) in tree.dfs_iter_with_path(&root_path) {
					match entry {
						Entry::Dir(dir_entry) => {
							let dir = dir_entry.extra_data().clone();
							if let Err(e) = std::fs::create_dir(&path)
								&& e.kind() != std::io::ErrorKind::AlreadyExists
							{
								entry_complete_sender
									.blocking_send((
										Err(e.with_context(
											"couldn't create directory during dir download",
										)),
										path,
										NonRootFSObject::Dir(Cow::Owned(dir)),
									))
									.unwrap();
								continue;
							}
							if let Err(e) = dir.set_dir_times(&path) {
								log::error!(
									"Failed to set dir times for downloaded dir {:?}: {}",
									path,
									e
								);
								entry_complete_sender
									.blocking_send((
										Err(e.with_context(
											"couldn't set directory times during dir download",
										)),
										path,
										NonRootFSObject::Dir(Cow::Owned(dir)),
									))
									.unwrap();
								continue;
							}
							entry_complete_sender
								.blocking_send((
									Ok(()),
									path,
									NonRootFSObject::Dir(Cow::Owned(dir)),
								))
								.unwrap();
						}
						Entry::File(file_entry) => {
							let file = file_entry.extra_data().clone();
							file_download_request_sender
								.blocking_send((file, path))
								.unwrap();
						}
					}
				}
			})
		};

		let file_handle = self.spawn_file_downloader_task(
			file_download_request_receiver,
			entry_complete_sender,
			downloaded_bytes,
		);

		Ok((dir_handle, file_handle))
	}

	fn spawn_file_downloader_task(
		self: Arc<Self>,
		mut file_download_request_receiver: tokio::sync::mpsc::Receiver<(RemoteFile, PathBuf)>,
		entry_complete_sender: tokio::sync::mpsc::Sender<(
			Result<(), Error>,
			PathBuf,
			NonRootFSObject<'static>,
		)>,
		downloaded_bytes: Arc<AtomicU64>,
	) -> tokio::task::JoinHandle<()> {
		tokio::task::spawn(async move {
			let semaphore = Arc::new(tokio::sync::Semaphore::new(self.max_parallel_requests));

			let mut join_set = tokio::task::JoinSet::new();
			while let Some((remote_file, path)) = file_download_request_receiver.recv().await {
				let permit = Arc::clone(&semaphore).acquire_owned().await.unwrap();
				let client = Arc::clone(&self);
				let entry_complete_sender = entry_complete_sender.clone();
				let downloaded_bytes = Arc::clone(&downloaded_bytes);
				join_set.spawn(async move {
					let (res, path, file) = client
						.inner_download_file_to_path(remote_file, path, &downloaded_bytes)
						.await;

					let _ = entry_complete_sender
						.send((res, path, NonRootFSObject::File(Cow::Owned(file))))
						.await;
					drop(permit);
				});
			}
			join_set.join_all().await;
		})
	}

	async fn inner_download_file_to_path(
		&self,
		remote_file: RemoteFile,
		path: PathBuf,
		downloaded_bytes: &AtomicU64,
	) -> (Result<(), Error>, PathBuf, RemoteFile) {
		let (local_file, path, remote_file) = match tokio::task::spawn_blocking(|| {
			if let Ok(meta) = std::fs::metadata(&path)
				&& FilenMetaExt::size(&meta) == remote_file.size()
				&& let Ok(mut file) = std::fs::File::open(&path)
			{
				if let Some(hash) = remote_file.hash() {
					let mut hasher = md5::Md5::new();

					let mut buffer = [0u8; 65536];
					loop {
						let bytes_read = match file.read(&mut buffer) {
							Ok(n) => n,
							Err(e) => return (Err(e.into()), path, remote_file),
						};
						if bytes_read == 0 {
							break;
						}
						hasher.update(&buffer[..bytes_read]);
					}
					if hasher.finalize().as_slice() == hash.as_ref() {
						return (Ok(None), path, remote_file);
					}
				}
			}
			let local_file = match std::fs::File::create(&path) {
				Ok(f) => f,
				Err(e) => return (Err(e.into()), path, remote_file),
			};
			(Ok(Some(local_file)), path, remote_file)
		})
		.await
		.unwrap()
		{
			(Ok(Some(local_file)), path, remote_file) => (local_file, path, remote_file),
			(res, path, remote_file) => {
				return (res.map(|_| ()), path, remote_file);
			}
		};

		let local_file = tokio::fs::File::from_std(local_file);

		match self
			.download_file_to_writer(
				&remote_file,
				&mut local_file.compat_write(),
				Some(Arc::new(|bytes| {
					downloaded_bytes.fetch_add(bytes as u64, std::sync::atomic::Ordering::Relaxed);
				})),
			)
			.await
		{
			Ok(_) => (Ok(()), path, remote_file),
			Err(e) => (Err(e), path, remote_file),
		}
	}
}

/// Callback trait for folder download operations
///
/// Folder downloads are implemented using a single sweep
/// While scanning the folder contents, files are downloaded in parallel
/// Progress is reported during the download process.
pub trait DirDownloadCallback {
	fn on_scan_progress(&self, known_dir: u64, known_files: u64, known_bytes: u64);
	fn on_scan_complete(&self, total_dirs: u64, total_files: u64, total_bytes: u64);
	fn on_download_update(&self, uploaded_dirs: u64, uploaded_files: u64, uploaded_bytes: u64);
	fn on_download_error(&self, path: &Path, error: Error);
	fn on_download_complete(&self);
}

struct FileDownloadResult(Result<RemoteFile, (Error, PathBuf)>);

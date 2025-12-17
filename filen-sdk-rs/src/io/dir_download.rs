use std::{
	borrow::Cow,
	io::Read,
	path::PathBuf,
	sync::{Arc, atomic::AtomicU64},
};

use md5::Digest;
use tokio_util::compat::TokioAsyncWriteCompatExt;

use crate::{
	Error,
	auth::Client,
	consts::CALLBACK_INTERVAL,
	error::{ErrorExt, ResultExt},
	fs::{
		NonRootFSObject,
		dir::{RemoteDirectory, UnsharedDirectoryType},
		file::{RemoteFile, traits::HasRemoteFileInfo},
	},
	io::{FilenMetaExt, HasFileInfo},
};

use super::{WalkError, fs_tree::Entry};

type EntryResult = (Result<(), Error>, PathBuf, NonRootFSObject<'static>);

impl Client {
	pub(crate) async fn download_fs_tree_from_target_into_path(
		self: Arc<Self>,
		error_callback: &mut impl FnMut(Vec<(Error, PathBuf, NonRootFSObject<'static>)>),
		progress_callback: &mut impl FnMut(
			Vec<(RemoteDirectory, PathBuf)>,
			Vec<(RemoteFile, PathBuf)>,
			u64,
		),
		path: PathBuf,
		tree: super::fs_tree::FSTree<RemoteDirectory, RemoteFile>,
		target_folder: UnsharedDirectoryType<'static>,
	) -> Result<(), Error> {
		let (entry_complete_sender, mut entry_complete_receiver) =
			tokio::sync::mpsc::channel::<EntryResult>(16);

		let mut update_interval = tokio::time::interval(CALLBACK_INTERVAL);

		let (file_download_request_sender, file_download_request_receiver) =
			tokio::sync::mpsc::channel::<(RemoteFile, PathBuf)>(self.max_parallel_requests);

		let downloaded_bytes = Arc::new(AtomicU64::new(0));

		let dir_handle = Arc::clone(&self).spawn_folder_maker_task(
			tree,
			entry_complete_sender.clone(),
			file_download_request_sender,
			target_folder,
			path,
		);

		let file_handle = self.spawn_file_downloader_task(
			file_download_request_receiver,
			entry_complete_sender,
			Arc::clone(&downloaded_bytes),
		);

		let mut completed_files = Vec::new();
		let mut completed_dirs = Vec::new();
		let mut errors = Vec::new();

		loop {
			tokio::select! {
				_ = update_interval.tick() => {
					let bytes = downloaded_bytes.swap(0, std::sync::atomic::Ordering::Relaxed);
					if !errors.is_empty() {
						error_callback(std::mem::take(&mut errors));
					}
					if completed_dirs.is_empty() && completed_files.is_empty() && bytes == 0 {
						continue;
					}
					progress_callback(
						std::mem::take(&mut completed_dirs),
						std::mem::take(&mut completed_files),
						bytes,
					);
				}
				entry_result = entry_complete_receiver.recv() => {
					let (res, path, obj) = match entry_result {
						Some(er) => er,
						None => break,
					};
					match res {
						Ok(()) => {
							match obj {
								NonRootFSObject::Dir(dir) => {
									completed_dirs.push((dir.into_owned(), path));
								}
								NonRootFSObject::File(file) => {
									completed_files.push((file.into_owned(), path));
								}
							}
						}
						Err(e) => {
							errors.push((e, path, obj));
						}
					}
				}
			}
		}

		// make sure everything is finalized
		dir_handle.await.unwrap()?;
		file_handle.await.unwrap();

		if !errors.is_empty() {
			error_callback(std::mem::take(&mut errors));
		}
		let bytes = downloaded_bytes.swap(0, std::sync::atomic::Ordering::Relaxed);
		if !completed_dirs.is_empty() || !completed_files.is_empty() || bytes != 0 {
			progress_callback(
				std::mem::take(&mut completed_dirs),
				std::mem::take(&mut completed_files),
				bytes,
			);
		}

		Ok(())
	}

	fn spawn_folder_maker_task(
		self: Arc<Self>,
		tree: super::fs_tree::FSTree<RemoteDirectory, RemoteFile>,
		entry_complete_sender: tokio::sync::mpsc::Sender<EntryResult>,
		file_download_request_sender: tokio::sync::mpsc::Sender<(RemoteFile, PathBuf)>,
		target_folder: UnsharedDirectoryType<'static>,
		root_path: PathBuf,
	) -> tokio::task::JoinHandle<Result<(), Error>> {
		tokio::task::spawn_blocking(move || {
			match (std::fs::create_dir_all(&root_path), &target_folder) {
				(Ok(()), UnsharedDirectoryType::Dir(target_folder)) => target_folder
					.set_dir_times(&root_path)
					.context("couldn't set directory times for newly created root directory")?,
				(Err(e), _) if e.kind() == std::io::ErrorKind::AlreadyExists => {
					if let UnsharedDirectoryType::Dir(target_folder) = target_folder {
						target_folder
							.set_dir_times(&root_path)
							.context("couldn't set directory times for root directory")?
					}
				}
				(Err(e), _) => {
					return Err(e.with_context("couldn't create root directory for dir download"));
				}
				_ => {}
			};

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
							.blocking_send((Ok(()), path, NonRootFSObject::Dir(Cow::Owned(dir))))
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
			Ok(())
		})
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
				&& let Some(hash) = remote_file.hash()
			{
				let mut hasher = sha2::Sha512::new();

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
					downloaded_bytes.fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
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
	/// Called periodically while /dir/download is listing the directory contents
	fn on_query_download_progress(&self, known_bytes: u64, total_bytes: Option<u64>);
	/// Called during tree building
	fn on_scan_progress(&self, known_dir: u64, known_files: u64, known_bytes: u64);
	/// Called when errors occur during tree building
	fn on_scan_errors(&self, errors: Vec<WalkError>);
	/// Called when tree building is complete
	fn on_scan_complete(&self, total_dirs: u64, total_files: u64, total_bytes: u64);
	/// Called periodically during the download process
	fn on_download_update(
		&self,
		downloaded_dirs: Vec<(RemoteDirectory, PathBuf)>,
		downloaded_files: Vec<(RemoteFile, PathBuf)>,
		downloaded_bytes: u64,
	);
	/// Called when errors occur during the download process
	fn on_download_errors(&self, errors: Vec<(Error, PathBuf, NonRootFSObject<'static>)>);
}

struct FileDownloadResult(Result<RemoteFile, (Error, PathBuf)>);

#[cfg(test)]
mod tests {

	use crate::{
		auth::{Client, StringifiedClient},
		current_allocation,
	};

	use super::*;

	struct TestDirDownloadCallback;
	impl DirDownloadCallback for TestDirDownloadCallback {
		fn on_query_download_progress(&self, known_bytes: u64, total_bytes: Option<u64>) {
			log::info!(
				"Listing progress: {} bytes{}",
				known_bytes,
				match total_bytes {
					Some(total) => format!("/{} bytes", total),
					None => String::new(),
				}
			);
		}

		fn on_scan_progress(&self, known_dir: u64, known_files: u64, known_bytes: u64) {
			log::info!(
				"Scan progress: {} dirs, {} files, {} bytes",
				known_dir,
				known_files,
				known_bytes
			);
		}

		fn on_scan_errors(&self, errors: Vec<WalkError>) {
			log::error!("Errors during walk: {:?}", errors);
		}

		fn on_scan_complete(&self, total_dirs: u64, total_files: u64, total_bytes: u64) {
			log::info!(
				"Scan complete: {} dirs, {} files, {} bytes",
				total_dirs,
				total_files,
				total_bytes
			);
		}

		fn on_download_update(
			&self,
			downloaded_dirs: Vec<(RemoteDirectory, PathBuf)>,
			downloaded_files: Vec<(RemoteFile, PathBuf)>,
			downloaded_bytes: u64,
		) {
			let requests = crate::GLOBAL_PARALLEL_REQUESTS
				.get_or_init(|| std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)))
				.load(std::sync::atomic::Ordering::Relaxed);
			let peak = crate::PEAK_PARALLEL_REQUESTS
				.get_or_init(|| std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)))
				.load(std::sync::atomic::Ordering::Relaxed);
			log::info!(
				"Download update: {} dirs, {} files, {} bytes, current requests: {}, peak requests: {}, current allocation : {:.2} MiB",
				downloaded_dirs.len(),
				downloaded_files.len(),
				downloaded_bytes,
				requests,
				peak,
				current_allocation() as f64 / 1024.0 / 1024.0
			);
		}

		fn on_download_errors(&self, errors: Vec<(Error, PathBuf, NonRootFSObject<'static>)>) {
			log::info!("Download errors: {:?}", errors);
		}
	}

	#[tokio::test]
	async fn test_dfs_walk() {
		dotenv::dotenv().ok();
		env_logger::init();
		log::info!(
			"Current allocation: {:.2} MiB",
			current_allocation() as f64 / 1024.0 / 1024.0
		);

		let client = Arc::new(Client::from_stringified(StringifiedClient {
			email: "endur1el@protonmail.com".to_string(),
			user_id: 108492,
			root_uuid: "1f929996-ceb3-42dd-882e-3988d5cfbba7".to_string(),
			auth_info: "e06656bbecec566fe1c718f848db6a8e7c2c94ba60a562379a897f0080d73c49".to_string(),
			private_key: "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".to_string(),
			api_key: "6WYWZ0ZcMNFoI0k7MKNwLVHrGMPUPpGPvelf7efAsAqgWppvJQzEoZbzoL9k3gk5".to_string(),
			auth_version: 2,
			max_parallel_requests: None,
			max_io_memory_usage: None,
		}).unwrap());

		let tmp_dir = client
			.find_item_in_dir(client.root(), "Scripts")
			.await
			.unwrap()
			.unwrap();

		let temp_dir = match tmp_dir {
			NonRootFSObject::Dir(dir) => dir,
			_ => panic!("Temp is not a directory"),
		};

		Arc::clone(&client)
			.download_dir_recursively(
				PathBuf::from("/Users/end/Documents/tmp/test_mkdir/"),
				&TestDirDownloadCallback,
				(&*temp_dir).into(),
			)
			.await
			.unwrap();
	}
}

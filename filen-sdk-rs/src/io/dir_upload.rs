use std::{
	collections::VecDeque,
	path::{Path, PathBuf},
	sync::{
		Arc,
		atomic::{AtomicU64, Ordering},
	},
};

use filen_types::fs::UuidStr;
use tokio_util::compat::TokioAsyncReadCompatExt;

use crate::{
	Error, ErrorKind,
	auth::Client,
	consts::{CALLBACK_INTERVAL, MAX_SMALL_PARALLEL_REQUESTS},
	error::ResultExt,
	fs::{HasUUID, dir::RemoteDirectory, file::RemoteFile},
	io::{
		FilenMetaExt,
		fs_tree_builder::{DirChildrenInfo, Entry, WalkError},
	},
};

/// Callback trait for folder upload operations
///
/// Folder uploads are implemented using a two step process
/// 1. Scan the folder to determine total size and number of files/folders
/// 2. Upload the folder contents while reporting progress
///
/// This means there can be a minor mismatch between the reported total size/files/folders
/// during scanning and the actual totals during upload, if files are added/removed
/// from the folder between the two steps.
pub trait DirUploadCallback: Send + Sync {
	fn on_scan_progress(&self, known_dirs: u64, known_files: u64, known_bytes: u64);
	fn on_scan_errors(&self, error: Vec<WalkError>);
	fn on_scan_complete(&self, total_dirs: u64, total_files: u64, total_bytes: u64);
	fn on_upload_progress(
		&self,
		uploaded_dirs: Vec<RemoteDirectory>,
		uploaded_files: Vec<RemoteFile>,
		uploaded_bytes: u64,
	);
	fn on_upload_errors(&self, errors: Vec<(PathBuf, Error)>);
}

struct AtomicDropCanceller {
	cancelled: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for AtomicDropCanceller {
	fn drop(&mut self) {
		self.cancelled.store(true, Ordering::SeqCst);
	}
}

impl Client {
	async fn upload_fs_tree_from_path_into_target(
		self: Arc<Self>,
		callback: Arc<dyn DirUploadCallback>,
		path: PathBuf,
		tree: &super::fs_tree_builder::FSTree,
		target_folder: &RemoteDirectory,
	) -> Result<(), Error> {
		let fs_root = match tree.root() {
			Entry::File(_) => {
				return Err(Error::custom(
					ErrorKind::IO,
					"upload_folder_from_path_into_target: root path is a file",
				));
			}
			Entry::Dir(dir_entry) => dir_entry,
		};

		let (file_upload_task_sender, mut file_upload_result_receiver) =
			tokio::sync::mpsc::channel::<FileUploadResult>(16);
		let (dir_upload_task_sender, mut dir_upload_result_receiver) =
			tokio::sync::mpsc::channel::<DirUploadResult>(16);

		let (entry_to_upload_sender, entry_to_upload_receiver) =
			tokio::sync::mpsc::channel::<EntryToUploadInfo>(64);

		let mut uploaded_dirs = Vec::new();
		let mut uploaded_files = Vec::new();
		let uploaded_bytes = Arc::new(AtomicU64::new(0));
		let mut upload_errors = Vec::new();

		let mut update_interval = tokio::time::interval(CALLBACK_INTERVAL);
		update_interval.reset();

		let mut unsent_children = VecDeque::new();

		dir_upload_task_sender
			.send(DirUploadResult(
				// have to clone here
				Ok((target_folder.clone(), fs_root.children_info())),
				path,
			))
			.await
			.expect("dir_upload_task_sender.send panicked");
		let mut in_flight = 1; // root dir upload

		let task = self.spawn_upload_task_manager(
			dir_upload_task_sender,
			file_upload_task_sender,
			entry_to_upload_receiver,
			uploaded_bytes.clone(),
		);

		loop {
			tokio::select! {
				_ = update_interval.tick() => {
					let bytes = uploaded_bytes.swap(0, Ordering::Relaxed);
					callback.on_upload_progress(std::mem::take(&mut uploaded_dirs), std::mem::take(&mut uploaded_files), bytes);

					if !upload_errors.is_empty() {
						callback.on_upload_errors(std::mem::take(&mut upload_errors));
					}

					drain_unsent(&entry_to_upload_sender, &mut unsent_children, &mut in_flight);
					if in_flight == 0 && unsent_children.is_empty() {
						break;
					}
				},
				Some(result) = file_upload_result_receiver.recv() => {
					in_flight -= 1;
					match result.0 {
						Ok(file) => {
							uploaded_files.push(file);
						},
						Err((error, path)) => {
							upload_errors.push((path, error));
						}
					}
					drain_unsent(&entry_to_upload_sender, &mut unsent_children, &mut in_flight);
					if in_flight == 0 && unsent_children.is_empty() {
						break;
					}
				}
				Some(result) = dir_upload_result_receiver.recv() => {
					in_flight -= 1;
					match result {
						DirUploadResult(Ok((dir, children_info)), path) => {
							let new_children = tree.list_children(children_info);
							let parent_uuid = *dir.uuid();
							uploaded_dirs.push(dir);
							for child in new_children {
								let entry_info = EntryToUploadInfo {
									entry: match child {
										Entry::Dir(dir) => EntryToUpload::Dir(
											path.join(tree.get_name(dir)).to_path_buf(),
											dir.children_info(),
										),
										Entry::File(file) => EntryToUpload::File(
											path.join(tree.get_name(file)).to_path_buf(),
										),
									},
									parent: parent_uuid,
								};
								if let Err(e) = entry_to_upload_sender.try_send(entry_info) {
									unsent_children.push_back(e.into_inner());
								} else {
									in_flight += 1
								};
							}
						}
						DirUploadResult(Err(error), path) => {
							upload_errors.push((path, error));
						}
					}
					drain_unsent(&entry_to_upload_sender, &mut unsent_children, &mut in_flight);
					if in_flight == 0 && unsent_children.is_empty() {
						break;
					}
				}
			}
		}
		std::mem::drop(entry_to_upload_sender);
		task.await.expect("upload task manager panicked");
		Ok(())
	}

	fn spawn_upload_task_manager(
		self: Arc<Self>,
		dir_upload_sender: tokio::sync::mpsc::Sender<DirUploadResult>,
		file_upload_sender: tokio::sync::mpsc::Sender<FileUploadResult>,
		mut entry_to_upload_receiver: tokio::sync::mpsc::Receiver<EntryToUploadInfo>,
		bytes_progress_counter: Arc<AtomicU64>,
	) -> tokio::task::JoinHandle<()> {
		let semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_SMALL_PARALLEL_REQUESTS));
		tokio::task::spawn(async move {
			// propogate aborts to all spawned tasks
			let mut join_set = tokio::task::JoinSet::new();
			loop {
				let permit = semaphore.clone().acquire_owned().await;
				{
					let permit = permit.expect("semaphore acquire failed");
					let entry_info = match entry_to_upload_receiver.recv().await {
						Some(info) => info,
						None => break, // channel closed, exit the loop
					};
					let client = self.clone();
					match entry_info.entry {
						EntryToUpload::File(path) => {
							let file_sender = file_upload_sender.clone();
							let bytes_counter = bytes_progress_counter.clone();
							join_set.spawn(async move {
								let result = client
									.upload_child_file_from_path(
										&path,
										&entry_info.parent,
										bytes_counter,
									)
									.await;
								let _ = file_sender
									.send(FileUploadResult(result.map_err(|e| (e, path))))
									.await;
								std::mem::drop(permit);
							});
						}
						EntryToUpload::Dir(path, children_info) => {
							let dir_sender = dir_upload_sender.clone();
							join_set.spawn(async move {
								let result = client
									.upload_child_dir_from_path(&path, &entry_info.parent)
									.await;
								let _ = dir_sender
									.send(DirUploadResult(
										result.map(|dir| (dir, children_info)),
										path,
									))
									.await;
								std::mem::drop(permit);
							});
						}
					}
				}
			}
			join_set.join_all().await;
		})
	}

	async fn upload_child_file_from_path(
		&self,
		path: &Path,
		parent: &UuidStr,
		uploaded_bytes: Arc<AtomicU64>,
	) -> Result<RemoteFile, Error> {
		let os_file = tokio::fs::File::open(path).await.map_err(|e| {
			Error::custom_with_source(ErrorKind::IO, e, Some(format!("opening file {:?}", path)))
		})?;
		let meta = os_file.metadata().await.map_err(|e| {
			Error::custom_with_source(
				ErrorKind::IO,
				e,
				Some(format!("getting metadata for file {:?}", path)),
			)
		})?;
		let size = FilenMetaExt::size(&meta);

		let base_file = self
			.make_file_builder(
				path.file_name()
					.expect("path name should be valid")
					.to_str()
					.expect("path name should be utf8"),
				parent,
			)
			.created(FilenMetaExt::created(&meta))
			.modified(FilenMetaExt::modified(&meta))
			.build();

		self.upload_file_from_reader(
			base_file.into(),
			&mut os_file.compat(),
			Some(Arc::new(|bytes_downloaded| {
				uploaded_bytes.fetch_add(bytes_downloaded, Ordering::Relaxed);
			})),
			Some(size),
		)
		.await
		.context("uploading file from path")
	}

	async fn upload_child_dir_from_path(
		&self,
		path: &Path,
		parent: &UuidStr,
	) -> Result<RemoteDirectory, Error> {
		let metadata = tokio::fs::metadata(path).await.map_err(|e| {
			Error::custom_with_source(
				ErrorKind::IO,
				e,
				Some(format!("getting metadata for directory {:?}", path)),
			)
		})?;
		let created = FilenMetaExt::created(&metadata);
		self.create_dir_with_created(
			parent,
			path.file_name()
				.expect("path name should be valid")
				.to_str()
				.expect("path name should be utf8")
				.to_owned(),
			created,
		)
		.await
		.context("creating directory during folder upload")
	}
}

fn drain_unsent(
	sender: &tokio::sync::mpsc::Sender<EntryToUploadInfo>,
	unsent: &mut VecDeque<EntryToUploadInfo>,
	in_flight: &mut usize,
) {
	while let Some(entry) = unsent.pop_front() {
		match sender.try_send(entry) {
			Ok(()) => *in_flight += 1,
			Err(e) => {
				unsent.push_front(e.into_inner());
				break;
			}
		}
	}
}

struct DirUploadResult(Result<(RemoteDirectory, DirChildrenInfo), Error>, PathBuf);
struct FileUploadResult(Result<RemoteFile, (Error, PathBuf)>);

struct EntryToUploadInfo {
	entry: EntryToUpload,
	parent: UuidStr,
}
enum EntryToUpload {
	File(PathBuf), // path, size
	Dir(PathBuf, DirChildrenInfo),
}

enum UploadTaskResult {
	FileUploadResult(Result<(RemoteFile, PathBuf), Error>),
	DirUploaded(Result<(RemoteDirectory, DirChildrenInfo, PathBuf), Error>),
}

struct UploadManager {
	client: Arc<Client>,
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

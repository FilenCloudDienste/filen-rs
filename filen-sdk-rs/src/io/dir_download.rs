use std::{
	borrow::Cow,
	ops::Deref,
	path::PathBuf,
	sync::{Arc, atomic::AtomicU64},
};

use crate::{
	Error,
	auth::shared_client::SharedClient,
	consts::CALLBACK_INTERVAL,
	error::{ErrorExt, ResultExt},
	fs::{
		categories::{Category, DirType, NonRootItemType, fs::CategoryFSExt},
		file::RemoteFile,
	},
	io::{
		client_impl::inner_download_to_path_with_hash_check,
		fs_tree::build_fs_tree_from_remote_iterator, meta_ext::DirTimesExt,
	},
};

use super::fs_tree::Entry;

type EntryResult<Cat: Category> = (Result<(), Error>, String, NonRootItemType<'static, Cat>);

/// Callback trait for folder download operations
///
/// Folder downloads are implemented using a single sweep
/// While scanning the folder contents, files are downloaded in parallel
/// Progress is reported during the download process.
pub trait DirDownloadCallback<Cat>: Send + Sync
where
	Cat: Category + ?Sized,
{
	/// Called periodically while /dir/download is listing the directory contents
	fn on_query_download_progress(&self, known_bytes: u64, total_bytes: Option<u64>);
	/// Called during tree building
	fn on_scan_progress(&self, known_dirs: u64, known_files: u64, known_bytes: u64);
	/// Called when errors occur during tree building
	fn on_scan_errors(&self, errors: Vec<Error>);
	/// Called when tree building is complete
	fn on_scan_complete(&self, total_dirs: u64, total_files: u64, total_bytes: u64);
	/// Called periodically during the download process
	fn on_download_update(
		&self,
		downloaded_dirs: Vec<(Cat::Dir, String)>,
		downloaded_files: Vec<(Cat::File, String)>,
		downloaded_bytes: u64,
	);
	/// Called when errors occur during the download process
	fn on_download_errors(&self, errors: Vec<(Error, String, NonRootItemType<'static, Cat>)>);
}

struct FileDownloadResult(Result<RemoteFile, (Error, String)>);

pub(crate) trait CategoryDirDownloadExt: CategoryFSExt {
	fn download_file_to_path_in_dir_download(
		client: &Self::Client,
		remote_file: Self::File,
		path: PathBuf,
		downloaded_bytes: &AtomicU64,
	) -> impl Future<Output = (Result<(), Error>, PathBuf, Self::File)> + Send {
		async {
			let (res, path) = inner_download_to_path_with_hash_check(
				client,
				&remote_file,
				path,
				Some(Arc::new(|bytes| {
					downloaded_bytes.fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
				})),
			)
			.await;

			(res, path, remote_file)
		}
	}

	fn spawn_folder_maker_task(
		tree: super::fs_tree::FSTree<Self::Dir, Self::File>,
		entry_complete_sender: tokio::sync::mpsc::Sender<EntryResult<Self>>,
		file_download_request_sender: tokio::sync::mpsc::Sender<(Self::File, String)>,
		target_folder: DirType<'static, Self>,
		root_path: String,
	) -> tokio::task::JoinHandle<Result<(), Error>> {
		tokio::task::spawn_blocking(move || {
			match (std::fs::create_dir_all(&root_path), &target_folder) {
				(Ok(()), DirType::<Self>::Dir(target_folder)) => target_folder
					.blocking_set_dir_times(root_path.as_ref())
					.context("couldn't set directory times for newly created root directory")?,
				(Err(e), _) if e.kind() == std::io::ErrorKind::AlreadyExists => {
					if let DirType::<Self>::Dir(target_folder) = target_folder {
						target_folder
							.blocking_set_dir_times(root_path.as_ref())
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
									NonRootItemType::<Self>::Dir(Cow::Owned(dir)),
								))
								.unwrap();
							continue;
						}
						if let Err(e) = dir.blocking_set_dir_times(path.as_ref()) {
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
									NonRootItemType::<Self>::Dir(Cow::Owned(dir)),
								))
								.unwrap();
							continue;
						}
						entry_complete_sender
							.blocking_send((
								Ok(()),
								path,
								NonRootItemType::<Self>::Dir(Cow::Owned(dir)),
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
			Ok(())
		})
	}

	fn spawn_file_downloader_task(
		client: Arc<Self::Client>,
		mut file_download_request_receiver: tokio::sync::mpsc::Receiver<(Self::File, String)>,
		entry_complete_sender: tokio::sync::mpsc::Sender<(
			Result<(), Error>,
			String,
			NonRootItemType<'static, Self>,
		)>,
		downloaded_bytes: Arc<AtomicU64>,
	) -> tokio::task::JoinHandle<()> {
		tokio::task::spawn(async move {
			let semaphore = Arc::new(tokio::sync::Semaphore::new(
				client.get_unauth_client().state().max_concurrency(),
			));

			let mut join_set = tokio::task::JoinSet::new();
			while let Some((remote_file, path)) = file_download_request_receiver.recv().await {
				let permit = Arc::clone(&semaphore).acquire_owned().await.unwrap();
				let client = Arc::clone(&client);
				let entry_complete_sender = entry_complete_sender.clone();
				let downloaded_bytes = Arc::clone(&downloaded_bytes);
				join_set.spawn(async move {
					let (res, _, file) = Self::download_file_to_path_in_dir_download(
						&client,
						remote_file,
						path.clone().into(),
						&downloaded_bytes,
					)
					.await;

					let _ = entry_complete_sender
						.send((res, path, NonRootItemType::<Self>::File(Cow::Owned(file))))
						.await;
					drop(permit);
				});
			}
			join_set.join_all().await;
		})
	}

	async fn download_dir_recursively<C>(
		client: Arc<Self::Client>,
		dir_path: String,
		callback: impl Deref<Target = C>,
		target: DirType<'static, Self>,
		context: Self::ListDirContext<'_>,
	) -> Result<(), Error>
	where
		C: DirDownloadCallback<Self> + ?Sized,
	{
		use filen_types::traits::CowHelpers;

		use crate::util::AtomicDropCanceller;

		let drop_canceller = AtomicDropCanceller::default();

		let callback_ref = callback.deref();

		let (tree, stats) = build_fs_tree_from_remote_iterator::<_, Self>(
			&client,
			target.as_borrowed_cow(),
			&mut |errors| {
				callback_ref.on_scan_errors(errors);
			},
			&mut |dirs, files, bytes| {
				callback_ref.on_scan_progress(dirs, files, bytes);
			},
			Some(&|current_bytes, total_bytes| {
				callback_ref.on_query_download_progress(current_bytes, total_bytes);
			}),
			drop_canceller.cancelled(),
			context,
		)
		.await?;

		let (dirs, files, bytes) = stats.snapshot();
		callback_ref.on_scan_complete(dirs, files, bytes);

		Self::download_fs_tree_from_target_into_path(
			client,
			&mut |errors| {
				callback_ref.on_download_errors(errors);
			},
			&mut |downloaded_dirs, downloaded_files, bytes| {
				callback_ref.on_download_update(downloaded_dirs, downloaded_files, bytes);
			},
			dir_path,
			tree,
			target,
		)
		.await
	}

	async fn download_fs_tree_from_target_into_path(
		client: Arc<Self::Client>,
		error_callback: &mut impl FnMut(Vec<(Error, String, NonRootItemType<'static, Self>)>),
		progress_callback: &mut impl FnMut(Vec<(Self::Dir, String)>, Vec<(Self::File, String)>, u64),
		path: String,
		tree: super::fs_tree::FSTree<Self::Dir, Self::File>,
		target_folder: DirType<'static, Self>,
	) -> Result<(), Error> {
		let (entry_complete_sender, mut entry_complete_receiver) =
			tokio::sync::mpsc::channel::<EntryResult<Self>>(16);

		let mut update_interval = tokio::time::interval(CALLBACK_INTERVAL);

		let (file_download_request_sender, file_download_request_receiver) =
			tokio::sync::mpsc::channel::<(Self::File, String)>(
				client.get_unauth_client().state().max_concurrency(),
			);

		let downloaded_bytes = Arc::new(AtomicU64::new(0));

		let dir_handle = Self::spawn_folder_maker_task(
			tree,
			entry_complete_sender.clone(),
			file_download_request_sender,
			target_folder,
			path,
		);

		let file_handle = Self::spawn_file_downloader_task(
			client,
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
								NonRootItemType::<Self>::Dir(dir) => {
									completed_dirs.push((dir.into_owned(), path));
								}
								NonRootItemType::<Self>::File(file) => {
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
}

impl<T> CategoryDirDownloadExt for T where T: CategoryFSExt {}

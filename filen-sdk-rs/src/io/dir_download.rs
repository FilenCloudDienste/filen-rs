use std::{
	borrow::Cow,
	ops::Deref,
	sync::{Arc, atomic::AtomicU64},
};

use crate::{
	Error,
	auth::shared_client::SharedClient,
	consts::CALLBACK_INTERVAL,
	error::{ErrorExt, ResultExt},
	fs::categories::{Category, DirType, NonRootItemType, fs::CategoryFSExt},
	io::{
		CanonicalPath, TransferFilter,
		abort_on_drop::AbortOnDropHandle,
		client_impl::{KeepLocalWhen, LocalWrite, inner_download_to_path_with_hash_check},
		fs_tree::{ScanCallbacks, build_fs_tree_from_remote_iterator},
		meta_ext::DirTimesExt,
	},
};

use super::fs_tree::Entry;

type EntryResult<Cat: Category> = (
	Result<LocalWrite, Error>,
	String,
	NonRootItemType<'static, Cat>,
);

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
	/// Called periodically with the files the download left out because [`ExistingLocalFile`]
	/// kept the local file at their path, with those paths. By default they are reported to
	/// [`on_download_update`](Self::on_download_update) as downloaded, with no bytes.
	fn on_download_skipped(&self, skipped_files: Vec<(Cat::File, String)>) {
		self.on_download_update(Vec::new(), skipped_files, 0);
	}
}

/// What a directory download does with a local file already at the path of a file it downloads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExistingLocalFile {
	/// Replace it, unless it already holds the remote file: its blake3 hash matches the one the
	/// remote file declares or, for a remote file declaring none, it has the same size and was not
	/// modified before the remote file.
	#[default]
	ReplaceUnlessSame,
	/// Leave it as it is, whatever it holds.
	Keep,
}

impl ExistingLocalFile {
	fn keep_local_when(self) -> KeepLocalWhen {
		match self {
			Self::ReplaceUnlessSame => KeepLocalWhen::SameHashOrNotOlder,
			Self::Keep => KeepLocalWhen::Always,
		}
	}
}

/// How a directory download treats the local directory. The default replaces local files unless
/// they already hold the remote file.
#[derive(Clone, Debug, Default)]
pub struct DirDownloadOptions {
	/// What to do with a local file already at the path of a file the download writes.
	pub existing: ExistingLocalFile,
	/// Which items inside the remote directory to download.
	pub filter: TransferFilter,
}

pub(crate) trait CategoryDirDownloadExt: CategoryFSExt {
	fn download_file_to_path_in_dir_download(
		client: &Self::Client,
		remote_file: Self::File,
		path: CanonicalPath,
		downloaded_bytes: &AtomicU64,
		keep: KeepLocalWhen,
	) -> impl Future<Output = (Result<LocalWrite, Error>, CanonicalPath, Self::File)> + Send {
		async move {
			let (res, path) = inner_download_to_path_with_hash_check(
				client,
				&remote_file,
				path,
				Some(Arc::new(|bytes| {
					downloaded_bytes.fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
				})),
				keep,
			)
			.await;

			(res, path, remote_file)
		}
	}

	fn spawn_folder_maker_task(
		tree: super::fs_tree::FSTree<Self::Dir, Self::File>,
		entry_complete_sender: tokio::sync::mpsc::Sender<EntryResult<Self>>,
		file_download_request_sender: tokio::sync::mpsc::Sender<(Self::File, CanonicalPath)>,
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

			let iter = tree.dfs_iter_with_path(&root_path);

			for (entry, path) in iter.canonicalize()? {
				// A remote name that is not a safe single path component (`..`,
				// absolute, separator-bearing) is rejected while building the
				// path, so it cannot escape the download root. Skip the entry;
				// every descendant of an unsafe directory shares the rejected
				// component and is skipped the same way, quarantining the whole
				// subtree.
				let path = match path {
					Ok(path) => path,
					Err(e) => {
						tracing::warn!(
							"skipping remote entry with unsafe path during dir download: {e}"
						);
						continue;
					}
				};
				match entry {
					Entry::Dir(dir_entry) => {
						let dir = dir_entry.extra_data().clone();
						if let Err(e) = std::fs::create_dir(&path)
							&& e.kind() != std::io::ErrorKind::AlreadyExists
						{
							// A send error means the receiver was dropped — the
							// caller cancelled the download. Stop gracefully
							// rather than panicking this detached blocking thread.
							if entry_complete_sender
								.blocking_send((
									Err(e.with_context(
										"couldn't create directory during dir download",
									)),
									path.into_string(),
									NonRootItemType::<Self>::Dir(Cow::Owned(dir)),
								))
								.is_err()
							{
								return Ok(());
							}
							continue;
						}
						if let Err(e) = dir.blocking_set_dir_times(path.as_ref()) {
							tracing::warn!(
								"Failed to set dir times for downloaded dir {:?}: {}",
								path,
								e
							);
							if entry_complete_sender
								.blocking_send((
									Err(e.with_context(
										"couldn't set directory times during dir download",
									)),
									path.into_string(),
									NonRootItemType::<Self>::Dir(Cow::Owned(dir)),
								))
								.is_err()
							{
								return Ok(());
							}
							continue;
						}
						if entry_complete_sender
							.blocking_send((
								Ok(LocalWrite::Written),
								path.into_string(),
								NonRootItemType::<Self>::Dir(Cow::Owned(dir)),
							))
							.is_err()
						{
							return Ok(());
						}
					}
					Entry::File(file_entry) => {
						let file = file_entry.extra_data().clone();
						if file_download_request_sender
							.blocking_send((file, path))
							.is_err()
						{
							return Ok(());
						}
					}
				}
			}
			Ok(())
		})
	}

	fn spawn_file_downloader_task(
		client: Arc<Self::Client>,
		mut file_download_request_receiver: tokio::sync::mpsc::Receiver<(
			Self::File,
			CanonicalPath,
		)>,
		entry_complete_sender: tokio::sync::mpsc::Sender<EntryResult<Self>>,
		downloaded_bytes: Arc<AtomicU64>,
		keep: KeepLocalWhen,
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
						path.clone(),
						&downloaded_bytes,
						keep,
					)
					.await;

					let _ = entry_complete_sender
						.send((
							res,
							path.into_string(),
							NonRootItemType::<Self>::File(Cow::Owned(file)),
						))
						.await;
					drop(permit);
				});
			}
			join_set.join_all().await;
		})
	}

	async fn download_fs_tree_from_target_into_path(
		client: Arc<Self::Client>,
		callbacks: DownloadCallbacks<
			'_,
			impl FnMut(Vec<(Error, String, NonRootItemType<'static, Self>)>),
			impl FnMut(Vec<(Self::Dir, String)>, Vec<(Self::File, String)>, u64),
			impl FnMut(Vec<(Self::File, String)>),
		>,
		path: String,
		tree: super::fs_tree::FSTree<Self::Dir, Self::File>,
		target_folder: DirType<'static, Self>,
		existing: ExistingLocalFile,
	) -> Result<(), Error> {
		let DownloadCallbacks {
			errors: error_callback,
			progress: progress_callback,
			skipped: skipped_callback,
		} = callbacks;
		let (entry_complete_sender, mut entry_complete_receiver) =
			tokio::sync::mpsc::channel::<EntryResult<Self>>(16);

		let mut update_interval = tokio::time::interval(CALLBACK_INTERVAL);

		let (file_download_request_sender, file_download_request_receiver) =
			tokio::sync::mpsc::channel::<(Self::File, CanonicalPath)>(
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

		// Tie the downloads to this future, as the directory upload does: dropping it aborts the
		// downloader and, through its `JoinSet`, every download it started. The walker is a
		// blocking task that an abort cannot stop, so it is the downloader's receiver closing with
		// it that ends the walk: the walker's next send fails, or the one it is blocked in wakes
		// with an error, and it returns its thread. Left detached, the downloader would go on
		// draining the walker's channel, and the walker would hold a blocking thread for every
		// file left in a large flat directory.
		let file_handle = AbortOnDropHandle::new(Self::spawn_file_downloader_task(
			client,
			file_download_request_receiver,
			entry_complete_sender,
			Arc::clone(&downloaded_bytes),
			existing.keep_local_when(),
		));

		let mut completed_files = Vec::new();
		let mut completed_dirs = Vec::new();
		let mut skipped_files = Vec::new();
		let mut errors = Vec::new();

		loop {
			tokio::select! {
				_ = update_interval.tick() => {
					let bytes = downloaded_bytes.swap(0, std::sync::atomic::Ordering::Relaxed);
					if !errors.is_empty() {
						error_callback(std::mem::take(&mut errors));
					}
					if !skipped_files.is_empty() {
						skipped_callback(std::mem::take(&mut skipped_files));
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
						Ok(write) => {
							match (obj, write) {
								(NonRootItemType::<Self>::Dir(dir), _) => {
									completed_dirs.push((dir.into_owned(), path));
								}
								(NonRootItemType::<Self>::File(file), LocalWrite::Written) => {
									completed_files.push((file.into_owned(), path));
								}
								(NonRootItemType::<Self>::File(file), LocalWrite::Kept) => {
									skipped_files.push((file.into_owned(), path));
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
		file_handle.join().await.unwrap();

		if !errors.is_empty() {
			error_callback(std::mem::take(&mut errors));
		}
		if !skipped_files.is_empty() {
			skipped_callback(std::mem::take(&mut skipped_files));
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

/// Where a directory download reports what it did.
pub(crate) struct DownloadCallbacks<'a, E, P, S> {
	/// The entries that failed.
	pub(crate) errors: &'a mut E,
	/// The directories created and files written, and the bytes written since the last call.
	pub(crate) progress: &'a mut P,
	/// The files left out for the local file kept at their path.
	pub(crate) skipped: &'a mut S,
}

#[allow(private_bounds)]
pub trait CategoryDirDownloadExtPub: CategoryDirDownloadExt {
	/// Downloads `target` and everything below it into the local directory `dir_path`, as
	/// [`Self::download_dir_recursively_with_options`] does with the default
	/// [`DirDownloadOptions`].
	fn download_dir_recursively<C>(
		client: Arc<Self::Client>,
		dir_path: String,
		callback: impl Deref<Target = C>,
		target: DirType<'static, Self>,
		context: Self::ListDirContext<'_>,
	) -> impl std::future::Future<Output = Result<(), Error>>
	where
		C: DirDownloadCallback<Self> + ?Sized,
	{
		Self::download_dir_recursively_with_options(
			client,
			dir_path,
			callback,
			target,
			context,
			DirDownloadOptions::default(),
		)
	}

	/// Downloads `target` and everything below it into the local directory `dir_path`, as
	/// `options` say.
	fn download_dir_recursively_with_options<C>(
		client: Arc<Self::Client>,
		dir_path: String,
		callback: impl Deref<Target = C>,
		target: DirType<'static, Self>,
		context: Self::ListDirContext<'_>,
		options: DirDownloadOptions,
	) -> impl std::future::Future<Output = Result<(), Error>>
	where
		C: DirDownloadCallback<Self> + ?Sized,
	{
		use tracing::Instrument as _;

		let DirDownloadOptions { existing, filter } = options;

		let span = tracing::info_span!("download_dir_recursively", dir_path = %dir_path);
		async move {
			use filen_types::traits::CowHelpers;

			use crate::util::AtomicDropCanceller;

			let drop_canceller = AtomicDropCanceller::default();

			let callback_ref = callback.deref();

			let (tree, stats) = build_fs_tree_from_remote_iterator::<_, Self>(
				&client,
				target.as_borrowed_cow(),
				&filter,
				ScanCallbacks {
					errors: &mut |errors| {
						callback_ref.on_scan_errors(errors);
					},
					progress: &mut |dirs, files, bytes| {
						callback_ref.on_scan_progress(dirs, files, bytes);
					},
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
				DownloadCallbacks {
					errors: &mut |errors| {
						callback_ref.on_download_errors(errors);
					},
					progress: &mut |downloaded_dirs, downloaded_files, bytes| {
						callback_ref.on_download_update(downloaded_dirs, downloaded_files, bytes);
					},
					skipped: &mut |skipped_files| {
						callback_ref.on_download_skipped(skipped_files);
					},
				},
				dir_path,
				tree,
				target,
				existing,
			)
			.await
		}
		.instrument(span)
	}
}

impl<T> CategoryDirDownloadExtPub for T where T: CategoryDirDownloadExt {}

#[cfg(test)]
mod tests {
	use std::{borrow::Cow, sync::Arc, time::Duration};

	use chrono::{DateTime, Utc};
	use filen_types::fs::{ParentUuid, StableUuid};
	use uuid::Uuid;

	use super::{CategoryDirDownloadExt, DownloadCallbacks, ExistingLocalFile};
	use crate::{
		auth::test_support::offline_client,
		crypto::{file::FileKey, shared::CreateRandom, v3::EncryptionKey},
		fs::{
			categories::{DirType, Normal},
			dir::{RemoteDirectory, meta::DecryptedDirectoryMeta},
			file::{
				RemoteFile,
				meta::{DecryptedFileMeta, FileMeta},
			},
		},
		io::fs_tree::test_support::tree_of_files,
	};

	/// More files than the walker can hand over before it blocks: the request channel and the
	/// download permits each hold the client's concurrency, 16 by default.
	const FILES: u128 = 64;

	const ROOT: Uuid = Uuid::from_u128(0xD1);

	fn file_in(parent: Uuid, n: u128) -> RemoteFile {
		let uuid = Uuid::from_u128(n);
		RemoteFile::from_meta(
			uuid,
			StableUuid::new_for_test(uuid),
			ParentUuid::Uuid(parent),
			1,
			1,
			"de-1",
			"bucket",
			DateTime::<Utc>::UNIX_EPOCH,
			false,
			FileMeta::Decoded(DecryptedFileMeta {
				name: Cow::Owned(format!("file-{n}")),
				size: 1,
				mime: Cow::Borrowed("application/octet-stream"),
				key: FileKey::V3(EncryptionKey::generate()),
				last_modified: DateTime::<Utc>::UNIX_EPOCH,
				created: None,
				hash: None,
			}),
		)
	}

	#[test]
	fn dropping_a_dir_download_gives_back_its_walkers_blocking_thread() {
		// One blocking thread, as when every thread under the cap is taken: the walker holds it,
		// so no download can open its file and the walker blocks once its channel is full. Only
		// stopping the walker frees the thread for anything else.
		let runtime = tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.max_blocking_threads(1)
			.build()
			.expect("building a test runtime");
		runtime.block_on(async {
			let local = tempfile::tempdir().expect("creating a temp dir");
			let root = RemoteDirectory::new_from_parts(
				ROOT,
				DecryptedDirectoryMeta {
					name: Cow::Borrowed("root"),
					created: None,
				},
				ParentUuid::Uuid(Uuid::from_u128(0xD0)),
				DateTime::<Utc>::UNIX_EPOCH,
			);
			let files = (1..=FILES).map(|n| file_in(ROOT, n)).collect();
			let tree = tree_of_files(ROOT, files);

			let (mut on_errors, mut on_progress, mut on_skipped) = (|_| {}, |_, _, _| {}, |_| {});
			let mut download = Box::pin(Normal::download_fs_tree_from_target_into_path(
				Arc::new(offline_client()),
				DownloadCallbacks {
					errors: &mut on_errors,
					progress: &mut on_progress,
					skipped: &mut on_skipped,
				},
				local
					.path()
					.join("root")
					.into_os_string()
					.into_string()
					.expect("a UTF-8 temp dir"),
				tree,
				DirType::Dir(Cow::Owned(root)),
				ExistingLocalFile::ReplaceUnlessSame,
			));
			// The first poll starts the walker and the downloads; nothing can finish.
			assert!(futures::poll!(download.as_mut()).is_pending());
			drop(download);

			let probe = tokio::task::spawn_blocking(|| ());
			// Only bounds a failure: a walker left running never lets the probe run.
			tokio::time::timeout(Duration::from_secs(30), probe)
				.await
				.expect("the dropped download's walker gave back its blocking thread")
				.expect("the probe ran");
		});
	}
}

use std::{
	fs::FileTimes,
	io::{self},
	os::unix::fs::MetadataExt,
	path::{Path, PathBuf},
	str::FromStr,
	sync::Arc,
	time::{Duration, SystemTime},
};

use crate::{
	auth::{
		AUTH_CLEANUP_INTERVAL, AuthCacheState, AuthStatus, CacheState, DB_FILE_NAME,
		FilenMobileCacheState, update_saved_db_state_cache_cleanup_time,
	},
	sql,
	traits::ProgressCallback,
};
use filen_sdk_rs::{
	fs::{
		HasName, HasUUID,
		file::{BaseFile, FileBuilder, RemoteFile, traits::HasFileInfo},
	},
	io::FilenMetaExt,
};
use filen_types::{crypto::Sha512Hash, fs::UuidStr};
use futures::{StreamExt, stream::FuturesUnordered};
use log::{debug, error, info, trace};
use sha2::Digest;
use tokio::{fs::DirEntry, io::AsyncReadExt, sync::mpsc::UnboundedReceiver};
use tokio_util::compat::{
	FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt,
};

#[cfg(windows)]
fn get_file_times(created: Option<SystemTime>, modified: Option<SystemTime>) -> FileTimes {
	use std::os::windows::fs::FileTimesExt;
	let mut times = FileTimes::new();
	if let Some(created) = created {
		times = times.set_created(created);
	}
	if let Some(modified) = modified {
		times = times.set_modified(modified);
	}
	times
}

#[cfg(unix)]
fn get_file_times(_created: Option<SystemTime>, modified: Option<SystemTime>) -> FileTimes {
	let mut times = FileTimes::new();
	if let Some(modified) = modified {
		times = times.set_modified(modified);
	}
	times
}

pub const CACHE_DIR: &str = "cache";
const TMP_DIR: &str = "tmp";
const THUMBNAIL_DIR: &str = "thumbnails";

const BUFFER_SIZE: u64 = 64 * 1024; // 64 KiB
const CALLBACK_INTERVAL: Duration = Duration::from_millis(200);

pub(crate) fn get_paths(files_path: &Path) -> (PathBuf, PathBuf, PathBuf) {
	let cache_dir = files_path.join(CACHE_DIR);
	let tmp_dir = files_path.join(TMP_DIR);
	let thumbnail_dir = files_path.join(THUMBNAIL_DIR);
	(cache_dir, tmp_dir, thumbnail_dir)
}

pub(crate) fn init(files_path: &Path) -> Result<(PathBuf, PathBuf, PathBuf), io::Error> {
	let (cache_dir, tmp_dir, thumbnail_dir) = get_paths(files_path);
	std::fs::create_dir_all(&cache_dir)?;
	std::fs::create_dir_all(&tmp_dir)?;
	std::fs::create_dir_all(&thumbnail_dir)?;
	Ok((cache_dir, tmp_dir, thumbnail_dir))
}

async fn update_task(
	mut receiver: UnboundedReceiver<u64>,
	file_size: u64,
	callback: Arc<dyn ProgressCallback + Send + Sync>,
) {
	callback.set_total(file_size);
	let mut last_update = SystemTime::now();
	let mut written_since_update = 0;
	loop {
		tokio::select! {
			bytes_written = receiver.recv() => {
				match bytes_written {
					Some(bytes) => {
						written_since_update += bytes;
						let now = SystemTime::now();
						if now.duration_since(last_update).expect("Impossible time comparison") > CALLBACK_INTERVAL {
							callback.on_progress(written_since_update);
							written_since_update = 0;
							last_update = now;
						}
					},
					None => {
						if written_since_update > 0 {
							callback.on_progress(written_since_update);
						}
						break;
					},
				}
			},
			_ = tokio::time::sleep(CALLBACK_INTERVAL) => {
				if written_since_update > 0 {
					callback.on_progress(written_since_update);
					last_update = SystemTime::now();
					written_since_update = 0;
				}
			}
		}
	}
}

impl AuthCacheState {
	// we use the uuid as the folder and the actual name of the file because otherwise we run into a bug in IOS
	// where files get shared as a full UUID

	pub(crate) fn get_cached_file_path_from_name(&self, uuid: &str, name: Option<&str>) -> PathBuf {
		self.cache_dir
			.join(format!("{}/{}", uuid, name.unwrap_or(uuid)))
	}

	pub(crate) fn get_cached_file_path(&self, file: &RemoteFile) -> PathBuf {
		self.get_cached_file_path_from_name(file.uuid().as_ref(), file.name())
	}

	pub(crate) async fn try_get_local_file_with_uuid(
		&self,
		uuid: &UuidStr,
	) -> Result<Option<PathBuf>, io::Error> {
		let dir_path = self.cache_dir.join(uuid.as_ref());
		match tokio::fs::read_dir(&dir_path).await {
			Ok(mut entries) => {
				if let Ok(Some(entry)) = entries.next_entry().await {
					if let Ok(Some(_)) = entries.next_entry().await {
						return Err(io::Error::other(format!(
							"Multiple files found for UUID {} in cache",
							uuid.as_ref()
						)));
					}
					return Ok(Some(entry.path()));
				}
				Ok(None)
			}
			Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
			Err(e) => Err(e),
		}
	}

	pub async fn download_file_io(
		&self,
		file: &RemoteFile,
		callback: Option<Arc<dyn ProgressCallback>>,
	) -> Result<PathBuf, io::Error> {
		let reader = self.client.get_file_reader(file).compat();
		let src = self.tmp_dir.join(file.uuid().as_ref());
		tokio::io::BufReader::with_capacity(BUFFER_SIZE.min(file.size) as usize, reader);
		let mut os_file = tokio::fs::File::create(&src).await?.compat_write();
		let (sender, receiver) = tokio::sync::mpsc::unbounded_channel::<u64>();
		let file_size = file.size();
		let callback: Option<Arc<dyn Fn(u64) + Send + Sync + 'static>> =
			if let Some(callback) = callback {
				tokio::task::spawn(async move {
					update_task(receiver, file_size, callback).await;
				});
				Some(Arc::new(move |bytes_written: u64| {
					let _ = sender.send(bytes_written);
				}) as Arc<dyn Fn(u64) + Send + Sync>)
			} else {
				None
			};
		self.client
			.download_file_to_writer(file, &mut os_file, callback)
			.await
			.map_err(|e| io::Error::other(format!("Failed to download file: {e}")))?;
		let os_file = os_file.into_inner().into_std().await;
		let created = file.created().map(Into::into);
		let modified = file.last_modified().map(Into::into);
		tokio::task::spawn_blocking(move || {
			os_file
				.set_times(get_file_times(created, modified))
				.map_err(io::Error::other)
		})
		.await??;

		let dst = self.get_cached_file_path(file);
		let parent = dst
			.parent()
			.expect("cached file path parent should always exist");
		tokio::fs::create_dir_all(parent).await?;
		tokio::fs::rename(&src, &dst).await?;
		Ok(dst)
	}

	pub async fn hash_local_file(
		&self,
		file_uuid: &UuidStr,
	) -> Result<Option<Sha512Hash>, io::Error> {
		let Some(path) = self.try_get_local_file_with_uuid(file_uuid).await? else {
			return Ok(None);
		};
		let mut os_file = match tokio::fs::File::open(path).await {
			Ok(file) => file,
			Err(e) if e.kind() == io::ErrorKind::NotFound => {
				return Ok(None);
			}
			Err(e) => return Err(e),
		};
		let file_size = os_file
			.metadata()
			.await
			.map(|m| FilenMetaExt::size(&m).min(BUFFER_SIZE))
			.unwrap_or(BUFFER_SIZE);
		let mut buffer = vec![0; (file_size as usize).min(BUFFER_SIZE as usize)];
		let mut hasher = sha2::Sha512::new();
		loop {
			let bytes_read = os_file.read(&mut buffer).await?;
			if bytes_read == 0 {
				break;
			}
			hasher.update(&buffer[..bytes_read]);
		}
		let hash = hasher.finalize();
		Ok(Some(hash.into()))
	}

	pub(crate) async fn io_upload_file(
		&self,
		file: BaseFile,
		os_file: tokio::fs::File,
		callback: Option<Arc<dyn ProgressCallback>>,
	) -> Result<RemoteFile, io::Error> {
		let meta = os_file.metadata().await?;
		let file_size = FilenMetaExt::size(&meta);

		let (sender, receiver) = tokio::sync::mpsc::unbounded_channel::<u64>();
		let reader_callback = if let Some(callback) = callback {
			tokio::task::spawn(async move {
				update_task(receiver, file_size, callback).await;
			});
			Some(Arc::new(move |bytes_written: u64| {
				let _ = sender.send(bytes_written);
			}) as Arc<dyn Fn(u64) + Send + Sync>)
		} else {
			None
		};

		let mut os_file = os_file.compat();

		let remote_file = self
			.client
			.upload_file_from_reader(file.into(), &mut os_file, reader_callback, Some(file_size))
			.await
			.map_err(|e| io::Error::other(format!("Failed to upload file: {e}")))?;

		Ok(remote_file)
	}

	async fn inner_upload_file(
		&self,
		file_builder: FileBuilder,
		os_file: tokio::fs::File,
		callback: Option<Arc<dyn ProgressCallback>>,
	) -> Result<(RemoteFile, tokio::fs::File), io::Error> {
		let meta = os_file.metadata().await?;
		let file = file_builder
			.created(FilenMetaExt::created(&meta))
			.modified(FilenMetaExt::modified(&meta))
			.build();

		let file_size = FilenMetaExt::size(&meta);

		let (sender, receiver) = tokio::sync::mpsc::unbounded_channel::<u64>();
		let reader_callback = if let Some(callback) = callback {
			tokio::task::spawn(async move {
				update_task(receiver, file_size, callback).await;
			});
			Some(Arc::new(move |bytes_written: u64| {
				let _ = sender.send(bytes_written);
			}) as Arc<dyn Fn(u64) + Send + Sync>)
		} else {
			None
		};

		let mut os_file = os_file.compat();

		let remote_file = self
			.client
			.upload_file_from_reader(file.into(), &mut os_file, reader_callback, Some(file_size))
			.await
			.map_err(|e| io::Error::other(format!("Failed to upload file: {e}")))?;

		Ok((remote_file, os_file.into_inner()))
	}

	pub(crate) async fn io_upload_updated_file(
		&self,
		old_uuid: &str,
		name: &str,
		parent_uuid: UuidStr,
		mime: String,
		callback: Option<Arc<dyn ProgressCallback>>,
	) -> Result<RemoteFile, io::Error> {
		let old_path = self.get_cached_file_path_from_name(old_uuid, Some(name));
		let old_file = tokio::fs::File::open(&old_path).await?;
		let file_builder = self.client.make_file_builder(name, &parent_uuid).mime(mime);
		let (file, _) = self
			.inner_upload_file(file_builder, old_file, callback)
			.await?;
		let new_path = self.get_cached_file_path(&file);
		let parent = new_path
			.parent()
			.expect("cached file path parent should always exist");
		tokio::fs::create_dir_all(parent).await?;
		tokio::fs::rename(&old_path, new_path).await?;
		if let Some(parent) = old_path.parent()
			&& let Err(e) = tokio::fs::remove_dir(parent).await
		{
			log::warn!(
				"Failed to remove old parent directory {}: {}",
				parent.display(),
				e
			)
		};
		Ok(file)
	}

	pub(crate) async fn io_upload_new_file(
		&self,
		name: &str,
		parent_uuid: UuidStr,
		mime: Option<String>,
	) -> Result<(RemoteFile, PathBuf), io::Error> {
		let mut file_builder = self.client.make_file_builder(name, &parent_uuid);
		if let Some(mime) = mime {
			file_builder = file_builder.mime(mime);
		}
		let target_path =
			self.get_cached_file_path_from_name(file_builder.get_uuid().as_ref(), Some(name));
		let parent_path = target_path
			.parent()
			.expect("cached file path should always have a parent");
		tokio::fs::create_dir_all(parent_path).await?;
		let os_file = tokio::fs::OpenOptions::new()
			.read(true)
			.append(true) // only for create
			.create(true)
			.open(&target_path)
			.await?;
		let (file, _) = self.inner_upload_file(file_builder, os_file, None).await?;
		Ok((file, target_path))
	}

	pub(crate) async fn io_delete_local(&self, uuid: &UuidStr) -> Result<(), io::Error> {
		let path = self.cache_dir.join(uuid.as_ref());
		if let Err(e) = match tokio::fs::metadata(&path).await {
			Ok(meta) => {
				if meta.is_dir() {
					tokio::fs::remove_dir_all(&path).await
				} else if meta.is_file() || meta.is_symlink() {
					tokio::fs::remove_file(&path).await
				} else {
					log::warn!(
						"Path {} is neither file nor directory, cannot delete",
						path.display()
					);
					Ok(())
				}
			}
			Err(e) => Err(e),
		} && e.kind() != io::ErrorKind::NotFound
		{
			return Err(e);
		}
		Ok(())
	}
}

async fn remove_dir_all_if_exists(path: &Path) {
	match tokio::fs::remove_dir_all(path).await {
		Ok(_) => {}
		Err(e) if e.kind() == io::ErrorKind::NotFound => {}
		Err(e) => {
			error!("Failed to remove directory {}: {}", path.display(), e);
		}
	}
}

async fn cleanup_uuid_dir(auth_state: &AuthCacheState, dir_path: &Path) {
	let Ok(mut dir) = tokio::fs::read_dir(dir_path).await else {
		log::warn!(
			"Tried to clean up directory {}, but it does not exist.",
			dir_path.display()
		);
		return;
	};

	let mut uuids: Vec<(UuidStr, DirEntry)> = Vec::new();

	loop {
		match dir.next_entry().await {
			Ok(Some(entry)) => {
				if let Ok(uuid) = UuidStr::from_str(&entry.file_name().to_string_lossy()) {
					uuids.push((uuid, entry));
				}
			}
			Ok(None) => break,
			Err(e) => {
				error!("Failed to read directory {}: {}", dir_path.display(), e);
				return;
			}
		}
	}

	let Ok(removed_uuid_positions) =
		sql::select_positions_not_in_uuids(&auth_state.conn(), uuids.iter().map(|(uuid, _)| *uuid))
	else {
		error!(
			"Failed to select positions not in uuids for directory {}",
			dir_path.display()
		);
		return;
	};

	let mut futures = FuturesUnordered::new();

	for i in removed_uuid_positions {
		let entry = &uuids[i].1;
		futures.push(async move {
			let path = entry.path();
			match entry.metadata().await {
				Ok(meta) if meta.is_file() => match tokio::fs::remove_file(&path).await {
					Ok(_) => {
						trace!("Removed file: {}", path.display());
					}
					Err(e) if e.kind() == io::ErrorKind::NotFound => {}
					Err(e) => {
						error!("Failed to remove file {}: {}", path.display(), e);
					}
				},
				Ok(_) => match tokio::fs::remove_dir_all(&path).await {
					Ok(_) => {
						trace!("Removed directory: {}", path.display());
					}
					Err(e) if e.kind() == io::ErrorKind::NotFound => {}
					Err(e) => {
						error!("Failed to remove directory {}: {}", path.display(), e);
					}
				},
				Err(e) if e.kind() == io::ErrorKind::NotFound => {}
				Err(e) => {
					error!("Failed to get metadata for {}: {}", path.display(), e);
				}
			}
		});
	}

	while (futures.next().await).is_some() {}
}

async fn process_subdir(
	subdir_entry: tokio::fs::DirEntry,
) -> std::io::Result<Option<(PathBuf, i64, u64)>> {
	// we match on file_type because it's generally free
	let file_type = match subdir_entry.file_type().await {
		Ok(ft) => ft,
		Err(e) => return Err(e),
	};
	let path = subdir_entry.path();
	// if the subfolder is a file or symlink, remove it and return an error
	if file_type.is_file() || file_type.is_symlink() {
		tokio::fs::remove_file(&path).await?;
		return Err(io::Error::new(
			io::ErrorKind::NotADirectory,
			format!("Expected directory but found file: {}", path.display()),
		));
	}

	// then we read the contents of the subfolder
	let mut contents = tokio::fs::read_dir(&path).await?;
	let Some(file_entry) = contents.next_entry().await? else {
		tokio::fs::remove_dir_all(path).await?;
		return Ok(None);
	};
	// we match on file_type first because it's generally free
	let file_type = file_entry.file_type().await?;
	if file_type.is_file() {
		// make sure there is only one file
		if contents.next_entry().await?.is_some() {
			log::warn!(
				"Multiple files found in cache subdirectory {}, removing all",
				path.display()
			);
			tokio::fs::remove_dir_all(path).await?;
			Ok(None)
		} else {
			let meta = file_entry.metadata().await?;
			Ok(Some((
				path,
				//
				meta.atime(),
				FilenMetaExt::size(&meta),
			)))
		}
	} else {
		// if it's not a file, we remove the directory
		tokio::fs::remove_dir_all(path).await?;
		Ok(None)
	}
}

async fn count_cache_files(dir: &Path) -> std::io::Result<Vec<(PathBuf, i64, u64)>> {
	let stream = tokio_stream::wrappers::ReadDirStream::new(tokio::fs::read_dir(dir).await?);

	let results = stream
		.map(|entry| async move { process_subdir(entry?).await })
		.buffer_unordered(128)
		// don't care about Ok(None), means we fixed the issue by removing invalid files
		// we also don't care about NotFound errors, they mean the file was already removed
		.filter_map(|res: Result<Option<_>, std::io::Error>| async {
			if let Err(e) = &res
				&& e.kind() == io::ErrorKind::NotFound
			{
				None
			} else {
				res.transpose()
			}
		})
		.collect::<Vec<_>>()
		.await;

	// we first want to make sure we try to cleanup as many files as possible
	// and only then return an error if there was one
	results.into_iter().collect()
}

async fn count_thumbnail_files(thumbnail_dir: &Path) -> std::io::Result<Vec<(PathBuf, i64, u64)>> {
	let stream =
		tokio_stream::wrappers::ReadDirStream::new(tokio::fs::read_dir(thumbnail_dir).await?);

	let results = stream
		.map(|entry| async move {
			let entry = entry?;
			let path = entry.path();
			let file_type = entry.file_type().await?;

			if file_type.is_file() {
				let meta = entry.metadata().await?;
				let modified = meta.atime();
				let size = FilenMetaExt::size(&meta);
				Ok(Some((path, modified, size)))
			} else if file_type.is_dir() {
				// if it's not a file, we remove the directory
				tokio::fs::remove_dir_all(path).await?;
				Ok(None)
			} else {
				tokio::fs::remove_file(path).await?;
				Ok(None)
			}
		})
		.buffer_unordered(128)
		// don't care about Ok(None), means we fixed the issue by removing invalid files
		// we also don't care about NotFound errors, they mean the file was already removed
		.filter_map(|res: Result<Option<_>, std::io::Error>| async {
			if let Err(e) = &res
				&& e.kind() == io::ErrorKind::NotFound
			{
				None
			} else {
				res.transpose()
			}
		})
		.collect::<Vec<_>>()
		.await;
	// we first want to make sure we try to cleanup as many files as possible
	// and only then return an error if there was one
	results.into_iter().collect()
}

const MIN_CACHED_FILES: usize = 5;

async fn remove_old_files<F>(dir: &Path, size_budget: u64, func: F) -> std::io::Result<u64>
where
	F: AsyncFnOnce(&Path) -> std::io::Result<Vec<(PathBuf, i64, u64)>>,
{
	let mut current_files = func(dir).await?;

	current_files.sort_unstable_by(|a, b| a.1.cmp(&b.1));

	let mut total_size: u64 = current_files.iter().map(|(_, _, size)| *size).sum();

	let mut file_count = current_files.len();

	for (path, _, size) in current_files {
		if total_size < size_budget || file_count < MIN_CACHED_FILES {
			break;
		}
		// remove dir all because we store files in per-file directories
		tokio::fs::remove_dir_all(&path).await?;
		total_size = total_size.saturating_sub(size);
		file_count -= 1;
	}

	Ok(total_size)
}

async fn remove_old_cache_files(cache_dir: &Path, size_budget: u64) {
	if let Err(e) = remove_old_files(cache_dir, size_budget, count_cache_files).await {
		error!(
			"Failed to remove old cache files in {}: {}",
			cache_dir.display(),
			e
		);
	}
}

async fn remove_old_thumbnails(thumbnail_dir: &Path, size_budget: u64) {
	if let Err(e) = remove_old_files(thumbnail_dir, size_budget, count_thumbnail_files).await {
		error!(
			"Failed to remove old thumbnail files in {}: {}",
			thumbnail_dir.display(),
			e
		);
	}
}

impl AuthCacheState {
	pub(crate) async fn should_cleanup(&self) -> bool {
		self.last_cleanup
			.read()
			.await
			.is_none_or(|t| t + AUTH_CLEANUP_INTERVAL <= chrono::Utc::now())
	}

	pub(crate) async fn cleanup_cache(&self) {
		if !self.should_cleanup().await {
			return;
		}

		let res = self.last_cleanup_sem.try_acquire();
		let _perm = match res {
			Ok(perm) => perm,
			Err(_) => {
				// another cleanup is already running
				return;
			}
		};

		futures::join!(
			cleanup_uuid_dir(self, &self.cache_dir),
			cleanup_uuid_dir(self, &self.tmp_dir),
			remove_old_cache_files(&self.cache_dir, self.cache_file_budget,),
			remove_old_thumbnails(&self.thumbnail_dir, self.thumbnail_file_budget,),
			cleanup_uuid_dir(self, &self.thumbnail_dir)
		);

		let mut lock = self.last_cleanup.write().await;
		let now = chrono::Utc::now();
		*lock = Some(now);
		if let Err(e) =
			update_saved_db_state_cache_cleanup_time(self.cache_state_file.as_ref(), now).await
		{
			log::error!("Failed to update cache cleanup time in saved db state: {e}");
		}
	}
}

impl CacheState {
	pub(crate) async fn cleanup_cache_if_necessary(&self) {
		debug!("Cleaning up cache at {}", self.files_dir.display());
		match self.status {
			AuthStatus::Authenticated(ref auth_state) => {
				debug!("Authenticated, cleaning up old files in cache directories");
				auth_state.cleanup_cache().await;
			}
			_ => {
				debug!("Not authenticated, removing all cache directories and database file");
				let (cache_dir, tmp_dir, thumbnail_dir) = get_paths(&self.files_dir);
				let db_file = self.files_dir.join(DB_FILE_NAME);
				futures::join!(
					remove_dir_all_if_exists(&cache_dir),
					remove_dir_all_if_exists(&tmp_dir),
					remove_dir_all_if_exists(&thumbnail_dir),
					async {
						// we can delete the db here because we are not authenticated
						// so we know there is no database connection open
						match tokio::fs::remove_file(&db_file).await {
							Ok(_) => info!("Removed database file: {}", db_file.display()),
							Err(e) if e.kind() == io::ErrorKind::NotFound => {
								info!(
									"Database file not found, nothing to remove: {}",
									db_file.display()
								);
							}
							Err(e) => {
								error!(
									"Failed to remove database file {}: {}",
									db_file.display(),
									e
								);
							}
						}
					}
				);
			}
		};
	}
}

impl FilenMobileCacheState {
	pub(crate) async fn async_launch_cleanup_task(&self) {
		trace!("Launching cleanup task asynchronously");
		let cache = self.async_get_cache_state_owned().await;
		crate::env::get_runtime().spawn(async move {
			cache.cleanup_cache_if_necessary().await;
		});
	}
	pub(crate) fn sync_launch_cleanup_task(&self) {
		trace!("Launching cleanup task synchronously");
		let cache = self.sync_get_cache_state_owned();
		crate::env::get_runtime().spawn(async move {
			cache.cleanup_cache_if_necessary().await;
		});
	}
}

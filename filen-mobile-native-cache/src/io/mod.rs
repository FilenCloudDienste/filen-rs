use std::{
	fs::FileTimes,
	io::{self},
	path::{Path, PathBuf},
	str::FromStr,
	sync::Arc,
	time::{Duration, SystemTime},
};

use crate::{
	auth::{AuthCacheState, AuthStatus, CacheState, DB_FILE_NAME, FilenMobileCacheState},
	ffi::ItemType,
	sql,
	traits::ProgressCallback,
};
use filen_sdk_rs::{
	fs::{
		HasUUID,
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
fn get_file_times(created: SystemTime, modified: SystemTime) -> FileTimes {
	use std::os::windows::fs::FileTimesExt;

	FileTimes::new().set_created(created).set_modified(modified)
}

#[cfg(unix)]
fn get_file_times(_created: SystemTime, modified: SystemTime) -> FileTimes {
	FileTimes::new().set_modified(modified)
}

pub const CACHE_DIR: &str = "cache";
const TMP_DIR: &str = "tmp";
const THUMBNAIL_DIR: &str = "thumbnails";

const BUFFER_SIZE: u64 = 64 * 1024; // 64 KiB
const CALLBACK_INTERVAL: Duration = Duration::from_millis(200);

fn get_paths(files_path: &Path) -> (PathBuf, PathBuf, PathBuf) {
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
	pub async fn download_file_io(
		&self,
		file: &RemoteFile,
		callback: Option<Arc<dyn ProgressCallback>>,
	) -> Result<PathBuf, io::Error> {
		let reader = self.client.get_file_reader(file).compat();
		let uuid = file.uuid().to_string();
		let src = self.tmp_dir.join(&uuid);
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
		let created = file.created().into();
		let modified = file.last_modified().into();
		tokio::task::spawn_blocking(move || {
			os_file
				.set_times(get_file_times(created, modified))
				.map_err(io::Error::other)
		})
		.await??;

		let dst = self.cache_dir.join(&uuid);
		tokio::fs::rename(&src, &dst).await?;
		Ok(dst)
	}

	pub async fn hash_local_file(&self, uuid: &str) -> Result<Option<Sha512Hash>, io::Error> {
		let path = self.cache_dir.join(uuid);
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
		let old_path = self.cache_dir.join(old_uuid);
		let old_file = tokio::fs::File::open(&old_path).await?;
		let file_builder = self.client.make_file_builder(name, &parent_uuid).mime(mime);
		let (file, _) = self
			.inner_upload_file(file_builder, old_file, callback)
			.await?;
		tokio::fs::rename(old_path, self.cache_dir.join(file.uuid().to_string())).await?;
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
		let uuid_str = file_builder.get_uuid().to_string();
		let target_path = self.cache_dir.join(uuid_str);
		let os_file = tokio::fs::OpenOptions::new()
			.read(true)
			.append(true) // only for create
			.create(true)
			.open(&target_path)
			.await?;
		let (file, _) = self.inner_upload_file(file_builder, os_file, None).await?;
		Ok((file, target_path))
	}

	pub(crate) async fn io_delete_local(
		&self,
		uuid: UuidStr,
		item_type: ItemType,
	) -> Result<(), io::Error> {
		let path = self.cache_dir.join(uuid.as_ref());
		if path.try_exists()? {
			match item_type {
				ItemType::File => tokio::fs::remove_file(&path).await,
				ItemType::Dir | ItemType::Root => tokio::fs::remove_dir(&path).await,
			}
		} else {
			Ok(())
		}
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

impl CacheState {
	pub(crate) async fn cleanup_cache(&self) {
		debug!("Cleaning up cache at {}", self.files_dir.display());
		match self.status {
			AuthStatus::Authenticated(ref auth_state) => {
				debug!("Authenticated, cleaning up old files in cache directories");
				futures::join!(
					cleanup_uuid_dir(auth_state, &auth_state.cache_dir),
					cleanup_uuid_dir(auth_state, &auth_state.tmp_dir),
					cleanup_uuid_dir(auth_state, &auth_state.thumbnail_dir)
				);
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
			cache.cleanup_cache().await;
		});
	}
	pub(crate) fn sync_launch_cleanup_task(&self) {
		trace!("Launching cleanup task synchronously");
		let cache = self.sync_get_cache_state_owned();
		crate::env::get_runtime().spawn(async move {
			cache.cleanup_cache().await;
		});
	}
}

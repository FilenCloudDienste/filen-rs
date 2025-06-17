use std::{
	fs::FileTimes,
	io::{self},
	path::{Path, PathBuf},
	sync::Arc,
	time::{Duration, SystemTime},
};

use crate::{PathValues, traits::ProgressCallback};
use chrono::{DateTime, Utc};
use filen_sdk_rs::{
	auth::Client,
	fs::{
		HasUUID,
		dir::RemoteDirectory,
		file::{RemoteFile, traits::HasFileInfo},
	},
};
use filen_types::crypto::Sha512Hash;
use log::debug;
use sha2::Digest;
use tokio::{
	io::{AsyncReadExt, AsyncWriteExt as TokioAsyncWriteExt},
	sync::mpsc::UnboundedReceiver,
};
use tokio_util::compat::{
	FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt,
};
use uuid::Uuid;

#[cfg(windows)]
use std::os::windows::fs::{FileTimesExt, MetadataExt};
#[cfg(windows)]
fn metadata_size(metadata: std::fs::Metadata) -> u64 {
	metadata.file_size().max(BUFFER_SIZE)
}
#[cfg(windows)]
fn raw_meta_size(metadata: std::fs::Metadata) -> u64 {
	metadata.file_size()
}
#[cfg(windows)]
fn metadata_created(metadata: &std::fs::Metadata) -> SystemTime {
	metadata.created().unwrap_or_else(|_| SystemTime::now())
}
#[cfg(windows)]
fn get_file_times(created: SystemTime, modified: SystemTime) -> FileTimes {
	FileTimes::new().set_created(created).set_modified(modified)
}

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

#[cfg(unix)]
fn metadata_size(metadata: std::fs::Metadata) -> u64 {
	metadata.size().max(BUFFER_SIZE)
}
#[cfg(unix)]
fn raw_meta_size(metadata: std::fs::Metadata) -> u64 {
	metadata.size()
}
#[cfg(unix)]
fn metadata_created(metadata: &std::fs::Metadata) -> SystemTime {
	metadata.modified().unwrap_or_else(|_| SystemTime::now())
}
#[cfg(unix)]
fn get_file_times(_created: SystemTime, modified: SystemTime) -> FileTimes {
	FileTimes::new().set_modified(modified)
}

fn metadata_modified(metadata: &std::fs::Metadata) -> DateTime<Utc> {
	metadata
		.modified()
		.map(DateTime::<Utc>::from)
		.unwrap_or_else(|_| Utc::now())
}

pub const FILES_DIR: &str = "native_cache/downloads";
const TMP_DIR: &str = "native_cache/tmp";

const BUFFER_SIZE: u64 = 64 * 1024; // 64 KiB
const CALLBACK_INTERVAL: Duration = Duration::from_millis(200);

async fn get_tmp_path(files_path: &Path, uuid: &str) -> Result<std::path::PathBuf, io::Error> {
	let tmp_path = files_path.join(TMP_DIR);
	tokio::fs::create_dir_all(&tmp_path).await?;
	Ok(tmp_path.join(uuid))
}

pub async fn get_file_path(
	files_path: &Path,
	path: &PathValues<'_>,
) -> Result<std::path::PathBuf, io::Error> {
	let download_path = files_path.join(FILES_DIR).join(path.full_path);
	// SAFETY: This unwrap is safe because FILES_DIR is a valid path
	tokio::fs::create_dir_all(download_path.parent().unwrap()).await?;
	Ok(download_path)
}

async fn update_task(
	mut receiver: UnboundedReceiver<u64>,
	file_size: u64,
	callback: Arc<dyn ProgressCallback + Send + Sync>,
) {
	callback.init(file_size);
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

pub async fn download_file(
	client: &Client,
	file: &RemoteFile,
	pvs: &PathValues<'_>,
	callback: Arc<dyn ProgressCallback + Send + Sync>,
	files_path: &Path,
) -> Result<PathBuf, io::Error> {
	let reader = client.get_file_reader(file).compat();
	let uuid = file.uuid().to_string();
	let src: std::path::PathBuf = get_tmp_path(files_path, &uuid).await?;
	tokio::io::BufReader::with_capacity(BUFFER_SIZE.min(file.size) as usize, reader);
	let mut os_file = tokio::fs::File::create(&src).await?.compat_write();
	let (sender, receiver) = tokio::sync::mpsc::unbounded_channel::<u64>();
	let file_size = file.size();
	let task = tokio::task::spawn(async move { update_task(receiver, file_size, callback).await });
	client
		.download_file_to_writer(
			file,
			&mut os_file,
			Some(Arc::new(move |bytes_written: u64| {
				let _ = sender.send(bytes_written);
			})),
		)
		.await
		.map_err(|e| io::Error::other(format!("Failed to download file: {}", e)))?;
	let os_file = os_file.into_inner().into_std().await;
	let created = file.created().into();
	let modified = file.last_modified().into();
	tokio::task::spawn_blocking(move || {
		os_file
			.set_times(get_file_times(created, modified))
			.map_err(io::Error::other)
	})
	.await??;

	let dst = get_file_path(files_path, pvs).await?;
	tokio::fs::rename(&src, &dst).await?;
	// don't need to await for the task to finish, as it will run in the background
	// this is for testing for now
	let _ = task.await;
	Ok(dst)
}

pub async fn hash_local_file(
	pvs: &PathValues<'_>,
	files_path: &Path,
) -> Result<Sha512Hash, io::Error> {
	let path = get_file_path(files_path, pvs).await?;
	let mut file = tokio::fs::File::open(path).await?;
	let file_size = file
		.metadata()
		.await
		.map(metadata_size)
		.unwrap_or(BUFFER_SIZE);
	let mut buffer = vec![0; (file_size as usize).min(BUFFER_SIZE as usize)];
	let mut hasher = sha2::Sha512::new();
	loop {
		let bytes_read = file.read(&mut buffer).await?;
		if bytes_read == 0 {
			break;
		}
		hasher.update(&buffer[..bytes_read]);
	}
	let hash = hasher.finalize();
	Ok(hash.into())
}

pub async fn upload_file(
	client: &Client,
	pvs: &PathValues<'_>,
	parent_uuid: Uuid,
	mime: Option<String>,
	callback: Option<Arc<dyn ProgressCallback + Send + Sync>>,
	files_path: &Path,
) -> Result<RemoteFile, io::Error> {
	let path = get_file_path(files_path, pvs).await?;
	debug!("Uploading file at {}", path.display());
	let os_file = tokio::fs::File::open(path).await?;
	let meta = os_file.metadata().await?;
	let mut file = client
		.make_file_builder(pvs.name, &parent_uuid)
		.created(metadata_created(&meta).into())
		.modified(metadata_modified(&meta));
	if let Some(mime) = mime {
		file = file.mime(mime);
	}
	let (sender, receiver) = tokio::sync::mpsc::unbounded_channel::<u64>();
	let reader_callback = if callback.is_some() {
		Some(Arc::new(move |bytes_written: u64| {
			let _ = sender.send(bytes_written);
		}) as Arc<dyn Fn(u64) + Send + Sync>)
	} else {
		None
	};
	let file_size = raw_meta_size(meta);
	let task = tokio::task::spawn(async move {
		if let Some(callback) = callback {
			update_task(receiver, file_size, callback).await;
		}
	});

	let remote_file = client
		.upload_file_from_reader(file.build().into(), &mut os_file.compat(), reader_callback)
		.await
		.map_err(|e| io::Error::other(format!("Failed to upload file: {}", e)))?;
	// don't need to await for the task to finish, as it will run in the background
	// this is for testing for now
	let _ = task.await;
	Ok(remote_file)
}

pub(crate) async fn create_file(
	client: &Client,
	pvs: &PathValues<'_>,
	parent_uuid: Uuid,
	mime: String,
	files_path: &Path,
) -> Result<RemoteFile, io::Error> {
	let target_path = get_file_path(files_path, pvs).await?;
	debug!("Creating file at {}", target_path.display());
	let mut os_file = tokio::fs::File::create(target_path).await?;
	os_file.flush().await?;
	drop(os_file);
	upload_file(client, pvs, parent_uuid, Some(mime), None, files_path).await
}

pub(crate) async fn create_dir(
	client: &Client,
	pvs: &PathValues<'_>,
	parent_uuid: Uuid,
	name: String,
	files_path: &Path,
) -> Result<RemoteDirectory, io::Error> {
	let remote_dir = client
		.create_dir(&parent_uuid, name)
		.await
		.map_err(|e| io::Error::other(format!("Failed to create directory: {}", e)))?;

	tokio::fs::create_dir_all(get_file_path(files_path, pvs).await?).await?;
	Ok(remote_dir)
}

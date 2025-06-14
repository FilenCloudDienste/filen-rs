use std::{
	fs::FileTimes,
	io::{self},
	path::{Path, PathBuf},
	time::SystemTime,
};

use crate::PathValues;
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
use futures::AsyncWriteExt;
use sha2::Digest;
use tokio::io::{AsyncReadExt, AsyncWriteExt as TokioAsyncWriteExt};
use tokio_util::compat::{FuturesAsyncReadCompatExt, FuturesAsyncWriteCompatExt};
use uuid::Uuid;

#[cfg(windows)]
use std::os::windows::fs::{FileTimesExt, MetadataExt};
#[cfg(windows)]
fn metadata_size(metadata: std::fs::Metadata) -> u64 {
	metadata.file_size().max(BUFFER_SIZE)
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
fn get_file_times(_created: SystemTime, modified: SystemTime) -> FileTimes {
	FileTimes::new().set_modified(modified)
}

fn metadata_modified(metadata: &std::fs::Metadata) -> DateTime<Utc> {
	metadata
		.modified()
		.map(DateTime::<Utc>::from)
		.unwrap_or_else(|_| Utc::now())
}

const FILES_DIR: &str = "native_cache/downloads";
const TMP_DIR: &str = "native_cache/tmp";

const BUFFER_SIZE: u64 = 64 * 1024; // 64 KiB

async fn get_tmp_path(uuid: &str) -> Result<std::path::PathBuf, io::Error> {
	tokio::fs::create_dir_all(TMP_DIR).await?;
	let tmp_path = AsRef::<Path>::as_ref(TMP_DIR).join(uuid);
	Ok(tmp_path)
}

pub async fn get_file_path(path: &PathValues<'_>) -> Result<std::path::PathBuf, io::Error> {
	let download_path = AsRef::<Path>::as_ref(FILES_DIR).join(path.full_path);
	// SAFETY: This unwrap is safe because FILES_DIR is a valid path
	tokio::fs::create_dir_all(download_path.parent().unwrap()).await?;
	Ok(download_path)
}

pub async fn download_file(
	client: &Client,
	file: &RemoteFile,
	pvs: &PathValues<'_>,
) -> Result<PathBuf, io::Error> {
	let reader = client.get_file_reader(file).compat();
	let uuid = file.uuid().to_string();
	let src: std::path::PathBuf = get_tmp_path(&uuid).await?;
	let mut buf_reader =
		tokio::io::BufReader::with_capacity(BUFFER_SIZE.min(file.size) as usize, reader);
	let mut os_file: tokio::fs::File = tokio::fs::File::create(&src).await?;
	tokio::io::copy_buf(&mut buf_reader, &mut os_file).await?;
	os_file.flush().await?;
	let os_file = os_file.into_std().await;
	let created = file.created().into();
	let modified = file.last_modified().into();
	tokio::task::spawn_blocking(move || {
		os_file
			.set_times(get_file_times(created, modified))
			.map_err(io::Error::other)
	})
	.await??;

	let dst = get_file_path(pvs).await?;
	tokio::fs::rename(&src, &dst).await?;
	Ok(dst)
}

pub async fn hash_local_file(pvs: &PathValues<'_>) -> Result<Sha512Hash, io::Error> {
	let path = get_file_path(pvs).await?;
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
) -> Result<RemoteFile, io::Error> {
	let path = get_file_path(pvs).await?;
	let os_file = tokio::fs::File::open(path).await?;
	let meta = os_file.metadata().await?;
	let mut file = client
		.make_file_builder(pvs.name, &parent_uuid)
		.created(meta.created()?.into())
		.modified(metadata_modified(&meta));
	if let Some(mime) = mime {
		file = file.mime(mime);
	}
	let writer = client
		.get_file_writer(file.build())
		.map_err(|e| io::Error::new(io::ErrorKind::InvalidFilename, e))?;
	let mut compat_writer = writer.compat_write();
	let file_size = metadata_size(meta);

	let mut buf_reader = tokio::io::BufReader::with_capacity(
		BUFFER_SIZE.min(file_size.min(BUFFER_SIZE)) as usize,
		os_file,
	);

	tokio::io::copy_buf(&mut buf_reader, &mut compat_writer).await?;
	compat_writer.flush().await?;
	let mut writer = compat_writer.into_inner();
	writer.close().await?;
	let remote_file = writer
		.into_remote_file()
		.ok_or_else(|| io::Error::other("Failed to convert writer into remote file"))?;
	Ok(remote_file)
}

pub(crate) async fn create_file(
	client: &Client,
	pvs: &PathValues<'_>,
	parent_uuid: Uuid,
	mime: String,
) -> Result<RemoteFile, io::Error> {
	let target_path = get_file_path(pvs).await?;
	let mut os_file = tokio::fs::File::create(target_path).await?;
	os_file.flush().await?;
	drop(os_file);
	upload_file(client, pvs, parent_uuid, Some(mime)).await
}

pub(crate) async fn create_dir(
	client: &Client,
	pvs: &PathValues<'_>,
	parent_uuid: Uuid,
	name: String,
) -> Result<RemoteDirectory, io::Error> {
	let remote_dir = client
		.create_dir(&parent_uuid, name)
		.await
		.map_err(|e| io::Error::other(format!("Failed to create directory: {}", e)))?;

	tokio::fs::create_dir_all(get_file_path(pvs).await?).await?;
	Ok(remote_dir)
}

use std::{
	fs::FileTimes,
	io::{self, Read},
	path::{Path, PathBuf},
	time::SystemTime,
};

use chrono::{DateTime, Utc};
use filen_sdk_rs::{
	auth::Client,
	fs::{
		HasUUID,
		file::{RemoteFile, traits::HasFileInfo},
	},
};
use filen_types::crypto::Sha512Hash;
use futures::AsyncWriteExt;
use sha2::Digest;
use tokio::io::AsyncWriteExt as TokioAsyncWriteExt;
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

use crate::ffi::FfiPathWithRoot;
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

fn get_tmp_path(uuid: &str) -> Result<std::path::PathBuf, io::Error> {
	std::fs::create_dir_all(TMP_DIR)?;
	let tmp_path = AsRef::<Path>::as_ref(TMP_DIR).join(uuid);
	Ok(tmp_path)
}

pub fn get_file_path(path: &FfiPathWithRoot) -> Result<std::path::PathBuf, io::Error> {
	let download_path = AsRef::<Path>::as_ref(FILES_DIR).join(&path.0);
	// SAFETY: This unwrap is safe because FILES_DIR is a valid path
	std::fs::create_dir_all(download_path.parent().unwrap())?;
	Ok(download_path)
}

pub async fn download_file(
	client: &Client,
	file: &RemoteFile,
	path: &FfiPathWithRoot,
) -> Result<PathBuf, io::Error> {
	let reader = client.get_file_reader(file).compat();
	let uuid = file.uuid().to_string();
	let src: std::path::PathBuf = get_tmp_path(&uuid)?;
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

	let dst = get_file_path(path)?;
	tokio::fs::rename(&src, &dst).await?;
	Ok(dst)
}

pub fn hash_local_file(path: &FfiPathWithRoot) -> Result<Sha512Hash, io::Error> {
	let path = get_file_path(path)?;
	let mut file = std::fs::File::open(path)?;
	let file_size = file.metadata().map(metadata_size).unwrap_or(BUFFER_SIZE);
	let mut buffer = vec![0; (file_size as usize).min(BUFFER_SIZE as usize)];
	let mut hasher = sha2::Sha512::new();
	loop {
		let bytes_read = file.read(&mut buffer)?;
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
	path: &FfiPathWithRoot,
	parent_uuid: Uuid,
) -> Result<RemoteFile, io::Error> {
	let path = get_file_path(path)?;
	let file_name = path
		.file_name()
		.and_then(|s| s.to_str())
		.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Invalid file name"))?
		.to_string();
	let os_file = tokio::fs::File::open(path).await?;
	let meta = os_file.metadata().await?;
	let file = client
		.make_file_builder(file_name, &parent_uuid)
		.created(meta.created()?.into())
		.modified(metadata_modified(&meta));
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

// pub(crate) async fn create_file(
// 	db: &FilenMobileDB,
// 	client: &Client,
// 	file_path: &str,
// 	parent_dir: &DBDirObject,
// ) -> Result<DBFile, io::Error> {
// 	let target_path = AsRef::<Path>::as_ref(&FILES_DIR).join(file_path);
// 	let parent_path = target_path.parent().unwrap_or_else(|| Path::new(FILES_DIR));
// 	std::fs::create_dir_all(parent_path)?;

// 	let file_name = target_path
// 		.file_name()
// 		.and_then(|s| s.to_str())
// 		.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Invalid file name"))?
// 		.to_string();

// 	std::fs::File::create(target_path)?;

// 	let file = client
// 		.make_file_builder(file_name, &parent_dir.uuid())
// 		.build();

// 	let mut writer = client.get_file_writer(file);
// 	writer.close().await?;
// 	let remote_file = writer
// 		.into_remote_file()
// 		.ok_or_else(|| io::Error::other("Failed to convert writer into remote file"))?;

// 	let file = DBFile::upsert_from_remote(&mut db.conn(), remote_file).map_err(io::Error::other)?;

// 	Ok(file)
// }

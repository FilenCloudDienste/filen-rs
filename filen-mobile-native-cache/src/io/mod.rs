use std::{
	io,
	path::{Path, PathBuf},
};

use filen_sdk_rs::{
	auth::Client,
	fs::{
		HasUUID,
		file::{BaseFile, RemoteFile},
	},
};
use filen_types::crypto::Sha512Hash;
use futures::{AsyncWriteExt, try_join};
use sha2::Digest;
use tokio::io::{AsyncReadExt, AsyncWriteExt as TokioAsyncWriteExt};
use tokio_util::compat::{FuturesAsyncReadCompatExt, FuturesAsyncWriteCompatExt};

#[cfg(windows)]
use std::os::windows::fs::MetadataExt;
#[cfg(windows)]
fn metadata_size(metadata: std::fs::Metadata) -> u64 {
	metadata.file_size().max(BUFFER_SIZE)
}

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
fn metadata_size(metadata: std::fs::Metadata) -> u64 {
	metadata.size().max(BUFFER_SIZE)
}

const DOWNLOAD_FILES_DIR: &str = "native_cache/downloads";
const TMP_DIR: &str = "native_cache/tmp";

const BUFFER_SIZE: u64 = 64 * 1024; // 64 KiB

static ONCE_CELL: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

async fn ensure_dirs_exist() -> Result<(), io::Error> {
	try_join!(
		tokio::fs::create_dir_all(DOWNLOAD_FILES_DIR),
		tokio::fs::create_dir_all(TMP_DIR)
	)?;
	Ok(())
}

async fn get_tmp_path(name: &str) -> Result<std::path::PathBuf, io::Error> {
	ONCE_CELL
		.get_or_try_init(|| async { ensure_dirs_exist().await })
		.await?;
	let tmp_path = AsRef::<Path>::as_ref(&TMP_DIR).join(name);
	Ok(tmp_path)
}

async fn get_download_path(name: &str) -> Result<std::path::PathBuf, io::Error> {
	ONCE_CELL
		.get_or_try_init(|| async { ensure_dirs_exist().await })
		.await?;
	let download_path = AsRef::<Path>::as_ref(&DOWNLOAD_FILES_DIR).join(name);
	Ok(download_path)
}

pub async fn download_file(
	client: &Client,
	file: &filen_sdk_rs::fs::file::RemoteFile,
) -> Result<PathBuf, io::Error> {
	let reader = client.get_file_reader(file).compat();
	let uuid = file.uuid().to_string();
	let src: std::path::PathBuf = get_tmp_path(&uuid).await?;
	let mut buf_reader =
		tokio::io::BufReader::with_capacity(BUFFER_SIZE.min(file.size) as usize, reader);
	let mut writer: tokio::fs::File = tokio::fs::File::create(&src).await?;
	tokio::io::copy_buf(&mut buf_reader, &mut writer).await?;
	writer.flush().await?;

	let dst = get_download_path(&uuid).await?;
	tokio::fs::rename(&src, &dst).await?;
	Ok(dst)
}

pub async fn hash_local_file(file: &RemoteFile) -> Result<Sha512Hash, io::Error> {
	let path = get_download_path(&file.uuid().to_string()).await?;
	let mut file = tokio::fs::File::open(&path).await?;
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

pub async fn upload_file(client: &Client, file: RemoteFile) -> Result<RemoteFile, io::Error> {
	let path = get_download_path(&file.uuid().to_string()).await?;
	let os_file = tokio::fs::File::open(&path).await?;

	let base_file: BaseFile = file.into();
	let writer = client.get_file_writer(base_file);
	let mut compat_writer = writer.compat_write();
	let file_size = os_file
		.metadata()
		.await
		.map(metadata_size)
		.unwrap_or(BUFFER_SIZE);

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

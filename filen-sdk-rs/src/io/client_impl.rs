#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use std::ops::Deref;
use std::sync::{Arc, atomic::Ordering};

use futures::{AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{
	Error,
	auth::Client,
	fs::file::{BaseFile, RemoteFile, traits::File},
	util::MaybeSendCallback,
};
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use crate::{fs::dir::UnsharedDirectoryType, io::dir_download::DirDownloadCallback};

const IO_BUFFER_SIZE: usize = 1024 * 64; // 64 KiB

impl Client {
	pub async fn download_file_to_writer<'a, T>(
		&'a self,
		file: &'a dyn File,
		writer: &mut T,
		callback: Option<MaybeSendCallback<'a, u64>>,
	) -> Result<(), Error>
	where
		T: 'a + AsyncWrite + Unpin,
	{
		self.download_file_to_writer_for_range(file, writer, callback, 0, file.size())
			.await
	}

	pub async fn download_file_to_writer_for_range<'a, T>(
		&'a self,
		file: &'a dyn File,
		writer: &mut T,
		callback: Option<MaybeSendCallback<'a, u64>>,
		start: u64,
		end: u64,
	) -> Result<(), Error>
	where
		T: 'a + AsyncWrite + Unpin,
	{
		let mut reader = self.get_file_reader_for_range(file, start, end);
		let mut buffer = [0u8; IO_BUFFER_SIZE];
		loop {
			let bytes_read = reader.read(&mut buffer).await?;
			if bytes_read == 0 {
				break;
			}
			writer.write_all(&buffer[..bytes_read]).await?;
			if let Some(callback) = &callback {
				callback(bytes_read as u64);
			}
		}
		writer.close().await?;
		Ok(())
	}

	// this could be optimized to avoid allocations by downloading directly to the writer
	// would need to allocate a buffer of file.size() + FILE_CHUNK_SIZE_EXTRA
	// and download to it sequentially, decrypting in place
	// and finally shrinking the buffer to file.size()
	pub async fn download_file(&self, file: &dyn File) -> Result<Vec<u8>, Error> {
		let mut writer = Vec::with_capacity(file.size() as usize);
		self.download_file_to_writer(file, &mut writer, None)
			.await?;
		Ok(writer)
	}

	pub async fn upload_file_from_reader<'a, T>(
		&'a self,
		base_file: Arc<BaseFile>,
		reader: &mut T,
		callback: Option<MaybeSendCallback<'a, u64>>,
		known_size: Option<u64>,
	) -> Result<RemoteFile, Error>
	where
		T: 'a + AsyncReadExt + Unpin,
	{
		let mut writer = self.inner_get_file_writer(base_file, callback, known_size)?;
		let mut buffer = [0u8; IO_BUFFER_SIZE];
		loop {
			let bytes_read = reader.read(&mut buffer).await?;
			if bytes_read == 0 {
				break;
			}
			writer.write_all(&buffer[..bytes_read]).await?;
		}
		writer.close().await?;
		// SAFETY: conversion will always succeed because we called close on the writer
		Ok(writer.into_remote_file().unwrap())
	}

	pub async fn upload_file(&self, file: Arc<BaseFile>, data: &[u8]) -> Result<RemoteFile, Error> {
		let mut reader = data;
		self.upload_file_from_reader(
			file,
			&mut reader,
			None,
			Some(data.len().try_into().unwrap()),
		)
		.await
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	pub async fn upload_dir_recursively<C>(
		self: Arc<Self>,
		dir_path: std::path::PathBuf,
		callback: impl Deref<Target = C>,
		target: &crate::fs::dir::RemoteDirectory,
	) -> Result<(), Error>
	where
		C: super::dir_upload::DirUploadCallback + ?Sized,
	{
		let drop_canceller = AtomicDropCanceller {
			cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
		};
		let ref_callback = callback.deref();

		let (tree, stats) = super::fs_tree::build_fs_tree_from_walkdir_iterator(
			&dir_path,
			&mut |errors| {
				ref_callback.on_scan_errors(errors);
			},
			&mut |dirs, files, bytes| {
				ref_callback.on_scan_progress(dirs, files, bytes);
			},
			&drop_canceller.cancelled,
		)?;

		let (dirs, files, bytes) = stats.snapshot();
		ref_callback.on_scan_complete(dirs, files, bytes);

		self.upload_fs_tree_from_path_into_target(callback, dir_path, &tree, target)
			.await
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	pub async fn download_dir_recursively<C>(
		self: Arc<Self>,
		dir_path: std::path::PathBuf,
		callback: impl Deref<Target = C>,
		target: UnsharedDirectoryType<'_>,
	) -> Result<(), Error>
	where
		C: DirDownloadCallback + ?Sized,
	{
		use filen_types::traits::CowHelpers;

		let drop_canceller = AtomicDropCanceller {
			cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
		};

		let callback_ref = callback.deref();

		let (tree, stats) = super::fs_tree::build_fs_tree_from_remote_iterator(
			Arc::clone(&self),
			target.as_borrowed_cow(),
			&mut |errors| {
				callback_ref.on_scan_errors(errors);
			},
			&mut |dirs, files, bytes| {
				callback_ref.on_scan_progress(dirs, files, bytes);
			},
			&|current_bytes, total_bytes| {
				callback_ref.on_query_download_progress(current_bytes, total_bytes);
			},
			&drop_canceller.cancelled,
		)
		.await?;

		let (dirs, files, bytes) = stats.snapshot();
		callback_ref.on_scan_complete(dirs, files, bytes);

		self.download_fs_tree_from_target_into_path(
			&mut |errors| {
				callback_ref.on_download_errors(errors);
			},
			&mut |downloaded_dirs, downloaded_files, bytes| {
				callback_ref.on_download_update(downloaded_dirs, downloaded_files, bytes);
			},
			dir_path,
			tree,
			target.into_owned_cow(),
		)
		.await
	}
}

struct AtomicDropCanceller {
	cancelled: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for AtomicDropCanceller {
	fn drop(&mut self) {
		self.cancelled.store(true, Ordering::Relaxed);
	}
}

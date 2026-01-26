use std::sync::{Arc, atomic::Ordering};
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use std::{
	ops::Deref,
	path::{Path, PathBuf},
};

use filen_types::crypto::Blake3Hash;
use filen_types::fs::UuidStr;
use futures::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::{
	Error,
	auth::Client,
	consts::CHUNK_SIZE_U64,
	fs::file::{
		BaseFile, FileBuilder, RemoteFile,
		traits::File,
		write::{DummyFuture, FileWriter},
	},
	util::{MaybeSend, MaybeSendCallback},
};
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use crate::{
	ErrorKind,
	error::ErrorExt,
	fs::dir::{HasUUIDContents, UnsharedDirectoryType},
	io::{FilenMetaExt, dir_download::DirDownloadCallback, meta_ext::FileTimesExt},
};

const IO_BUFFER_SIZE: usize = 1024 * 64; // 64 KiB

pub enum UploadInfo<'a> {
	Builder(FileBuilder),
	Parent(&'a UuidStr),
}

impl Client {
	// todo make private, use download_file_to_path instead
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
		let buffer_size =
			std::cmp::min(end.checked_sub(start).unwrap_or_default(), CHUNK_SIZE_U64) as usize;
		// change to BorrowedBuf when `core_io_borrowed_buf` is stabilized
		// https://github.com/rust-lang/rust/issues/117693
		let mut buffer = vec![0u8; buffer_size];
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

	pub async fn inner_upload_file_from_reader<'a, T, F, Fut>(
		&'a self,
		base_file: Arc<BaseFile>,
		reader: &mut T,
		callback: Option<MaybeSendCallback<'a, u64>>,
		known_size: Option<u64>,
		confirm_completion_callback: Option<F>,
	) -> Result<RemoteFile, Error>
	where
		T: 'a + AsyncReadExt + Unpin,
		F: 'a + FnOnce(Blake3Hash, u64) -> Fut + MaybeSend,
		Fut: 'a + Future<Output = Result<(), Error>> + MaybeSend,
		FileWriter<'a, F, Fut>: Unpin,
	{
		let mut writer = self.inner_get_file_writer(
			base_file,
			callback,
			known_size,
			confirm_completion_callback,
		)?;
		let buffer_size = known_size
			.map(|size| std::cmp::min(size, CHUNK_SIZE_U64) as usize)
			.unwrap_or(IO_BUFFER_SIZE);
		// change to BorrowedBuf when `core_io_borrowed_buf` is stabilized
		// https://github.com/rust-lang/rust/issues/117693
		let mut buffer = vec![0u8; buffer_size];
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
		let mut writer = self
			.inner_get_file_writer::<'a, fn(Blake3Hash, u64) -> DummyFuture, DummyFuture>(
				base_file, callback, known_size, None,
			)?;
		let buffer_size = known_size
			.map(|size| std::cmp::min(size, CHUNK_SIZE_U64) as usize)
			.unwrap_or(IO_BUFFER_SIZE);
		// change to BorrowedBuf when `core_io_borrowed_buf` is stabilized
		// https://github.com/rust-lang/rust/issues/117693
		let mut buffer = vec![0u8; buffer_size];
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
		dir_path: PathBuf,
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
		dir_path: PathBuf,
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

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	async fn inner_download_file_to_path(
		&self,
		remote_file: &dyn File,
		path: &Path,
		callback: Option<MaybeSendCallback<'_, u64>>,
	) -> Result<(), Error> {
		let mod_time = match tokio::fs::metadata(path).await {
			Ok(m) => Some(FilenMetaExt::modified(&m)),
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
			Err(e) => return Err(e.into()),
		};

		let parent = path.parent().ok_or_else(|| {
			std::io::Error::new(
				std::io::ErrorKind::InvalidInput,
				"Provided path has no parent directory",
			)
		})?;
		let tmp_path = parent
			.join(remote_file.uuid().as_ref())
			.with_extension("filendl");
		let tmp_file = tokio::fs::OpenOptions::new()
			.create(true)
			.truncate(true)
			.open(&tmp_path)
			.await?;
		let mut writer = tmp_file.compat_write();

		self.download_file_to_writer(remote_file, &mut writer, callback)
			.await?;

		let file_times = FileTimesExt::get_file_times(remote_file);

		let tmp_file = writer.into_inner().into_std().await;
		tokio::task::spawn_blocking(move || tmp_file.set_times(file_times))
			.await
			.unwrap()?;

		// Try and make sure we are not overwriting a file that has changed since we started downloading
		// There's still an unavoidable race condition if the file changes between the metadata check and the rename
		// but there is literally no way to avoid that without an OS-level exclusive file lock or atomic file swap
		// which are not widely supported across platforms
		// This at least covers the common case where the file is modified while we are downloading
		if let Some(mod_time) = mod_time {
			let current_meta = tokio::fs::metadata(&tmp_path).await?;
			let current_mod_time = FilenMetaExt::modified(&current_meta);
			if current_mod_time != mod_time {
				return Err(Error::custom(
					ErrorKind::FileChangedDuringSync,
					format!("File at path {:?} was modified during download", path),
				));
			}
		}

		tokio::fs::rename(&tmp_path, path).await?;
		Ok(())
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	pub(crate) async fn inner_download_to_path_with_hash_check(
		&self,
		remote_file: &dyn File,
		path: PathBuf,
		callback: Option<MaybeSendCallback<'_, u64>>,
	) -> (Result<(), Error>, PathBuf) {
		let size = remote_file.size();
		let hash = remote_file.hash();
		let mtime = remote_file
			.last_modified()
			.unwrap_or_else(|| remote_file.timestamp());
		let (need_download, path) =
			tokio::task::spawn_blocking(move || -> (Result<bool, std::io::Error>, PathBuf) {
				let file = match std::fs::File::open(&path) {
					Ok(f) => f,
					Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Ok(true), path),
					Err(e) => return (Err(e), path),
				};
				let meta = match file.metadata() {
					Ok(m) => m,
					Err(e) => return (Err(e), path),
				};

				if FilenMetaExt::size(&meta) != size {
					return (Ok(true), path);
				}

				if let Some(expected_hash) = hash {
					let mut hasher = blake3::Hasher::new();
					match hasher.update_reader(&file) {
						Ok(_) => {}
						Err(e) => return (Err(e), path),
					};
					let computed_hash: Blake3Hash = hasher.finalize().into();
					(Ok(computed_hash != expected_hash), path)
				} else {
					// fallback to mtime check
					let local_mtime = FilenMetaExt::modified(&meta);
					(Ok(local_mtime < mtime), path)
				}
			})
			.await
			.unwrap();
		let need_download = match need_download {
			Ok(v) => v,
			Err(e) => return (Err(e.into()), path),
		};

		if need_download {
			let res = self
				.inner_download_file_to_path(remote_file, &path, callback)
				.await;
			(res, path)
		} else {
			(Ok(()), path)
		}
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	pub async fn download_file_to_path<'a>(
		&'a self,
		remote_file: &'a dyn File,
		path: PathBuf,
		callback: Option<MaybeSendCallback<'a, u64>>,
	) -> Result<(), Error> {
		let (res, _path) = self
			.inner_download_to_path_with_hash_check(remote_file, path, callback)
			.await;
		res
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	pub async fn upload_file_from_path(
		&self,
		parent: &impl HasUUIDContents,
		callback: Option<MaybeSendCallback<'_, u64>>,
		path: PathBuf,
	) -> Result<(RemoteFile, std::fs::File), Error> {
		self.upload_file_from_path_with_info(UploadInfo::Parent(parent.uuid()), path, callback)
			.await
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	pub async fn upload_file_from_path_with_info<'a>(
		&self,
		info: UploadInfo<'a>,
		path: PathBuf,
		callback: Option<MaybeSendCallback<'_, u64>>,
	) -> Result<(RemoteFile, std::fs::File), Error> {
		let (meta, file, path) = tokio::task::spawn_blocking(|| {
			let file = std::fs::File::open(&path)?;
			let meta = file.metadata()?;
			Ok::<_, std::io::Error>((meta, file, path))
		})
		.await
		.unwrap()?;

		let mut file_builder = match info {
			UploadInfo::Builder(builder) => builder,
			UploadInfo::Parent(parent_uuid) => {
				let name = path
					.file_name()
					.ok_or_else(|| {
						Error::custom(
							ErrorKind::IO,
							format!("Provided path {} has no file name", path.display()),
						)
					})?
					.to_str()
					.ok_or_else(|| {
						Error::custom(
							ErrorKind::IO,
							format!(
								"Provided path {} has invalid UTF-8 in file name",
								path.display()
							),
						)
					})?
					.to_owned();

				self.make_file_builder(name, parent_uuid)
			}
		};

		if file_builder.get_created().is_none() {
			file_builder = file_builder.created(FilenMetaExt::created(&meta));
		}
		if file_builder.get_modified().is_none() {
			file_builder = file_builder.modified(FilenMetaExt::modified(&meta));
		}

		let original_size = FilenMetaExt::size(&meta);

		let mut reader = tokio::fs::File::from_std(file).compat();

		let file = file_builder.build();

		let file = self
			.inner_upload_file_from_reader(
				Arc::new(file),
				&mut reader,
				callback,
				Some(original_size),
				Some(move |_hash, size| async move {
					if original_size != size {
						return Err(Error::custom(
							ErrorKind::FileChangedDuringSync,
							format!("File at path {} was modified during upload", path.display()),
						));
					}
					let res = tokio::task::spawn_blocking(move || (std::fs::metadata(&path), path))
						.await
						.unwrap();
					let (new_meta, path) = match res.0 {
						Ok(meta) => (meta, res.1),
						Err(e) => {
							return Err(e.with_context(format!(
								"File at path {} was modified during upload",
								res.1.display()
							)));
						}
					};
					let modified = FilenMetaExt::modified(&new_meta);
					let expected_modified = FilenMetaExt::modified(&meta);
					if modified != expected_modified {
						return Err(Error::custom(
							ErrorKind::FileChangedDuringSync,
							format!("File at path {} was modified during upload", path.display()),
						));
					}
					Ok(())
				}),
			)
			.await?;
		Ok((file, reader.into_inner().into_std().await))
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

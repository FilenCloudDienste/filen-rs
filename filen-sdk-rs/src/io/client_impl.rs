use std::sync::Arc;
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use std::{
	ops::Deref,
	path::{Path, PathBuf},
};

use filen_types::crypto::Blake3Hash;
use futures::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::{
	Error,
	auth::{Client, shared_client::SharedClient},
	consts::CHUNK_SIZE,
	fs::file::{
		BaseFile, FileBuilder, RemoteFile,
		client_impl::{FileReaderSharedClientExt, build_exif_tee_from_builder},
		exif::ExifTeeState,
		traits::File,
		write::{DummyFuture, FileWriter},
	},
	progress::ThrottledProgress,
	util::{MaybeArc, MaybeSend, MaybeSendCallback},
};
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use crate::{
	ErrorKind,
	error::ErrorExt,
	fs::{
		categories::{DirType, Normal},
		file::FileBuilderOptionalName,
	},
	io::{CanonicalPath, FilenMetaExt, meta_ext::FileTimesExt},
};

const IO_BUFFER_SIZE: usize = 1024 * 64; // 64 KiB

impl Client {
	/// Internal entry point used by `upload_file*`. Takes a `FileBuilder`
	/// (consumed), constructs an EXIF-teeing writer when applicable, then
	/// drives the chunked upload via the unified writer pipeline.
	#[allow(clippy::too_many_arguments)]
	pub(crate) async fn inner_upload_from_builder<'a, T, F, Fut>(
		&'a self,
		builder: FileBuilder,
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
		let (exif_tee, base_file) = build_exif_tee_from_builder(builder);
		let base_file = Arc::new(base_file);

		self.inner_upload_file_from_reader(
			base_file,
			reader,
			callback,
			known_size,
			confirm_completion_callback,
			exif_tee,
		)
		.await
	}

	#[allow(clippy::too_many_arguments)]
	pub(crate) async fn inner_upload_file_from_reader<'a, T, F, Fut>(
		&'a self,
		base_file: Arc<BaseFile>,
		reader: &mut T,
		callback: Option<MaybeSendCallback<'a, u64>>,
		known_size: Option<u64>,
		confirm_completion_callback: Option<F>,
		exif_tee: Option<ExifTeeState>,
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
			exif_tee,
		);
		let buffer_size = known_size
			.map(|size| usize::try_from(size).unwrap_or(usize::MAX).min(CHUNK_SIZE))
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

	#[tracing::instrument(level = "debug", name = "upload_file", skip_all)]
	pub async fn upload_file_from_reader<'a, T>(
		&'a self,
		builder: FileBuilder,
		reader: &mut T,
		callback: Option<MaybeSendCallback<'a, u64>>,
		known_size: Option<u64>,
	) -> Result<RemoteFile, Error>
	where
		T: 'a + AsyncReadExt + Unpin,
	{
		self.inner_upload_from_builder::<T, fn(Blake3Hash, u64) -> DummyFuture, DummyFuture>(
			builder, reader, callback, known_size, None,
		)
		.await
	}

	pub async fn upload_file(
		&self,
		builder: FileBuilder,
		data: &[u8],
	) -> Result<RemoteFile, Error> {
		let mut reader = data;
		self.upload_file_from_reader(
			builder,
			&mut reader,
			None,
			Some(data.len().try_into().unwrap()),
		)
		.await
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[tracing::instrument(name = "upload_dir_recursively", skip_all, fields(dir_path = %dir_path.display()))]
	pub async fn upload_dir_recursively<C>(
		self: Arc<Self>,
		dir_path: PathBuf,
		callback: impl Deref<Target = C>,
		target: &crate::fs::dir::RemoteDirectory,
	) -> Result<(), Error>
	where
		C: super::dir_upload::DirUploadCallback + ?Sized,
	{
		use crate::util::AtomicDropCanceller;

		let drop_canceller = AtomicDropCanceller::default();
		let ref_callback = callback.deref();

		let (tree, stats) = super::fs_tree::build_fs_tree_from_walkdir_iterator(
			&dir_path,
			&mut |errors| {
				ref_callback.on_scan_errors(errors);
			},
			&mut |dirs, files, bytes| {
				ref_callback.on_scan_progress(dirs, files, bytes);
			},
			drop_canceller.cancelled(),
		)?;

		let (dirs, files, bytes) = stats.snapshot();
		ref_callback.on_scan_complete(dirs, files, bytes);

		self.upload_fs_tree_from_path_into_target(callback, dir_path, &tree, target)
			.await
	}

	// #[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	// pub async fn download_dir_recursively<C>(
	// 	self: Arc<Self>,
	// 	dir_path: String,
	// 	callback: impl Deref<Target = C>,
	// 	target: DirectoryType<'_>,
	// ) -> Result<(), Error>
	// where
	// 	C: DirDownloadCallback + ?Sized,
	// {
	// 	use filen_types::traits::CowHelpers;

	// 	use crate::util::AtomicDropCanceller;

	// 	let drop_canceller = AtomicDropCanceller::default();

	// 	let callback_ref = callback.deref();

	// 	let (tree, stats) = super::fs_tree::build_fs_tree_from_remote_iterator(
	// 		&self,
	// 		target.as_borrowed_cow(),
	// 		&mut |errors| {
	// 			callback_ref.on_scan_errors(errors);
	// 		},
	// 		&mut |dirs, files, bytes| {
	// 			callback_ref.on_scan_progress(dirs, files, bytes);
	// 		},
	// 		&|current_bytes, total_bytes| {
	// 			callback_ref.on_query_download_progress(current_bytes, total_bytes);
	// 		},
	// 		drop_canceller.cancelled(),
	// 	)
	// 	.await?;

	// 	let (dirs, files, bytes) = stats.snapshot();
	// 	callback_ref.on_scan_complete(dirs, files, bytes);

	// 	self.download_fs_tree_from_target_into_path(
	// 		&mut |errors| {
	// 			callback_ref.on_download_errors(errors);
	// 		},
	// 		&mut |downloaded_dirs, downloaded_files, bytes| {
	// 			callback_ref.on_download_update(downloaded_dirs, downloaded_files, bytes);
	// 		},
	// 		dir_path,
	// 		tree,
	// 		target.into_owned_cow(),
	// 	)
	// 	.await
	// }

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	pub async fn upload_file_from_path(
		&self,
		parent: &DirType<'_, Normal>,
		path: PathBuf,
		callback: Option<MaybeSendCallback<'_, u64>>,
	) -> Result<(RemoteFile, std::fs::File), Error> {
		use crate::fs::HasUUID;

		self.upload_file_from_path_with_builder(
			FileBuilderOptionalName::new(parent.uuid()),
			path,
			callback,
		)
		.await
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	pub async fn upload_file_from_path_with_builder(
		&self,
		builder: FileBuilderOptionalName,
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

		let mut file_builder = builder.into_builder(
			&|| {
				path.file_name()
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
					})
			},
			self,
		)?;

		if file_builder.get_created().is_none() {
			file_builder = file_builder.created(FilenMetaExt::created(&meta));
		}
		if file_builder.get_modified().is_none() {
			file_builder = file_builder.modified(FilenMetaExt::modified(&meta));
		}

		let original_size = FilenMetaExt::size(&meta);

		let mut reader = tokio::fs::File::from_std(file).compat();

		let uploaded = self
			.inner_upload_from_builder(
				file_builder,
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
		Ok((uploaded, reader.into_inner().into_std().await))
	}
}

/// A temp file written beside its destination, removed on drop unless
/// [`commit`](Self::commit) renamed it onto the destination, so the
/// destination only ever holds a whole file. Without the removal, any early
/// return before the commit (network error, `FileChangedDuringSync`, task
/// cancellation) would leave the partial file behind — inside a sync tree,
/// where the next scan pass, which only filters the quarantine dir, would
/// treat it as a new local file and upload the partial garbage.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub(crate) struct TmpFileGuard {
	path: Option<PathBuf>,
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
impl TmpFileGuard {
	/// Creates `name` in `destination`'s directory, truncating a file an
	/// earlier run left there under that name (a download killed before its
	/// guard ran), and arms its removal.
	pub(crate) async fn create_beside(
		destination: &Path,
		name: &str,
	) -> std::io::Result<(Self, tokio::fs::File)> {
		let parent = destination.parent().ok_or_else(|| {
			std::io::Error::new(
				std::io::ErrorKind::InvalidInput,
				"Provided path has no parent directory",
			)
		})?;
		let path = parent.join(name);
		let file = tokio::fs::OpenOptions::new()
			.write(true)
			.create(true)
			.truncate(true)
			.open(&path)
			.await?;
		Ok((Self { path: Some(path) }, file))
	}

	/// Renames the temp file onto `destination`; from then on dropping the
	/// guard leaves it be. On `Err` the temp file is removed as the guard drops.
	pub(crate) async fn commit(mut self, destination: &Path) -> std::io::Result<()> {
		if let Some(tmp) = &self.path {
			tokio::fs::rename(tmp, destination).await?;
		}
		self.path = None;
		Ok(())
	}
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
impl Drop for TmpFileGuard {
	fn drop(&mut self) {
		if let Some(path) = self.path.take() {
			let _ = std::fs::remove_file(path);
		}
	}
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
async fn inner_download_file_to_path<SC>(
	unauth_client: &SC,
	remote_file: &dyn File,
	path: &CanonicalPath,
	callback: Option<MaybeSendCallback<'_, u64>>,
) -> Result<(), Error>
where
	SC: SharedClient,
{
	let mod_time = match tokio::fs::metadata(path).await {
		Ok(m) => Some(FilenMetaExt::modified(&m)),
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
		Err(e) => return Err(e.into()),
	};

	// Every path out of this function before the commit unlinks the temp file
	// rather than leak it into the sync tree.
	let (tmp_guard, tmp_file) =
		TmpFileGuard::create_beside(path.as_ref(), &format!("{}.filendl", remote_file.uuid()))
			.await?;
	let mut writer = tmp_file.compat_write();

	unauth_client
		.download_file_to_writer(remote_file, &mut writer, callback)
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
		// Re-stat the destination, not the temp file (whose mtime was just set to the remote's), to
		// detect whether something else modified the destination while we downloaded.
		let current_meta = tokio::fs::metadata(path).await?;
		let current_mod_time = FilenMetaExt::modified(&current_meta);
		if current_mod_time != mod_time {
			return Err(Error::custom(
				ErrorKind::FileChangedDuringSync,
				format!("File at path {:?} was modified during download", path),
			));
		}
	}

	tmp_guard.commit(path.as_ref()).await?;
	Ok(())
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tracing::instrument(level = "debug", name = "download_file", skip_all)]
pub(crate) async fn inner_download_to_path_with_hash_check<SC>(
	unauth_client: &SC,
	remote_file: &dyn File,
	path: CanonicalPath,
	callback: Option<MaybeSendCallback<'_, u64>>,
) -> (Result<(), Error>, CanonicalPath)
where
	SC: SharedClient,
{
	let size = remote_file.size();
	let hash = remote_file.hash();
	let mtime = remote_file
		.last_modified()
		.unwrap_or_else(|| remote_file.timestamp());
	let (need_download, path) =
		tokio::task::spawn_blocking(move || -> (Result<bool, std::io::Error>, CanonicalPath) {
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
		let res = inner_download_file_to_path(unauth_client, remote_file, &path, callback).await;
		(res, path)
	} else {
		(Ok(()), path)
	}
}

#[allow(private_bounds, async_fn_in_trait)]
pub trait IoSharedClientExt<'a>: SharedClient {
	// todo make private, use download_file_to_path instead
	async fn download_file_to_writer<'b, T>(
		&'a self,
		file: &'b dyn File,
		writer: &mut T,
		callback: Option<MaybeSendCallback<'b, u64>>,
	) -> Result<(), Error>
	where
		T: 'b + AsyncWrite + Unpin;

	async fn download_file_to_writer_for_range<'b, T>(
		&'b self,
		file: &'b dyn File,
		writer: &mut T,
		callback: Option<MaybeSendCallback<'a, u64>>,
		start: u64,
		end: u64,
	) -> Result<(), Error>
	where
		T: 'b + AsyncWrite + Unpin;

	// this could be optimized to avoid allocations by downloading directly to the writer
	// would need to allocate a buffer of file.size() + FILE_CHUNK_SIZE_EXTRA
	// and download to it sequentially, decrypting in place
	// and finally shrinking the buffer to file.size()
	async fn download_file(&self, file: &dyn File) -> Result<Vec<u8>, Error>;

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	async fn download_file_to_path<'b>(
		&'b self,
		remote_file: &'b dyn File,
		path: &Path,
		callback: Option<MaybeSendCallback<'b, u64>>,
	) -> Result<(), Error>;
}

impl<'a, SC> IoSharedClientExt<'a> for SC
where
	SC: SharedClient,
{
	async fn download_file_to_writer<'b, T>(
		&'a self,
		file: &'b dyn File,
		writer: &mut T,
		callback: Option<MaybeSendCallback<'b, u64>>,
	) -> Result<(), Error>
	where
		T: 'b + AsyncWrite + Unpin,
	{
		self.download_file_to_writer_for_range(file, writer, callback, 0, file.size())
			.await
	}

	async fn download_file_to_writer_for_range<'b, T>(
		&'b self,
		file: &'b dyn File,
		writer: &mut T,
		callback: Option<MaybeSendCallback<'a, u64>>,
		start: u64,
		end: u64,
	) -> Result<(), Error>
	where
		T: 'b + AsyncWrite + Unpin,
	{
		let progress = ThrottledProgress::new(callback);
		// Drive the throttle from chunk-download completion inside the reader (smooth, concurrent)
		// rather than from read-out (bursty: the ordered reader releases head-of-line-blocked chunks
		// together). The throttle still rate-limits the actual callback to one per CALLBACK_INTERVAL.
		let reader_callback = progress
			.clone()
			.map(|p| MaybeArc::new(move |bytes| p.report(bytes)) as MaybeSendCallback<u64>);
		let mut reader =
			self.get_file_reader_for_range_with_callback(file, start, end, reader_callback);
		let buffer_size = usize::try_from(end.saturating_sub(start))
			.unwrap_or(usize::MAX)
			.min(CHUNK_SIZE);
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
		if let Some(progress) = &progress {
			progress.flush();
		}
		writer.close().await?;
		Ok(())
	}

	async fn download_file(&self, file: &dyn File) -> Result<Vec<u8>, Error> {
		let mut writer = Vec::with_capacity(usize::try_from(file.size()).unwrap_or(0));
		self.download_file_to_writer(file, &mut writer, None)
			.await?;
		Ok(writer)
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	async fn download_file_to_path<'b>(
		&'b self,
		remote_file: &'b dyn File,
		path: &Path,
		callback: Option<MaybeSendCallback<'b, u64>>,
	) -> Result<(), Error> {
		let (res, _path) = inner_download_to_path_with_hash_check(
			self,
			remote_file,
			CanonicalPath::new(path).map_err(|e| {
				Error::custom_with_source(
					ErrorKind::IO,
					e,
					Some(format!("Failed to canonicalize path: {}", path.display())),
				)
			})?,
			callback,
		)
		.await;
		res
	}
}

#[cfg(all(test, not(all(target_family = "wasm", target_os = "unknown"))))]
mod tmp_file_guard_tests {
	use std::{ffi::OsString, io::ErrorKind, path::Path};

	use tokio::io::AsyncWriteExt;

	use super::TmpFileGuard;

	#[tokio::test]
	async fn a_temp_file_beside_its_destination_is_removed_unless_committed() {
		let dir = tempfile::tempdir().unwrap();
		let destination = dir.path().join("photo.jpg");
		std::fs::write(&destination, b"old").unwrap();
		let tmp_path = dir.path().join("photo.jpg.part");
		{
			// Dropping the guard uncommitted is every early return mid-download.
			let (_guard, mut file) = TmpFileGuard::create_beside(&destination, "photo.jpg.part")
				.await
				.unwrap();
			file.write_all(b"partial").await.unwrap();
			file.flush().await.unwrap();
			assert!(tmp_path.exists());
		}
		assert!(!tmp_path.exists());
		assert_eq!(std::fs::read(&destination).unwrap(), b"old");
	}

	#[tokio::test]
	async fn a_committed_temp_file_replaces_its_destination() {
		let dir = tempfile::tempdir().unwrap();
		let destination = dir.path().join("photo.jpg");
		std::fs::write(&destination, b"old").unwrap();
		let (guard, mut file) = TmpFileGuard::create_beside(&destination, "photo.jpg.part")
			.await
			.unwrap();
		file.write_all(b"new").await.unwrap();
		file.flush().await.unwrap();
		drop(file);
		guard.commit(&destination).await.unwrap();
		assert_eq!(std::fs::read(&destination).unwrap(), b"new");
		let names = std::fs::read_dir(dir.path())
			.unwrap()
			.map(|entry| entry.unwrap().file_name())
			.collect::<Vec<_>>();
		assert_eq!(names, vec![OsString::from("photo.jpg")]);
	}

	#[tokio::test]
	async fn a_temp_file_left_by_an_earlier_run_is_truncated() {
		let dir = tempfile::tempdir().unwrap();
		let destination = dir.path().join("photo.jpg");
		std::fs::write(dir.path().join("photo.jpg.part"), b"stale").unwrap();
		let (_guard, file) = TmpFileGuard::create_beside(&destination, "photo.jpg.part")
			.await
			.unwrap();
		assert_eq!(file.metadata().await.unwrap().len(), 0);
	}

	#[tokio::test]
	async fn a_destination_without_a_parent_is_refused() {
		let Err(e) = TmpFileGuard::create_beside(Path::new("/"), "root.part").await else {
			panic!("a temp file beside / was created");
		};
		assert_eq!(e.kind(), ErrorKind::InvalidInput);
		assert_eq!(e.to_string(), "Provided path has no parent directory");
	}
}

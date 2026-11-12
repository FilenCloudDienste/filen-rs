use std::sync::Arc;
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use std::{
	future::poll_fn,
	io::{Read, Seek, SeekFrom, Write},
	ops::Deref,
	path::{Path, PathBuf},
	sync::atomic::AtomicBool,
};

use filen_types::crypto::Blake3Hash;
use futures::{AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{
	Error,
	auth::{Client, shared_client::SharedClient},
	fs::file::{
		BaseFile, FileBuilder, RemoteFile,
		client_impl::{FileReaderSharedClientExt, build_exif_tee_from_builder},
		exif::ExifTeeState,
		read::FileReader,
		traits::File,
		write::{DummyFuture, FileWriter},
	},
	progress::ThrottledProgress,
	util::{MaybeArc, MaybeSend, MaybeSendCallback},
};
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use crate::{
	ErrorKind,
	consts::CHUNK_SIZE_U64,
	error::ErrorExt,
	fs::{
		categories::{DirType, Normal},
		file::{FileBuilderOptionalName, chunk::Chunk},
	},
	io::{
		CanonicalPath, FilenMetaExt, PartialDownload,
		meta_ext::FileTimesExt,
		upload_resume::{
			ClientResumeBackend, ResumeCallbacks, ResumeSource, UploadJournalStore, upload_resuming,
		},
	},
	util::AtomicDropCanceller,
};

const IO_BUFFER_SIZE: usize = 1024 * 64; // 64 KiB

/// The buffer a reader-to-writer copy of `len` bytes (unknown when `None`) reads through:
/// [`IO_BUFFER_SIZE`] at most, never a whole chunk. The buffer lives outside the memory budget
/// for the whole copy, including while the copy waits for that budget; the file writer and
/// reader assemble their chunks on their own.
fn copy_buffer_len(len: Option<u64>) -> usize {
	len.and_then(|len| usize::try_from(len).ok())
		.map_or(IO_BUFFER_SIZE, |len| len.min(IO_BUFFER_SIZE))
}

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
		// change to BorrowedBuf when `core_io_borrowed_buf` is stabilized
		// https://github.com/rust-lang/rust/issues/117693
		let mut buffer = vec![0u8; copy_buffer_len(known_size)];
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

	/// Uploads the contents of the local directory `dir_path` into `target`, following the
	/// symlinks inside it, as [`Self::upload_dir_recursively_with_options`] does with the default
	/// [`DirUploadOptions`](super::DirUploadOptions).
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
		self.upload_dir_recursively_with_options(
			dir_path,
			callback,
			target,
			super::DirUploadOptions::default(),
		)
		.await
	}

	/// Uploads the contents of the local directory `dir_path` into `target`, as `options` say.
	/// `dir_path` itself is followed when it is a symlink.
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[tracing::instrument(name = "upload_dir_recursively", skip_all, fields(dir_path = %dir_path.display()))]
	pub async fn upload_dir_recursively_with_options<C>(
		self: Arc<Self>,
		dir_path: PathBuf,
		callback: impl Deref<Target = C>,
		target: &crate::fs::dir::RemoteDirectory,
		options: super::DirUploadOptions,
	) -> Result<(), Error>
	where
		C: super::dir_upload::DirUploadCallback + ?Sized,
	{
		let super::DirUploadOptions {
			symlinks,
			existing,
			filter,
			journal,
		} = options;
		let ref_callback = callback.deref();
		let mut skipped_symlinks = Vec::new();

		// A send fails only once this future has been dropped, which also sets the flag the scan
		// stops on, so nobody is left waiting for the lost event.
		let (dir_path, tree, stats) = scan_off_task(
			move |events, cancelled| {
				super::fs_tree::build_fs_tree_from_walkdir_iterator(
					&dir_path,
					symlinks,
					&filter,
					&mut |errors| {
						let _ = events.send(ScanEvent::Errors(errors));
					},
					&mut |dirs, files, bytes| {
						let _ = events.send(ScanEvent::Progress { dirs, files, bytes });
					},
					&mut |path| {
						let _ = events.send(ScanEvent::SkippedSymlink(path));
					},
					cancelled,
				)
				.map(|(tree, stats)| (dir_path, tree, stats))
			},
			|event| match event {
				ScanEvent::Progress { dirs, files, bytes } => {
					ref_callback.on_scan_progress(dirs, files, bytes);
				}
				ScanEvent::Errors(errors) => ref_callback.on_scan_errors(errors),
				ScanEvent::SkippedSymlink(path) => skipped_symlinks.push(path),
			},
		)
		.await?;

		if !skipped_symlinks.is_empty() {
			ref_callback.on_scan_skipped_symlinks(skipped_symlinks);
		}
		let (dirs, files, bytes) = stats.snapshot();
		ref_callback.on_scan_complete(dirs, files, bytes);

		self.upload_fs_tree_from_path_into_target(
			callback, dir_path, &tree, target, existing, journal,
		)
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
		let (meta, file, path) = open_source(path).await?;
		let file_builder = self.resolve_path_builder(builder, &path, &meta)?;
		let original_size = FilenMetaExt::size(&meta);

		let (exif_tee, base_file) = build_exif_tee_from_builder(file_builder);
		let mut writer = self.inner_get_file_writer(
			Arc::new(base_file),
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
			exif_tee,
		);
		let file = read_file_into_chunks(&mut writer, file, original_size).await?;
		writer.close().await?;
		let uploaded = writer
			.into_remote_file()
			.expect("a closed writer holds the uploaded file (should be impossible)");
		Ok((uploaded, file))
	}

	/// Uploads the file at `path` as
	/// [`upload_file_from_path_with_builder`](Self::upload_file_from_path_with_builder) does, but
	/// continues an earlier, unfinished upload of the same file into the same directory under the
	/// same name, and keeps a journal in `journal` of what this one sent, for a later call to
	/// continue it when it does not finish.
	///
	/// The journal holds the upload's uuid, keys and which chunks the server acknowledged,
	/// encrypted with the account's master keys. An earlier upload is continued only when the
	/// file's size, modification time and (on Unix) inode are unchanged, and when it can be
	/// finished well within about a day of its first chunk, how long the server is assumed to keep
	/// unfinished chunks; otherwise the upload starts over under a new uuid and keys. A file of fewer
	/// than 8 chunks is never journaled. Every byte is read again to hash the file, but only the
	/// chunks not acknowledged are sent. An upload the server already finished, whose answer was
	/// lost, is not sent again: the finished file is returned. If the server refuses to finish a
	/// continued upload because chunks are missing or it no longer has the upload, the upload
	/// starts over once.
	///
	/// `on_resumed` is called once, before anything is sent, with how many bytes an earlier upload
	/// sent that are not sent again, when there are any. `callback` reports the bytes sent, so the
	/// two add up to the file's size; after an upload started over, the bytes sent again count
	/// again. When the server refuses a chunk because the earlier upload was finished after all,
	/// `on_resumed` is called once more, with the bytes this upload did not send.
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	pub async fn upload_file_from_path_resuming(
		&self,
		builder: FileBuilderOptionalName,
		path: PathBuf,
		journal: &dyn UploadJournalStore,
		callback: Option<MaybeSendCallback<'_, u64>>,
		on_resumed: Option<MaybeSendCallback<'_, u64>>,
	) -> Result<(RemoteFile, std::fs::File), Error> {
		let (meta, file, path) = open_source(path).await?;
		let builder = self.resolve_path_builder(builder, &path, &meta)?;
		upload_resuming(
			&ClientResumeBackend { client: self },
			ResumeSource {
				builder,
				file,
				meta,
				path,
			},
			journal,
			ResumeCallbacks {
				callback,
				on_resumed,
			},
		)
		.await
	}

	/// `builder` for the upload of the file at `path`, whose metadata is `meta`: named after the
	/// file unless it has a name, and with the file's created and modified times unless it has
	/// its own.
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	fn resolve_path_builder(
		&self,
		builder: FileBuilderOptionalName,
		path: &Path,
		meta: &std::fs::Metadata,
	) -> Result<FileBuilder, Error> {
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
			file_builder = file_builder.created(FilenMetaExt::created(meta));
		}
		if file_builder.get_modified().is_none() {
			file_builder = file_builder.modified(FilenMetaExt::modified(meta));
		}
		Ok(file_builder)
	}
}

/// Opens the file at `path` and reads its metadata, on a blocking thread.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
async fn open_source(path: PathBuf) -> Result<(std::fs::Metadata, std::fs::File, PathBuf), Error> {
	let (meta, file, path) = tokio::task::spawn_blocking(|| {
		let file = std::fs::File::open(&path)?;
		let meta = file.metadata()?;
		Ok::<_, std::io::Error>((meta, file, path))
	})
	.await
	.unwrap()?;
	Ok((meta, file, path))
}

/// What a local scan running on a blocking thread reports back to the task awaiting it.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
enum ScanEvent {
	Progress {
		dirs: u64,
		files: u64,
		bytes: u64,
	},
	Errors(Vec<Error>),
	/// A symlink the scan left out, by its local path.
	SkippedSymlink(PathBuf),
}

/// Runs `scan` on a blocking thread and hands each event it sends to `on_event` on the awaiting
/// task, so that task stays free to notice a cancellation while a large tree is walked. Dropping
/// the returned future sets the flag `scan` is given; a scan that checks it stops instead of
/// walking on with nobody waiting for the result.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
async fn scan_off_task<T>(
	scan: impl FnOnce(&tokio::sync::mpsc::UnboundedSender<ScanEvent>, &AtomicBool) -> Result<T, Error>
	+ Send
	+ 'static,
	mut on_event: impl FnMut(ScanEvent),
) -> Result<T, Error>
where
	T: Send + 'static,
{
	let drop_canceller = AtomicDropCanceller::default();
	let cancelled = Arc::clone(drop_canceller.cancelled());
	let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
	let scan = tokio::task::spawn_blocking(move || scan(&events, &cancelled));
	// The channel closes when `scan` returns and drops its sender.
	while let Some(event) = received.recv().await {
		on_event(event);
	}
	scan.await.map_err(std::io::Error::from)?
}

/// Reads `file`'s declared `size` bytes straight into `writer`'s chunks, one blocking task per
/// chunk, and returns `file`. No buffer outside the memory budget: an upload waiting for the
/// budget to grant its next chunk holds nothing the size of a chunk.
///
/// A file that ends early leaves the shortfall to the writer's completion check. One that goes on
/// past `size` fails as any reader yielding more than its declared size does.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
async fn read_file_into_chunks<'a, F, Fut>(
	writer: &mut FileWriter<'a, F, Fut>,
	mut file: std::fs::File,
	size: u64,
) -> Result<std::fs::File, Error>
where
	F: 'a + FnOnce(Blake3Hash, u64) -> Fut + MaybeSend,
	Fut: 'a + Future<Output = Result<(), Error>> + MaybeSend,
	FileWriter<'a, F, Fut>: Unpin,
{
	let mut remaining = size;
	while remaining > 0 {
		let chunk = poll_fn(|cx| writer.poll_next_chunk(cx)).await?;
		let len = chunk
			.capacity()
			.min(usize::try_from(remaining).unwrap_or(usize::MAX));
		let (data, permit) = chunk.into_parts();
		let (returned, data) =
			on_blocking_thread(file, data, move |file, data| fill_from(file, data, len)).await?;
		file = returned;
		let filled = data.len();
		writer.submit_chunk(Chunk::from_parts(data, permit))?;
		if filled < len {
			return Ok(file);
		}
		remaining -= filled as u64;
	}
	if size == 0 {
		return Ok(file);
	}
	// the read that finds the end, as any reader's last; a byte there means the file grew
	let (file, past_end) = on_blocking_thread(file, Vec::with_capacity(1), |file, data| {
		fill_from(file, data, 1)
	})
	.await?;
	writer.write_all(&past_end).await?;
	Ok(file)
}

/// Downloads `remote_file` into `file`, writing each decrypted chunk from its own memory on a
/// blocking thread, and returns `file`. No buffer outside the memory budget: a download waiting
/// for the budget to grant its next chunk holds nothing the size of a chunk.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
async fn download_into_file<SC>(
	client: &SC,
	remote_file: &dyn File,
	mut file: std::fs::File,
	start: u64,
	callback: Option<MaybeSendCallback<'_, u64>>,
) -> Result<std::fs::File, Error>
where
	SC: SharedClient,
{
	let (mut reader, progress) =
		reader_with_progress(client, remote_file, start, remote_file.size(), callback);
	while let Some(chunk) = poll_fn(|cx| reader.poll_next_chunk(cx)).await? {
		let (data, permit) = chunk.into_parts();
		let (returned, data) =
			on_blocking_thread(file, data, |file, data| file.write_all(data)).await?;
		file = returned;
		reader.recycle_chunk(Chunk::from_parts(data, permit));
	}
	if let Some(progress) = &progress {
		progress.flush();
	}
	Ok(file)
}

/// Runs `op` over `file` and `data` on a blocking thread, handing both back.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub(super) async fn on_blocking_thread(
	mut file: std::fs::File,
	mut data: Vec<u8>,
	op: impl FnOnce(&mut std::fs::File, &mut Vec<u8>) -> std::io::Result<()> + Send + 'static,
) -> Result<(std::fs::File, Vec<u8>), Error> {
	let (file, data, done) = tokio::task::spawn_blocking(move || {
		let done = op(&mut file, &mut data);
		(file, data, done)
	})
	.await
	.map_err(std::io::Error::from)?;
	done?;
	Ok((file, data))
}

/// Appends what `reader` yields to `data` until `data` holds `len` bytes or `reader` ends,
/// reading into `data`'s own memory.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub(super) fn fill_from(
	reader: &mut impl Read,
	data: &mut Vec<u8>,
	len: usize,
) -> std::io::Result<()> {
	let mut filled = data.len();
	// zeroed first: reading into spare capacity needs `core_io_borrowed_buf`
	// https://github.com/rust-lang/rust/issues/117693
	data.resize(len.max(filled), 0);
	let read = loop {
		let Some(rest) = data.get_mut(filled..).filter(|rest| !rest.is_empty()) else {
			break Ok(());
		};
		match reader.read(rest) {
			Ok(0) => break Ok(()),
			Ok(n) => filled += n,
			Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
			Err(e) => break Err(e),
		}
	};
	data.truncate(filled);
	read
}

/// A temp file written beside its destination, removed on drop unless
/// [`commit`](Self::commit) renamed it onto the destination, so the
/// destination only ever holds a whole file. Without the removal, any early
/// return before the commit (network error, `FileChangedDuringSync`, task
/// cancellation) would leave the partial file behind — inside a sync tree,
/// where the next scan pass, which only filters the quarantine dir, would
/// treat it as a new local file and upload the partial garbage. A guard from
/// [`open_to_resume`](Self::open_to_resume) keeps it instead, for a later
/// download to continue.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub(crate) struct TmpFileGuard {
	path: Option<PathBuf>,
	unfinished: PartialDownload,
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
		Ok((
			Self {
				path: Some(path),
				unfinished: PartialDownload::Discard,
			},
			file,
		))
	}

	/// Opens `name` in `destination`'s directory to continue writing a file of `size` bytes where
	/// an earlier download left it, returning how many bytes it already holds. A file longer than
	/// `size` cannot be the start of it and is emptied. The guard keeps the file when dropped
	/// uncommitted.
	pub(crate) async fn open_to_resume(
		destination: &Path,
		name: &str,
		size: u64,
	) -> std::io::Result<(Self, std::fs::File, u64)> {
		let parent = destination.parent().ok_or_else(|| {
			std::io::Error::new(
				std::io::ErrorKind::InvalidInput,
				"Provided path has no parent directory",
			)
		})?;
		let path = parent.join(name);
		let (path, file, held) = tokio::task::spawn_blocking(move || {
			let mut file = std::fs::OpenOptions::new()
				.write(true)
				.create(true)
				.truncate(false)
				.open(&path)?;
			let mut held = file.metadata()?.len();
			if held > size {
				file.set_len(0)?;
				held = 0;
			}
			file.seek(SeekFrom::Start(held))?;
			Ok::<_, std::io::Error>((path, file, held))
		})
		.await
		.map_err(std::io::Error::from)??;
		Ok((
			Self {
				path: Some(path),
				unfinished: PartialDownload::Resume,
			},
			file,
			held,
		))
	}

	/// Where the temp file is.
	fn path(&self) -> &Path {
		self.path
			.as_deref()
			.expect("an uncommitted guard holds its path (should be impossible)")
	}

	/// Removes the temp file, whatever the guard keeps when dropped.
	pub(crate) fn discard(mut self) {
		self.unfinished = PartialDownload::Discard;
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
			match self.unfinished {
				PartialDownload::Discard => {
					let _ = std::fs::remove_file(path);
				}
				PartialDownload::Resume => {}
			}
		}
	}
}

/// Why a resumed download was refused: the file put together from the bytes an earlier download
/// left and the rest does not hash to what the remote file declares. Its temp file is removed, so
/// the next download starts over.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[derive(Debug, thiserror::Error)]
#[error("The resumed download of {name} does not match the file's hash")]
pub(crate) struct ResumedDownloadMismatch {
	name: String,
}

/// Whether the file at `path` hashes to `hash`.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
fn has_hash(path: &Path, hash: Blake3Hash) -> std::io::Result<bool> {
	let mut hasher = blake3::Hasher::new();
	hasher.update_reader(std::fs::File::open(path)?)?;
	let computed: Blake3Hash = hasher.finalize().into();
	Ok(computed == hash)
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
async fn inner_download_file_to_path<SC>(
	unauth_client: &SC,
	remote_file: &dyn File,
	path: &CanonicalPath,
	callback: Option<MaybeSendCallback<'_, u64>>,
	partial: PartialDownload,
	on_resumed: Option<MaybeSendCallback<'_, u64>>,
) -> Result<(), Error>
where
	SC: SharedClient,
{
	let mod_time = match tokio::fs::metadata(path).await {
		Ok(m) => Some(FilenMetaExt::modified(&m)),
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
		Err(e) => return Err(e.into()),
	};

	// Named by the remote file's uuid: a file's contents never change under its uuid (a changed
	// file is a new version with a new uuid), so the bytes a resumed download finds in it are the
	// start of the same file.
	let tmp_name = format!("{}.{}", remote_file.uuid(), super::DOWNLOAD_TMP_EXT);
	// Unless resuming, every path out of this function before the commit unlinks the temp file
	// rather than leak it into the sync tree.
	let (tmp_guard, tmp_file, held) = match partial {
		PartialDownload::Discard => {
			let (guard, file) = TmpFileGuard::create_beside(path.as_ref(), &tmp_name).await?;
			(guard, file.into_std().await, 0)
		}
		PartialDownload::Resume => {
			TmpFileGuard::open_to_resume(path.as_ref(), &tmp_name, remote_file.size()).await?
		}
	};
	// The chunk `held` ends in is downloaded whole, so only the chunks before it are not
	// downloaded again.
	let not_downloaded = held - held % CHUNK_SIZE_U64;
	if not_downloaded > 0
		&& let Some(on_resumed) = on_resumed
	{
		on_resumed(not_downloaded);
	}

	let tmp_file =
		match download_into_file(unauth_client, remote_file, tmp_file, held, callback).await {
			Ok(tmp_file) => tmp_file,
			// Nothing a later call could continue from: the bytes are what fails the check, so a
			// resume would only find the same mismatch, under another error.
			Err(error) if error.kind() == ErrorKind::ContentHashMismatch => {
				tmp_guard.discard();
				return Err(error);
			}
			Err(error) => return Err(error),
		};

	// What an earlier download left could have been changed since; the declared hash vouches for
	// the whole file. Without one, the bytes are taken as they are.
	if held > 0
		&& let Some(hash) = remote_file.hash()
	{
		let tmp_path = tmp_guard.path().to_path_buf();
		let matches = tokio::task::spawn_blocking(move || has_hash(&tmp_path, hash))
			.await
			.map_err(std::io::Error::from)??;
		if !matches {
			tmp_guard.discard();
			return Err(Error::custom_with_source(
				ErrorKind::IO,
				ResumedDownloadMismatch {
					name: remote_file.name().unwrap_or("the file").to_owned(),
				},
				None::<&str>,
			));
		}
	}

	let file_times = FileTimesExt::get_file_times(remote_file);

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

/// What lets a download onto a path leave the local file there instead: a file of any other size
/// is always replaced.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeepLocalWhen {
	/// Its blake3 matches the remote's declared hash or, where the remote declares none, its mtime
	/// is not older than the remote's.
	SameHashOrNotOlder,
	/// Its blake3 matches the remote's declared hash; with no declared hash it is replaced. For a
	/// caller that records the path as holding the remote's bytes afterwards, which an mtime
	/// cannot vouch for: a same-size remote replacement stamped no newer than the local file
	/// would otherwise leave the old bytes in place. The sync engine and
	/// [`IoSharedClientExt::download_file_to_path_unless_same_hash`] are those callers.
	SameHash,
	/// It exists, whatever it holds.
	Always,
}

/// What a download onto a path did with the file there.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalWrite {
	/// Wrote the remote file's contents there.
	Written,
	/// Left the local file as it was, as [`KeepLocalWhen`] allows.
	Kept,
}

/// Whether the file at `path` has to be (re)downloaded to hold `size` bytes hashing to `hash`,
/// stamped `mtime`, under `keep`.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
fn needs_download(
	path: &Path,
	size: u64,
	hash: Option<Blake3Hash>,
	mtime: chrono::DateTime<chrono::Utc>,
	keep: KeepLocalWhen,
) -> std::io::Result<bool> {
	let file = match std::fs::File::open(path) {
		Ok(f) => f,
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
		Err(e) => return Err(e),
	};
	match keep {
		KeepLocalWhen::Always => return Ok(false),
		KeepLocalWhen::SameHashOrNotOlder | KeepLocalWhen::SameHash => {}
	}
	let meta = file.metadata()?;
	if FilenMetaExt::size(&meta) != size {
		return Ok(true);
	}
	match (hash, keep) {
		(Some(expected_hash), _) => {
			let mut hasher = blake3::Hasher::new();
			hasher.update_reader(&file)?;
			let computed_hash: Blake3Hash = hasher.finalize().into();
			Ok(computed_hash != expected_hash)
		}
		(None, KeepLocalWhen::SameHashOrNotOlder) => Ok(FilenMetaExt::modified(&meta) < mtime),
		(None, KeepLocalWhen::SameHash) => Ok(true),
		(None, KeepLocalWhen::Always) => Ok(false),
	}
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tracing::instrument(level = "debug", name = "download_file", skip_all)]
pub(crate) async fn inner_download_to_path_with_hash_check<SC>(
	unauth_client: &SC,
	remote_file: &dyn File,
	path: CanonicalPath,
	callback: Option<MaybeSendCallback<'_, u64>>,
	keep: KeepLocalWhen,
	partial: PartialDownload,
	on_resumed: Option<MaybeSendCallback<'_, u64>>,
) -> (Result<LocalWrite, Error>, CanonicalPath)
where
	SC: SharedClient,
{
	let size = remote_file.size();
	let hash = remote_file.hash();
	let mtime = remote_file
		.last_modified()
		.unwrap_or_else(|| remote_file.timestamp());
	let (need_download, path) = tokio::task::spawn_blocking(move || {
		(needs_download(path.as_ref(), size, hash, mtime, keep), path)
	})
	.await
	.unwrap();
	let need_download = match need_download {
		Ok(v) => v,
		Err(e) => return (Err(e.into()), path),
	};

	if need_download {
		let res = inner_download_file_to_path(
			unauth_client,
			remote_file,
			&path,
			callback,
			partial,
			on_resumed,
		)
		.await;
		(res.map(|()| LocalWrite::Written), path)
	} else {
		(Ok(LocalWrite::Kept), path)
	}
}

/// Downloads `remote_file` to `path` unless the file already there is the same, judged by `keep`.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub(crate) async fn download_to_path<'b, SC>(
	client: &'b SC,
	remote_file: &'b dyn File,
	path: &Path,
	callback: Option<MaybeSendCallback<'b, u64>>,
	keep: KeepLocalWhen,
) -> Result<(), Error>
where
	SC: SharedClient,
{
	download_to_path_keeping(
		client,
		remote_file,
		path,
		callback,
		keep,
		PartialDownload::Discard,
		None,
	)
	.await
}

/// [`download_to_path`], doing with what it wrote of the file as `partial` says when it does not
/// finish, and calling `on_resumed` with the bytes it continues from without downloading them
/// again, if any.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
async fn download_to_path_keeping<'b, SC>(
	client: &'b SC,
	remote_file: &'b dyn File,
	path: &Path,
	callback: Option<MaybeSendCallback<'b, u64>>,
	keep: KeepLocalWhen,
	partial: PartialDownload,
	on_resumed: Option<MaybeSendCallback<'b, u64>>,
) -> Result<(), Error>
where
	SC: SharedClient,
{
	let (res, _path) = inner_download_to_path_with_hash_check(
		client,
		remote_file,
		CanonicalPath::new(path).map_err(|e| {
			Error::custom_with_source(
				ErrorKind::IO,
				e,
				Some(format!("Failed to canonicalize path: {}", path.display())),
			)
		})?,
		callback,
		keep,
		partial,
		on_resumed,
	)
	.await;
	res.map(|_| ())
}

/// A reader over bytes `start..end` of `file`, and the throttled progress it reports to
/// `callback`, which needs a flush once the bytes are written. Progress is driven from each
/// chunk's download inside the reader (smooth, concurrent) rather than from read-out (bursty: the
/// ordered reader releases head-of-line-blocked chunks together). The throttle still rate-limits
/// the actual callback to one per CALLBACK_INTERVAL.
fn reader_with_progress<'a, SC>(
	client: &'a SC,
	file: &'a dyn File,
	start: u64,
	end: u64,
	callback: Option<MaybeSendCallback<'a, u64>>,
) -> (FileReader<'a>, Option<MaybeArc<ThrottledProgress<'a>>>)
where
	SC: SharedClient,
{
	let progress = ThrottledProgress::new(callback);
	let reader_callback = progress
		.clone()
		.map(|p| MaybeArc::new(move |bytes| p.report(bytes)) as MaybeSendCallback<u64>);
	let reader = client.get_file_reader_for_range_with_callback(file, start, end, reader_callback);
	(reader, progress)
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

	/// Downloads the whole file. Fails with [`ErrorKind::ContentHashMismatch`] when its bytes do
	/// not hash to the BLAKE3 its metadata declares, if it declares one.
	// this could be optimized to avoid allocations by downloading directly to the writer
	// would need to allocate a buffer of file.size() + FILE_CHUNK_SIZE_EXTRA
	// and download to it sequentially, decrypting in place
	// and finally shrinking the buffer to file.size()
	async fn download_file(&self, file: &dyn File) -> Result<Vec<u8>, Error>;

	/// Downloads the whole file to `path`, through a temp file beside it that is renamed into
	/// place only once every byte is written. A file whose bytes do not hash to the BLAKE3 its
	/// metadata declares fails with [`ErrorKind::ContentHashMismatch`] and leaves `path` as it was.
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	async fn download_file_to_path<'b>(
		&'b self,
		remote_file: &'b dyn File,
		path: &Path,
		callback: Option<MaybeSendCallback<'b, u64>>,
	) -> Result<(), Error>;

	/// Downloads `remote_file` to `path`, replacing the file already there unless its blake3
	/// matches the hash `remote_file` declares. Unlike
	/// [`download_file_to_path`](Self::download_file_to_path), a remote file that declares no hash
	/// always replaces a local file of the same size, whatever their modification times.
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	async fn download_file_to_path_unless_same_hash<'b>(
		&'b self,
		remote_file: &'b dyn File,
		path: &Path,
		callback: Option<MaybeSendCallback<'b, u64>>,
	) -> Result<(), Error>;

	/// Downloads `remote_file` to `path` as
	/// [`download_file_to_path_unless_same_hash`](Self::download_file_to_path_unless_same_hash)
	/// does, but continues where an earlier, unfinished call for the same file left off, and keeps
	/// what it wrote when it does not finish, to be continued by a later call. The bytes written so
	/// far are kept beside `path` in a temp file named after the file's uuid. A resumed file is
	/// checked against the hash `remote_file` declares, if any, and on a mismatch the temp file is
	/// removed and an error returned, so the next call starts over. So is one read whole, which
	/// fails with [`ErrorKind::ContentHashMismatch`].
	///
	/// `on_resumed` is called once, before anything is downloaded, with how many of the bytes an
	/// earlier call left are not downloaded again, when there are any: those in the whole chunks
	/// before the one the earlier call stopped in, which is downloaded whole. `callback` reports
	/// the bytes downloaded, so the two add up to the file's size.
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	async fn download_file_to_path_resuming<'b>(
		&'b self,
		remote_file: &'b dyn File,
		path: &Path,
		callback: Option<MaybeSendCallback<'b, u64>>,
		on_resumed: Option<MaybeSendCallback<'b, u64>>,
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
		let (mut reader, progress) = reader_with_progress(self, file, start, end, callback);
		// change to BorrowedBuf when `core_io_borrowed_buf` is stabilized
		// https://github.com/rust-lang/rust/issues/117693
		// At least one byte: a read into an empty buffer returns 0 at once, without the reader
		// reaching the end of an empty file, where it checks the file's hash.
		let mut buffer = vec![0u8; copy_buffer_len(Some(end.saturating_sub(start))).max(1)];
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
		download_to_path(
			self,
			remote_file,
			path,
			callback,
			KeepLocalWhen::SameHashOrNotOlder,
		)
		.await
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	async fn download_file_to_path_unless_same_hash<'b>(
		&'b self,
		remote_file: &'b dyn File,
		path: &Path,
		callback: Option<MaybeSendCallback<'b, u64>>,
	) -> Result<(), Error> {
		download_to_path(self, remote_file, path, callback, KeepLocalWhen::SameHash).await
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	async fn download_file_to_path_resuming<'b>(
		&'b self,
		remote_file: &'b dyn File,
		path: &Path,
		callback: Option<MaybeSendCallback<'b, u64>>,
		on_resumed: Option<MaybeSendCallback<'b, u64>>,
	) -> Result<(), Error> {
		download_to_path_keeping(
			self,
			remote_file,
			path,
			callback,
			KeepLocalWhen::SameHash,
			PartialDownload::Resume,
			on_resumed,
		)
		.await
	}
}

#[cfg(all(test, not(all(target_family = "wasm", target_os = "unknown"))))]
mod copy_buffer_tests {
	use filen_types::fs::Uuid;

	use super::{
		IoSharedClientExt, copy_buffer_len, path_transfer_tests::peak_bytes_parked_on_the_budget,
	};
	use crate::{
		ErrorKind,
		auth::{http::ClientConfig, test_support::offline_client, unauth::UnauthClient},
		consts::{CHUNK_SIZE, CHUNK_SIZE_U64},
		fs::drive_job::test_support::stored_file,
	};

	#[test]
	fn a_copy_reads_through_at_most_64_kib() {
		assert_eq!(copy_buffer_len(Some(10 * 1024 * 1024)), 65_536);
		assert_eq!(copy_buffer_len(Some(u64::MAX)), 65_536);
		assert_eq!(copy_buffer_len(None), 65_536);
	}

	#[test]
	fn a_copy_shorter_than_64_kib_reads_through_its_own_length() {
		assert_eq!(copy_buffer_len(Some(100)), 100);
	}

	#[test]
	fn a_reader_upload_waiting_for_the_memory_budget_holds_one_copy_buffer() {
		let client = offline_client();
		let data = vec![7u8; CHUNK_SIZE];
		let mut reader = &data[..];
		let builder = client
			.make_file_builder("payload.bin", Uuid::from_u128(1))
			.unwrap();

		let peak = peak_bytes_parked_on_the_budget(
			client.client().state().memory_semaphore(),
			client.upload_file_from_reader(builder, &mut reader, None, Some(CHUNK_SIZE_U64)),
		);

		assert!(peak < 128 * 1024, "the waiting upload held {peak} bytes");
	}

	#[test]
	fn a_writer_download_waiting_for_the_memory_budget_holds_one_copy_buffer() {
		let client = UnauthClient::from_config(ClientConfig::default()).unwrap();
		let file = stored_file(
			Uuid::from_u128(1),
			Uuid::from_u128(2),
			"payload.bin",
			CHUNK_SIZE_U64,
			1,
			None,
		);
		let mut writer = Vec::new();

		let peak = peak_bytes_parked_on_the_budget(
			client.state().memory_semaphore(),
			client.download_file_to_writer(&file, &mut writer, None),
		);

		assert!(peak < 128 * 1024, "the waiting download held {peak} bytes");
	}

	#[tokio::test]
	async fn a_download_of_an_empty_file_declaring_the_hash_of_other_bytes_fails() {
		let client = UnauthClient::from_config(ClientConfig::default()).unwrap();
		let file = stored_file(
			Uuid::from_u128(1),
			Uuid::from_u128(2),
			"empty.bin",
			0,
			0,
			Some(blake3::hash(b"other bytes").into()),
		);

		let error = client.download_file(&file).await.unwrap_err();

		assert_eq!(error.kind(), ErrorKind::ContentHashMismatch);
	}

	#[tokio::test]
	async fn a_download_of_an_empty_file_declaring_the_hash_of_no_bytes_is_empty() {
		let client = UnauthClient::from_config(ClientConfig::default()).unwrap();
		let file = stored_file(
			Uuid::from_u128(1),
			Uuid::from_u128(2),
			"empty.bin",
			0,
			0,
			Some(blake3::hash(b"").into()),
		);

		assert_eq!(client.download_file(&file).await.unwrap(), Vec::<u8>::new());
	}
}

#[cfg(all(test, not(all(target_family = "wasm", target_os = "unknown"))))]
mod tmp_file_guard_tests {
	use std::{ffi::OsString, io::ErrorKind, path::Path};

	use tokio::io::AsyncWriteExt;

	use filen_types::crypto::Blake3Hash;

	use super::{TmpFileGuard, has_hash};

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
	async fn a_resumed_temp_file_continues_after_what_it_holds() {
		let dir = tempfile::tempdir().unwrap();
		let destination = dir.path().join("photo.jpg");
		std::fs::write(dir.path().join("photo.jpg.part"), b"held").unwrap();
		let (guard, mut file, held) =
			TmpFileGuard::open_to_resume(&destination, "photo.jpg.part", 10)
				.await
				.unwrap();
		assert_eq!(held, 4);
		std::io::Write::write_all(&mut file, b" more").unwrap();
		drop(file);
		guard.commit(&destination).await.unwrap();
		assert_eq!(std::fs::read(&destination).unwrap(), b"held more");
	}

	#[tokio::test]
	async fn a_resumed_temp_file_longer_than_the_file_starts_over() {
		let dir = tempfile::tempdir().unwrap();
		let destination = dir.path().join("photo.jpg");
		let tmp_path = dir.path().join("photo.jpg.part");
		std::fs::write(&tmp_path, b"far too long").unwrap();
		let (_guard, _file, held) = TmpFileGuard::open_to_resume(&destination, "photo.jpg.part", 5)
			.await
			.unwrap();
		assert_eq!(held, 0);
		assert_eq!(std::fs::metadata(&tmp_path).unwrap().len(), 0);
	}

	#[tokio::test]
	async fn a_resumable_temp_file_is_kept_unless_discarded() {
		let dir = tempfile::tempdir().unwrap();
		let destination = dir.path().join("photo.jpg");
		let tmp_path = dir.path().join("photo.jpg.part");
		{
			let (_guard, mut file, _) =
				TmpFileGuard::open_to_resume(&destination, "photo.jpg.part", 10)
					.await
					.unwrap();
			std::io::Write::write_all(&mut file, b"partial").unwrap();
		}
		assert_eq!(std::fs::read(&tmp_path).unwrap(), b"partial");

		let (guard, _file, held) = TmpFileGuard::open_to_resume(&destination, "photo.jpg.part", 10)
			.await
			.unwrap();
		assert_eq!(held, 7);
		guard.discard();
		assert!(!tmp_path.exists());
		assert!(!destination.exists());
	}

	#[test]
	fn a_file_has_the_hash_of_its_contents_only() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("photo.jpg");
		std::fs::write(&path, b"contents").unwrap();
		let hash: Blake3Hash = blake3::hash(b"contents").into();
		let other: Blake3Hash = blake3::hash(b"other").into();
		assert!(has_hash(&path, hash).unwrap());
		assert!(!has_hash(&path, other).unwrap());
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

#[cfg(all(test, not(all(target_family = "wasm", target_os = "unknown"))))]
mod path_transfer_tests {
	use std::{
		io::{ErrorKind as IoErrorKind, Read, Seek},
		sync::Arc,
		time::Duration,
	};

	use filen_types::fs::Uuid;
	use tempfile::TempDir;
	use tokio::sync::Semaphore;

	use super::{IoSharedClientExt, fill_from, read_file_into_chunks};
	use crate::{
		ErrorKind,
		alloc_meter::peak_bytes,
		auth::{Client, http::ClientConfig, test_support::offline_client, unauth::UnauthClient},
		consts::{CHUNK_SIZE, CHUNK_SIZE_U64},
		fs::{
			drive_job::test_support::stored_file,
			file::{FileBuilderOptionalName, write::FileWriterDefault},
		},
	};

	/// Yields `bytes` at most `step` at a time; past them, fails once with `end` if set.
	struct Trickle<'a> {
		bytes: &'a [u8],
		step: usize,
		end: Option<IoErrorKind>,
	}

	impl Read for Trickle<'_> {
		fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
			if self.bytes.is_empty()
				&& let Some(kind) = self.end.take()
			{
				return Err(kind.into());
			}
			let n = buf.len().min(self.step).min(self.bytes.len());
			let (head, rest) = self.bytes.split_at(n);
			buf[..n].copy_from_slice(head);
			self.bytes = rest;
			Ok(n)
		}
	}

	#[test]
	fn fill_from_fills_in_place_across_short_reads() {
		let mut reader = Trickle {
			bytes: b"0123456789",
			step: 3,
			end: None,
		};
		let mut data = Vec::with_capacity(16);
		let memory = data.as_ptr();

		fill_from(&mut reader, &mut data, 10).unwrap();

		assert_eq!(data, b"0123456789");
		assert_eq!((data.as_ptr(), data.capacity()), (memory, 16));
	}

	#[test]
	fn fill_from_stops_at_the_length_and_leaves_the_rest_unread() {
		let mut reader = Trickle {
			bytes: b"0123456789",
			step: 3,
			end: None,
		};
		let mut data = Vec::with_capacity(16);

		fill_from(&mut reader, &mut data, 4).unwrap();

		assert_eq!(data, b"0123");
		assert_eq!(reader.bytes, b"456789");
	}

	#[test]
	fn fill_from_stops_where_a_shorter_reader_ends() {
		let mut reader = Trickle {
			bytes: b"012",
			step: 2,
			end: None,
		};
		let mut data = Vec::with_capacity(16);

		fill_from(&mut reader, &mut data, 10).unwrap();

		assert_eq!(data, b"012");
	}

	#[test]
	fn fill_from_retries_an_interrupted_read() {
		let mut reader = Trickle {
			bytes: b"012",
			step: 3,
			end: Some(IoErrorKind::Interrupted),
		};
		let mut data = Vec::with_capacity(16);

		fill_from(&mut reader, &mut data, 10).unwrap();

		assert_eq!(data, b"012");
	}

	#[test]
	fn fill_from_keeps_the_bytes_read_before_a_failed_read() {
		let mut reader = Trickle {
			bytes: b"012",
			step: 3,
			end: Some(IoErrorKind::PermissionDenied),
		};
		let mut data = Vec::with_capacity(16);

		let err = fill_from(&mut reader, &mut data, 10).unwrap_err();

		assert_eq!(err.kind(), IoErrorKind::PermissionDenied);
		assert_eq!(data, b"012");
	}

	fn file_holding(contents: &[u8]) -> (TempDir, std::fs::File) {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("payload.bin");
		std::fs::write(&path, contents).unwrap();
		let file = std::fs::File::open(path).unwrap();
		(dir, file)
	}

	fn writer_declaring(client: &Client, size: u64) -> FileWriterDefault<'_> {
		let file = client
			.make_file_builder("payload.bin", Uuid::from_u128(1))
			.unwrap()
			.build();
		client.inner_get_file_writer(Arc::new(file), None, Some(size), None, None)
	}

	#[tokio::test]
	async fn a_zero_byte_upload_reads_nothing() {
		let client = offline_client();
		let (_dir, file) = file_holding(b"written after the stat");
		let mut writer = writer_declaring(&client, 0);

		let mut file = read_file_into_chunks(&mut writer, file, 0).await.unwrap();

		assert_eq!(file.stream_position().unwrap(), 0);
	}

	#[tokio::test]
	async fn a_file_of_exactly_one_chunk_is_read_to_its_end() {
		let client = offline_client();
		let (_dir, file) = file_holding(&vec![7u8; CHUNK_SIZE]);
		let mut writer = writer_declaring(&client, CHUNK_SIZE as u64);

		let mut file = read_file_into_chunks(&mut writer, file, CHUNK_SIZE as u64)
			.await
			.unwrap();

		assert_eq!(file.stream_position().unwrap(), CHUNK_SIZE as u64);
	}

	#[tokio::test]
	async fn a_file_that_grew_past_its_declared_size_fails_the_upload() {
		let client = offline_client();
		let (_dir, file) = file_holding(b"0123456789");
		let mut writer = writer_declaring(&client, 5);

		let err = read_file_into_chunks(&mut writer, file, 5)
			.await
			.unwrap_err();

		assert_eq!(err.kind(), ErrorKind::IO);
		assert_eq!(
			err.downcast_ref::<std::io::Error>()
				.map(std::io::Error::kind),
			Some(IoErrorKind::InvalidData)
		);
		assert_eq!(
			err.inner_message().as_deref(),
			Some("reader produced more bytes than the declared size of 5 bytes")
		);
	}

	/// The most bytes `transfer` holds while it waits for its first chunk: the whole memory `budget`
	/// is held, so the chunk is never granted. The paused clock only runs out once the transfer is
	/// parked on the budget with no disk I/O in flight; everything polled on this thread is metered.
	pub(super) fn peak_bytes_parked_on_the_budget<T>(
		budget: &Semaphore,
		transfer: impl Future<Output = T>,
	) -> u64 {
		let _held = budget
			.try_acquire_many(u32::try_from(budget.available_permits()).unwrap())
			.unwrap();
		let runtime = tokio::runtime::Builder::new_current_thread()
			.enable_time()
			.start_paused(true)
			.build()
			.unwrap();

		let (waited, peak) = peak_bytes(|| {
			runtime
				.block_on(async { tokio::time::timeout(Duration::from_secs(60), transfer).await })
		});

		assert!(
			waited.is_err(),
			"the transfer got past the exhausted budget"
		);
		peak
	}

	#[test]
	fn an_upload_waiting_for_the_memory_budget_holds_no_read_buffer() {
		let client = offline_client();
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("payload.bin");
		std::fs::write(&path, vec![7u8; CHUNK_SIZE]).unwrap();

		let peak = peak_bytes_parked_on_the_budget(
			client.client().state().memory_semaphore(),
			client.upload_file_from_path_with_builder(
				FileBuilderOptionalName::new(Uuid::from_u128(1)),
				path,
				None,
			),
		);

		assert!(peak < 64 * 1024, "the waiting upload held {peak} bytes");
	}

	#[test]
	fn a_download_waiting_for_the_memory_budget_holds_no_write_buffer() {
		let client = UnauthClient::from_config(ClientConfig::default()).unwrap();
		let file = stored_file(
			Uuid::from_u128(1),
			Uuid::from_u128(2),
			"payload.bin",
			CHUNK_SIZE_U64,
			1,
			None,
		);
		let dir = tempfile::tempdir().unwrap();

		let peak = peak_bytes_parked_on_the_budget(
			client.state().memory_semaphore(),
			client.download_file_to_path(&file, &dir.path().join("payload.bin"), None),
		);

		assert!(peak < 64 * 1024, "the waiting download held {peak} bytes");
	}

	#[tokio::test]
	async fn a_resumable_download_that_does_not_match_its_hash_keeps_no_temp_file() {
		let client = UnauthClient::from_config(ClientConfig::default()).unwrap();
		let file = stored_file(
			Uuid::from_u128(1),
			Uuid::from_u128(2),
			"empty.bin",
			0,
			0,
			Some(blake3::hash(b"other bytes").into()),
		);
		let dir = tempfile::tempdir().unwrap();

		let error = client
			.download_file_to_path_resuming(&file, &dir.path().join("empty.bin"), None, None)
			.await
			.unwrap_err();

		assert_eq!(error.kind(), ErrorKind::ContentHashMismatch);
		assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
	}
}

#[cfg(all(test, not(all(target_family = "wasm", target_os = "unknown"))))]
mod needs_download_tests {
	use std::{
		fs::FileTimes,
		path::{Path, PathBuf},
		time::{Duration, SystemTime},
	};

	use chrono::{DateTime, Utc};
	use filen_types::crypto::Blake3Hash;
	use tempfile::TempDir;

	use super::{KeepLocalWhen, needs_download};

	const CONTENTS: &[u8] = b"local bytes";

	fn remote_stamp() -> DateTime<Utc> {
		DateTime::from_timestamp(1_700_000_000, 0).unwrap()
	}

	/// A file holding [`CONTENTS`], stamped a minute after [`remote_stamp`].
	fn newer_local_file() -> (TempDir, PathBuf) {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("a.txt");
		std::fs::write(&path, CONTENTS).unwrap();
		let newer = SystemTime::from(remote_stamp()) + Duration::from_secs(60);
		std::fs::File::options()
			.write(true)
			.open(&path)
			.unwrap()
			.set_times(FileTimes::new().set_modified(newer))
			.unwrap();
		(dir, path)
	}

	fn len() -> u64 {
		u64::try_from(CONTENTS.len()).unwrap()
	}

	fn check(path: &Path, hash: Option<Blake3Hash>, keep: KeepLocalWhen) -> bool {
		needs_download(path, len(), hash, remote_stamp(), keep).unwrap()
	}

	#[test]
	fn a_hashless_remote_of_the_same_size_keeps_a_newer_local_file_on_mtime() {
		let (_dir, path) = newer_local_file();
		assert!(!check(&path, None, KeepLocalWhen::SameHashOrNotOlder));
	}

	#[test]
	fn a_hashless_remote_of_the_same_size_replaces_a_newer_local_file_when_only_a_hash_may_keep_it()
	{
		let (_dir, path) = newer_local_file();
		assert!(check(&path, None, KeepLocalWhen::SameHash));
	}

	#[test]
	fn a_remote_declaring_the_local_files_hash_keeps_it() {
		let (_dir, path) = newer_local_file();
		let hash = Blake3Hash::from(blake3::hash(CONTENTS));
		assert!(!check(&path, Some(hash), KeepLocalWhen::SameHashOrNotOlder));
	}

	#[test]
	fn a_remote_declaring_the_local_files_hash_keeps_it_when_only_a_hash_may_keep_it() {
		let (_dir, path) = newer_local_file();
		let hash = Blake3Hash::from(blake3::hash(CONTENTS));
		assert!(!check(&path, Some(hash), KeepLocalWhen::SameHash));
	}

	#[test]
	fn a_remote_declaring_another_hash_replaces_the_local_file() {
		let (_dir, path) = newer_local_file();
		let hash = Blake3Hash::from(blake3::hash(b"other bytes"));
		assert!(check(&path, Some(hash), KeepLocalWhen::SameHashOrNotOlder));
	}

	#[test]
	fn any_local_file_is_kept_when_it_is_always_kept() {
		let (_dir, path) = newer_local_file();
		assert!(!check(&path, None, KeepLocalWhen::Always));
		let other_hash = Blake3Hash::from(blake3::hash(b"other bytes"));
		assert!(!check(&path, Some(other_hash), KeepLocalWhen::Always));
		assert!(
			!needs_download(
				&path,
				len() + 1,
				Some(other_hash),
				remote_stamp(),
				KeepLocalWhen::Always
			)
			.unwrap()
		);
	}

	#[test]
	fn a_missing_local_file_is_downloaded_even_when_local_files_are_always_kept() {
		let dir = tempfile::tempdir().unwrap();
		assert!(check(
			&dir.path().join("missing"),
			None,
			KeepLocalWhen::Always
		));
	}

	#[test]
	fn a_missing_local_file_is_downloaded() {
		let dir = tempfile::tempdir().unwrap();
		assert!(check(
			&dir.path().join("absent"),
			None,
			KeepLocalWhen::SameHashOrNotOlder
		));
	}
}

#[cfg(all(test, not(all(target_family = "wasm", target_os = "unknown"))))]
mod scan_off_task_tests {
	use std::{
		sync::{atomic::Ordering, mpsc},
		time::Duration,
	};

	use super::{ScanEvent, scan_off_task};
	use crate::{Error, ErrorKind};

	#[tokio::test]
	async fn dropping_the_scan_future_stops_the_blocking_scan() {
		let (started_tx, started_rx) = tokio::sync::oneshot::channel();
		let (stopped_tx, stopped_rx) = mpsc::channel();
		let mut scan = Box::pin(scan_off_task(
			move |_events, cancelled| {
				started_tx.send(()).unwrap();
				while !cancelled.load(Ordering::Relaxed) {
					std::thread::yield_now();
				}
				stopped_tx.send(()).unwrap();
				Err::<(), _>(Error::custom(ErrorKind::Cancelled, "cancelled"))
			},
			|_| {},
		));
		tokio::select! {
			_ = &mut scan => panic!("the scan cannot finish before it is cancelled"),
			started = started_rx => started.unwrap(),
		}
		drop(scan);
		stopped_rx
			.recv_timeout(Duration::from_secs(30))
			.expect("the blocking scan kept running after its future was dropped");
	}

	#[tokio::test]
	async fn the_scans_events_reach_the_awaiting_task_in_order() {
		let mut seen = Vec::new();
		let result = scan_off_task(
			|events, _cancelled| {
				events
					.send(ScanEvent::Progress {
						dirs: 1,
						files: 2,
						bytes: 3,
					})
					.unwrap();
				events
					.send(ScanEvent::Errors(vec![Error::custom(
						ErrorKind::IO,
						"unreadable",
					)]))
					.unwrap();
				Ok(7)
			},
			|event| {
				seen.push(match event {
					ScanEvent::Progress { dirs, files, bytes } => format!("{dirs}/{files}/{bytes}"),
					ScanEvent::Errors(errors) => format!("{} error(s)", errors.len()),
					ScanEvent::SkippedSymlink(path) => format!("skipped {}", path.display()),
				});
			},
		)
		.await
		.unwrap();
		assert_eq!(result, 7);
		assert_eq!(seen, ["1/2/3", "1 error(s)"]);
	}

	#[tokio::test]
	async fn a_scan_error_is_returned() {
		let err = scan_off_task(
			|_events, _cancelled| Err::<(), _>(Error::custom(ErrorKind::IO, "unreadable root")),
			|_| {},
		)
		.await
		.unwrap_err();
		assert_eq!(err.kind(), ErrorKind::IO);
	}
}

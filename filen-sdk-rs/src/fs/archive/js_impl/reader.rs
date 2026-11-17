//! `openArchiveReader` and the `ArchiveReader` handle it returns, for the wasm and uniffi
//! bindings: one open archive whose calls share what it holds (see the core
//! [`ArchiveReader`](CoreReader)).
//!
//! Every call runs on the commander thread, as the other archive calls do, and the reader moves
//! there with it: it holds nothing bound to the thread that opened it. A JS function or foreign
//! callback is only ever called on its own thread, its arguments sent there over the call's
//! ordered channel. `close` gives up what the reader holds at once, for every handle on it;
//! freeing the handle (or the foreign object's release) does the same once no call still runs
//! on it.

use std::sync::{Arc, atomic::Ordering};

use filen_macros::js_type;
use tokio::sync::mpsc::UnboundedSender;

use crate::{
	Error,
	auth::{Client, unauth::UnauthClient},
	fs::{
		archive::{
			ArchiveReader as CoreReader, EntriesReadFailed,
			extract::{self, ArchiveEntry, ArchiveEntryId, ListCallback, ListConfig},
		},
		file::enums::RemoteFileType,
	},
	job::{JobControl, SdkError, sdk_error},
	js::ManagedFuture,
	util::MaybeSend,
};

use super::{ExpansionLimit, ExtractSettings, ListChannel, ListDelivery, Listed, checked_password};

/// An open archive, from `openArchiveReader` or `openNested`. Its calls share what it holds:
/// the index read when it was opened, and the archive's chunks and decoder states read last,
/// within the client's retained budget. `close` gives that up at once; a call made after it
/// fails with `InvalidState`.
#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
#[cfg_attr(feature = "wasm-full", wasm_bindgen::prelude::wasm_bindgen)]
pub struct ArchiveReader {
	inner: CoreReader,
}

impl ArchiveReader {
	fn new(inner: CoreReader) -> Self {
		Self { inner }
	}

	/// The core reader, for a call to take to the commander thread.
	fn reader(&self) -> CoreReader {
		self.inner.clone()
	}
}

/// What an entries pass did, whether it completed, was cancelled or failed.
#[derive(Debug, Clone)]
#[js_type(export, no_deser, no_default)]
pub struct EntriesReadReport {
	/// Entries written whole to their destinations, which were closed.
	pub entries: u64,
	/// Bytes written, in writes that completed.
	pub bytes_written: u64,
	/// Bytes of the archive read, each counted once.
	pub bytes_read: u64,
	/// Every entry written was checked against a checksum the archive has for it: `false`
	/// whenever `error` is set, for a 7z entry whose header lists no CRC-32, and for a tar's
	/// members, which have none of their own.
	pub checked: bool,
	/// Why the pass ended early: kind `Cancelled` when cancelled, or the error that stopped it.
	/// The entry being written then is unverified, and its destination was never closed.
	pub error: Option<SdkError>,
}

impl EntriesReadReport {
	fn new(result: Result<crate::fs::archive::EntriesReadReport, EntriesReadFailed>) -> Self {
		let (report, error) = match result {
			Ok(report) => (report, None),
			Err(failed) => (failed.report, Some(sdk_error(failed.error))),
		};
		Self {
			entries: report.entries,
			bytes_written: report.bytes_written,
			bytes_read: report.bytes_read,
			checked: report.checked && error.is_none(),
			error,
		}
	}
}

/// A listing's settings for a reader, which every call of the reader keeps.
fn reader_config(
	expansion_limit: Option<ExpansionLimit>,
	skip_mac_metadata: Option<bool>,
	password: Option<String>,
) -> Result<ListConfig, Error> {
	let password = checked_password(password)?;
	Ok(ExtractSettings {
		expansion_limit,
		skip_mac_metadata,
		..ExtractSettings::default()
	}
	.into_list_config(password))
}

/// Which client opens an archive.
enum Opener {
	Client(Arc<Client>),
	Unauth(Arc<UnauthClient>),
}

impl Opener {
	/// Opens `archive` as the job of a managed future, its progress going to `sender`.
	async fn open(
		self,
		archive: RemoteFileType<'static>,
		config: ListConfig,
		sender: UnboundedSender<ListDelivery>,
		control: JobControl,
	) -> Result<ArchiveReader, Error> {
		let channel = ListChannel::new(sender);
		let opened = match self {
			Self::Client(client) => {
				client
					.open_archive_reader(archive, config, channel, control)
					.await
			}
			Self::Unauth(client) => {
				client
					.open_archive_reader(archive, config, channel, control)
					.await
			}
		};
		opened
			.map(ArchiveReader::new)
			.map_err(|failed| Error::unshared(failed.error))
	}
}

/// Lists the reader's entries as the job of a managed future, its callbacks going to `sender`.
async fn list_job(
	reader: CoreReader,
	sender: UnboundedSender<ListDelivery>,
	control: JobControl,
) -> Result<Listed, Error> {
	let channel = ListChannel::new(sender);
	let undelivered = Arc::clone(&channel.undelivered);
	let result = reader.list(channel, control).await;
	Ok((result, undelivered.load(Ordering::Relaxed)))
}

/// Opening a nested archive reports nothing: its listing is the reader's own `list`.
struct Unreported;

impl ListCallback for Unreported {
	fn on_entries_batch(&self, _: Vec<ArchiveEntry>) {}

	fn on_update(&self, _: extract::ListUpdate) {}
}

/// Opens the archive `entry` holds as the job of a managed future.
async fn open_nested_job(
	reader: CoreReader,
	entry: ArchiveEntryId,
	password: Option<String>,
	control: JobControl,
) -> Result<ArchiveReader, Error> {
	let password = checked_password(password)?;
	reader
		.open_nested(entry, password, Unreported, control)
		.await
		.map(ArchiveReader::new)
		.map_err(|failed| Error::unshared(failed.error))
}

/// Runs `call` on the commander thread as a job of `managed_future`: its abort signal stops it
/// through the [`JobControl`] it is handed, which pauses it too.
async fn managed<T, F, Fut>(managed_future: ManagedFuture, call: F) -> Result<T, Error>
where
	F: FnOnce(JobControl) -> Fut + Send + 'static,
	Fut: Future<Output = Result<T, Error>> + MaybeSend + 'static,
	T: Send + 'static,
{
	#[cfg(feature = "wasm-full")]
	{
		managed_future.into_js_managed_commander_job(call)?.await
	}
	#[cfg(not(feature = "wasm-full"))]
	{
		managed_future.into_js_managed_commander_job(call).await
	}
}

/// Most of a range read's buffer allocated up front; a longer range grows it as it is read.
#[cfg(feature = "uniffi")]
const RANGE_PREALLOCATED: u64 = 16 << 20;

/// Longest range the uniffi `read_entry_range` returns in memory; a longer one is written to a
/// file through `download_entry` or `read_entries`.
#[cfg(feature = "uniffi")]
const MAX_RANGE_BYTES: u64 = 64 << 20;

#[cfg(feature = "uniffi")]
mod uniffi_bindings {
	use std::{
		path::{Path, PathBuf},
		pin::Pin,
		sync::{Arc, Mutex, PoisonError},
		task::{Context, Poll, ready},
	};

	use filen_types::fs::Uuid;
	use futures::AsyncWrite;
	use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

	use super::{
		ArchiveReader, EntriesReadReport, MAX_RANGE_BYTES, Opener, RANGE_PREALLOCATED, list_job,
		managed, open_nested_job, reader_config,
	};
	use crate::{
		Error, ErrorKind,
		auth::{JsClient, js_impls::UnauthJsClient},
		fs::archive::{
			ArchiveFormat, EntryByteAccess, ReaderDownloadEntryConfig,
			extract::{self, ArchiveEntryId, EntryDownloadCallback},
			js_impl::{
				EntryDownloadChannel, EntryDownloadReport, EntryDownloadUpdate, ListReport,
				uniffi_impl::{
					DownloadArchiveEntryCallback, ListArchiveCallback, ListArchiveConfig,
					deliver_entry_download, deliver_list, download_into,
				},
			},
		},
		js::{AnyFile, ManagedFuture},
		thumbnail::EmbeddedPreviewResult,
	};

	/// Receives an entries pass's progress and the entries it wrote, in the order the pass
	/// made them, before the call returns.
	#[uniffi::export(with_foreign)]
	pub trait ReadEntriesCallback: Send + Sync {
		/// `entry` was written whole to the file at `path`, which was closed: the pass checked
		/// it against its checksum (see the report's `checked`).
		fn on_entry_done(&self, entry: ArchiveEntryId, path: String);
		/// The pass's progress, throttled; the last one comes once the pass ended.
		fn on_update(&self, update: EntryDownloadUpdate);
	}

	/// What `ArchiveReader::download_entry` downloads, and where to.
	#[derive(uniffi::Record)]
	pub struct ArchiveReaderDownloadConfig {
		/// The entry, a file of the archive, from the reader's `list`.
		pub entry: ArchiveEntryId,
		/// The file the entry is written to, in a directory that exists. A file there is
		/// replaced once the whole entry was written, and kept as it was otherwise.
		pub path: String,
		/// As `DownloadArchiveEntryConfig.max_solid_skip`: no default, `None` allows any skip.
		pub max_solid_skip: Option<u64>,
	}

	/// A callback of an entries pass, as the pass made it.
	enum ReadEntriesDelivery {
		Update(extract::EntryDownloadUpdate),
		Done(ArchiveEntryId, String),
	}

	struct ReadEntriesChannel(tokio::sync::mpsc::UnboundedSender<ReadEntriesDelivery>);

	impl EntryDownloadCallback for ReadEntriesChannel {
		fn on_update(&self, update: extract::EntryDownloadUpdate) {
			let _ = self.0.send(ReadEntriesDelivery::Update(update));
		}
	}

	/// The file of the entry a pass is writing, removed when the guard drops with the entry
	/// unfinished.
	#[derive(Default)]
	struct Unfinished(Arc<Mutex<Option<PathBuf>>>);

	impl Drop for Unfinished {
		fn drop(&mut self) {
			let path = self.0.lock().unwrap_or_else(PoisonError::into_inner).take();
			if let Some(path) = path {
				let _ = std::fs::remove_file(path);
			}
		}
	}

	/// The file an entry of a pass is written to, which tells the app once the pass closed it.
	struct EntryFile {
		file: Compat<tokio::fs::File>,
		/// Until closed: what to tell, and the path the pass forgets once it was told.
		announce: Option<(
			ArchiveEntryId,
			PathBuf,
			tokio::sync::mpsc::UnboundedSender<ReadEntriesDelivery>,
		)>,
		writing: Arc<Mutex<Option<PathBuf>>>,
	}

	impl AsyncWrite for EntryFile {
		fn poll_write(
			self: Pin<&mut Self>,
			cx: &mut Context<'_>,
			buf: &[u8],
		) -> Poll<std::io::Result<usize>> {
			Pin::new(&mut self.get_mut().file).poll_write(cx, buf)
		}

		fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
			Pin::new(&mut self.get_mut().file).poll_flush(cx)
		}

		fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
			let this = self.get_mut();
			ready!(Pin::new(&mut this.file).poll_close(cx))?;
			if let Some((entry, path, sender)) = this.announce.take() {
				*this.writing.lock().unwrap_or_else(PoisonError::into_inner) = None;
				let _ = sender.send(ReadEntriesDelivery::Done(
					entry,
					path.to_string_lossy().into_owned(),
				));
			}
			Poll::Ready(Ok(()))
		}
	}

	/// Opens a reader of `archive` through `opener`, as the uniffi `open_archive_reader` calls
	/// do.
	async fn open_reader(
		opener: Opener,
		archive: AnyFile,
		config: ListArchiveConfig,
		password: Option<String>,
		callback: Arc<dyn ListArchiveCallback>,
		managed_future: ManagedFuture,
	) -> Result<Arc<ArchiveReader>, Error> {
		let config = reader_config(config.expansion_limit, config.skip_mac_metadata, password)?;
		let archive = crate::fs::file::enums::RemoteFileType::try_from(archive)?;
		managed_future
			.into_ordered_job(
				move |delivery| deliver_list(callback.as_ref(), delivery),
				move |sender, control| opener.open(archive, config, sender, control),
			)
			.await
			.map(Arc::new)
	}

	#[uniffi::export]
	impl JsClient {
		/// Opens `archive` for the calls of an [`ArchiveReader`], under `config`'s settings,
		/// which every call of the reader keeps: a zip, 7z or RAR has its index read and held
		/// for them, a tar or single compressed file only its head. Opening takes one of the
		/// archive job slots for its own duration and reports its progress to `callback`
		/// (which gets no entries: the reader's `list` lists them). Fails as a listing does: a
		/// 7z or RAR whose index is encrypted needs `password` here.
		pub async fn open_archive_reader(
			&self,
			archive: AnyFile,
			config: ListArchiveConfig,
			password: Option<String>,
			callback: Arc<dyn ListArchiveCallback>,
			managed_future: ManagedFuture,
		) -> Result<Arc<ArchiveReader>, Error> {
			open_reader(
				Opener::Client(self.inner()),
				archive,
				config,
				password,
				callback,
				managed_future,
			)
			.await
		}
	}

	#[uniffi::export]
	impl UnauthJsClient {
		/// `open_archive_reader` without a session: `archive` is a file a public link gives.
		pub async fn open_archive_reader(
			&self,
			archive: AnyFile,
			config: ListArchiveConfig,
			password: Option<String>,
			callback: Arc<dyn ListArchiveCallback>,
			managed_future: ManagedFuture,
		) -> Result<Arc<ArchiveReader>, Error> {
			open_reader(
				Opener::Unauth(self.inner()),
				archive,
				config,
				password,
				callback,
				managed_future,
			)
			.await
		}
	}

	#[uniffi::export]
	impl ArchiveReader {
		/// This reader's own id: a new one each time an archive is opened.
		pub fn id(&self) -> Uuid {
			self.inner.id()
		}

		/// The id of its archive, which the ids of its entries carry.
		pub fn archive_id(&self) -> Uuid {
			self.inner.archive_id()
		}

		/// What the archive is.
		pub fn format(&self) -> Option<ArchiveFormat> {
			self.inner.format()
		}

		/// Whether the reader holds the archive's index right now; one given up for room
		/// another reader needed is read again by the next call that needs it.
		pub fn index_held(&self) -> bool {
			self.inner.index_held()
		}

		/// Gives up what the reader holds, and closes the readers of archives nested in it.
		/// Every later call fails with `InvalidState`, and so does a range read already
		/// running, at its next chunk; a job already running (a listing, a download, a pass)
		/// runs to its end. Never waits on a lock.
		pub fn close(&self) {
			self.inner.close();
		}

		/// Lists the archive's entries as `list_archive` does, from the held index when there
		/// is one, without reading the archive again.
		pub async fn list(
			&self,
			callback: Arc<dyn ListArchiveCallback>,
			managed_future: ManagedFuture,
		) -> Result<ListReport, Error> {
			let reader = self.reader();
			managed_future
				.into_ordered_job(
					move |delivery| deliver_list(callback.as_ref(), delivery),
					move |sender, control| list_job(reader, sender, control),
				)
				.await
				.map(|(result, undelivered)| ListReport::new(result, undelivered))
		}

		/// How `entry`'s bytes can be read: by range (`Mapped`, `Seekable`), only whole
		/// (`Whole`), or only in an ordered pass (`Sequential`).
		pub async fn entry_byte_access(
			&self,
			entry: ArchiveEntryId,
			managed_future: ManagedFuture,
		) -> Result<EntryByteAccess, Error> {
			let reader = self.reader();
			managed(managed_future, move |control| async move {
				reader
					.entry_byte_access(entry, control)
					.await
					.map_err(Error::unshared)
			})
			.await
		}

		/// Bytes `start..end` of `entry`, which has to be `Mapped` or `Seekable`, unchecked
		/// (the entry's checksum covers it whole), at most 64 MiB of it: a longer range is
		/// refused (`InvalidState`), to be written to a file instead. Takes no job slot unless
		/// the reader reads its index again (see the core `ArchiveReader::entry_byte_access`).
		pub async fn read_entry_range(
			&self,
			entry: ArchiveEntryId,
			start: u64,
			end: u64,
			managed_future: ManagedFuture,
		) -> Result<Vec<u8>, Error> {
			let reader = self.reader();
			if end.saturating_sub(start) > MAX_RANGE_BYTES {
				return Err(Error::custom(
					ErrorKind::InvalidState,
					format!(
						"a range of more than {MAX_RANGE_BYTES} bytes is written to a file \
						 (download_entry or read_entries), not returned"
					),
				));
			}
			managed(managed_future, move |control| async move {
				let len = end.saturating_sub(start).min(RANGE_PREALLOCATED);
				let mut read = futures::io::Cursor::new(Vec::with_capacity(
					usize::try_from(len).unwrap_or(usize::MAX),
				));
				reader
					.read_entry_range(entry, start, end, &mut read, control)
					.await
					.map_err(Error::unshared)?;
				Ok(read.into_inner())
			})
			.await
		}

		/// Writes the config's `entry` to the file at its `path` as `download_archive_entry`
		/// does, starting from the held index.
		pub async fn download_entry(
			&self,
			config: ArchiveReaderDownloadConfig,
			callback: Arc<dyn DownloadArchiveEntryCallback>,
			managed_future: ManagedFuture,
		) -> Result<EntryDownloadReport, Error> {
			let reader = self.reader();
			let path = PathBuf::from(config.path);
			let settings = ReaderDownloadEntryConfig {
				max_solid_skip: config.max_solid_skip,
			};
			let entry = config.entry;
			managed_future
				.into_ordered_job(
					move |update| deliver_entry_download(callback.as_ref(), update),
					// the temp file is created in the job: `tokio::fs` needs the SDK's runtime
					move |sender, control| async move {
						download_into(&path, |mut file| async move {
							reader
								.download_entry(
									entry,
									&mut file,
									settings,
									EntryDownloadChannel(sender),
									control,
								)
								.await
						})
						.await
					},
				)
				.await
				.map(EntryDownloadReport::new)
		}

		/// Reads `entries` in one ordered pass, each to a new file in the directory `dir`,
		/// named for it (`callback` gets its path once it was written whole and closed). The
		/// pass takes one of the archive job slots and gives it back once the last entry is
		/// written; `max_solid_skip` bounds what it may decode and throw away to reach them
		/// (`None` allows any). The file of an entry the pass did not finish is removed.
		pub async fn read_entries(
			&self,
			entries: Vec<ArchiveEntryId>,
			dir: String,
			max_solid_skip: Option<u64>,
			callback: Arc<dyn ReadEntriesCallback>,
			managed_future: ManagedFuture,
		) -> Result<EntriesReadReport, Error> {
			let reader = self.reader();
			let dir = PathBuf::from(dir);
			managed_future
				.into_ordered_job(
					move |delivery| match delivery {
						ReadEntriesDelivery::Update(update) => callback.on_update(update.into()),
						ReadEntriesDelivery::Done(entry, path) => {
							callback.on_entry_done(entry, path);
						}
					},
					move |sender, control| async move {
						// the file of an entry the pass did not finish goes, also when the job is
						// dropped
						let writing = Unfinished::default();
						let result = reader
							.read_entries(
								&entries,
								|entry, _| entry_file(&dir, entry, sender.clone(), &writing.0),
								max_solid_skip,
								ReadEntriesChannel(sender.clone()),
								control,
							)
							.await;
						drop(writing);
						Ok(EntriesReadReport::new(result))
					},
				)
				.await
		}

		/// Opens the archive `entry` holds (a `Mapped` or `Seekable` one) as a reader of its
		/// own, which reads it through this one; `password` is the nested archive's.
		pub async fn open_nested(
			&self,
			entry: ArchiveEntryId,
			password: Option<String>,
			managed_future: ManagedFuture,
		) -> Result<Arc<ArchiveReader>, Error> {
			let reader = self.reader();
			managed_future
				.into_js_managed_commander_job(move |control| {
					open_nested_job(reader, entry, password, control)
				})
				.await
				.map(Arc::new)
		}

		/// Writes the embedded preview of `entry`, a RAW photo read by range, to `path`:
		/// written beside it and renamed into place, and on `NoPreview` no file is left.
		pub async fn write_entry_embedded_preview(
			&self,
			entry: ArchiveEntryId,
			path: String,
			managed_future: ManagedFuture,
		) -> Result<EmbeddedPreviewResult, Error> {
			let reader = self.reader();
			managed(managed_future, move |control| async move {
				let path = PathBuf::from(path);
				let (guard, file) = crate::io::client_impl::TmpFileGuard::create_beside(
					&path,
					&format!("{}.{}", Uuid::new_v4(), crate::io::DOWNLOAD_TMP_EXT),
				)
				.await?;
				let mut file = file.compat_write();
				let preview = reader
					.write_entry_embedded_preview(entry, &mut file, control)
					.await
					.map_err(Error::unshared)?;
				drop(file);
				if preview.is_some() {
					guard.commit(&path).await?;
				}
				Ok(EmbeddedPreviewResult::from(preview))
			})
			.await
		}
	}

	/// A new file in `dir` for `entry`, which the pass writes it to.
	async fn entry_file(
		dir: &Path,
		entry: ArchiveEntryId,
		sender: tokio::sync::mpsc::UnboundedSender<ReadEntriesDelivery>,
		writing: &Arc<Mutex<Option<PathBuf>>>,
	) -> Result<Option<EntryFile>, Error> {
		let path = dir.join(format!("{}-{}", entry.index, Uuid::new_v4()));
		let file = tokio::fs::File::create(&path).await?;
		*writing.lock().unwrap_or_else(PoisonError::into_inner) = Some(path.clone());
		Ok(Some(EntryFile {
			file: file.compat_write(),
			announce: Some((entry, path, sender)),
			writing: Arc::clone(writing),
		}))
	}

	#[cfg(test)]
	mod tests {
		use super::*;

		#[tokio::test]
		async fn an_unfinished_entrys_file_goes_when_the_pass_is_dropped() {
			let dir = std::env::temp_dir().join(format!("filen-entries-{}", Uuid::new_v4()));
			std::fs::create_dir_all(&dir).unwrap();
			let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
			let writing = Unfinished::default();
			let entry = ArchiveEntryId {
				archive: Uuid::new_v4(),
				index: 3,
			};
			let mut file = entry_file(&dir, entry, sender, &writing.0)
				.await
				.unwrap()
				.unwrap();
			futures::AsyncWriteExt::write_all(&mut file, b"part of it")
				.await
				.unwrap();
			assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
			// the pass's future dropped mid-entry: its file goes with the guard
			drop(file);
			drop(writing);
			assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
			std::fs::remove_dir(&dir).unwrap();
		}
	}
}

#[cfg(feature = "wasm-full")]
mod wasm_bindings {
	use std::{
		pin::Pin,
		task::{Context, Poll, ready},
	};

	use filen_macros::js_type;
	use filen_types::fs::UuidStr;
	use futures::AsyncWrite;
	use serde::Serialize;
	use tokio::sync::oneshot;
	use wasm_bindgen::{JsCast, JsValue, prelude::wasm_bindgen};
	use web_sys::js_sys;

	use super::{
		ArchiveReader, EntriesReadReport, Opener, list_job, managed, open_nested_job, reader_config,
	};
	use crate::{
		Error, ErrorKind,
		auth::{JsClient, js_impls::UnauthJsClient},
		fs::{
			archive::{
				ArchiveFormat, EntryByteAccess, ReaderDownloadEntryConfig,
				extract::{self, ArchiveEntryId, EntryDownloadCallback},
				js_impl::{
					EntryDownloadChannel, EntryDownloadReport, EntryDownloadUpdate, ExpansionLimit,
					ListReport,
					wasm_impl::{ListCallbacks, required_solid_skip},
				},
			},
			file::{enums::RemoteFileType, service_worker::StreamWriter},
		},
		js::{AnyFile, ManagedFuture, call_callback, stream_writer},
		thumbnail::EmbeddedPreviewResult,
	};

	/// A JS `WritableStream` as the reader writes it: closing it waits until the stream took
	/// every byte and closed, and fails when it did not; dropped unclosed, it aborts the stream.
	struct ClosingStream {
		writer: StreamWriter,
		/// The stream's own end, until it was seen.
		closed: Option<oneshot::Receiver<Result<(), Error>>>,
		/// The writer sent its last data.
		writer_closed: bool,
	}

	impl ClosingStream {
		fn new(stream: web_sys::WritableStream) -> Result<Self, Error> {
			let (writer, closed) = stream_writer(
				stream,
				None::<fn(u64)>,
				"failed to convert WritableStream to AsyncWrite",
			)?;
			Ok(Self {
				writer,
				closed: Some(closed),
				writer_closed: false,
			})
		}
	}

	impl AsyncWrite for ClosingStream {
		fn poll_write(
			self: Pin<&mut Self>,
			cx: &mut Context<'_>,
			buf: &[u8],
		) -> Poll<std::io::Result<usize>> {
			Pin::new(&mut self.get_mut().writer).poll_write(cx, buf)
		}

		fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
			Pin::new(&mut self.get_mut().writer).poll_flush(cx)
		}

		fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
			let this = self.get_mut();
			if !this.writer_closed {
				ready!(Pin::new(&mut this.writer).poll_close(cx))?;
				this.writer_closed = true;
			}
			let Some(closed) = this.closed.as_mut() else {
				return Poll::Ready(Ok(()));
			};
			let result = ready!(Pin::new(closed).poll(cx));
			this.closed = None;
			Poll::Ready(match result {
				Ok(Ok(())) => Ok(()),
				Ok(Err(error)) => Err(std::io::Error::other(error)),
				Err(_) => Err(std::io::Error::other(
					"the task writing the stream ended before closing it",
				)),
			})
		}
	}

	/// How `openArchiveReader` opens its archive.
	#[js_type(import, no_ser, no_default)]
	pub struct OpenArchiveReaderParams {
		/// The archive: any file the client can read.
		pub archive: AnyFile,
		/// See `ListArchiveParams.expansionLimit`; every call of the reader keeps it.
		#[serde(default)]
		#[tsify(optional)]
		pub expansion_limit: Option<ExpansionLimit>,
		/// See `ListArchiveParams.skipMacMetadata`; every call of the reader keeps it.
		#[serde(default)]
		#[tsify(optional)]
		pub skip_mac_metadata: Option<bool>,
		/// The opening's progress, throttled; the last one comes once it ended.
		#[tsify(type = "(update: ListUpdate) => void", optional)]
		#[serde(default, deserialize_with = "crate::js::optional_function")]
		pub on_update: Option<js_sys::Function>,
		/// The signals that pause and cancel the opening.
		#[serde(default)]
		pub managed_future: ManagedFuture,
	}

	/// How `ArchiveReader.list` reports its entries.
	#[js_type(import, no_ser, no_default)]
	pub struct ReaderListParams {
		/// See `ListArchiveParams.onEntriesBatch`.
		#[tsify(type = "(entries: ArchiveEntry[]) => void", optional)]
		#[serde(default, deserialize_with = "crate::js::optional_function")]
		pub on_entries_batch: Option<js_sys::Function>,
		/// See `ListArchiveParams.onUpdate`.
		#[tsify(type = "(update: ListUpdate) => void", optional)]
		#[serde(default, deserialize_with = "crate::js::optional_function")]
		pub on_update: Option<js_sys::Function>,
		/// The signals that pause and cancel the listing.
		#[serde(default)]
		pub managed_future: ManagedFuture,
	}

	/// What `ArchiveReader.readEntryRange` reads, and where to.
	#[js_type(import, no_ser, no_default)]
	pub struct ReadEntryRangeParams {
		/// The entry, `mapped` or `seekable`.
		pub entry: ArchiveEntryId,
		/// The first byte of the range.
		#[tsify(type = "bigint | number")]
		pub start: u64,
		/// The byte after the range, at most the entry's size.
		#[tsify(type = "bigint | number")]
		pub end: u64,
		/// Where the range goes; closed once all of it was written, aborted otherwise.
		#[tsify(type = "WritableStream<Uint8Array>")]
		#[serde(with = "serde_wasm_bindgen::preserve")]
		pub writer: web_sys::WritableStream,
		/// The signals that cancel the read.
		#[serde(default)]
		pub managed_future: ManagedFuture,
	}

	/// What `ArchiveReader.downloadEntry` downloads, and where to.
	#[js_type(import, no_ser, no_default)]
	pub struct ReaderDownloadEntryParams {
		/// The entry, a file of the archive, from the reader's `list`.
		pub entry: ArchiveEntryId,
		/// See `DownloadArchiveEntryParams.writer`.
		#[tsify(type = "WritableStream<Uint8Array>")]
		#[serde(with = "serde_wasm_bindgen::preserve")]
		pub writer: web_sys::WritableStream,
		/// See `DownloadArchiveEntryParams.maxSolidSkip`: required, `null` allows any skip.
		#[tsify(type = "bigint | number | null")]
		#[serde(deserialize_with = "required_solid_skip")]
		pub max_solid_skip: Option<u64>,
		/// See `DownloadArchiveEntryParams.onUpdate`.
		#[tsify(type = "(update: EntryDownloadUpdate) => void", optional)]
		#[serde(default, deserialize_with = "crate::js::optional_function")]
		pub on_update: Option<js_sys::Function>,
		/// The signals that pause and cancel the download.
		#[serde(default)]
		pub managed_future: ManagedFuture,
	}

	/// What `ArchiveReader.readEntries` reads, and where to.
	#[js_type(import, no_ser, no_default)]
	pub struct ReadEntriesParams {
		/// The entries, files of the archive; read in the archive's order whatever order they
		/// are given in.
		pub entries: Vec<ArchiveEntryId>,
		/// Called as the pass reaches each entry, with its size when the archive states it:
		/// the stream it is written to (closed once it was written whole and checked), or
		/// `undefined` to decode it and throw it away. A throw, or anything but a stream, ends
		/// the pass.
		#[tsify(
			type = "(entry: ArchiveEntryId, size: bigint | undefined) => WritableStream<Uint8Array> | undefined"
		)]
		#[serde(with = "serde_wasm_bindgen::preserve")]
		pub writer_for: js_sys::Function,
		/// Most bytes the pass may decode and throw away to reach the entries, in all: required,
		/// as `DownloadArchiveEntryParams.maxSolidSkip` is; `null` allows any.
		#[tsify(type = "bigint | number | null")]
		#[serde(deserialize_with = "required_solid_skip")]
		pub max_solid_skip: Option<u64>,
		/// The pass's progress, throttled; the last one comes once it ended.
		#[tsify(type = "(update: EntryDownloadUpdate) => void", optional)]
		#[serde(default, deserialize_with = "crate::js::optional_function")]
		pub on_update: Option<js_sys::Function>,
		/// The signals that pause and cancel the pass.
		#[serde(default)]
		pub managed_future: ManagedFuture,
	}

	/// What `ArchiveReader.openNested` opens.
	#[js_type(import, no_ser, no_default)]
	pub struct OpenNestedParams {
		/// The entry holding the archive, `mapped` or `seekable`.
		pub entry: ArchiveEntryId,
		/// The signals that pause and cancel the opening.
		#[serde(default)]
		pub managed_future: ManagedFuture,
	}

	/// Whose embedded preview `ArchiveReader.writeEntryEmbeddedPreview` writes, and where to.
	#[js_type(import, no_ser, no_default)]
	pub struct WriteEntryEmbeddedPreviewParams {
		/// The entry, a RAW photo read by range (`mapped` or `seekable`).
		pub entry: ArchiveEntryId,
		/// See `WriteEmbeddedPreviewParams.writer`.
		#[tsify(type = "WritableStream<Uint8Array>")]
		#[serde(with = "serde_wasm_bindgen::preserve")]
		pub writer: web_sys::WritableStream,
		/// The signals that cancel the write.
		#[serde(default)]
		pub managed_future: ManagedFuture,
	}

	/// A callback of an entries pass, as the pass made it, for the JS thread.
	enum ReadEntriesDelivery {
		Update(extract::EntryDownloadUpdate),
		/// The pass reached an entry: `writerFor`'s stream for it goes back on `reply`.
		Writer {
			entry: ArchiveEntryId,
			size: Option<u64>,
			reply: oneshot::Sender<Result<Option<ClosingStream>, Error>>,
		},
	}

	struct ReadEntriesChannel(tokio::sync::mpsc::UnboundedSender<ReadEntriesDelivery>);

	impl EntryDownloadCallback for ReadEntriesChannel {
		fn on_update(&self, update: extract::EntryDownloadUpdate) {
			let _ = self.0.send(ReadEntriesDelivery::Update(update));
		}
	}

	/// `value` as JS, as the SDK's records are: objects rather than maps, large numbers as
	/// BigInt.
	fn to_js(value: &impl Serialize) -> Result<JsValue, Error> {
		let serializer = serde_wasm_bindgen::Serializer::new()
			.serialize_maps_as_objects(true)
			.serialize_large_number_types_as_bigints(true);
		value
			.serialize(&serializer)
			.map_err(|error| Error::custom(ErrorKind::Conversion, error.to_string()))
	}

	/// The stream `writer_for` gives `entry`, on this thread.
	fn stream_for(
		writer_for: &js_sys::Function,
		entry: ArchiveEntryId,
		size: Option<u64>,
	) -> Result<Option<ClosingStream>, Error> {
		let stream = writer_for
			.call2(&JsValue::UNDEFINED, &to_js(&entry)?, &to_js(&size)?)
			.map_err(|thrown| {
				Error::custom(
					ErrorKind::Conversion,
					format!("writerFor threw: {thrown:?}"),
				)
			})?;
		if stream.is_undefined() || stream.is_null() {
			return Ok(None);
		}
		let stream = stream.dyn_into::<web_sys::WritableStream>().map_err(|_| {
			Error::custom(
				ErrorKind::Conversion,
				"writerFor returned neither a WritableStream nor undefined",
			)
		})?;
		ClosingStream::new(stream).map(Some)
	}

	/// Opens a reader of the params' archive through `opener`, as the wasm `openArchiveReader`
	/// calls do.
	async fn open_reader(
		opener: Opener,
		params: OpenArchiveReaderParams,
		password: Option<String>,
	) -> Result<ArchiveReader, Error> {
		let config = reader_config(params.expansion_limit, params.skip_mac_metadata, password)?;
		let archive = RemoteFileType::try_from(params.archive)?;
		let callbacks = ListCallbacks {
			on_entries_batch: None,
			on_update: params.on_update,
		};
		params
			.managed_future
			.into_ordered_job(
				move |delivery| callbacks.deliver(delivery),
				move |sender, control| opener.open(archive, config, sender, control),
			)
			.await
	}

	#[filen_macros::js_exports]
	#[wasm_bindgen(js_class = "Client")]
	impl JsClient {
		/// Opens an archive for the calls of an `ArchiveReader`, under the params' settings,
		/// which every call of the reader keeps: a zip, 7z or RAR has its index read and held
		/// for them, a tar or single compressed file only its head. Opening takes one of the
		/// archive job slots for its own duration. Fails as a listing does: a 7z or RAR whose
		/// index is encrypted needs `password` here. `close` (or `free`) the reader once done.
		#[wasm_bindgen(js_name = "openArchiveReader")]
		pub async fn open_archive_reader(
			&self,
			params: OpenArchiveReaderParams,
			password: Option<String>,
		) -> Result<ArchiveReader, Error> {
			open_reader(Opener::Client(self.inner()), params, password).await
		}
	}

	#[filen_macros::js_exports]
	#[wasm_bindgen(js_class = "UnauthClient")]
	impl UnauthJsClient {
		/// `Client.openArchiveReader` without a session: the archive is a file a public link
		/// gives.
		#[wasm_bindgen(js_name = "openArchiveReader")]
		pub async fn open_archive_reader(
			&self,
			params: OpenArchiveReaderParams,
			password: Option<String>,
		) -> Result<ArchiveReader, Error> {
			open_reader(Opener::Unauth(self.inner()), params, password).await
		}
	}

	#[filen_macros::js_exports]
	#[wasm_bindgen]
	impl ArchiveReader {
		/// This reader's own id: a new one each time an archive is opened.
		#[wasm_bindgen(getter)]
		pub fn id(&self) -> UuidStr {
			UuidStr::from(&self.inner.id())
		}

		/// The id of its archive, which the ids of its entries carry.
		#[wasm_bindgen(getter, js_name = "archiveId")]
		pub fn archive_id(&self) -> UuidStr {
			UuidStr::from(&self.inner.archive_id())
		}

		/// What the archive is.
		#[wasm_bindgen(getter)]
		pub fn format(&self) -> Option<ArchiveFormat> {
			self.inner.format()
		}

		/// Whether the reader holds the archive's index right now; one given up for room
		/// another reader needed is read again by the next call that needs it.
		#[wasm_bindgen(getter, js_name = "indexHeld")]
		pub fn index_held(&self) -> bool {
			self.inner.index_held()
		}

		/// Gives up what the reader holds, and closes the readers of archives nested in it.
		/// Every later call fails with `InvalidState`, and so does a range read already
		/// running, at its next chunk; a job already running (a listing, a download, a pass)
		/// runs to its end. Never waits on a lock.
		pub fn close(&self) {
			self.inner.close();
		}

		/// Lists the archive's entries as `listArchive` does, from the held index when there
		/// is one, without reading the archive again.
		pub async fn list(&self, params: ReaderListParams) -> Result<ListReport, Error> {
			let reader = self.reader();
			let callbacks = ListCallbacks {
				on_entries_batch: params.on_entries_batch,
				on_update: params.on_update,
			};
			params
				.managed_future
				.into_ordered_job(
					move |delivery| callbacks.deliver(delivery),
					move |sender, control| list_job(reader, sender, control),
				)
				.await
				.map(|(result, undelivered)| ListReport::new(result, undelivered))
		}

		/// How an entry's bytes can be read: by range (`mapped`, `seekable`), only whole
		/// (`whole`), or only in an ordered pass (`sequential`).
		#[wasm_bindgen(js_name = "entryByteAccess")]
		pub async fn entry_byte_access(
			&self,
			entry: ArchiveEntryId,
			managed_future: Option<ManagedFuture>,
		) -> Result<EntryByteAccess, Error> {
			let reader = self.reader();
			managed(
				managed_future.unwrap_or_default(),
				move |control| async move {
					reader
						.entry_byte_access(entry, control)
						.await
						.map_err(Error::unshared)
				},
			)
			.await
		}

		/// Writes a range of a `mapped` or `seekable` entry to a stream, unchecked (the entry's
		/// checksum covers it whole), and closes the stream. Takes no job slot unless the reader
		/// reads its index again, which `managedFuture` pauses and cancels.
		#[wasm_bindgen(js_name = "readEntryRange")]
		pub async fn read_entry_range(&self, params: ReadEntryRangeParams) -> Result<(), Error> {
			let reader = self.reader();
			let mut writer = ClosingStream::new(params.writer)?;
			let (entry, start, end) = (params.entry, params.start, params.end);
			managed(params.managed_future, move |control| async move {
				reader
					.read_entry_range(entry, start, end, &mut writer, control)
					.await
					.map_err(Error::unshared)
			})
			.await
		}

		/// Writes an entry to a stream as `downloadArchiveEntry` does, starting from the held
		/// index; resolves with the report whether it completed, was cancelled or failed.
		#[wasm_bindgen(js_name = "downloadEntry")]
		pub async fn download_entry(
			&self,
			params: ReaderDownloadEntryParams,
		) -> Result<EntryDownloadReport, Error> {
			let reader = self.reader();
			let mut writer = ClosingStream::new(params.writer)?;
			let settings = ReaderDownloadEntryConfig {
				max_solid_skip: params.max_solid_skip,
			};
			let entry = params.entry;
			let on_update = params.on_update;
			params
				.managed_future
				.into_ordered_job(
					move |update: extract::EntryDownloadUpdate| {
						call_callback(on_update.as_ref(), &EntryDownloadUpdate::from(update))
					},
					move |sender, control| async move {
						Ok(reader
							.download_entry(
								entry,
								&mut writer,
								settings,
								EntryDownloadChannel(sender),
								control,
							)
							.await)
					},
				)
				.await
				.map(EntryDownloadReport::new)
		}

		/// Reads entries in one ordered pass, each to the stream `writerFor` gives it as the
		/// pass reaches it. The pass takes one of the archive job slots and gives it back once
		/// the last entry is written; resolves with the report whether it completed, was
		/// cancelled or failed.
		#[wasm_bindgen(js_name = "readEntries")]
		pub async fn read_entries(
			&self,
			params: ReadEntriesParams,
		) -> Result<EntriesReadReport, Error> {
			let reader = self.reader();
			let ReadEntriesParams {
				entries,
				writer_for,
				max_solid_skip,
				on_update,
				managed_future,
			} = params;
			managed_future
				.into_ordered_job(
					move |delivery| match delivery {
						ReadEntriesDelivery::Update(update) => {
							call_callback(on_update.as_ref(), &EntryDownloadUpdate::from(update))
						}
						ReadEntriesDelivery::Writer { entry, size, reply } => {
							let _ = reply.send(stream_for(&writer_for, entry, size));
						}
					},
					move |sender, control| async move {
						let asks = sender.clone();
						let result = reader
							.read_entries(
								&entries,
								move |entry, size| {
									let asks = asks.clone();
									async move {
										let (reply, stream) = oneshot::channel();
										asks.send(ReadEntriesDelivery::Writer {
											entry,
											size,
											reply,
										})
										.map_err(|_| writer_unanswered())?;
										stream.await.map_err(|_| writer_unanswered())?
									}
								},
								max_solid_skip,
								ReadEntriesChannel(sender),
								control,
							)
							.await;
						Ok(result)
					},
				)
				.await
				.map(EntriesReadReport::new)
		}

		/// Opens the archive an entry holds (a `mapped` or `seekable` one) as a reader of its
		/// own, which reads it through this one; `password` is the nested archive's.
		#[wasm_bindgen(js_name = "openNested")]
		pub async fn open_nested(
			&self,
			params: OpenNestedParams,
			password: Option<String>,
		) -> Result<ArchiveReader, Error> {
			let reader = self.reader();
			let entry = params.entry;
			params
				.managed_future
				.into_js_managed_commander_job(move |control| {
					open_nested_job(reader, entry, password, control)
				})?
				.await
		}

		/// Writes the embedded preview of an entry, a RAW photo read by range, to a stream,
		/// closed on every settled answer (empty for `noPreview`) and aborted on a failure.
		#[wasm_bindgen(js_name = "writeEntryEmbeddedPreview")]
		pub async fn write_entry_embedded_preview(
			&self,
			params: WriteEntryEmbeddedPreviewParams,
		) -> Result<EmbeddedPreviewResult, Error> {
			let reader = self.reader();
			let mut writer = ClosingStream::new(params.writer)?;
			let entry = params.entry;
			managed(params.managed_future, move |control| async move {
				reader
					.write_entry_embedded_preview(entry, &mut writer, control)
					.await
					.map(EmbeddedPreviewResult::from)
					.map_err(Error::unshared)
			})
			.await
		}
	}

	/// The JS thread did not answer a pass asking for an entry's stream: the call was dropped.
	fn writer_unanswered() -> Error {
		Error::custom(
			ErrorKind::Cancelled,
			"the call ended before writerFor answered",
		)
	}
}

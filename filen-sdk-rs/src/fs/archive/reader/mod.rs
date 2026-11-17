//! A handle on one archive in the drive, which its calls share so each reads only what it needs:
//! an index read once is held for the calls after it, and so are the archive's chunks read last
//! (within [`ArchiveConfig::retained_budget`]). No job stays alive between two calls: each runs
//! a codec job of its own, for its own duration, and is handed what the reader holds.

#![warn(unreachable_pub, unused_qualifications)]

mod sequential;
mod slice;

use std::sync::{
	Arc, Mutex, PoisonError,
	atomic::{AtomicBool, Ordering},
};

use filen_types::fs::Uuid;
use futures::AsyncWrite;

use crate::{
	Error, ErrorKind,
	auth::Client,
	fs::{
		HasUUID,
		archive::{
			config::ArchiveConfig,
			extract::{
				ArchiveEntryId, EntryDownloadCallback, EntryDownloadConfig, EntryDownloadError,
				EntryDownloadFailed, EntryDownloadPhase, EntryDownloadReport,
				EntryDownloadReporter, ListCallback, ListConfig, ListFailed, ListReport,
				codec::{HeldIndex, IndexUse, Task},
				reading::{ReadJob, download, list, open},
			},
			format::ArchiveFormat,
			retained::Retained,
		},
		drive_job::backend::{ClientBackend, DriveBackend},
		file::{enums::RemoteFileType, traits::HasFileInfo},
	},
	job::{JobControl, report::JobFailed},
	util::MaybeSend,
};

pub use sequential::{EntriesReadFailed, EntriesReadReport};
pub use slice::EntryByteAccess;
pub(crate) use slice::Resolved;

/// How [`ArchiveReader::download_entry`] reads its entry; the reader's settings (from
/// [`Client::open_archive_reader`]) give the rest.
#[derive(Debug, Clone)]
pub struct ReaderDownloadEntryConfig {
	/// As [`EntryDownloadConfig::max_solid_skip`]: `Some(0)` by default.
	pub max_solid_skip: Option<u64>,
}

impl Default for ReaderDownloadEntryConfig {
	fn default() -> Self {
		Self {
			max_solid_skip: EntryDownloadConfig::default().max_solid_skip,
		}
	}
}

/// An open archive: see [`Client::open_archive_reader`].
pub struct ArchiveReader {
	reader: Reader<ClientBackend>,
}

impl Client {
	/// Opens `archive` for the calls of an [`ArchiveReader`], under `config`'s settings, which
	/// every call of the reader keeps.
	///
	/// A zip or 7z has its first chunk and its index read, and the index is held for the
	/// reader's later calls, which then start at their entries (a 7z's keys derived for it come
	/// with it). A tar or single compressed file has only its head read. Opening takes one of the
	/// [`ArchiveConfig::job_concurrency`] archive job slots for its own duration, reports its
	/// progress to `callback` (which gets no entries: [`ArchiveReader::list`] lists them) and can
	/// be paused and cancelled through `control`.
	///
	/// Fails as a listing does: a 7z whose index is encrypted needs the password here.
	pub async fn open_archive_reader(
		self: Arc<Self>,
		archive: RemoteFileType<'static>,
		config: ListConfig,
		callback: impl ListCallback,
		control: JobControl,
	) -> Result<ArchiveReader, ListFailed> {
		let config_of_client = self.archives().clone();
		let reader = Reader::new(
			Arc::new(ClientBackend::new(self)),
			archive,
			config_of_client,
			config,
		);
		reader.open(callback, control).await?;
		Ok(ArchiveReader { reader })
	}
}

impl ArchiveReader {
	/// This reader's own id: a new one each time an archive is opened.
	pub fn id(&self) -> Uuid {
		self.reader.id
	}

	/// The id of its archive, which the [`ArchiveEntryId`]s of its entries carry: the file's
	/// uuid, so the ids [`Client::list_archive`] gives are this reader's too.
	pub fn archive_id(&self) -> Uuid {
		self.reader.archive.uuid()
	}

	/// What the archive is; `None` only if opening could not tell, which fails the open.
	pub fn format(&self) -> Option<ArchiveFormat> {
		self.reader.format()
	}

	/// Whether the reader holds the archive's index right now. A held index can be given up
	/// for room another reader needs (see
	/// [`ArchiveConfig::retained_budget`](crate::fs::archive::ArchiveConfig::retained_budget)):
	/// the next call that needs it reads it again, and holds it again when it can.
	pub fn index_held(&self) -> bool {
		self.reader.index_held()
	}

	/// Gives up what the reader holds. Every later call fails with
	/// [`ErrorKind::InvalidState`]; a call already running goes on to its end.
	pub fn close(&self) {
		self.reader.close();
	}

	/// How `entry`'s bytes can be read (see [`EntryByteAccess`]): resolved from the index the
	/// reader holds (for a stored zip entry, reading its local header too, through the chunks
	/// the reader keeps), then kept with the index. Fails as
	/// [`ArchiveReader::download_entry`] does, for an entry of another archive, one not in this
	/// one, a directory or link, or one an extraction would skip. A bare tar's file members are
	/// `Mapped` once a listing found where they lie (the reader lists the tar for it when it
	/// holds no listing); a compressed tar's members and a single compressed file are
	/// `Sequential`.
	pub async fn entry_byte_access(
		&self,
		entry: ArchiveEntryId,
	) -> Result<EntryByteAccess, Arc<Error>> {
		self.reader.entry_byte_access(entry).await
	}

	/// Writes bytes `start..end` of `entry`, which has to be
	/// [`EntryByteAccess::Mapped`], to `writer`, then closes `writer`. Only the archive's
	/// chunks the range lies in are read, a few ahead while the client's file-IO budget has room
	/// for them, and those the reader keeps not at all; no job slot is taken. The bytes are not
	/// checked against the entry's checksum, which covers it whole. Fails with
	/// [`ErrorKind::InvalidState`] for a range past the entry's end or an entry not mapped;
	/// dropping the future stops the read.
	pub async fn read_entry_range<W>(
		&self,
		entry: ArchiveEntryId,
		start: u64,
		end: u64,
		writer: &mut W,
	) -> Result<(), Arc<Error>>
	where
		W: AsyncWrite + Unpin + MaybeSend,
	{
		self.reader
			.read_entry_range(entry, start, end, writer)
			.await
	}

	/// Reads `entries` in one ordered pass, in the archive's order whatever order they are given
	/// in: each is written to the writer `writer_for` gives it, with its size when the archive
	/// states it (no writer: it is decoded and thrown away), and that writer is closed once the
	/// entry was checked against its checksum. The pass takes one of the
	/// [`ArchiveConfig::job_concurrency`] archive job slots and gives it back as soon as the last
	/// entry is written, the rest of the archive unread; a bare tar's members are read as ranges
	/// of it instead, with no slot. Ask for a window of entries, not the whole archive: each pass
	/// decodes a solid block from its start up to its first entry.
	///
	/// `max_solid_skip` bounds what the pass may decode and throw away to reach the entries, in
	/// all: in 7z solid blocks, or a compressed tar a listing found the members of. A pass over
	/// more fails with
	/// [`ErrorKind::ArchiveSolidSkipExceeded`](crate::ErrorKind::ArchiveSolidSkipExceeded)
	/// before any of it is read; `None` allows any. Fails as
	/// [`ArchiveReader::download_entry`] does on an entry that is no file to read; progress goes
	/// to `callback`, and `control` pauses and cancels the pass.
	pub async fn read_entries<W, F>(
		&self,
		entries: &[ArchiveEntryId],
		writer_for: F,
		max_solid_skip: Option<u64>,
		callback: impl EntryDownloadCallback,
		control: JobControl,
	) -> Result<EntriesReadReport, EntriesReadFailed>
	where
		W: AsyncWrite + Unpin + MaybeSend,
		F: FnMut(ArchiveEntryId, Option<u64>) -> Option<W> + MaybeSend,
	{
		self.reader
			.read_entries(entries, writer_for, max_solid_skip, callback, control)
			.await
	}

	/// Lists the archive's entries, as [`Client::list_archive`] does with the reader's settings,
	/// a zip's or 7z's from the index the reader holds. A bare tar is walked by seeking past its
	/// members' data, so only the chunks its headers (and its PAX and GNU long-name records) lie
	/// in are read, and its hash is not checked; a compressed tar is read through. Where a tar's
	/// members lie is held as an index is, 16 bytes a member.
	pub async fn list(
		&self,
		callback: impl ListCallback,
		control: JobControl,
	) -> Result<ListReport, ListFailed> {
		self.reader.list(callback, control).await
	}

	/// Writes the data of `entry`, a file of the zip or 7z, to `writer`, then closes `writer`,
	/// as [`Client::download_archive_entry`] does with the reader's settings: checked against
	/// the CRC-32 or authentication code the archive has for it before `writer` is closed. Only
	/// the entry is read when the reader holds the index (and, for a 7z entry in a solid
	/// block, what the block stores before it). A bare tar's file member is read as a range of
	/// the archive, with no job slot, and `report.checked` is `false`: a tar has no checksum of
	/// its members. A compressed tar's or a single compressed file's fails with
	/// [`ErrorKind::ArchiveUnsupported`]: [`ArchiveReader::read_entries`] reads those.
	pub async fn download_entry<W>(
		&self,
		entry: ArchiveEntryId,
		writer: &mut W,
		config: ReaderDownloadEntryConfig,
		callback: impl EntryDownloadCallback,
		control: JobControl,
	) -> Result<EntryDownloadReport, EntryDownloadFailed>
	where
		W: AsyncWrite + Unpin + MaybeSend,
	{
		self.reader
			.download_entry(entry, writer, config.max_solid_skip, callback, control)
			.await
	}
}

/// A listing's progress nobody hears of: a call that reads the index again for itself.
struct Unreported;

impl ListCallback for Unreported {
	fn on_entries_batch(&self, _: Vec<crate::fs::archive::extract::ArchiveEntry>) {}
	fn on_update(&self, _: crate::fs::archive::extract::ListUpdate) {}
}

/// What a reader holds and how it reads, over any drive backend.
pub(crate) struct Reader<B> {
	backend: Arc<B>,
	archive: RemoteFileType<'static>,
	id: Uuid,
	config: ArchiveConfig,
	settings: ListConfig,
	format: Mutex<Option<ArchiveFormat>>,
	/// What it keeps between its calls; `None` for a reader of one call, which keeps nothing.
	retained: Option<Arc<Retained>>,
	closed: AtomicBool,
}

impl<B: DriveBackend> Reader<B> {
	/// A reader of `archive`, under the client's `config` and the reader's `settings`.
	pub(crate) fn new(
		backend: Arc<B>,
		archive: RemoteFileType<'static>,
		config: ArchiveConfig,
		settings: ListConfig,
	) -> Self {
		let retained = Retained::new(Arc::clone(&config.retained), config.retained_budget());
		Self {
			retained: Some(retained),
			..Self::once(backend, archive, config, settings)
		}
	}

	/// A reader for one call, which keeps nothing: what [`Client::list_archive`] and
	/// [`Client::download_archive_entry`] read through.
	pub(crate) fn once(
		backend: Arc<B>,
		archive: RemoteFileType<'static>,
		config: ArchiveConfig,
		settings: ListConfig,
	) -> Self {
		Self {
			backend,
			archive,
			id: Uuid::new_v4(),
			config,
			settings,
			format: Mutex::new(None),
			retained: None,
			closed: AtomicBool::new(false),
		}
	}

	pub(crate) fn format(&self) -> Option<ArchiveFormat> {
		*self.format.lock().unwrap_or_else(PoisonError::into_inner)
	}

	fn note_format(&self, format: Option<ArchiveFormat>) {
		if let Some(format) = format {
			*self.format.lock().unwrap_or_else(PoisonError::into_inner) = Some(format);
		}
	}

	pub(crate) fn index_held(&self) -> bool {
		self.retained
			.as_ref()
			.is_some_and(|retained| retained.index().is_some())
	}

	pub(crate) fn close(&self) {
		self.closed.store(true, Ordering::SeqCst);
		if let Some(retained) = &self.retained {
			retained.close();
		}
	}

	/// The error of a call on a closed reader.
	pub(crate) fn ensure_open(&self) -> Result<(), Error> {
		if self.closed.load(Ordering::SeqCst) {
			return Err(Error::custom(
				ErrorKind::InvalidState,
				"the archive reader is closed",
			));
		}
		Ok(())
	}

	/// Where the next job gets the index from: the one held, or read and handed back to be held.
	fn index_use(&self) -> IndexUse {
		match self.retained.as_ref().map(|retained| retained.index()) {
			Some(Some(index)) => IndexUse::Held(index),
			Some(None) => IndexUse::ReadAndReturn,
			None => IndexUse::Read,
		}
	}

	/// The archive's index: the one held, or else read again (and held again when it can be).
	pub(crate) async fn held_index(&self) -> Result<Arc<HeldIndex>, Arc<Error>> {
		if let Some(index) = self.retained.as_ref().and_then(|retained| retained.index()) {
			return Ok(index);
		}
		let opened = open(self.read_job(JobControl::default()), Unreported)
			.await
			.map_err(|failed| failed.error)?;
		self.note_format(Some(opened.format));
		let index: Arc<HeldIndex> = Arc::from(opened.index.ok_or_else(|| {
			Arc::new(Error::custom(
				ErrorKind::InvalidState,
				"the archive has no index",
			))
		})?);
		if let Some(retained) = &self.retained
			&& !self.closed.load(Ordering::SeqCst)
		{
			retained.hold_index(Arc::clone(&index));
		}
		Ok(index)
	}

	/// Holds `index`, which a job read, for the calls after it.
	pub(crate) fn hold(&self, index: Option<Box<HeldIndex>>) {
		if let (Some(retained), Some(index)) = (&self.retained, index)
			&& !self.closed.load(Ordering::SeqCst)
		{
			retained.hold_index(Arc::from(index));
		}
	}

	pub(crate) async fn open(
		&self,
		callback: impl ListCallback,
		control: JobControl,
	) -> Result<(), ListFailed> {
		let opened = open(self.read_job(control), callback).await?;
		self.note_format(Some(opened.format));
		self.hold(opened.index);
		Ok(())
	}

	pub(crate) async fn list(
		&self,
		callback: impl ListCallback,
		control: JobControl,
	) -> Result<ListReport, ListFailed> {
		if let Err(error) = self.ensure_open() {
			return Err(JobFailed {
				report: ListReport::empty(),
				error: Arc::new(error),
			});
		}
		// a reader that keeps what it reads lists a bare tar by seeking past its members' data;
		// one for a single call reads it whole, as a listing always did, its hash checked
		let archive = self.archive.uuid();
		let task = match self.retained {
			Some(_) => Task::ListSeeking { archive },
			None => Task::List { archive },
		};
		let listed = list(self.read_job(control), task, callback).await?;
		self.note_format(listed.report.format);
		self.hold(listed.index);
		Ok(listed.report)
	}

	pub(crate) async fn download_entry<W>(
		&self,
		entry: ArchiveEntryId,
		writer: &mut W,
		max_solid_skip: Option<u64>,
		callback: impl EntryDownloadCallback,
		control: JobControl,
	) -> Result<EntryDownloadReport, EntryDownloadFailed>
	where
		W: AsyncWrite + Unpin + MaybeSend,
	{
		if let Err(error) = self.ensure_open() {
			return Err(JobFailed {
				report: EntryDownloadReport::default(),
				error: Arc::new(error),
			});
		}
		if let Some(ArchiveFormat::Tar { codec: None }) = self.format()
			&& self.retained.is_some()
		{
			return self.download_member(entry, writer, callback).await;
		}
		let (downloaded, index) = download(
			self.read_job(control),
			entry,
			writer,
			max_solid_skip,
			callback,
		)
		.await;
		self.hold(index);
		downloaded
	}

	/// Downloads a bare tar's member, whose data lies in the archive as it is: as a range of the
	/// archive, which takes no job slot. A tar holds no checksum of its members.
	async fn download_member<W>(
		&self,
		entry: ArchiveEntryId,
		writer: &mut W,
		callback: impl EntryDownloadCallback,
	) -> Result<EntryDownloadReport, EntryDownloadFailed>
	where
		W: AsyncWrite + Unpin + MaybeSend,
	{
		let reporter = EntryDownloadReporter::new(callback, self.archive.size());
		reporter.set_phase(EntryDownloadPhase::Reading);
		let read = match self.resolve(entry).await {
			Ok(Resolved::Mapped { size, .. }) => {
				reporter.entry_found(Some(size));
				self.read_entry_range(entry, 0, size, writer)
					.await
					.map(|()| size)
			}
			Ok(Resolved::Whole { .. } | Resolved::Sequential { .. }) => {
				Err(Arc::new(EntryDownloadError::NotIndexed.into()))
			}
			Err(error) => Err(error),
		};
		match read {
			Ok(size) => {
				reporter.wrote(size);
				reporter.finish(EntryDownloadPhase::Done);
				Ok(EntryDownloadReport {
					bytes_written: size,
					bytes_read: size,
					checked: false,
				})
			}
			Err(error) => {
				reporter.finish(EntryDownloadPhase::Failed);
				Err(JobFailed {
					report: EntryDownloadReport::default(),
					error,
				})
			}
		}
	}

	/// What a job of this reader reads, and how.
	pub(crate) fn read_job(&self, control: JobControl) -> ReadJob<B> {
		ReadJob {
			backend: Arc::clone(&self.backend),
			archive: self.archive.clone(),
			config: self.config.clone(),
			settings: self.settings.clone(),
			index: self.index_use(),
			chunks: self.retained.clone(),
			control,
		}
	}
}

#[cfg(test)]
mod tests;

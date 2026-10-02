//! Downloading one file entry of a zip or 7z to a writer, without extracting anything into the
//! drive. See [`Client::download_archive_entry`](crate::auth::Client::download_archive_entry).

use std::{sync::Arc, time::Duration};

use filen_macros::js_type;
use futures::{AsyncWrite, AsyncWriteExt};

use crate::{
	Error, ErrorKind,
	fs::{
		archive::{
			config::ArchiveConfig,
			format::ArchiveFormat,
			input::{FeedSink, ReadingDriver, ReadingJob, start_reading},
			worker::{CodecStart, EntryHead, EntryKind, WorkerEvent, unexpected_event},
		},
		drive_job::backend::DriveBackend,
		file::enums::RemoteFileType,
	},
	job::{
		self, JobControl, Stopped,
		report::{JobFailed, JobPhase, JobReport, JobState, Progress, RunCore, Snapshot, Units},
	},
	util::{MaybeArc, MaybeSend, MaybeSendSync},
};

use super::{
	EntrySelection, ExtractSkipReason,
	codec::Selection,
	engine::CodecResult,
	report::{ArchiveEntryId, ReadsArchive, RunState},
};

/// Where a download is. The last three are where it ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[js_type(export, no_deser, no_default)]
pub enum EntryDownloadPhase {
	/// Waiting for another archive job to finish; nothing is held meanwhile.
	WaitingForWorker,
	/// Reading the archive's index, then the entry (and what a solid 7z block stores before it).
	Reading,
	/// Wrote the entry and closed the writer.
	Done,
	/// Ended early by a cancel.
	Cancelled,
	/// Ended early by an error.
	Failed,
}

impl JobPhase for EntryDownloadPhase {
	const DONE: Self = Self::Done;
	const CANCELLED: Self = Self::Cancelled;
	const FAILED: Self = Self::Failed;
}

/// One progress callback of a download.
#[derive(Debug, Clone)]
pub struct EntryDownloadUpdate {
	/// Where the download is.
	pub phase: EntryDownloadPhase,
	/// Whether it runs, is paused or winds down.
	pub run_state: RunState,
	/// Bytes of the archive read so far, each counted once, of `archive_bytes`.
	pub bytes_read: u64,
	/// The archive's size in bytes.
	pub archive_bytes: u64,
	/// Bytes of the entry written so far.
	pub bytes_written: u64,
	/// The entry's size in bytes as the archive states it; `None` until the download reached
	/// the entry.
	pub entry_bytes: Option<u64>,
	/// Bytes of the archive read per second, over the last 10 seconds of running time; `None`
	/// until there is a rate.
	pub bytes_per_second: Option<u64>,
	/// The time left to write the rest of the entry, at the rate it was written over the last 10
	/// seconds of running time; `None` while `entry_bytes` is, while there is no rate, or while
	/// the download winds down, zero once it ended.
	pub eta: Option<Duration>,
	/// Time spent running, paused time left out.
	pub active_time: Duration,
}

/// Receives a download's progress. All calls come from the one job, in order.
pub trait EntryDownloadCallback: MaybeSendSync + 'static {
	/// The download's progress, throttled; the last one comes once the download ended.
	fn on_update(&self, update: EntryDownloadUpdate);
}

impl<T: EntryDownloadCallback + ?Sized> EntryDownloadCallback for Arc<T> {
	fn on_update(&self, update: EntryDownloadUpdate) {
		(**self).on_update(update);
	}
}

/// What a download did, whether it completed, was cancelled or failed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EntryDownloadReport {
	/// Bytes of the entry written to the writer, in writes that completed (one a cancel cut
	/// short is not counted).
	pub bytes_written: u64,
	/// Bytes of the archive read, each counted once.
	pub bytes_read: u64,
	/// The archive's checksum for the entry matched every byte written. `false` on an early
	/// end, and for a 7z entry whose header lists no CRC-32.
	pub checked: bool,
}

impl JobReport for EntryDownloadReport {
	const NAME: &'static str = "entry download";
}

/// A download that ended early: cancelled, or stopped by an error. What it wrote by then is
/// unverified, and the writer was not closed.
pub type EntryDownloadFailed = JobFailed<EntryDownloadReport>;

/// A 7z entry its solid block stores after more bytes of other files than the download
/// allowed to decode and throw away first: the error's source, reached with
/// [`Error::downcast_ref`], under [`ErrorKind::ArchiveSolidSkipExceeded`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
	"the entry is stored in a solid block after {skipped_bytes} bytes of other files, more than \
	 the {limit} the download allows to decode first"
)]
pub struct SolidSkipExceeded {
	/// Bytes the block stores before the entry, decoded and thrown away to reach it (see
	/// [`EntryAccess::SolidBlock`](super::EntryAccess::SolidBlock)).
	pub skipped_bytes: u64,
	/// The most the download allowed, in bytes.
	pub limit: u64,
	/// Archive bytes downloading the entry would fetch, estimated at the block's average
	/// compression ratio.
	pub estimated_packed_bytes: u64,
}

impl From<SolidSkipExceeded> for Error {
	fn from(error: SolidSkipExceeded) -> Self {
		Error::custom_with_source(ErrorKind::ArchiveSolidSkipExceeded, error, None::<&str>)
	}
}

/// Why the entry chosen cannot be downloaded.
#[derive(Debug, thiserror::Error)]
pub(super) enum EntryDownloadError {
	/// A tar's or a single compressed file's, which are only read front to back.
	#[error("only a zip's or 7z's entries can be downloaded alone")]
	NotIndexed,
	/// A directory (refused once the index is read), or an entry the codec sent nothing for.
	#[error("the entry is not a file")]
	NotAFile,
	/// An entry an extraction would skip, for this reason.
	#[error("the entry cannot be downloaded: {0:?}")]
	Skipped(ExtractSkipReason),
}

impl From<EntryDownloadError> for Error {
	fn from(error: EntryDownloadError) -> Self {
		let kind = match &error {
			EntryDownloadError::NotIndexed => ErrorKind::ArchiveUnsupported,
			EntryDownloadError::NotAFile => ErrorKind::InvalidState,
			EntryDownloadError::Skipped(reason) => match reason {
				ExtractSkipReason::UnsupportedMethod
				| ExtractSkipReason::UnsupportedType
				| ExtractSkipReason::Sparse => ErrorKind::ArchiveUnsupported,
				ExtractSkipReason::OverlappingData => ErrorKind::ArchiveCorrupt,
				ExtractSkipReason::Symlink { .. }
				| ExtractSkipReason::Hardlink { .. }
				| ExtractSkipReason::Device
				| ExtractSkipReason::PathTooLong
				| ExtractSkipReason::PathTooDeep
				| ExtractSkipReason::UnsafePath
				| ExtractSkipReason::AntiItem
				| ExtractSkipReason::MacMetadata => ErrorKind::InvalidState,
			},
		};
		Error::custom_with_source(kind, error, None::<&str>)
	}
}

/// What a download counts, next to the job-agnostic [`RunCore`].
pub(crate) struct EntryDownloadState {
	core: RunCore<(), EntryDownloadPhase>,
	archive_bytes: u64,
	bytes_read: u64,
	bytes_written: u64,
	entry_bytes: Option<u64>,
}

impl JobState for EntryDownloadState {
	type Phase = EntryDownloadPhase;
	type Event = ();
	type Callback = dyn EntryDownloadCallback;

	fn core(&mut self) -> &mut RunCore<(), EntryDownloadPhase> {
		&mut self.core
	}

	/// The rate is of the archive read, the time left of the entry written: a solid 7z block
	/// reads much before it writes anything.
	fn progress(&self) -> Progress {
		let total = self.entry_bytes.unwrap_or(0);
		Progress {
			bytes_done: self.bytes_read,
			units: Units {
				done: self.bytes_written,
				settled: if self.core.is_finished() {
					total
				} else {
					self.bytes_written
				},
				total,
			},
		}
	}

	fn deliver(
		&mut self,
		callback: &dyn EntryDownloadCallback,
		snapshot: Snapshot<(), EntryDownloadPhase>,
	) {
		// before the entry's size is known nothing is left to write, which reads as no time left
		let eta = if self.entry_bytes.is_none() && !self.core.is_finished() {
			None
		} else {
			snapshot.eta
		};
		callback.on_update(EntryDownloadUpdate {
			phase: snapshot.phase,
			run_state: snapshot.run_state,
			bytes_read: self.bytes_read,
			archive_bytes: self.archive_bytes,
			bytes_written: self.bytes_written,
			entry_bytes: self.entry_bytes,
			bytes_per_second: snapshot.bytes_per_second,
			eta,
			active_time: snapshot.active_time,
		});
	}

	fn settle(&mut self) {}
}

impl ReadsArchive for EntryDownloadState {
	fn bytes_read(&mut self) -> &mut u64 {
		&mut self.bytes_read
	}
}

/// A download's reporter: the job-agnostic [`job::report::Reporter`] over an
/// [`EntryDownloadState`].
pub(crate) type EntryDownloadReporter = job::report::Reporter<EntryDownloadState>;

impl EntryDownloadReporter {
	pub(crate) fn new(callback: impl EntryDownloadCallback, archive_bytes: u64) -> MaybeArc<Self> {
		Self::from_parts(
			EntryDownloadState {
				core: RunCore::new(EntryDownloadPhase::WaitingForWorker),
				archive_bytes,
				bytes_read: 0,
				bytes_written: 0,
				entry_bytes: None,
			},
			Box::new(callback),
		)
	}

	/// The download reached its entry, which the archive states is `size` bytes long.
	fn entry_found(&self, size: Option<u64>) {
		self.with_state(|state| {
			state.entry_bytes = size;
			state.core.mark_changed();
		});
	}

	/// `total` bytes of the entry were written so far.
	fn wrote(&self, total: u64) {
		self.with_state(|state| {
			state.bytes_written = total;
			state.core.mark_changed();
		});
	}
}

/// `archive` and the selection of its entry `entry` alone; a download of an entry of another
/// archive is refused before it starts, with its last update.
pub(crate) fn choose_entry(
	archive: RemoteFileType<'static>,
	entry: ArchiveEntryId,
	reporter: &EntryDownloadReporter,
) -> Result<(RemoteFileType<'static>, Selection), EntryDownloadFailed> {
	// the check a partial extraction of the one entry makes
	match EntrySelection::new(archive, vec![entry], Vec::new()) {
		Ok(selection) => Ok(selection.into_parts()),
		Err(error) => {
			reporter.finish(EntryDownloadPhase::Failed);
			Err(EntryDownloadFailed {
				report: EntryDownloadReport::default(),
				error: Arc::new(error),
			})
		}
	}
}

/// What [`run_download`] needs.
pub(crate) struct DownloadTask<B> {
	pub(crate) backend: Arc<B>,
	pub(crate) control: JobControl,
	pub(crate) reporter: MaybeArc<EntryDownloadReporter>,
	pub(crate) archive: RemoteFileType<'static>,
	/// The ordinal of the entry chosen.
	pub(crate) ordinal: u64,
	pub(crate) config: ArchiveConfig,
	/// Starts the codec, on a [`Task::Download`](super::codec::Task::Download) of the entry;
	/// called once the job holds its lease.
	pub(crate) start: CodecStart<CodecResult>,
}

/// Runs a download: waits for a job slot, starts the codec, serves it the archive and writes the
/// entry it sends to `writer`, which is closed once the codec returned having checked it.
pub(crate) async fn run_download<B: DriveBackend, W: AsyncWrite + Unpin + MaybeSend>(
	task: DownloadTask<B>,
	writer: &mut W,
) -> Result<EntryDownloadReport, EntryDownloadFailed> {
	let DownloadTask {
		backend,
		control,
		reporter,
		archive,
		ordinal,
		config,
		start,
	} = task;
	let started = start_reading(
		backend,
		Arc::new(archive),
		ReadingJob {
			config: &config,
			control: &control,
			reporter: &reporter,
			reading: EntryDownloadPhase::Reading,
			name: EntryDownloadReport::NAME,
		},
		start,
	)
	.await;
	let (_lease, feed) = match started {
		Ok(started) => started,
		Err((phase, error)) => {
			reporter.finish(phase);
			return Err(EntryDownloadFailed {
				report: EntryDownloadReport::default(),
				error,
			});
		}
	};
	let mut driver = ReadingDriver::new(feed, &control, &reporter);
	let mut sink = EntrySink {
		writer,
		control: &control,
		reporter: &reporter,
		ordinal,
		state: SinkState::Waiting,
		bytes_written: 0,
	};
	let mut outcome = driver.run(&mut sink).await;
	if outcome.is_ok() && driver.fatal.error().is_none() {
		if sink.state == SinkState::Ended {
			// the codec returned, so the data written was checked: only now is it finished
			match control.until_stopping(sink.writer.close()).await {
				Ok(Ok(())) => {}
				Ok(Err(error)) => driver.fatal.record(Arc::new(Error::from(error))),
				Err(Stopped) => {
					reporter.wind_down(&control);
					outcome = Err(Stopped);
				}
			}
		} else {
			driver
				.fatal
				.record(Arc::new(EntryDownloadError::NotAFile.into()));
		}
	}
	let bytes_read = driver.bytes_read();
	reporter.set_bytes_read(bytes_read);
	let checked = driver
		.end
		.as_ref()
		.is_some_and(|end| end.unchecked_entries == 0);
	let (phase, result) = driver
		.fatal
		.end(outcome, &control, EntryDownloadReport::NAME);
	reporter.finish(phase);
	let report = EntryDownloadReport {
		bytes_written: sink.bytes_written,
		bytes_read,
		checked: result.is_ok() && checked,
	};
	match result {
		Ok(()) => Ok(report),
		Err(error) => Err(EntryDownloadFailed { report, error }),
	}
}

/// How far a download's codec has sent its entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SinkState {
	/// Nothing of it yet.
	Waiting,
	/// Its data, written as it comes.
	Writing,
	/// All of it, checked against the archive's checksum for it.
	Ended,
}

/// Takes a download's codec events: the entry chosen, its data written to `writer` as it comes.
struct EntrySink<'d, W> {
	writer: &'d mut W,
	control: &'d JobControl,
	reporter: &'d EntryDownloadReporter,
	/// The entry chosen.
	ordinal: u64,
	state: SinkState,
	bytes_written: u64,
}

impl<W: AsyncWrite + Unpin + MaybeSend> FeedSink for EntrySink<'_, W> {
	async fn take(&mut self, event: WorkerEvent) -> Result<(), Error> {
		match (self.state, event) {
			(
				SinkState::Waiting,
				WorkerEvent::Opened(ArchiveFormat::Zip | ArchiveFormat::SevenZ),
			) => {}
			(
				SinkState::Waiting,
				WorkerEvent::Opened(ArchiveFormat::Tar { .. } | ArchiveFormat::Single { .. }),
			) => return Err(EntryDownloadError::NotIndexed.into()),
			(
				SinkState::Waiting,
				WorkerEvent::Entry(EntryHead {
					ordinal,
					kind: EntryKind::File { size },
					..
				}),
			) if ordinal == self.ordinal => {
				self.state = SinkState::Writing;
				self.reporter.entry_found(size);
			}
			(SinkState::Waiting, WorkerEvent::Skipped(member))
				if member.ordinal == self.ordinal =>
			{
				return Err(EntryDownloadError::Skipped(member.reason).into());
			}
			(SinkState::Waiting, WorkerEvent::Entry(_) | WorkerEvent::Skipped(_)) => {
				return Err(EntryDownloadError::NotAFile.into());
			}
			(SinkState::Writing, WorkerEvent::Data(data)) => {
				// a stop ends the write; the driver sees the job stopping next
				let Ok(written) = self
					.control
					.until_stopping(self.writer.write_all(&data))
					.await
				else {
					return Ok(());
				};
				written?;
				self.bytes_written = self.bytes_written.saturating_add(data.len() as u64);
				self.reporter.wrote(self.bytes_written);
			}
			(SinkState::Writing, WorkerEvent::FileEnd) => self.state = SinkState::Ended,
			// the feed answers asks, and a download's codec sends nothing else
			(
				_,
				WorkerEvent::Ask { .. }
				| WorkerEvent::Opened(_)
				| WorkerEvent::Entry(_)
				| WorkerEvent::Skipped(_)
				| WorkerEvent::Data(_)
				| WorkerEvent::FileEnd
				| WorkerEvent::Link(_)
				| WorkerEvent::Listed(_)
				| WorkerEvent::Head(_),
			) => return Err(unexpected_event()),
		}
		Ok(())
	}
}

#[cfg(test)]
mod tests;

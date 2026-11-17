//! Reading several entries in one ordered pass: one codec job reads the archive in its order and
//! writes each entry asked for to a writer of its own, then ends, giving back its job slot, as
//! soon as the last one is written. What a solid block or a compressed tar stores before and
//! between them is decoded and thrown away. A bare tar's members, which lie in the archive as
//! they are, are read as ranges of it instead, with no job at all.

use std::sync::Arc;

use filen_types::fs::Uuid;
use futures::{AsyncWrite, AsyncWriteExt};

use crate::{
	Error, ErrorKind,
	fs::{
		archive::{
			extract::{
				ArchiveEntryId, EntryDownloadCallback, EntryDownloadError, EntryDownloadPhase,
				EntryDownloadReporter, SolidSkipExceeded,
				codec::{HeldIndex, Selection, Task},
				reading::{CodecInput, ReadJob, download_extraction, start_codec},
			},
			format::ArchiveFormat,
			input::{
				FeedSink, ReadingDriver, ReadingJob, start_reading, unstalled, write_unstalled,
			},
			worker::{EntryHead, EntryKind, WorkerEvent, unexpected_event},
		},
		drive_job::{cancelled, exceeds_limit},
	},
	job::{
		JobControl, Stopped,
		report::{JobFailed, JobReport},
	},
	util::MaybeSend,
};

use super::{Reader, Resolved};

/// What [`ArchiveReader::read_entries`](super::ArchiveReader::read_entries) did, whether it
/// completed, was cancelled or failed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EntriesReadReport {
	/// Entries written whole to their writers, which were closed.
	pub entries: u64,
	/// Bytes written to the writers, in writes that completed.
	pub bytes_written: u64,
	/// Bytes of the archive read, each counted once.
	pub bytes_read: u64,
	/// Every entry written was checked against a checksum the archive has for it. `false` on an
	/// early end, for a 7z entry whose header lists no CRC-32, and for a tar's members, which
	/// have none of their own (a compressed tar's stream checksum covers the whole tar, which a
	/// pass ending at its last entry does not reach).
	pub checked: bool,
}

impl JobReport for EntriesReadReport {
	const NAME: &'static str = "entries read";
}

/// A pass that ended early: cancelled, or stopped by an error. Entries written whole by then
/// were checked and their writers closed; what the entry being written got is unverified, and
/// its writer is left unclosed.
pub type EntriesReadFailed = JobFailed<EntriesReadReport>;

impl Reader {
	/// Reads `entries` in one ordered pass (see the module docs), each written to the writer
	/// `writer_for` gives it (in archive order, with its size when the archive states it), or
	/// decoded and thrown away when it gives none.
	pub(crate) async fn read_entries<W, F, Fut>(
		&self,
		entries: &[ArchiveEntryId],
		mut writer_for: F,
		max_solid_skip: Option<u64>,
		callback: impl EntryDownloadCallback,
		control: JobControl,
	) -> Result<EntriesReadReport, EntriesReadFailed>
	where
		W: AsyncWrite + Unpin + MaybeSend,
		F: FnMut(ArchiveEntryId, Option<u64>) -> Fut + MaybeSend,
		Fut: Future<Output = Result<Option<W>, Error>> + MaybeSend,
	{
		let failed = |error: Arc<Error>| EntriesReadFailed {
			report: EntriesReadReport::default(),
			error,
		};
		self.ensure_open()
			.map_err(|error| failed(Arc::new(error)))?;
		let archive = self.source.id();
		if entries.is_empty() || entries.iter().any(|entry| entry.archive != archive) {
			return Err(failed(Arc::new(Error::custom(
				ErrorKind::InvalidState,
				"the entries to read are none, or of another archive",
			))));
		}
		let selection = Selection::new(
			entries.iter().map(|entry| u64::from(entry.index)),
			Vec::new(),
		);
		if matches!(self.format(), Some(ArchiveFormat::Tar { codec: None }))
			&& self.retained.is_some()
		{
			return self.read_members(&selection, writer_for, &control).await;
		}
		if let Some(limit) = max_solid_skip
			&& let Some(skipped) = self.tar_thrown_away(&selection)
			&& exceeds_limit(skipped, limit)
		{
			// a compressed tar listed: what the pass decodes and throws away is known up front
			return Err(failed(Arc::new(
				SolidSkipExceeded {
					skipped_bytes: skipped,
					limit,
					estimated_packed_bytes: self.source.len(),
				}
				.into(),
			)));
		}
		let left = selection.ordinals().len();
		let ReadJob {
			memory,
			source,
			name,
			expansion_base,
			config,
			settings,
			index,
			chunks,
			control: _,
		} = self.read_job(control.clone());
		let reporter = EntryDownloadReporter::new(callback, source.len());
		let read_end = index.data_end(selection.ordinals());
		let task = Task::Download {
			selection,
			max_solid_skip,
		};
		let input = CodecInput {
			name: &name,
			len: source.len(),
			expansion_base,
		};
		let start = start_codec(input, &config, download_extraction(settings), task, index);
		let started = start_reading(
			memory,
			source,
			ReadingJob {
				config: &config,
				control: &control,
				reporter: &reporter,
				reading: EntryDownloadPhase::Reading,
				name: EntriesReadReport::NAME,
				chunks,
				read_end,
			},
			start,
		)
		.await;
		let (_lease, feed) = match started {
			Ok(started) => started,
			Err((phase, error)) => {
				reporter.finish(phase);
				return Err(failed(error));
			}
		};
		let mut driver = ReadingDriver::new(feed, &control, &reporter);
		let mut sink = EntriesSink {
			archive,
			writer_for: &mut writer_for,
			control: &control,
			reporter: &reporter,
			current: Current::Between,
			left,
			entries: 0,
			bytes_written: 0,
			index: None,
		};
		let outcome = driver.run(&mut sink).await;
		if outcome.is_ok() && driver.fatal.error().is_none() && sink.left > 0 {
			driver
				.fatal
				.record(Arc::new(EntryDownloadError::NotAFile.into()));
		}
		let bytes_read = driver.bytes_read();
		reporter.set_bytes_read(bytes_read);
		let checked = driver
			.end
			.as_ref()
			.is_some_and(|end| end.unchecked_entries == 0)
			&& !matches!(self.format(), Some(ArchiveFormat::Tar { .. }));
		let (phase, result) = driver.fatal.end(outcome, &control, EntriesReadReport::NAME);
		reporter.finish(phase);
		let report = EntriesReadReport {
			entries: sink.entries,
			bytes_written: sink.bytes_written,
			bytes_read,
			checked: result.is_ok() && checked,
		};
		self.hold(sink.index);
		match result {
			Ok(()) => Ok(report),
			Err(error) => Err(EntriesReadFailed { report, error }),
		}
	}

	/// Reads a bare tar's members chosen, in order, each as a range of the archive.
	async fn read_members<W, F, Fut>(
		&self,
		selection: &Selection,
		mut writer_for: F,
		control: &JobControl,
	) -> Result<EntriesReadReport, EntriesReadFailed>
	where
		W: AsyncWrite + Unpin + MaybeSend,
		F: FnMut(ArchiveEntryId, Option<u64>) -> Fut + MaybeSend,
		Fut: Future<Output = Result<Option<W>, Error>> + MaybeSend,
	{
		let mut report = EntriesReadReport::default();
		// one table for the whole pass, held or not, so a member never costs a listing of its own
		let table = match self.tar_table(control).await {
			Ok(table) => table,
			Err(error) => return Err(EntriesReadFailed { report, error }),
		};
		for &ordinal in selection.ordinals() {
			let entry = ArchiveEntryId::of(self.source.id(), ordinal);
			let read = async {
				if control.is_stopping() {
					return Err(cancelled(EntriesReadReport::NAME));
				}
				let Resolved::Mapped { size, .. } = self.tar_access(&table, None, ordinal)? else {
					return Err(Arc::new(EntryDownloadError::NotAFile.into()));
				};
				let asked = control
					.until_stopping(unstalled(writer_for(entry, Some(size))))
					.await
					.map_err(|Stopped| cancelled(EntriesReadReport::NAME))?;
				let Some(mut writer) = asked.and_then(|asked| asked).map_err(Arc::new)? else {
					return Ok(None);
				};
				// a stop ends the read mid-member
				self.read_entry_range(entry, 0, size, &mut writer, control)
					.await?;
				Ok(Some(size))
			};
			match read.await {
				// a member no writer took is passed over, not written
				Ok(None) => {}
				Ok(Some(size)) => {
					report.entries += 1;
					report.bytes_written = report.bytes_written.saturating_add(size);
					report.bytes_read = report.bytes_read.saturating_add(size);
				}
				Err(error) => return Err(EntriesReadFailed { report, error }),
			}
		}
		Ok(report)
	}

	/// Bytes a pass over a compressed tar decodes and throws away to reach the members chosen,
	/// when a listing found where they lie.
	fn tar_thrown_away(&self, selection: &Selection) -> Option<u64> {
		let index = self.retained.as_ref()?.index()?;
		let HeldIndex::Tar(table) = &*index else {
			return None;
		};
		let mut reached = 0u64;
		let mut thrown = 0u64;
		for &ordinal in selection.ordinals() {
			let span = table.members.get(usize::try_from(ordinal).ok()?)?;
			let offset = span.data?.get();
			thrown = thrown.saturating_add(offset.saturating_sub(reached));
			reached = offset.saturating_add(span.size);
		}
		Some(thrown)
	}
}

/// Where a pass is in what the codec sends.
enum Current<W> {
	/// Between entries.
	Between,
	/// In an entry asked for, written to its writer.
	Writing(W),
	/// In an entry asked for whose data no writer takes.
	Dropping,
}

/// Takes a pass's codec events: each entry asked for written to its own writer, closed once
/// the codec checked it.
struct EntriesSink<'p, W, F> {
	archive: Uuid,
	writer_for: &'p mut F,
	control: &'p JobControl,
	reporter: &'p EntryDownloadReporter,
	current: Current<W>,
	/// Entries asked for not sent yet.
	left: usize,
	entries: u64,
	bytes_written: u64,
	index: Option<Box<HeldIndex>>,
}

impl<W, F, Fut> FeedSink for EntriesSink<'_, W, F>
where
	W: AsyncWrite + Unpin + MaybeSend,
	F: FnMut(ArchiveEntryId, Option<u64>) -> Fut + MaybeSend,
	Fut: Future<Output = Result<Option<W>, Error>> + MaybeSend,
{
	async fn take(&mut self, event: WorkerEvent) -> Result<(), Error> {
		match (&mut self.current, event) {
			(Current::Between, WorkerEvent::Opened(_)) => {}
			(
				Current::Between,
				WorkerEvent::Entry(EntryHead {
					ordinal,
					kind: EntryKind::File { size },
					..
				}),
			) => {
				let entry = ArchiveEntryId::of(self.archive, ordinal);
				self.left = self.left.saturating_sub(1);
				// a stop ends the wait for a writer; the driver sees the job stopping next
				let Ok(asked) = self
					.control
					.until_stopping(unstalled((self.writer_for)(entry, size)))
					.await
				else {
					return Ok(());
				};
				self.current = match asked?? {
					Some(writer) => Current::Writing(writer),
					None => Current::Dropping,
				};
			}
			(Current::Between, WorkerEvent::Skipped(member)) => {
				return Err(EntryDownloadError::Skipped(member.reason).into());
			}
			(Current::Between, WorkerEvent::Entry(_) | WorkerEvent::Link(_)) => {
				return Err(EntryDownloadError::NotAFile.into());
			}
			(Current::Writing(writer), WorkerEvent::Data(data)) => {
				// a stop ends the write; the driver sees the job stopping next
				let Ok(written) = self
					.control
					.until_stopping(write_unstalled(writer, &data))
					.await
				else {
					return Ok(());
				};
				written?;
				self.bytes_written = self.bytes_written.saturating_add(data.len() as u64);
				self.reporter.wrote(self.bytes_written);
			}
			(Current::Dropping, WorkerEvent::Data(_)) => {}
			(Current::Writing(writer), WorkerEvent::FileEnd) => {
				// the codec sends the end once the data matched its checksum
				let Ok(closed) = self.control.until_stopping(unstalled(writer.close())).await
				else {
					return Ok(());
				};
				closed??;
				self.entries += 1;
				self.current = Current::Between;
			}
			(Current::Dropping, WorkerEvent::FileEnd) => self.current = Current::Between,
			(_, WorkerEvent::Indexed(index)) => self.index = Some(index),
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

//! Listing an archive's entries without extracting any. See
//! [`Client::list_archive`](crate::auth::Client::list_archive).

use std::{sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use filen_macros::js_type;

use crate::{
	Error,
	consts::CALLBACK_INTERVAL,
	fs::{
		HasUUID,
		archive::{
			config::ArchiveConfig,
			format::ArchiveFormat,
			input::{CodecFeed, Fed, ReadingJob, start_reading},
			password::ArchivePassword,
			worker::{WorkerEvent, WorkerLink, codec_failed, worker_died},
		},
		drive_job::{Fatal, backend::DriveBackend},
		file::enums::RemoteFileType,
	},
	job::{
		self, JobControl, Stopped,
		report::{JobFailed, JobPhase, JobReport, JobState, Progress, RunCore, Snapshot, Units},
	},
	util::{MaybeArc, MaybeSendSync, sleep},
};

use super::{
	DuplicateEntries, ExpansionLimit, ExtractConfig, ExtractSkipReason,
	codec::ArchiveEnd,
	engine::CodecResult,
	report::{ArchiveEntryId, CALLBACK_BATCH, RunState},
};

/// What kind of item an archive entry is.
#[derive(Debug, Clone, PartialEq, Eq)]
#[js_type(export, no_deser, tagged, camel_case_fields, no_default)]
pub enum ArchiveEntryKind {
	/// A file.
	File,
	/// A directory.
	Dir,
	/// A symbolic link to `target`, as stored and cut to at most 4096 bytes.
	Symlink {
		/// The link's target as stored, at most 4096 bytes.
		target: String,
	},
	/// A tar hard link: a second name for the earlier file at `target`, as stored and cut to at
	/// most 4096 bytes. Extracted as a copy of that file, when it was extracted: a partial
	/// extraction ([`ExtractRequest::Entries`](super::ExtractRequest::Entries)) has to take the
	/// entry `target_id` too, and when that is a hard link as well, its own target in turn.
	/// `target_id` is `None` when the link names no file the archive stores before it, and is
	/// skipped.
	Hardlink {
		/// The path of the file it names, as stored, at most 4096 bytes.
		target: String,
		/// That file's entry; `None` when the archive stores no such file before the link.
		target_id: Option<ArchiveEntryId>,
	},
	/// A device node or FIFO.
	Device,
	/// Something else the SDK does not extract: a tar multivolume continuation or vendor type, or
	/// a 7z deletion marker.
	Other,
}

/// An entry of an archive, and what extracting it would do.
#[derive(Debug, Clone, PartialEq, Eq)]
#[js_type(export, no_deser, no_default)]
// Independent facts a listing reports about one entry, each a field of the bindings' record:
// they are not one state, so no enum stands in for them.
#[allow(clippy::struct_excessive_bools)]
pub struct ArchiveEntry {
	/// What extracting some entries (`ExtractRequest::Entries`, `extractArchiveEntries`) takes
	/// to extract it.
	pub id: ArchiveEntryId,
	/// Its path as the archive stores it, cut to at most 4096 bytes.
	pub stored_path: String,
	/// Whether `stored_path` was cut.
	pub stored_path_truncated: bool,
	/// Where extracting it puts it below the extraction's root, as drive names: before the
	/// keep-both names a collision (with another entry, or an item in the destination) calls
	/// for. `None` when its path cannot be extracted (see `skip`).
	pub path: Option<String>,
	pub kind: ArchiveEntryKind,
	/// The size of the file it extracts to: as the archive states it, for a hard link its
	/// target's, and for a single compressed file what it decodes to, in bytes. `None` for a
	/// directory.
	pub size: Option<u64>,
	/// When it was last changed, as the archive states it.
	#[cfg_attr(
		feature = "wasm-full",
		tsify(type = "bigint", optional),
		serde(
			with = "filen_types::serde::time::optional",
			skip_serializing_if = "Option::is_none"
		)
	)]
	pub modified: Option<DateTime<Utc>>,
	/// Its data is encrypted.
	pub encrypted: bool,
	/// How a zip's or 7z's entry is compressed, for display (`Deflate`, `LZMA2`, `BCJ+LZMA`,
	/// `method 98`); `None` for an entry without data, and for a tar's members or a single file,
	/// which the archive's own compression covers (see the listing's `format`).
	pub method: Option<String>,
	/// Why extracting it would skip it; `None` for an entry an extraction creates (or may: an
	/// AppleDouble file told by its name alone). A link's target is in `kind`, and left empty
	/// here.
	pub skip: Option<ExtractSkipReason>,
	/// Its stored path was made into valid drive names (an extraction reports it renamed, for
	/// `PathRewritten`).
	pub path_rewritten: bool,
	/// Its path reads as something it is not (an extraction reports it as a misleading name).
	pub misleading_name: bool,
	/// macOS metadata: a `__MACOSX` folder, an AppleDouble file (named `._name`, or any file in
	/// a `__MACOSX` folder), or a tar's hard link to one. A file is told by its first bytes,
	/// which a listing leaving metadata out reads as an extraction does: a tar's always, a
	/// zip's or 7z's while it reads little enough of the archive for it (as a link's target), a
	/// single file's never. One it did not read is marked by its path alone and `skip` leaves it
	/// out: an extraction extracts it when it is an ordinary file after all. A `__MACOSX` folder
	/// is marked by its path. Leaving metadata out, the listing decides one as an extraction
	/// does, once every entry was listed: skipped when everything in it is left out, not when it
	/// holds anything of the user's, or nothing. See the extraction's `skip_mac_metadata`.
	pub mac_metadata: bool,
}

/// What a listing found out about the archive's password.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[js_type(export, no_deser, no_default)]
pub enum PasswordCheck {
	/// Nothing in the archive is encrypted.
	NotNeeded,
	/// Entries are encrypted, and no password was given.
	Required,
	/// The password given opened an encrypted entry, or the encrypted index.
	Right,
	/// The password given does not open the entries.
	Wrong,
	/// A password was given, and no encrypted entry was small enough to check it on up front:
	/// an extraction checks it as it reads them.
	Unchecked,
}

/// What a listing counts, over every entry of the archive.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[js_type(export, no_deser, no_default)]
pub struct ListTotals {
	/// Entries the archive holds.
	pub entries: u64,
	/// Directory entries an extraction creates (those only implied by the paths below them are
	/// left out).
	pub dirs: u64,
	/// Files an extraction creates.
	pub files: u64,
	/// Bytes of the files an extraction creates, as the archive states them.
	pub bytes: u64,
	/// Entries an extraction skips.
	pub skipped: u64,
	/// Bytes of the skipped entries, as the archive states them.
	pub bytes_skipped: u64,
}

impl ListTotals {
	/// Counts `entry` in. The sizes are the archive's word, so their sums saturate rather than
	/// overflow, as an extraction's do.
	fn count(&mut self, entry: &ArchiveEntry) {
		self.entries += 1;
		let size = entry.size.unwrap_or(0);
		match (&entry.skip, &entry.kind) {
			(Some(_), _) => {
				self.skipped += 1;
				self.bytes_skipped = self.bytes_skipped.saturating_add(size);
			}
			(None, ArchiveEntryKind::Dir) => self.dirs += 1,
			(None, _) => {
				self.files += 1;
				self.bytes = self.bytes.saturating_add(size);
			}
		}
	}
}

/// How a listing reads an archive, and which extraction's verdicts it shows: those of an
/// extraction with the same settings.
#[derive(Debug, Clone)]
pub struct ListConfig {
	/// As [`ExtractConfig::expansion_limit`]: a 7z link's target is read only while the
	/// archive states no more than this allows.
	pub expansion_limit: Option<ExpansionLimit>,
	/// As [`ExtractConfig::skip_mac_metadata`]: whether the entries left out as macOS metadata
	/// are listed skipped.
	pub skip_mac_metadata: bool,
	/// As [`ExtractConfig::password`]: checked on the index or an entry when it can be (see
	/// [`ArchiveListing::password`]).
	pub password: Option<ArchivePassword>,
}

impl Default for ListConfig {
	fn default() -> Self {
		let ExtractConfig {
			expansion_limit,
			skip_mac_metadata,
			password,
			..
		} = ExtractConfig::default();
		Self {
			expansion_limit,
			skip_mac_metadata,
			password,
		}
	}
}

impl From<ListConfig> for ExtractConfig {
	/// An extraction with the listing's settings, and neither cap.
	fn from(config: ListConfig) -> Self {
		Self {
			max_bytes: None,
			max_items: None,
			expansion_limit: config.expansion_limit,
			skip_mac_metadata: config.skip_mac_metadata,
			password: config.password,
		}
	}
}

/// Most entries an [`ArchiveListing`] keeps; the callback receives every one.
pub const MAX_LISTED_ENTRIES: usize = 10_000;

/// Most bytes of text (paths, targets, methods) the entries an [`ArchiveListing`] keeps may hold
/// in all, as one entry can hold 8 KiB of paths: once they would pass it, the rest are only
/// counted.
pub const MAX_LISTED_BYTES: usize = 16 << 20;

/// What a listing found: the archive, and its entries in the order of its index (a tar's in the
/// order it stores them), except that the folders in `__MACOSX` folders come last when macOS
/// metadata is left out (see [`ArchiveEntry::mac_metadata`]).
///
/// An archive may hold a million entries: the listing keeps the first [`MAX_LISTED_ENTRIES`], as
/// long as their text fits [`MAX_LISTED_BYTES`], and counts the rest, which the callback
/// receives in batches as they are read, so an app keeps what it shows without the SDK building
/// every entry up front.
#[derive(Debug, Clone)]
pub struct ArchiveListing {
	/// `None` when the listing ended before it could tell.
	pub format: Option<ArchiveFormat>,
	/// What the listing found out about the password.
	pub password: PasswordCheck,
	/// The entries kept, up to [`MAX_LISTED_ENTRIES`].
	pub entries: Vec<ArchiveEntry>,
	/// Entries the callback received that `entries` leaves out.
	pub omitted_entries: u64,
	/// Counts over every entry listed, those left out of `entries` included.
	pub totals: ListTotals,
	/// Bytes of the archive that belong to no entry (see
	/// [`ExtractReport::unaccounted_bytes`](super::ExtractReport::unaccounted_bytes)).
	pub unaccounted_bytes: u64,
	/// Names a zip lists more than once: only the last entry of each is listed, and extracted.
	pub duplicates: Option<DuplicateEntries>,
}

impl JobReport for ArchiveListing {
	const NAME: &'static str = "listing";
}

/// A listing that ended early: cancelled, or stopped by an error (a damaged archive, a wrong
/// password for a 7z whose index is encrypted); it holds the entries read until then.
pub type ListFailed = JobFailed<ArchiveListing>;

/// Where a listing is. The last three are where it ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[js_type(export, no_deser, no_default)]
pub enum ListPhase {
	/// Waiting for another archive job to finish; nothing is held meanwhile.
	WaitingForWorker,
	/// Reading the archive: a zip's or 7z's index, or the whole of a tar or compressed file.
	Reading,
	/// Listed every entry.
	Done,
	/// Ended early by a cancel.
	Cancelled,
	/// Ended early by an error.
	Failed,
}

impl JobPhase for ListPhase {
	const DONE: Self = Self::Done;
	const CANCELLED: Self = Self::Cancelled;
	const FAILED: Self = Self::Failed;
}

/// One progress callback of a listing.
#[derive(Debug, Clone)]
pub struct ListUpdate {
	/// Where the listing is.
	pub phase: ListPhase,
	/// Whether it runs, is paused or winds down.
	pub run_state: RunState,
	/// Bytes of the archive read so far, of `archive_bytes`.
	pub bytes_read: u64,
	/// The archive's size in bytes.
	pub archive_bytes: u64,
	/// Entries listed so far.
	pub entries: u64,
	/// Bytes of the archive read per second, over the last 10 seconds of running time; `None`
	/// until there is a rate.
	pub bytes_per_second: Option<u64>,
	/// The time left to read the rest of the archive at that rate; `None` while there is no rate
	/// or the listing winds down, zero once it ended.
	pub eta: Option<Duration>,
	/// Time spent running, paused time left out.
	pub active_time: Duration,
}

/// Receives a listing's entries and progress. All calls come from the one job, in order.
pub trait ListCallback: MaybeSendSync + 'static {
	/// Entries, in batches as they are read, each batch before the update that counts it.
	fn on_entries_batch(&self, entries: Vec<ArchiveEntry>);
	/// The listing's progress, throttled; the last one comes once the listing ended.
	fn on_update(&self, update: ListUpdate);
}

impl<T: ListCallback + ?Sized> ListCallback for Arc<T> {
	fn on_entries_batch(&self, entries: Vec<ArchiveEntry>) {
		(**self).on_entries_batch(entries);
	}

	fn on_update(&self, update: ListUpdate) {
		(**self).on_update(update);
	}
}

/// What a listing counts, next to the job-agnostic [`RunCore`].
pub(crate) struct ListState {
	core: RunCore<(), ListPhase>,
	archive_bytes: u64,
	bytes_read: u64,
	entries: u64,
	/// Entries not delivered yet: they go out in batches, each before the next update.
	pending: Vec<ArchiveEntry>,
	/// The job ended: whatever it did not read, it never will.
	ended: bool,
}

impl JobState for ListState {
	type Phase = ListPhase;
	type Event = ();
	type Callback = dyn ListCallback;

	fn core(&mut self) -> &mut RunCore<(), ListPhase> {
		&mut self.core
	}

	fn progress(&self) -> Progress {
		Progress {
			bytes_done: self.bytes_read,
			units: Units {
				done: self.bytes_read,
				settled: if self.ended {
					self.archive_bytes
				} else {
					self.bytes_read
				},
				total: self.archive_bytes,
			},
		}
	}

	fn deliver(&mut self, callback: &dyn ListCallback, snapshot: Snapshot<(), ListPhase>) {
		if !self.pending.is_empty() {
			callback.on_entries_batch(std::mem::take(&mut self.pending));
		}
		callback.on_update(ListUpdate {
			phase: snapshot.phase,
			run_state: snapshot.run_state,
			bytes_read: self.bytes_read,
			archive_bytes: self.archive_bytes,
			entries: self.entries,
			bytes_per_second: snapshot.bytes_per_second,
			eta: snapshot.eta,
			active_time: snapshot.active_time,
		});
	}

	fn settle(&mut self) {
		self.ended = true;
	}
}

/// A listing's reporter: the job-agnostic [`job::report::Reporter`] over a [`ListState`].
pub(crate) type ListReporter = job::report::Reporter<ListState>;

impl ListReporter {
	pub(crate) fn new(callback: impl ListCallback, archive_bytes: u64) -> MaybeArc<Self> {
		Self::from_parts(
			ListState {
				core: RunCore::new(ListPhase::WaitingForWorker),
				archive_bytes,
				bytes_read: 0,
				entries: 0,
				pending: Vec::new(),
				ended: false,
			},
			Box::new(callback),
		)
	}

	pub(crate) fn set_bytes_read(&self, bytes_read: u64) {
		self.with_state(|state| {
			if state.bytes_read != bytes_read {
				state.bytes_read = bytes_read;
				state.core.mark_changed();
			}
		});
	}

	/// Delivered in a batch of up to [`CALLBACK_BATCH`] entries right before the next update.
	pub(crate) fn listed(&self, entry: ArchiveEntry) {
		let mut full = false;
		self.with_state(|state| {
			state.entries += 1;
			state.pending.push(entry);
			state.core.mark_changed();
			full = state.pending.len() >= CALLBACK_BATCH;
		});
		if full {
			self.flush_then_call(|_| {});
		}
	}
}

/// Adds `entry` to `listing`: kept while it holds fewer than [`MAX_LISTED_ENTRIES`] whose text,
/// `kept_bytes` so far, fits [`MAX_LISTED_BYTES`]; counted either way.
fn add_entry(listing: &mut ArchiveListing, kept_bytes: &mut usize, entry: &ArchiveEntry) {
	listing.totals.count(entry);
	let bytes = entry.text_bytes();
	if listing.omitted_entries == 0
		&& listing.entries.len() < MAX_LISTED_ENTRIES
		&& *kept_bytes + bytes <= MAX_LISTED_BYTES
	{
		*kept_bytes += bytes;
		listing.entries.push(entry.clone());
	} else {
		listing.omitted_entries += 1;
	}
}

impl ArchiveEntry {
	/// The bytes of text it holds.
	pub(crate) fn text_bytes(&self) -> usize {
		let target = match &self.kind {
			ArchiveEntryKind::Symlink { target } | ArchiveEntryKind::Hardlink { target, .. } => {
				target.len()
			}
			_ => 0,
		};
		self.stored_path.len()
			+ self.path.as_ref().map_or(0, String::len)
			+ self.method.as_ref().map_or(0, String::len)
			+ target
	}
}

/// What [`run_list`] needs.
pub(crate) struct ListTask<B> {
	pub(crate) backend: Arc<B>,
	pub(crate) control: JobControl,
	pub(crate) reporter: MaybeArc<ListReporter>,
	pub(crate) archive: RemoteFileType<'static>,
	pub(crate) config: ArchiveConfig,
	/// Starts the codec; called once the job holds its lease and memory floor.
	pub(crate) start: Box<dyn FnOnce() -> Result<WorkerLink<CodecResult>, Error> + Send>,
}

/// Runs a listing: waits for a job slot, starts the codec, and serves it the archive, collecting
/// the entries it lists.
pub(crate) async fn run_list<B: DriveBackend>(
	task: ListTask<B>,
) -> Result<ArchiveListing, ListFailed> {
	let ListTask {
		backend,
		control,
		reporter,
		archive,
		config,
		start,
	} = task;
	let mut listing = ArchiveListing {
		format: None,
		password: PasswordCheck::NotNeeded,
		entries: Vec::new(),
		omitted_entries: 0,
		totals: ListTotals::default(),
		unaccounted_bytes: 0,
		duplicates: None,
	};
	let started = start_reading(
		backend,
		Arc::new(archive),
		ReadingJob {
			config: &config,
			control: &control,
			reporter: &reporter,
			reading: ListPhase::Reading,
			name: ArchiveListing::NAME,
		},
		start,
	)
	.await;
	let (_lease, feed) = match started {
		Ok(started) => started,
		Err((phase, error)) => {
			reporter.finish(phase);
			return Err(ListFailed {
				report: listing,
				error,
			});
		}
	};
	let mut lister = Lister {
		feed,
		kept_bytes: 0,
		control,
		reporter,
		config,
		end: None,
		fatal: Fatal::default(),
	};
	let outcome = lister.run(&mut listing).await;
	let Lister {
		control,
		reporter,
		fatal,
		end,
		..
	} = lister;
	if let Some(end) = end {
		listing.password = end.password;
		listing.unaccounted_bytes = end.unaccounted_bytes;
		listing.duplicates = end.duplicates;
	}
	let (phase, result) = fatal.end(outcome, &control, ArchiveListing::NAME);
	reporter.finish(phase);
	match result {
		Ok(()) => Ok(listing),
		Err(error) => Err(ListFailed {
			report: listing,
			error,
		}),
	}
}

/// The async driver of a listing's codec: it only ever reads the archive, and creates nothing.
struct Lister<B> {
	feed: CodecFeed<B, ArchiveEnd>,
	/// The text of the entries the listing keeps.
	kept_bytes: usize,
	control: JobControl,
	reporter: MaybeArc<ListReporter>,
	config: ArchiveConfig,
	/// How the archive ended, once the codec returned it.
	end: Option<ArchiveEnd>,
	fatal: Fatal,
}

impl<B: DriveBackend> Lister<B> {
	/// Serves the codec until it has returned; `Err` when stopped.
	async fn run(&mut self, listing: &mut ArchiveListing) -> Result<(), Stopped> {
		loop {
			let pause_requested = self.control.is_pause_requested();
			self.reporter.set_pause_requested(pause_requested);
			if self.control.is_stopping() {
				// nothing a listing does has to finish
				self.feed.drop_all();
				self.reporter.wind_down(&self.control);
				return Err(Stopped);
			}
			if self.feed.events_closed() && (self.end.is_some() || self.fatal.error().is_some()) {
				self.feed.release();
				return Ok(());
			}
			if pause_requested {
				if !self.feed.fetching() {
					self.feed
						.wait_out_pause(&self.reporter, &self.control, &self.config)
						.await?;
					continue;
				}
			} else {
				self.feed.advance(&self.reporter.ops());
				self.report_bytes_read();
			}
			tokio::select! {
				biased;
				() = self.control.stopping() => {},
				() = self.control.pause_changed(pause_requested) => {},
				fed = self.feed.next(!pause_requested, true) => match fed {
					Fed::Fetched(result) => {
						if let Err(error) = result {
							self.fatal.stop(Arc::new(error), &self.control, &*self.reporter);
						}
						self.report_bytes_read();
					}
					Fed::Asked => self.report_bytes_read(),
					Fed::Event(event) => self.on_event(event, listing),
					Fed::EventsClosed => {}
					Fed::Finished(result) => self.codec_finished(result),
				},
				() = sleep(CALLBACK_INTERVAL) => self.tick(pause_requested),
			}
		}
	}

	fn on_event(&mut self, event: WorkerEvent, listing: &mut ArchiveListing) {
		match event {
			WorkerEvent::Opened(format) => listing.format = Some(format),
			WorkerEvent::Listed(entry) => {
				add_entry(listing, &mut self.kept_bytes, &entry);
				self.reporter.listed(*entry);
			}
			other => debug_assert!(false, "a listing codec sent {other:?}"),
		}
	}

	fn codec_finished(&mut self, result: CodecResult) {
		match result {
			Ok(end) => self.end = Some(end),
			Err(error) => {
				// an error the driver caused (it failed a fetch) is already the job's
				let archive = self.feed.archive().uuid();
				if codec_failed(archive, &error, self.fatal.error().is_some()) {
					self.fatal.record(Arc::new(error));
				}
			}
		}
	}

	/// Reports the bytes of the archive the codec has read.
	fn report_bytes_read(&self) {
		self.reporter.set_bytes_read(self.feed.bytes_read());
	}

	fn tick(&mut self, pause_requested: bool) {
		self.report_bytes_read();
		self.reporter.tick();
		if self.feed.give_up_if_stalled(pause_requested) {
			self.fatal
				.stop(Arc::new(worker_died()), &self.control, &*self.reporter);
		}
	}
}

#[cfg(test)]
mod tests;

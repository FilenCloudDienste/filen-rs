//! The jobs an archive reader runs, one per call: opening the archive, listing it and
//! downloading an entry, each handed what the reader holds (the index, its kept chunks) and
//! handing back the index it read. [`Client::list_archive`](crate::auth::Client::list_archive)
//! and [`Client::download_archive_entry`](crate::auth::Client::download_archive_entry) run the
//! same jobs, for a reader that keeps nothing.

use std::sync::Arc;

use futures::AsyncWrite;

use crate::{
	Error,
	fs::{
		HasName, HasUUID,
		archive::{
			config::ArchiveConfig,
			format::ArchiveFormat,
			input::{FeedSink, ReadingDriver, ReadingJob, start_reading},
			retained::Retained,
			worker::{self, CodecStart, WorkerEvent, unexpected_event},
		},
		drive_job::backend::DriveBackend,
		file::{enums::RemoteFileType, traits::HasFileInfo},
	},
	job::{JobControl, report::JobReport},
	util::MaybeSend,
};

use super::{
	ArchiveEntryId, EntryDownloadCallback, EntryDownloadFailed, EntryDownloadReport, ExtractConfig,
	ListCallback, ListConfig, ListFailed, ListReport,
	codec::{CodecLimits, HeldIndex, IndexUse, StreamJob, Task, extract_stream, open_archive},
	download::{DownloadTask, EntryDownloadReporter, choose_entry, run_download},
	engine::CodecResult,
	list::{ListPhase, ListReporter, ListTask, Listed, run_list},
};

/// What one job of a reader reads, and how.
pub(crate) struct ReadJob<B> {
	pub(crate) backend: Arc<B>,
	pub(crate) archive: RemoteFileType<'static>,
	/// The client's archive settings.
	pub(crate) config: ArchiveConfig,
	/// The reader's.
	pub(crate) settings: ListConfig,
	pub(crate) index: IndexUse,
	/// Where the archive's chunks read last are kept for the reader's next jobs.
	pub(crate) chunks: Option<Arc<Retained>>,
	pub(crate) control: JobControl,
}

/// What opening found.
pub(crate) struct Opened {
	pub(crate) format: ArchiveFormat,
	/// A zip's or 7z's index.
	pub(crate) index: Option<Box<HeldIndex>>,
}

/// Opens the archive: what it is, and a zip's or 7z's index (see
/// [`open_archive`](super::codec::open_archive)).
pub(crate) async fn open<B: DriveBackend>(
	job: ReadJob<B>,
	callback: impl ListCallback,
) -> Result<Opened, ListFailed> {
	let ReadJob {
		backend,
		archive,
		config,
		settings,
		index: _,
		chunks,
		control,
	} = job;
	let reporter = ListReporter::new(callback, archive.size());
	let codec = stream_job(
		&archive,
		&config,
		settings.into_extraction(),
		Task::List {
			archive: archive.uuid(),
		},
		IndexUse::ReadAndReturn,
	);
	let start: CodecStart<CodecResult> =
		Box::new(move || worker::start(move |port| open_archive(&port, codec)));
	let failed = |error| ListFailed {
		report: ListReport::empty(),
		error,
	};
	let started = start_reading(
		backend,
		Arc::new(archive),
		ReadingJob {
			config: &config,
			control: &control,
			reporter: &reporter,
			reading: ListPhase::Reading,
			name: ListReport::NAME,
			chunks,
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
	let mut sink = OpenSink::default();
	let outcome = driver.run(&mut sink).await;
	let (phase, result) = driver.fatal.end(outcome, &control, ListReport::NAME);
	reporter.finish(phase);
	result.map_err(failed)?;
	match sink.format {
		Some(format) => Ok(Opened {
			format,
			index: sink.index,
		}),
		None => Err(failed(Arc::new(unexpected_event()))),
	}
}

/// Takes an opening codec's events: what the archive is, and its index.
#[derive(Default)]
struct OpenSink {
	format: Option<ArchiveFormat>,
	index: Option<Box<HeldIndex>>,
}

impl FeedSink for OpenSink {
	async fn take(&mut self, event: WorkerEvent) -> Result<(), Error> {
		match event {
			WorkerEvent::Opened(format) => self.format = Some(format),
			WorkerEvent::Indexed(index) => self.index = Some(index),
			WorkerEvent::Ask { .. }
			| WorkerEvent::Entry(_)
			| WorkerEvent::Skipped(_)
			| WorkerEvent::Data(_)
			| WorkerEvent::FileEnd
			| WorkerEvent::Link(_)
			| WorkerEvent::Listed(_)
			| WorkerEvent::Head(_) => return Err(unexpected_event()),
		}
		Ok(())
	}
}

/// Lists the archive.
pub(crate) async fn list<B: DriveBackend>(
	job: ReadJob<B>,
	task: Task,
	callback: impl ListCallback,
) -> Result<Listed, ListFailed> {
	let ReadJob {
		backend,
		archive,
		config,
		settings,
		index,
		chunks,
		control,
	} = job;
	let start = start_codec(&archive, &config, settings.into_extraction(), task, index);
	run_list(ListTask {
		backend,
		control,
		reporter: ListReporter::new(callback, archive.size()),
		archive,
		config,
		start,
		chunks,
	})
	.await
}

/// Downloads `entry`; what the download did, and the index it read.
pub(crate) async fn download<B: DriveBackend, W: AsyncWrite + Unpin + MaybeSend>(
	job: ReadJob<B>,
	entry: ArchiveEntryId,
	writer: &mut W,
	max_solid_skip: Option<u64>,
	callback: impl EntryDownloadCallback,
) -> (
	Result<EntryDownloadReport, EntryDownloadFailed>,
	Option<Box<HeldIndex>>,
) {
	let ReadJob {
		backend,
		archive,
		config,
		settings,
		index,
		chunks,
		control,
	} = job;
	let reporter = EntryDownloadReporter::new(callback, archive.size());
	let (archive, selection) = match choose_entry(archive, entry, &reporter) {
		Ok(chosen) => chosen,
		Err(failed) => return (Err(failed), None),
	};
	let extraction = download_extraction(settings);
	let task = Task::Download {
		selection,
		max_solid_skip,
	};
	let start = start_codec(&archive, &config, extraction, task, index);
	run_download(
		DownloadTask {
			backend,
			control,
			reporter,
			archive,
			ordinal: u64::from(entry.index),
			config,
			start,
			chunks,
		},
		writer,
	)
	.await
}

/// The settings a job's codec reads `archive` under: the client's `archives`, the job's
/// `config`, its `task`, and where it gets the index from.
pub(super) fn stream_job(
	archive: &RemoteFileType<'static>,
	archives: &ArchiveConfig,
	config: ExtractConfig,
	task: Task,
	index: IndexUse,
) -> StreamJob {
	StreamJob {
		name: archive.name().unwrap_or_default().to_owned(),
		len: archive.size(),
		limits: CodecLimits {
			decoder_memory: archives.codec_mem_budget(),
			max_members: archives.max_members,
			expansion: config.expansion_limit,
			max_index_bytes: archives.max_index_bytes,
			max_bytes: config.max_bytes,
		},
		password: config.password,
		skip_mac_metadata: config.skip_mac_metadata,
		task,
		index,
	}
}

/// Starts the codec that reads `archive` for `task` (see [`stream_job`]).
pub(crate) fn start_codec(
	archive: &RemoteFileType<'static>,
	archives: &ArchiveConfig,
	config: ExtractConfig,
	task: Task,
	index: IndexUse,
) -> CodecStart<CodecResult> {
	let job = stream_job(archive, archives, config, task, index);
	Box::new(move || worker::start(move |port| extract_stream(&port, job)))
}

/// What a download's codec reads under, of a reader's `settings`: an entry is chosen by its id,
/// so macOS metadata is not left out.
pub(crate) fn download_extraction(settings: ListConfig) -> ExtractConfig {
	ExtractConfig {
		skip_mac_metadata: false,
		..settings.into_extraction()
	}
}

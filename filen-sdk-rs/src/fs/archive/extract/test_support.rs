//! What the extraction and listing tests share: the archive in a fake drive, the settings a test
//! job runs with, and a listing of it with the real codec.

use std::sync::{Arc, Mutex};

use filen_types::{crypto::Blake3Hash, fs::Uuid};
use tokio::task::JoinHandle;

use crate::{
	auth::http::ClientConfig,
	fs::{
		HasName, HasUUID,
		archive::{
			config::{ArchiveConfig, CODEC_MEM_BUDGET},
			password::ArchivePassword,
			worker,
		},
		drive_job::test_support::{FakeBackend, remote_file},
		file::{enums::RemoteFileType, traits::HasFileInfo},
	},
	job::JobControl,
	util::MaybeArc,
};

use super::{
	ArchiveEntry, ExpansionLimit, ExtractRoot, ListCallback, ListFailed, ListReport, ListUpdate,
	codec::{CodecLimits, Selection, StreamJob, Task, extract_stream},
	engine::ArchiveDisposal,
	list::{ListReporter, ListTask, run_list},
};

/// The archive every test extracts, the directory it is in, and the one it is extracted into:
/// fixed, so a failing test runs the same way again.
pub(super) const ARCHIVE: Uuid = Uuid::from_u128(0xA);
pub(super) const ARCHIVE_PARENT: Uuid = Uuid::from_u128(0xA0);
pub(super) const DESTINATION: Uuid = Uuid::from_u128(0xD);

pub(super) struct Setup {
	pub(super) backend: Arc<FakeBackend>,
	pub(super) destination: Uuid,
	pub(super) archive: RemoteFileType<'static>,
}

pub(super) fn setup(name: &str, bytes: Vec<u8>, configure: impl FnOnce(&mut FakeBackend)) -> Setup {
	setup_in(DESTINATION, name, bytes, None, configure)
}

/// [`setup`] for a job that extracts into `destination`, with `hash` in the archive's metadata.
pub(super) fn setup_in(
	destination: Uuid,
	name: &str,
	bytes: Vec<u8>,
	hash: Option<Blake3Hash>,
	configure: impl FnOnce(&mut FakeBackend),
) -> Setup {
	let archive = remote_file(ARCHIVE, ARCHIVE_PARENT, name, &bytes, hash);
	let mut backend = FakeBackend::new(destination);
	backend.contents.insert(archive.uuid(), bytes);
	configure(&mut backend);
	Setup {
		backend: Arc::new(backend),
		destination,
		archive,
	}
}

/// Members a test archive may have, for the codec and the driver alike.
pub(super) const MAX_MEMBERS: u64 = 2000;

/// The archive settings of a test job, with [`MAX_MEMBERS`].
pub(super) fn test_config() -> ArchiveConfig {
	let mut config = ArchiveConfig::new(&ClientConfig::default());
	config.max_members = MAX_MEMBERS;
	config
}

pub(super) struct Options {
	pub(super) root: ExtractRoot,
	pub(super) control: JobControl,
	pub(super) max_bytes: Option<u64>,
	pub(super) max_items: Option<u64>,
	pub(super) dispose: Option<ArchiveDisposal>,
	pub(super) password: Option<ArchivePassword>,
	/// Shared between jobs that compete for its slots.
	pub(super) config: ArchiveConfig,
	/// Every entry, or those a partial extraction chose.
	pub(super) selection: Option<Selection>,
	pub(super) expansion: Option<ExpansionLimit>,
}

impl Default for Options {
	fn default() -> Self {
		Self {
			root: ExtractRoot::NewFolder { name: None },
			control: JobControl::default(),
			max_bytes: None,
			max_items: None,
			dispose: None,
			password: None,
			config: test_config(),
			selection: None,
			expansion: Some(ExpansionLimit::default()),
		}
	}
}

/// What the real codec is given for `setup`'s archive.
pub(super) fn stream_job(setup: &Setup, options: &Options) -> StreamJob {
	StreamJob {
		name: setup.archive.name().unwrap().to_owned(),
		len: setup.archive.size(),
		limits: CodecLimits {
			decoder_memory: CODEC_MEM_BUDGET,
			max_members: options.config.max_members,
			expansion: options.expansion,
			max_index_bytes: 32 << 20,
			max_bytes: options.max_bytes,
		},
		password: options.password.clone(),
		skip_mac_metadata: true,
		task: Task::Extract(options.selection.clone()),
	}
}

#[derive(Default)]
pub(super) struct ListRecorder {
	/// The size of each `on_entries_batch` call, and every entry.
	pub(super) batches: Mutex<Vec<usize>>,
	pub(super) entries: Mutex<Vec<ArchiveEntry>>,
	pub(super) updates: Mutex<Vec<ListUpdate>>,
}

impl ListRecorder {
	pub(super) fn last(&self) -> ListUpdate {
		self.updates.lock().unwrap().last().unwrap().clone()
	}
}

impl ListCallback for ListRecorder {
	fn on_entries_batch(&self, entries: Vec<ArchiveEntry>) {
		self.batches.lock().unwrap().push(entries.len());
		self.entries.lock().unwrap().extend(entries);
	}

	fn on_update(&self, update: ListUpdate) {
		self.updates.lock().unwrap().push(update);
	}
}

pub(super) struct Listing {
	pub(super) running: JoinHandle<Result<ListReport, ListFailed>>,
	pub(super) recorder: Arc<ListRecorder>,
	pub(super) reporter: MaybeArc<ListReporter>,
}

/// Lists `setup`'s archive with the real codec.
pub(super) fn list(setup: &Setup, control: JobControl, config: ArchiveConfig) -> Listing {
	let recorder = Arc::new(ListRecorder::default());
	let reporter = ListReporter::new(Arc::clone(&recorder), setup.archive.size());
	let job = StreamJob {
		task: Task::List {
			archive: setup.archive.uuid(),
		},
		..stream_job(
			setup,
			&Options {
				config: config.clone(),
				..Options::default()
			},
		)
	};
	let running = tokio::spawn(run_list(ListTask {
		backend: Arc::clone(&setup.backend),
		control,
		reporter: MaybeArc::clone(&reporter),
		archive: setup.archive.clone(),
		config,
		start: Box::new(move || worker::start(move |port| extract_stream(&port, job))),
	}));
	Listing {
		running,
		recorder,
		reporter,
	}
}

//! Reading an archive back before its sources are deleted for good: the extracting codec reads
//! the archive as the server holds it, and every entry has to match the source it was written
//! from, byte for byte, with nothing missing, skipped or left over.
//!
//! The encoders are the SDK's own and a defect in one would otherwise go unnoticed until the
//! archive is extracted, by which time a permanent deletion has left the data nowhere else.
//! Trashed sources can be restored, so a disposal to the trash is not read back.
//!
//! The read goes through the same reader, limits and input as extracting (see
//! [`CodecFeed`]): its memory is the codec's budget and the feed's floor of one input chunk, the
//! chunk being fetched or the one the reader holds, with chunks fetched ahead only while the
//! client's memory has room right now. A zip's or 7z's reader also keeps up to two chunks it read
//! before, part of its own state as when extracting. A pause is seen when the reader asks for a
//! chunk not fetched yet: the fetches in flight finish, what was fetched ahead is given back, and
//! the reader's state, with the chunk it holds, stays resident; nothing of the client's memory
//! budget is held.

use std::{
	collections::{HashMap, HashSet},
	sync::Arc,
};

use crate::{
	Error,
	consts::CALLBACK_INTERVAL,
	fs::{
		HasName, HasUUID,
		archive::{
			config::ArchiveConfig,
			extract::codec::{ArchiveEnd, CodecLimits, StreamJob, Task, extract_stream},
			format::ArchiveFormat,
			input::{CodecFeed, Fed},
			worker::{self, EntryHead, EntryKind, WorkerEvent, WorkerLink, worker_died},
		},
		drive_job::backend::DriveBackend,
		file::{RemoteFile, enums::RemoteFileType, traits::HasFileInfo},
	},
	job::{JobControl, Stopped},
	util::{MaybeArc, sleep},
};

use super::{
	codec::{ArchiveEntry, CompressJob},
	report::Reporter,
};

/// What the reading codec returns.
pub(crate) type ReadBackResult = Result<ArchiveEnd, Error>;

/// Starts the reading codec over an archive, by its name and length.
pub(crate) type StartReadBack =
	Box<dyn FnOnce(String, u64) -> Result<WorkerLink<ReadBackResult>, Error> + Send>;

/// What reading an archive back needs besides its files.
pub(crate) struct ReadBack {
	/// The archive's directories, by path.
	pub(crate) dirs: Vec<String>,
	pub(crate) start: StartReadBack,
}

impl ReadBack {
	/// Reads back what `job` writes through the extracting codec, under the client's `config`,
	/// with the password it was written with.
	pub(crate) fn as_extracting(job: &CompressJob, config: &ArchiveConfig) -> Self {
		let (dirs, password) = match job {
			CompressJob::Archive { format, entries } => {
				let dirs = entries
					.iter()
					.filter_map(|entry| match entry {
						ArchiveEntry::Dir { path, .. } => Some(path.clone()),
						ArchiveEntry::File { .. } => None,
					})
					.collect();
				// a copy: the reading codec runs on a worker of its own, apart from the job's
				(dirs, format.password().cloned())
			}
			CompressJob::Single { .. } => (Vec::new(), None),
		};
		let limits = CodecLimits {
			decoder_memory: config.codec_mem_budget(),
			max_members: config.max_members,
			// the archive is the job's own: whatever it expands to was read to write it
			expansion: None,
			max_index_bytes: config.max_index_bytes,
			// reading back creates nothing in the drive
			max_bytes: None,
		};
		Self {
			dirs,
			start: Box::new(move |name, len| {
				let job = StreamJob {
					name,
					len,
					limits,
					password,
					// every entry the job wrote comes back, whatever its name
					skip_mac_metadata: false,
					task: Task::Extract(None),
				};
				worker::start(move |port| extract_stream(&port, job))
			}),
		}
	}
}

/// Whether `archive` reads back as exactly `files` (each by its path, with the BLAKE3 of what was
/// read of its source) and the directories of `read_back`. `Err` once the job stops.
pub(crate) async fn reads_back<B: DriveBackend>(
	backend: &Arc<B>,
	control: &JobControl,
	reporter: &MaybeArc<Reporter>,
	archive: &RemoteFile,
	read_back: ReadBack,
	files: HashMap<String, blake3::Hash>,
) -> Result<bool, Stopped> {
	let ReadBack { dirs, start } = read_back;
	let checked = Check {
		files,
		dirs: dirs.into_iter().collect(),
		single: false,
		current: None,
	};
	let link = match start(
		archive.name().unwrap_or_default().to_owned(),
		archive.size(),
	) {
		Ok(link) => link,
		Err(error) => return Ok(unread(archive, &Unread::Failed(error))),
	};
	reporter.verifying(archive.size());
	match read(backend, control, reporter, archive, link, checked).await? {
		Ok(()) => Ok(true),
		Err(why) => Ok(unread(archive, &why)),
	}
}

/// Why an archive did not read back as its sources.
enum Unread {
	/// It is not what was written: why, for the log.
	Differs(String),
	/// Reading it failed: its reader did not start, a chunk could not be fetched, or the reader
	/// failed or died. The archive may well be right.
	Failed(Error),
}

impl From<String> for Unread {
	fn from(why: String) -> Self {
		Self::Differs(why)
	}
}

/// Logs why `archive` did not read back, which keeps the sources; `false`.
fn unread(archive: &RemoteFile, why: &Unread) -> bool {
	match why {
		Unread::Differs(why) => tracing::error!(
			"archive {} does not read back as its sources ({why}): they are kept",
			archive.uuid()
		),
		Unread::Failed(error) => tracing::warn!(
			"archive {}: failed to read it back, so its sources are kept: {error}",
			archive.uuid()
		),
	}
	false
}

/// What is left to find in the archive.
struct Check {
	files: HashMap<String, blake3::Hash>,
	dirs: HashSet<String>,
	/// The archive is one compressed file, named after the archive instead of its source.
	single: bool,
	/// The file entry being read, and the hash of its data so far.
	current: Option<(String, blake3::Hasher)>,
}

impl Check {
	/// Takes one of the codec's events; `Err` with why the archive is not what was written.
	fn take(&mut self, event: WorkerEvent) -> Result<(), String> {
		match event {
			WorkerEvent::Ask { .. } => unreachable!("the feed answers the reader's asks"),
			WorkerEvent::Opened(layout) => {
				self.single = matches!(layout, ArchiveFormat::Single { .. });
			}
			WorkerEvent::Entry(EntryHead { path, kind, .. }) => {
				let path = path.joined();
				match kind {
					EntryKind::Dir if self.dirs.remove(&path) => {}
					EntryKind::Dir => return Err(format!("it holds an extra directory {path}")),
					EntryKind::File { .. } => self.current = Some((path, blake3::Hasher::new())),
				}
			}
			WorkerEvent::Data(data) => match &mut self.current {
				Some((_, hasher)) => {
					hasher.update(&data);
				}
				None => return Err("it holds data outside any file".to_owned()),
			},
			WorkerEvent::FileEnd => {
				let (path, hasher) = self.current.take().ok_or("it ends a file it never began")?;
				let expected = if self.single && self.files.len() == 1 {
					self.files.drain().next().map(|(_, hash)| hash)
				} else {
					self.files.remove(&path)
				};
				if expected != Some(hasher.finalize()) {
					return Err(format!("{path} is not its source"));
				}
			}
			WorkerEvent::Skipped(member) => {
				return Err(format!("{} could not be read", member.path));
			}
			WorkerEvent::Link(link) => {
				return Err(format!("{} is a hard link", link.unresolved.path));
			}
			WorkerEvent::Listed(_) => return Err("its reader listed it".to_owned()),
			WorkerEvent::Head(_) => return Err("its reader wrote".to_owned()),
		}
		Ok(())
	}

	/// Whether the archive, ended as `end`, held everything and nothing else.
	fn complete(&self, end: &ArchiveEnd) -> Result<(), String> {
		if let Some((path, _)) = self.files.iter().next() {
			return Err(format!("{path} is missing"));
		}
		if let Some(path) = self.dirs.iter().next() {
			return Err(format!("the directory {path} is missing"));
		}
		if end.unaccounted_bytes > 0 || end.duplicates.is_some() {
			return Err("it holds data of no entry".to_owned());
		}
		Ok(())
	}
}

/// Serves the reading codec the archive's chunks and checks what it reads; the outer `Err` once
/// the job stops, the inner one with why the archive did not read back.
async fn read<B: DriveBackend>(
	backend: &Arc<B>,
	control: &JobControl,
	reporter: &MaybeArc<Reporter>,
	archive: &RemoteFile,
	link: WorkerLink<ReadBackResult>,
	mut check: Check,
) -> Result<Result<(), Unread>, Stopped> {
	let file = Arc::new(RemoteFileType::from(archive.clone()));
	let mut feed = CodecFeed::new(Arc::clone(backend), file, link, reporter.op());
	// what the reader read so far, each chunk counted once however often it asks for it
	let mut verified = 0;
	let mut report_verified = |feed: &CodecFeed<B, ArchiveEnd>| {
		let read = feed.bytes_read();
		reporter.archive_verified(read - verified);
		verified = read;
	};
	loop {
		let pause_requested = control.is_pause_requested();
		reporter.set_pause_requested(pause_requested);
		if control.is_stopping() {
			return Err(Stopped);
		}
		if pause_requested {
			// seen once the reader waits for its next chunk, which a pause then holds back: it
			// has read the ones before, and nothing is in flight
			if feed.owes_codec() && !feed.fetching() {
				report_verified(&feed);
				feed.wait_out_pause(reporter, control).await?;
				continue;
			}
		} else {
			feed.advance(&reporter.ops());
			report_verified(&feed);
		}
		tokio::select! {
			biased;
			() = control.stopping() => {},
			() = control.pause_changed(pause_requested) => {},
			fed = feed.next(true, true) => match fed {
				Fed::Fetched(Err(error)) => return Ok(Err(Unread::Failed(error))),
				Fed::Fetched(Ok(())) | Fed::Asked => report_verified(&feed),
				Fed::Event(event) => {
					if let Err(why) = check.take(event) {
						return Ok(Err(why.into()));
					}
				}
				Fed::EventsClosed => {}
				Fed::Finished(Ok(end)) => return Ok(check.complete(&end).map_err(Unread::from)),
				Fed::Finished(Err(error)) => return Ok(Err(Unread::Failed(error))),
			},
			() = sleep(CALLBACK_INTERVAL) => {
				reporter.tick();
				if feed.give_up_if_stalled(pause_requested) {
					return Ok(Err(Unread::Failed(worker_died())));
				}
			}
		}
	}
}

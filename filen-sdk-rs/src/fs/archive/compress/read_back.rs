//! Reading an archive back before its sources are deleted for good: the extracting codec reads
//! the archive as the server holds it, and every entry has to match the source it was written
//! from, byte for byte, with nothing missing, skipped or left over.
//!
//! The encoders are the SDK's own and a defect in one would otherwise go unnoticed until the
//! archive is extracted, by which time a permanent deletion has left the data nowhere else.
//! Trashed sources can be restored, so a disposal to the trash is not read back.
//!
//! The read goes through the same reader and limits as extracting, one chunk at a time. Its
//! memory is the codec's budget and the job's two-chunk floor: the chunk being fetched and the
//! one the reader holds. A zip's or 7z's reader also keeps up to two chunks it read before, part
//! of its own state as when extracting. A pause is seen when the reader asks for its next chunk:
//! the fetch in flight finishes, the floor is given back, and the reader's state, with the chunk
//! it holds, stays resident; nothing of the client's memory budget is held.

use std::collections::{HashMap, HashSet};

use crate::{
	Error,
	fs::{
		HasName, HasUUID,
		archive::{
			config::ArchiveConfig,
			extract::codec::{ArchiveEnd, CodecLimits, StreamJob, Task, extract_stream},
			format::ArchiveFormat,
			password::ArchivePassword,
			worker::{self, ARCHIVE_STALL_TIMEOUT, EntryHead, EntryKind, WorkerEvent, WorkerLink},
		},
		drive_job::backend::DriveBackend,
		file::{RemoteFile, enums::RemoteFileType, traits::HasFileInfo},
	},
	job::{JobControl, Stopped},
	util::{MaybeArc, sleep},
};

use super::{codec::ArchiveEntry, report::Reporter};

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
	/// Where the reader's memory floor comes from.
	pub(crate) config: ArchiveConfig,
}

impl ReadBack {
	/// Reads back an archive of `entries` through the extracting codec, under the client's
	/// `config`, with the `password` it was written with.
	pub(crate) fn as_extracting(
		entries: &[ArchiveEntry],
		config: &ArchiveConfig,
		password: Option<ArchivePassword>,
	) -> Self {
		let dirs = entries
			.iter()
			.filter_map(|entry| match entry {
				ArchiveEntry::Dir { path, .. } => Some(path.clone()),
				ArchiveEntry::File { .. } => None,
			})
			.collect();
		let limits = CodecLimits {
			decoder_memory: config.codec_mem_budget,
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
			config: config.clone(),
		}
	}
}

/// Whether `archive` reads back as exactly `files` (each by its path, with the BLAKE3 of what was
/// read of its source) and the directories of `read_back`. `Err` once the job stops; a pause
/// holds the read between two chunks, holding no floor.
pub(crate) async fn reads_back<B: DriveBackend>(
	backend: &B,
	control: &JobControl,
	reporter: &MaybeArc<Reporter>,
	archive: &RemoteFile,
	read_back: ReadBack,
	files: HashMap<String, blake3::Hash>,
) -> Result<bool, Stopped> {
	let ReadBack {
		dirs,
		start,
		config,
	} = read_back;
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
		Err(error) => {
			return Ok(differs(
				archive,
				&format!("its reader did not start: {error}"),
			));
		}
	};
	reporter.verifying(archive.size());
	let reading = Reading {
		backend,
		control,
		reporter,
		config: &config,
	};
	match read(reading, archive, link, checked).await? {
		Ok(()) => Ok(true),
		Err(why) => Ok(differs(archive, &why)),
	}
}

fn differs(archive: &RemoteFile, why: &str) -> bool {
	tracing::error!(
		"archive {} does not read back as its sources ({why}): they are kept",
		archive.uuid()
	);
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
			WorkerEvent::Ask { .. } => unreachable!("the reader's asks are answered first"),
			WorkerEvent::Opened(layout) => {
				self.single = matches!(layout, ArchiveFormat::Single { .. });
			}
			WorkerEvent::Entry(EntryHead { path, kind, .. }) => {
				let path = path
					.segments
					.iter()
					.map(AsRef::as_ref)
					.collect::<Vec<&str>>()
					.join("/");
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

/// What serving the reader takes from its job.
struct Reading<'a, B> {
	backend: &'a B,
	control: &'a JobControl,
	reporter: &'a MaybeArc<Reporter>,
	config: &'a ArchiveConfig,
}

/// Serves the reading codec the archive's chunks and checks what it reads; the outer `Err` once
/// the job stops, the inner one with why the archive differs.
async fn read<B: DriveBackend>(
	Reading {
		backend,
		control,
		reporter,
		config,
	}: Reading<'_, B>,
	archive: &RemoteFile,
	mut link: WorkerLink<ReadBackResult>,
	mut check: Check,
) -> Result<Result<(), String>, Stopped> {
	let file = RemoteFileType::from(archive.clone());
	let mut floor = Some(control.until_stopping(config.floor()).await?);
	// a zip or 7z reader may ask for a chunk again; progress counts each once
	let mut fetched = HashSet::new();
	loop {
		// a codec that neither asks nor tells anything for this long is given up on
		let event = tokio::select! {
			biased;
			() = control.stopping() => return Err(Stopped),
			event = link.events.recv() => event,
			() = sleep(ARCHIVE_STALL_TIMEOUT) => {
				link.retire();
				return Ok(Err("its reader stopped responding".to_owned()));
			}
		};
		let Some(event) = event else {
			break;
		};
		if let WorkerEvent::Ask { index, reply, .. } = event {
			// the reader waits for its chunk, so a pause holds nothing in flight
			if control.is_pause_requested() {
				floor = None;
			}
			reporter.checkpoint(control).await?;
			if floor.is_none() {
				floor = Some(control.until_stopping(config.floor()).await?);
			}
			let _op = reporter.op();
			match control
				.until_stopping(backend.fetch_chunk(&file, index))
				.await?
			{
				Ok(chunk) => {
					if fetched.insert(index) {
						reporter.archive_verified(chunk.len() as u64);
					}
					let _ = reply.send(Ok(chunk));
				}
				Err(error) => return Ok(Err(format!("reading it failed: {error}"))),
			}
			continue;
		}
		if let Err(why) = check.take(event) {
			return Ok(Err(why));
		}
	}
	let ended = tokio::select! {
		biased;
		() = control.stopping() => return Err(Stopped),
		ended = &mut link.done => ended,
		() = sleep(ARCHIVE_STALL_TIMEOUT) => {
			link.retire();
			return Ok(Err("its reader stopped responding".to_owned()));
		}
	};
	Ok(match ended {
		Ok(Ok(end)) => check.complete(&end),
		Ok(Err(error)) => Err(error.to_string()),
		Err(_) => Err("its reader died".to_owned()),
	})
}

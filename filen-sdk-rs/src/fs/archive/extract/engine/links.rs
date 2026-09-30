//! A tar's hard links: each is extracted as a copy of the file it names, fetched back from the
//! drive once that file is registered.

use std::{
	collections::{HashMap, VecDeque},
	sync::Arc,
};

use filen_types::fs::Uuid;
use futures::stream::FuturesUnordered;

use crate::{
	Error, ErrorKind,
	consts::MAX_SMALL_PARALLEL_REQUESTS,
	fs::{
		archive::{
			dispose::DisposalBackend,
			entry_path::ArchivePath,
			extract::{
				codec::LinkKeys,
				report::{ExtractStage, entry_index},
			},
			input::{take_memory, whole_chunk},
			worker::{LinkHead, SkippedMember},
		},
		categories::{NonRootItemType, Normal},
		drive_job::CHUNKS_PER_FILE,
		file::{enums::RemoteFileType, traits::HasFileInfo},
	},
	util::{MaybeSendBoxFuture, SeededMap},
};

use super::{
	Driver, FilePhase, FileSource, LinkChunk, LinkPhase, LinkSource, MAX_OPEN_FILES, NewFile,
	SlotSource,
};

/// The files a tar's hard links may name, by [`LinkKeys`] of the path each was sent at: one the
/// codec sent, or another hard link's copy. A tar may hold a million files, each of which a
/// link after it may name, so each costs 16 bytes in the map (and its share of the map's spare
/// room), and 24 more once registered: what an open one is, its slot in the open files tells.
#[derive(Default)]
struct LinkTargets {
	/// The job's key for the paths.
	keys: LinkKeys,
	by_key: SeededMap<u64, LinkTarget>,
	/// The uuid and size of each registered target.
	registered: Vec<(Uuid, u64)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinkTarget {
	/// The file or hard link of this ordinal, not registered yet.
	Open(u32),
	/// Registered, its uuid and size at this index of [`LinkTargets::registered`].
	Registered(u32),
}

/// What a hard link's target is, once looked up.
#[derive(Clone, Copy)]
enum Named {
	/// The file or hard link of this ordinal, not registered yet.
	Open(u64),
	Registered {
		uuid: Uuid,
		size: u64,
	},
}

impl LinkTargets {
	/// Notes the file or hard link `ordinal` as the one that links after it naming `key` name:
	/// a later file at the same path takes over.
	fn open(&mut self, key: u64, ordinal: u64) {
		self.by_key
			.insert(key, LinkTarget::Open(entry_index(ordinal)));
	}

	fn get(&self, key: u64) -> Option<Named> {
		Some(match *self.by_key.get(&key)? {
			LinkTarget::Open(ordinal) => Named::Open(u64::from(ordinal)),
			LinkTarget::Registered(at) => {
				let (uuid, size) = self.registered[at as usize];
				Named::Registered { uuid, size }
			}
		})
	}

	/// File `ordinal`, noted under `key`, was registered as `uuid`, `size` bytes; nothing when a
	/// later file took the key over.
	fn registered(&mut self, key: u64, ordinal: u64, uuid: Uuid, size: u64) {
		let Some(target) = self.by_key.get_mut(&key) else {
			return;
		};
		if *target == LinkTarget::Open(entry_index(ordinal))
			&& let Ok(at) = u32::try_from(self.registered.len())
		{
			*target = LinkTarget::Registered(at);
			self.registered.push((uuid, size));
		}
	}
}

/// A tar's hard links as the job works them off: the files they may name, the links waiting for
/// theirs, those ready to open, and the copies in flight.
#[derive(Default)]
pub(super) struct Links {
	/// The files a tar's hard links may name: kept for a tar only, whose links name files by
	/// path.
	targets: LinkTargets,
	/// Hard links waiting for the file they name to be registered, by that file's ordinal, and
	/// how many there are.
	waiting: HashMap<u64, Vec<PendingLink>>,
	pub(super) waiting_count: usize,
	/// Hard links whose file is registered, opened only as fast as the files open before them
	/// are worked off: a thousand links to one file do not all start at once.
	pub(super) ready: VecDeque<(TakenLink, Uuid)>,
	/// What the hard links taken on copy in all, charged against the job's expansion limit.
	bytes: u64,
	/// The files hard links copy, being fetched by uuid.
	pub(super) sources: FuturesUnordered<MaybeSendBoxFuture<'static, LinkSource>>,
	/// Chunks of those files, being fetched for a copy.
	pub(super) chunks: FuturesUnordered<MaybeSendBoxFuture<'static, LinkChunk>>,
}

/// A hard link waiting for the file it names to be registered.
struct PendingLink {
	link: TakenLink,
	/// What it is reported as if that file never is.
	unresolved: SkippedMember,
}

/// A hard link taken on, not opened yet.
pub(super) struct TakenLink {
	/// Its file, found by the links after it that name its path.
	file: NewFile,
}

impl<B: DisposalBackend> Driver<B> {
	/// Drops the hard links taken on and not opened yet, which a stopping job never starts: they
	/// are not attempted.
	pub(super) fn drop_taken_links(&mut self) {
		let links = self.links.waiting_count + self.links.ready.len();
		let bytes = self
			.links
			.waiting
			.values()
			.flatten()
			.map(|waiting| waiting.link.file.size.unwrap_or(0))
			.chain(
				self.links
					.ready
					.iter()
					.map(|(link, _)| link.file.size.unwrap_or(0)),
			)
			// sizes the archive states: two can overflow
			.fold(0, u64::saturating_add);
		if links > 0 {
			self.reporter.files_not_attempted(links as u64, bytes);
		}
		self.links.waiting.clear();
		self.links.waiting_count = 0;
		self.links.ready.clear();
	}

	/// What the hard links that name `path` find the file there by.
	pub(super) fn link_key(&self, path: &ArchivePath) -> u64 {
		self.links.targets.keys.of(path)
	}

	/// Notes open file `ordinal`, found by `key`, as the one the hard links after it that name
	/// its path copy: a later file of the same path is the one links after it name.
	pub(super) fn link_target(&mut self, ordinal: u64, key: u64) {
		if self.files.contains_key(&ordinal) {
			self.links.targets.open(key, ordinal);
		}
	}

	/// A tar hard link: extracted as a copy of the file it names (one the codec sent, or another
	/// link's copy), once that one is registered, or skipped when there is none (it was skipped,
	/// it failed, or no file of its path came before).
	///
	/// What links copy is charged against the
	/// [`ExpansionLimit`](crate::fs::archive::ExpansionLimit), as what a compressed
	/// archive decodes to is: a bare tar of one file and a thousand links to it would otherwise
	/// upload that file a thousand times. Past the limit the job fails, as a compressed one does.
	///
	/// Its name is taken as it comes, as any entry's, so the names the entries after it get do
	/// not hang on when its target is registered; a link skipped after all leaves that name
	/// taken, and a later entry of the same name gets a keep-both name.
	pub(super) fn on_link(&mut self, link: LinkHead) {
		let ordinal = link.ordinal();
		let LinkHead {
			path,
			modified,
			target,
			unresolved,
		} = link;
		let (target, size) = match self.links.targets.get(self.links.targets.keys.of(&target)) {
			Some(named @ Named::Registered { size, .. }) => (named, size),
			Some(named @ Named::Open(ordinal)) => match self.pending_size(ordinal) {
				Some(size) => (named, size),
				None => return self.on_skipped(unresolved),
			},
			None => return self.on_skipped(unresolved),
		};
		if let Some(limit) = self.expansion
			&& !limit.allows(
				self.feed.bytes_read(),
				self.links.bytes.saturating_add(size),
			) {
			return self.stop_with(Error::custom(
				ErrorKind::ArchiveTooLarge,
				format!(
					"the archive's hard links copy more than {} times its size",
					limit.ratio
				),
			));
		}
		self.links.bytes = self.links.bytes.saturating_add(size);
		let entry = self.entry_id(ordinal);
		self.report_path(entry, &path);
		let Some(mut file) = self.new_file(ordinal, &path, Some(size), modified) else {
			return;
		};
		// a link may be named by the links after it, as the file it copies is
		let key = self.link_key(&path);
		self.links.targets.open(key, ordinal);
		file.link_key = Some(key);
		let link = TakenLink { file };
		match target {
			Named::Registered { uuid, .. } => self.links.ready.push_back((link, uuid)),
			Named::Open(target_ordinal) => {
				self.links.waiting_count += 1;
				self.links
					.waiting
					.entry(target_ordinal)
					.or_default()
					.push(PendingLink { link, unresolved });
			}
		}
	}

	/// The size of file `ordinal` that hard links wait for: open and not failed, or a hard link
	/// taken on and not opened yet; `None` when it is neither, so has nothing to copy.
	fn pending_size(&self, ordinal: u64) -> Option<u64> {
		if let Some(file) = self.files.get(&ordinal) {
			return (!file.failed()).then(|| file.bytes());
		}
		self.links
			.waiting
			.values()
			.flatten()
			.map(|waiting| &waiting.link)
			.chain(self.links.ready.iter().map(|(link, _)| link))
			.find(|link| link.file.ordinal == ordinal)
			.map(|link| link.file.size.unwrap_or(0))
	}

	/// Opens the hard links whose file is registered, while the files open leave room and the
	/// fetches of their targets are as many at once as other small requests.
	pub(super) fn open_ready_links(&mut self) {
		while self.files.len() < MAX_OPEN_FILES
			&& self.links.sources.len() < MAX_SMALL_PARALLEL_REQUESTS
			&& let Some((TakenLink { file }, target)) = self.links.ready.pop_front()
		{
			self.open_file(NewFile {
				source: FileSource::Link { target },
				..file
			});
		}
	}

	/// File `ordinal` was registered as `uuid`, `size` bytes: the hard links waiting for it copy
	/// it now, as fast as they are opened.
	pub(super) fn link_target_registered(
		&mut self,
		ordinal: u64,
		key: Option<u64>,
		uuid: Uuid,
		size: u64,
	) {
		if let Some(key) = key {
			self.links.targets.registered(key, ordinal, uuid, size);
		}
		let waiting = self.links.waiting.remove(&ordinal).unwrap_or_default();
		self.links.waiting_count -= waiting.len();
		self.links.ready.extend(
			waiting
				.into_iter()
				.map(|PendingLink { link, .. }| (link, uuid)),
		);
	}

	/// File `ordinal` failed: the hard links waiting for it have nothing to copy, and are
	/// skipped, no item after all.
	pub(super) fn link_target_failed(&mut self, ordinal: u64) {
		let waiting = self.links.waiting.remove(&ordinal).unwrap_or_default();
		self.links.waiting_count -= waiting.len();
		for PendingLink { link, unresolved } in waiting {
			self.items -= 1;
			self.links.bytes -= link.file.size.unwrap_or(0);
			// the links waiting for this one fail with it
			self.link_target_failed(link.file.ordinal);
			self.on_skipped(unresolved);
		}
	}

	/// Starts fetching the next chunk of each hard link's target whose copy can go on: its
	/// directory exists, it has room for another upload, and memory is free right now.
	pub(super) fn copy_links(&mut self) {
		let ready: Vec<(u64, Arc<RemoteFileType<'static>>)> = self
			.files
			.iter()
			.filter_map(|(ordinal, file)| match &file.source {
				SlotSource::Link(LinkPhase::Idle(source))
					if file.phase == FilePhase::Receiving
						&& file.uploading < CHUNKS_PER_FILE
						&& self.dirs.slots[file.parent].created_uuid().is_some() =>
				{
					Some((*ordinal, Arc::clone(source)))
				}
				_ => None,
			})
			.collect();
		for (ordinal, source) in ready {
			let Some(permit) = take_memory(&self.output_slot, &self.memory) else {
				return;
			};
			let file = self.files.get_mut(&ordinal).expect("just found");
			file.source = SlotSource::Link(LinkPhase::Fetching(Arc::clone(&source)));
			let index = file.next_index;
			let backend = Arc::clone(&self.backend);
			let op = self.reporter.op();
			self.links.chunks.push(Box::pin(async move {
				let result = backend
					.fetch_chunk(&source, index)
					.await
					.and_then(|data| whole_chunk(&source, index, data));
				(ordinal, result, permit, op)
			}));
		}
	}

	pub(super) fn link_source_fetched(
		&mut self,
		ordinal: u64,
		result: Result<NonRootItemType<'static, Normal>, Error>,
	) {
		let source = match result {
			Ok(NonRootItemType::File(file)) => RemoteFileType::from(file.into_owned()),
			Ok(NonRootItemType::Dir(_)) => {
				let error = Error::custom(ErrorKind::InvalidState, "a hard link names a directory");
				return self.fail_file(ordinal, ExtractStage::Upload, Arc::new(error));
			}
			Err(error) => {
				let error = Arc::new(error);
				self.note_error(&error);
				return self.fail_file(ordinal, ExtractStage::Upload, error);
			}
		};
		let Some(file) = self.files.get_mut(&ordinal) else {
			return;
		};
		// an empty file has no chunk to copy
		if source.chunks() == 0 {
			file.phase = file.phase.end();
		}
		file.source = SlotSource::Link(LinkPhase::Idle(Arc::new(source)));
		self.finalize_ready();
	}

	pub(super) fn link_chunk_fetched(&mut self, (ordinal, result, permit, _op): LinkChunk) {
		let Some(file) = self.files.get_mut(&ordinal) else {
			return;
		};
		let SlotSource::Link(LinkPhase::Fetching(source)) = &file.source else {
			unreachable!("a chunk is fetched only for a link fetching one");
		};
		let source = Arc::clone(source);
		let chunks = source.chunks();
		file.source = SlotSource::Link(LinkPhase::Idle(source));
		if file.failed() {
			return;
		}
		match result {
			Ok(data) => {
				self.upload(ordinal, data, permit);
				if let Some(file) = self.files.get_mut(&ordinal)
					&& file.next_index == chunks
				{
					file.phase = file.phase.end();
				}
			}
			Err(error) => {
				let error = Arc::new(error);
				self.note_error(&error);
				self.fail_file(ordinal, ExtractStage::Upload, error);
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use filen_types::fs::Uuid;

	use super::LinkTargets;
	use crate::alloc_meter;

	#[test]
	fn a_tars_link_targets_take_at_most_96_bytes_a_file() {
		// the bound ArchiveConfig::max_members states
		const BYTES_A_FILE: u64 = 96;
		const FILES: u64 = 1_000_000;
		let ((), peak) = alloc_meter::peak_bytes(|| {
			let mut targets = LinkTargets::default();
			for ordinal in 0..FILES {
				// as spread as a hash's first bytes
				let key = ordinal.wrapping_mul(0x9E37_79B9_7F4A_7C15);
				targets.open(key, ordinal);
				targets.registered(key, ordinal, Uuid::from_u128(u128::from(ordinal)), ordinal);
			}
		});
		assert!(
			peak <= FILES * BYTES_A_FILE,
			"{} bytes a file",
			peak / FILES
		);
	}
}

//! A tar's hard links: each is extracted as a copy of the file it names, fetched back from the
//! drive once that file is registered.

use std::sync::Arc;

use filen_types::fs::Uuid;

use crate::{
	Error, ErrorKind,
	consts::MAX_SMALL_PARALLEL_REQUESTS,
	fs::{
		archive::{
			dispose::DisposalBackend,
			entry_path::ArchivePath,
			input::{take_memory, whole_chunk},
			worker::{LinkHead, SkippedMember},
		},
		categories::{NonRootItemType, Normal},
		drive_job::CHUNKS_PER_FILE,
		file::{enums::RemoteFileType, traits::HasFileInfo},
	},
	util::SeededMap,
};

use super::{
	super::{codec::LinkKeys, report::ExtractStage},
	Driver, FileSource, LinkChunk, MAX_OPEN_FILES, NewFile,
};

/// A hard link's copy of the file it names: that file's chunks, fetched one at a time (their
/// hash is taken in order) and uploaded as the link's.
#[derive(Default)]
pub(super) struct LinkCopy {
	/// The file, once fetched by its uuid.
	source: Option<Arc<RemoteFileType<'static>>>,
	/// A chunk of it is being fetched.
	fetching: bool,
}

/// The files a tar's hard links may name, by [`LinkKeys`] of the path each was sent at: one the
/// codec sent, or another hard link's copy. A tar may hold a million files, each of which a
/// link after it may name, so each costs 16 bytes in the map (and its share of the map's spare
/// room), and 24 more once registered: what an open one is, its slot in the open files tells.
#[derive(Default)]
pub(super) struct LinkTargets {
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
		// the member cap keeps ordinals far below u32::MAX; one past it is never named
		if let Ok(ordinal) = u32::try_from(ordinal) {
			self.by_key.insert(key, LinkTarget::Open(ordinal));
		}
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
		if *target == LinkTarget::Open(u32::try_from(ordinal).unwrap_or(u32::MAX))
			&& let Ok(at) = u32::try_from(self.registered.len())
		{
			*target = LinkTarget::Registered(at);
			self.registered.push((uuid, size));
		}
	}
}

/// A hard link waiting for the file it names to be registered.
pub(super) struct PendingLink {
	link: TakenLink,
	/// What it is reported as if that file never is.
	unresolved: SkippedMember,
}

/// A hard link taken on, not opened yet.
pub(super) struct TakenLink {
	file: NewFile,
	/// What the links after it that name its path find it by.
	key: u64,
}

impl<B: DisposalBackend> Driver<B> {
	/// Drops the hard links taken on and not opened yet, which a stopping job never starts: they
	/// are not attempted.
	pub(super) fn drop_taken_links(&mut self) {
		let links = self.links_waiting + self.ready_links.len();
		let bytes = self
			.waiting_links
			.values()
			.flatten()
			.map(|waiting| waiting.link.file.size.unwrap_or(0))
			.chain(
				self.ready_links
					.iter()
					.map(|(link, _)| link.file.size.unwrap_or(0)),
			)
			// sizes the archive states: two can overflow
			.fold(0, u64::saturating_add);
		if links > 0 {
			self.reporter.files_not_attempted(links as u64, bytes);
		}
		self.waiting_links.clear();
		self.links_waiting = 0;
		self.ready_links.clear();
	}

	/// Notes open file `ordinal`, at `path`, as the one the hard links after it that name that
	/// path copy: a later file of the same path is the one links after it name.
	pub(super) fn link_target(&mut self, ordinal: u64, path: &ArchivePath) {
		let Some(file) = self.files.get_mut(&ordinal) else {
			return;
		};
		let key = self.link_targets.keys.of(path);
		file.link_key = Some(key);
		self.link_targets.open(key, ordinal);
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
		let LinkHead {
			ordinal,
			path,
			modified,
			target,
			unresolved,
		} = link;
		let (target, size) = match self.link_targets.get(self.link_targets.keys.of(&target)) {
			Some(Named::Registered { uuid, size }) => (Ok(uuid), size),
			Some(Named::Open(ordinal)) => match self.pending_size(ordinal) {
				Some(size) => (Err(ordinal), size),
				None => return self.on_skipped(unresolved),
			},
			None => return self.on_skipped(unresolved),
		};
		if let Some(limit) = self.expansion
			&& !limit.allows(self.feed.bytes_read(), self.link_bytes.saturating_add(size))
		{
			return self.stop_with(Error::custom(
				ErrorKind::ArchiveTooLarge,
				format!(
					"the archive's hard links copy more than {} times its size",
					limit.ratio
				),
			));
		}
		self.link_bytes += size;
		let entry = self.entry_id(ordinal);
		self.report_path(entry, &path);
		let Some(file) = self.new_file(ordinal, &path, Some(size), modified) else {
			return;
		};
		// a link may be named by the links after it, as the file it copies is
		let key = self.link_targets.keys.of(&path);
		self.link_targets.open(key, ordinal);
		let link = TakenLink { file, key };
		match target {
			Ok(uuid) => self.ready_links.push_back((link, uuid)),
			Err(target_ordinal) => {
				self.links_waiting += 1;
				self.waiting_links
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
			return (!file.failed).then(|| file.bytes());
		}
		self.waiting_links
			.values()
			.flatten()
			.map(|waiting| &waiting.link)
			.chain(self.ready_links.iter().map(|(link, _)| link))
			.find(|link| link.file.ordinal == ordinal)
			.map(|link| link.file.size.unwrap_or(0))
	}

	/// Opens the hard links whose file is registered, while the files open leave room and the
	/// fetches of their targets are as many at once as other small requests.
	pub(super) fn open_ready_links(&mut self) {
		while self.files.len() < MAX_OPEN_FILES
			&& self.link_sources.len() < MAX_SMALL_PARALLEL_REQUESTS
			&& let Some((TakenLink { file, key }, target)) = self.ready_links.pop_front()
		{
			let ordinal = file.ordinal;
			self.open_file(NewFile {
				source: FileSource::Link { target },
				..file
			});
			if let Some(file) = self.files.get_mut(&ordinal) {
				file.link_key = Some(key);
			}
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
			self.link_targets.registered(key, ordinal, uuid, size);
		}
		let waiting = self.waiting_links.remove(&ordinal).unwrap_or_default();
		self.links_waiting -= waiting.len();
		self.ready_links.extend(
			waiting
				.into_iter()
				.map(|PendingLink { link, .. }| (link, uuid)),
		);
	}

	/// File `ordinal` failed: the hard links waiting for it have nothing to copy, and are
	/// skipped, no item after all.
	pub(super) fn link_target_failed(&mut self, ordinal: u64) {
		let waiting = self.waiting_links.remove(&ordinal).unwrap_or_default();
		self.links_waiting -= waiting.len();
		for PendingLink { link, unresolved } in waiting {
			self.items -= 1;
			self.link_bytes -= link.file.size.unwrap_or(0);
			// the links waiting for this one fail with it
			self.link_target_failed(link.file.ordinal);
			self.on_skipped(unresolved);
		}
	}

	/// Starts fetching the next chunk of each hard link's target whose copy can go on: its
	/// directory exists, it has room for another upload, and memory is free right now.
	pub(super) fn copy_links(&mut self) {
		let ready: Vec<u64> = self
			.files
			.iter()
			.filter(|(_, file)| {
				file.copy
					.as_ref()
					.is_some_and(|copy| copy.source.is_some() && !copy.fetching)
					&& !file.failed && !file.ended
					&& file.uploading < CHUNKS_PER_FILE
					&& self.dirs[file.parent].created_uuid().is_some()
			})
			.map(|(ordinal, _)| *ordinal)
			.collect();
		for ordinal in ready {
			let Some(permit) = take_memory(&self.output_slot, &self.memory) else {
				return;
			};
			let file = self.files.get_mut(&ordinal).expect("just found");
			let copy = file.copy.as_mut().expect("just found");
			copy.fetching = true;
			let source = Arc::clone(copy.source.as_ref().expect("just found"));
			let index = file.next_index;
			let backend = Arc::clone(&self.backend);
			let op = self.reporter.op();
			self.link_chunks.push(Box::pin(async move {
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
		file.ended = source.chunks() == 0;
		file.copy.as_mut().expect("a link copies").source = Some(Arc::new(source));
		self.finalize_ready();
	}

	pub(super) fn link_chunk_fetched(&mut self, (ordinal, result, permit, _op): LinkChunk) {
		let Some(file) = self.files.get_mut(&ordinal) else {
			return;
		};
		let copy = file.copy.as_mut().expect("a link copies");
		copy.fetching = false;
		let chunks = copy.source.as_ref().expect("fetched from").chunks();
		if file.failed {
			return;
		}
		match result {
			Ok(data) => {
				self.upload(ordinal, data, permit);
				if let Some(file) = self.files.get_mut(&ordinal) {
					file.ended = file.next_index == chunks;
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

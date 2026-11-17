//! Archives inside archives: an entry read by range (`Mapped` or `Seekable`) opened as an archive
//! of its own, its bytes read through the reader of the archive it is in. An entry only read
//! whole or front to back is not opened: it would have to be held in memory, which no reader
//! does. A zip inside a deflated entry has its index at its end, so opening it decodes the entry
//! once to there; checkpoints make later reads of it cheap.

#![warn(unreachable_pub, unused_qualifications)]

use std::sync::{Arc, Mutex, atomic::AtomicBool};

use filen_types::{crypto::Blake3Hash, fs::Uuid};

use crate::{
	Error, ErrorKind,
	consts::CHUNK_SIZE_U64,
	fs::archive::{
		ArchivePassword,
		extract::{
			ArchiveEntryId, ListConfig,
			codec::{HeldIndex, expansion_exceeded},
		},
		format::ArchiveFormat,
		input::ArchiveBytes,
		retained::Retained,
	},
	util::MaybeSendBoxFuture,
};

use super::{Reader, Resolved};

/// Most archives one is nested in: an archive in an archive in an archive in the drive.
pub(crate) const MAX_NESTING: u8 = 3;

/// An entry of an archive another reader opened, read as an archive's bytes.
pub(crate) struct EntryBytes {
	parent: Arc<Reader>,
	entry: ArchiveEntryId,
	len: u64,
	id: Uuid,
}

impl ArchiveBytes for EntryBytes {
	fn len(&self) -> u64 {
		self.len
	}

	fn id(&self) -> Uuid {
		self.id
	}

	/// No hash covers an entry; its archive's checksum for it does, which a range skips.
	fn hash(&self) -> Option<Blake3Hash> {
		None
	}

	fn chunk(&self, index: u64) -> MaybeSendBoxFuture<'_, Result<Vec<u8>, Error>> {
		Box::pin(async move {
			let start = index.saturating_mul(CHUNK_SIZE_U64).min(self.len);
			let end = start.saturating_add(CHUNK_SIZE_U64).min(self.len);
			let capacity = usize::try_from(end - start).expect("a chunk fits in memory");
			let mut chunk = futures::io::Cursor::new(Vec::with_capacity(capacity));
			self.parent
				.read_entry_range(self.entry, start, end, &mut chunk)
				.await
				.map_err(unshared)?;
			Ok(chunk.into_inner())
		})
	}
}

/// The error a shared one stands for: itself when nothing else holds it, else one with it as
/// its source, of the same kind.
pub(crate) fn unshared(error: Arc<Error>) -> Error {
	Arc::try_unwrap(error)
		.unwrap_or_else(|shared| Error::custom_with_source(shared.kind(), shared, None::<&str>))
}

/// The id a nested archive's entries carry: derived from the archive it is in and its place
/// there, so the same entry opened again gets the same id (a UUID of version 8, its bytes a
/// BLAKE3 of both).
pub(crate) fn nested_id(outer: Uuid, ordinal: u64) -> Uuid {
	let mut hasher = blake3::Hasher::new();
	hasher.update(b"filen archive entry");
	hasher.update(outer.as_bytes());
	hasher.update(&ordinal.to_le_bytes());
	let bytes: [u8; 16] = hasher.finalize().as_bytes()[..16]
		.try_into()
		.expect("16 bytes");
	uuid::Builder::from_custom_bytes(bytes).into_uuid()
}

impl Reader {
	/// A reader of `bytes`, an entry of `parent`'s archive stored under `name`, under the
	/// parent's settings but for `password`.
	pub(crate) fn nested(
		parent: &Self,
		bytes: Arc<EntryBytes>,
		name: String,
		password: Option<ArchivePassword>,
	) -> Self {
		let config = parent.config.clone();
		let retained = Retained::new(Arc::clone(&config.retained), config.retained_budget());
		Self {
			memory: Arc::clone(&parent.memory),
			source: bytes,
			name,
			id: Uuid::new_v4(),
			config,
			settings: ListConfig {
				password,
				..parent.settings.clone()
			},
			format: Mutex::new(None),
			retained: Some(retained),
			depth: parent.depth + 1,
			// the chain counts against what the drive stores of its outermost archive
			expansion_base: Some(parent.expansion_base.unwrap_or(parent.source.len())),
			flights: Mutex::default(),
			closed: AtomicBool::new(false),
		}
	}

	/// The entry `entry` of this reader's archive, as an archive's bytes, with the name it is
	/// stored under; refused for an entry only read whole or front to back, and past
	/// [`MAX_NESTING`].
	pub(crate) async fn entry_bytes(
		self: &Arc<Self>,
		entry: ArchiveEntryId,
	) -> Result<(Arc<EntryBytes>, String), Arc<Error>> {
		if self.depth >= MAX_NESTING {
			return Err(Arc::new(Error::custom(
				ErrorKind::ArchiveUnsupported,
				format!("an archive nested more than {MAX_NESTING} deep"),
			)));
		}
		let len = match self.resolve(entry).await? {
			Resolved::Mapped { size, .. } | Resolved::Seekable { size, .. } => size,
			Resolved::Whole { .. } | Resolved::Sequential { .. } => {
				return Err(Arc::new(Error::custom(
					ErrorKind::ArchiveUnsupported,
					"the entry is only read whole: extract it to open it",
				)));
			}
		};
		// a nested archive is read from its end, a zip's at least: what it decodes to is what
		// opening it may cost, held to the chain's limit before any of it is
		let base = self.expansion_base.unwrap_or(self.source.len());
		if let Some(limit) = self.settings.expansion_limit
			&& !limit.allows(base, len)
		{
			return Err(Arc::new(expansion_exceeded(limit)));
		}
		let name = self.entry_name(entry).await?;
		let bytes = Arc::new(EntryBytes {
			parent: Arc::clone(self),
			entry,
			len,
			id: nested_id(self.archive_uuid(), u64::from(entry.index)),
		});
		Ok((bytes, name))
	}

	/// The last segment of the path `entry` is stored at, which tells some formats; empty for a
	/// tar's member, whose path the reader does not keep (its contents tell its format).
	async fn entry_name(&self, entry: ArchiveEntryId) -> Result<String, Arc<Error>> {
		let ordinal = u64::from(entry.index);
		if !matches!(
			self.format(),
			Some(ArchiveFormat::Zip | ArchiveFormat::SevenZ | ArchiveFormat::Rar { .. })
		) {
			return Ok(String::new());
		}
		let index = self.held_index().await?;
		let path = match &*index {
			HeldIndex::Zip(index) => index
				.entries
				.iter()
				.find(|entry| entry.ordinal == ordinal)
				.map(|entry| entry.name.as_str()),
			HeldIndex::SevenZ { index, .. } => usize::try_from(ordinal)
				.ok()
				.and_then(|at| index.entries.get(at))
				.map(|entry| entry.name.as_str()),
			HeldIndex::Rar(table) => usize::try_from(ordinal)
				.ok()
				.and_then(|at| table.members.get(at))
				.map(|member| member.name.as_str()),
			HeldIndex::Tar(_) => None,
		};
		Ok(path
			.and_then(|path| path.rsplit(['/', '\\']).next())
			.unwrap_or_default()
			.to_owned())
	}
}

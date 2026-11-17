//! An entry's bytes by range. A mapped entry (stored as it is: a stored, unencrypted zip entry,
//! a 7z entry of an unencrypted folder that only copies, a bare tar's member) lies in the
//! archive at an offset, so a range of it is a range of the archive file: read through the
//! reader's kept chunks, with no codec job, no job slot and no decoding.

use std::sync::Arc;

use filen_macros::js_type;
use futures::{AsyncWrite, AsyncWriteExt, StreamExt, stream::FuturesOrdered};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::{
	Error, ErrorKind,
	consts::CHUNK_SIZE_U64,
	fs::{
		HasUUID,
		archive::{
			extract::{
				ArchiveEntryId, EntryDownloadError, ExtractSkipReason, codec::HeldIndex,
				path_refusal,
			},
			format::{ArchiveFormat, StreamCodec},
			input::{PREFETCH_CHUNKS, take_memory, whole_chunk},
			sevenz::read::{Method, SevenZIndex, SevenZKind},
			zip::{
				LOCAL_HEADER_SIG, METHOD_DEFLATE, METHOD_STORED,
				read::{LOCAL_HEADER_LEN, LOCAL_HEADER_LEN_U64, ZipEncryption, ZipIndex, ZipKind},
			},
		},
		drive_job::backend::DriveBackend,
		file::traits::HasFileInfo,
	},
	job::JobControl,
	util::{MaybeSend, MaybeSendBoxFuture},
};

use super::{
	Reader, Unreported,
	inflate::{CipherSpec, Framing, SeekCodec, SeekStream, StreamKey},
};

/// How an entry's bytes can be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[js_type(export, no_deser, tagged, camel_case_fields, no_default)]
pub enum EntryByteAccess {
	/// Stored in the archive as it is: any range of it is read directly
	/// ([`ArchiveReader::read_entry_range`](super::ArchiveReader::read_entry_range)), only the
	/// archive's chunks it lies in fetched, with no job slot and no decoding. A range read is
	/// not checked against the entry's checksum, which covers the whole entry: a download
	/// ([`ArchiveReader::download_entry`](super::ArchiveReader::download_entry)) checks it.
	Mapped {
		/// The entry's size in bytes.
		size: u64,
	},
	/// Deflated, or stored under ZipCrypto or WinZip AES (a zip entry), or a member of a gzip
	/// tar: any range of it is read through a decoder that goes on from where the read before it
	/// stopped, or from a checkpoint it laid every few MiB of output, so reading on costs about
	/// the range and a jump back about one checkpoint's spacing. The decoding runs off the
	/// thread that drives the SDK's tasks, takes no job slot, and is not checked against the
	/// entry's checksum, which covers the whole entry: a download checks it.
	Seekable {
		/// The entry's size in bytes.
		size: u64,
	},
	/// Compressed or encrypted, read whole by
	/// [`ArchiveReader::download_entry`](super::ArchiveReader::download_entry), which checks it
	/// against its checksum.
	Whole {
		/// The entry's size in bytes, as the archive states it.
		size: u64,
	},
	/// A member of a tar compressed other than with gzip, or a single compressed file: only
	/// read front to back.
	Sequential {
		/// Its size in bytes, when a listing found it.
		size: Option<u64>,
	},
}

/// What resolving an entry found, kept for the reader's later calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Resolved {
	/// The entry's data starts `offset` bytes into the archive.
	Mapped {
		offset: u64,
		size: u64,
	},
	/// The entry is bytes `offset..offset + size` of what `stream` decodes to.
	Seekable {
		stream: SeekStream,
		offset: u64,
		size: u64,
	},
	Whole {
		size: u64,
	},
	Sequential {
		size: Option<u64>,
	},
}

impl From<Resolved> for EntryByteAccess {
	fn from(resolved: Resolved) -> Self {
		match resolved {
			Resolved::Mapped { size, .. } => Self::Mapped { size },
			Resolved::Seekable { size, .. } => Self::Seekable { size },
			Resolved::Whole { size } => Self::Whole { size },
			Resolved::Sequential { size } => Self::Sequential { size },
		}
	}
}

/// How a zip's or 7z's entry is read, from the index alone; a zip's entry read by range needs its
/// local header too.
enum FromIndex {
	Resolved(Resolved),
	/// A zip entry read by range, whose data follows its local header at `header` (counted from
	/// the archive's start), and has to end by `limit`.
	ZipHeader {
		header: u64,
		limit: u64,
		compressed: u64,
		size: u64,
		how: ZipRange,
	},
}

/// How a zip entry's data is read by range.
#[derive(Clone, Copy)]
enum ZipRange {
	/// As it is stored.
	Mapped,
	/// Through a decoder.
	Seekable {
		codec: SeekCodec,
		cipher: CipherSpec,
	},
}

fn not_held() -> Error {
	Error::custom(
		ErrorKind::InvalidState,
		"the entry is not in the archive the reader reads",
	)
}

/// What reading `ordinal` of the zip `index` costs, by the index alone.
fn zip_access(index: &ZipIndex, ordinal: u64) -> Result<FromIndex, Error> {
	let Some(entry) = index.entries.iter().find(|entry| entry.ordinal == ordinal) else {
		if index
			.overlapping
			.iter()
			.any(|entry| entry.ordinal == ordinal)
		{
			return Err(EntryDownloadError::Skipped(ExtractSkipReason::OverlappingData).into());
		}
		return Err(not_held());
	};
	if entry.kind != ZipKind::File {
		return Err(EntryDownloadError::NotAFile.into());
	}
	if let Some(reason) = path_refusal(&entry.name) {
		return Err(EntryDownloadError::Skipped(reason).into());
	}
	if !entry.supported() {
		return Err(EntryDownloadError::Skipped(ExtractSkipReason::UnsupportedMethod).into());
	}
	let cipher = match entry.encryption {
		ZipEncryption::None => CipherSpec::None,
		ZipEncryption::ZipCrypto { check } => CipherSpec::ZipCrypto { check },
		ZipEncryption::Aes { strength, .. } => CipherSpec::Aes { strength },
	};
	let how = match (entry.method, cipher) {
		(METHOD_STORED, CipherSpec::None) if entry.compressed_size == entry.size => {
			ZipRange::Mapped
		}
		(METHOD_STORED, CipherSpec::ZipCrypto { .. } | CipherSpec::Aes { .. }) => {
			ZipRange::Seekable {
				codec: SeekCodec::Copy,
				cipher,
			}
		}
		(METHOD_DEFLATE, _) => ZipRange::Seekable {
			codec: SeekCodec::Deflate,
			cipher,
		},
		_ => return Ok(FromIndex::Resolved(Resolved::Whole { size: entry.size })),
	};
	Ok(FromIndex::ZipHeader {
		header: index.shift.saturating_add(entry.header_offset),
		limit: index.shift.saturating_add(entry.data_limit),
		compressed: entry.compressed_size,
		size: entry.size,
		how,
	})
}

/// What reading `ordinal` of the 7z `index` costs.
fn sevenz_access(index: &SevenZIndex, ordinal: u64) -> Result<Resolved, Error> {
	let entry = usize::try_from(ordinal)
		.ok()
		.and_then(|at| index.entries.get(at))
		.filter(|entry| entry.ordinal == ordinal)
		.ok_or_else(not_held)?;
	match entry.kind {
		SevenZKind::File => {}
		// its data tells whether it is a link: a download reads it to find out
		SevenZKind::Reparse => return Ok(Resolved::Whole { size: entry.size }),
		SevenZKind::Anti => {
			return Err(EntryDownloadError::Skipped(ExtractSkipReason::AntiItem).into());
		}
		SevenZKind::Dir | SevenZKind::Symlink => return Err(EntryDownloadError::NotAFile.into()),
	}
	if let Some(reason) = path_refusal(&entry.name) {
		return Err(EntryDownloadError::Skipped(reason).into());
	}
	if !index.supported(entry) {
		return Err(EntryDownloadError::Skipped(ExtractSkipReason::UnsupportedMethod).into());
	}
	let Some(stream) = entry.stream else {
		// an empty file, which no folder stores
		return Ok(Resolved::Mapped { offset: 0, size: 0 });
	};
	let folder = &index.folders[stream.folder];
	let copies = matches!(&folder.coders[..], [coder] if coder.method == Some(Method::Copy))
		&& folder.packed.len() == 1;
	if !copies {
		return Ok(Resolved::Whole { size: entry.size });
	}
	// a folder that only copies stores its files one after another in its one packed stream,
	// solid or not
	let (Some(&packed_at), Some(&packed_len)) = (
		index.pack_offsets.get(folder.first_pack),
		index.pack_sizes.get(folder.first_pack),
	) else {
		return Err(not_held());
	};
	let within = stream
		.offset
		.checked_add(entry.size)
		.is_some_and(|end| end <= packed_len);
	if !within {
		return Err(Error::custom(
			ErrorKind::ArchiveCorrupt,
			"a 7z entry runs past its folder's packed data",
		));
	}
	Ok(Resolved::Mapped {
		offset: packed_at + stream.offset,
		size: entry.size,
	})
}

/// A chunk of the archive read for a range, and the memory it holds while it is written.
type RangeChunk = (Arc<Vec<u8>>, Option<OwnedSemaphorePermit>);

impl<B: DriveBackend> Reader<B> {
	/// How `entry`'s bytes can be read: resolved once, then kept with the index.
	pub(crate) async fn entry_byte_access(
		&self,
		entry: ArchiveEntryId,
	) -> Result<EntryByteAccess, Arc<Error>> {
		self.resolve(entry).await.map(EntryByteAccess::from)
	}

	/// Writes bytes `start..end` of the mapped `entry` to `writer`, then closes it.
	pub(crate) async fn read_entry_range<W: AsyncWrite + Unpin + MaybeSend>(
		&self,
		entry: ArchiveEntryId,
		start: u64,
		end: u64,
		writer: &mut W,
	) -> Result<(), Arc<Error>> {
		let within = |size: u64| {
			if start > end || end > size {
				return Err(Arc::new(Error::custom(
					ErrorKind::InvalidState,
					format!("the range {start}..{end} is not within the entry's {size} bytes"),
				)));
			}
			Ok(())
		};
		match self.resolve(entry).await? {
			Resolved::Mapped { offset, size } => {
				within(size)?;
				self.read_archive(offset + start, offset + end, writer)
					.await?;
			}
			Resolved::Seekable {
				stream,
				offset,
				size,
			} => {
				within(size)?;
				// closes the writer
				return self
					.read_seekable(stream, offset + start..offset + end, writer)
					.await;
			}
			Resolved::Whole { .. } | Resolved::Sequential { .. } => {
				return Err(Arc::new(Error::custom(
					ErrorKind::InvalidState,
					"the entry is only read whole, or front to back",
				)));
			}
		}
		writer
			.close()
			.await
			.map_err(|error| Arc::new(error.into()))?;
		Ok(())
	}

	/// How `entry` is read, from what the reader keeps, or else from its index.
	pub(super) async fn resolve(&self, entry: ArchiveEntryId) -> Result<Resolved, Arc<Error>> {
		self.ensure_open()?;
		if entry.archive != self.archive.uuid() {
			return Err(Arc::new(not_held()));
		}
		let ordinal = u64::from(entry.index);
		if let Some(resolved) = self
			.retained
			.as_ref()
			.and_then(|retained| retained.access(ordinal))
		{
			return Ok(resolved);
		}
		let resolved = match self.format() {
			Some(ArchiveFormat::Zip | ArchiveFormat::SevenZ) => {
				let index = self.held_index().await?;
				match &*index {
					HeldIndex::Zip(index) => match zip_access(index, ordinal)? {
						FromIndex::Resolved(resolved) => resolved,
						FromIndex::ZipHeader {
							header,
							limit,
							compressed,
							size,
							how,
						} => {
							let (offset, compressed) =
								self.zip_data(header, limit, compressed).await?;
							match how {
								ZipRange::Mapped => Resolved::Mapped { offset, size },
								ZipRange::Seekable { codec, cipher } => Resolved::Seekable {
									stream: SeekStream {
										key: StreamKey::Entry(ordinal),
										start: offset,
										len: compressed,
										codec,
										framing: Framing::Raw,
										cipher,
									},
									offset: 0,
									size,
								},
							}
						}
					},
					HeldIndex::SevenZ { index, .. } => sevenz_access(index, ordinal)?,
					HeldIndex::Tar(_) => return Err(Arc::new(not_held())),
				}
			}
			Some(ArchiveFormat::Tar { codec }) => match self.tar_table().await? {
				Some(index) => {
					let HeldIndex::Tar(table) = &*index else {
						return Err(Arc::new(not_held()));
					};
					let span = usize::try_from(ordinal)
						.ok()
						.and_then(|at| table.members.get(at))
						.ok_or_else(not_held)?;
					match (codec, span.data) {
						(_, None) => {
							return Err(Arc::new(EntryDownloadError::NotAFile.into()));
						}
						(None, Some(offset)) => Resolved::Mapped {
							offset: offset.get(),
							size: span.size,
						},
						(Some(StreamCodec::Gzip), Some(offset)) => Resolved::Seekable {
							stream: SeekStream {
								key: StreamKey::Gzip,
								start: 0,
								len: self.archive.size(),
								codec: SeekCodec::Deflate,
								framing: Framing::Gzip,
								cipher: CipherSpec::None,
							},
							offset: offset.get(),
							size: span.size,
						},
						(Some(_), Some(_)) => Resolved::Sequential {
							size: Some(span.size),
						},
					}
				}
				// no room to hold where its members lie: read through, as ever
				None => Resolved::Sequential { size: None },
			},
			Some(ArchiveFormat::Single { .. }) => Resolved::Sequential { size: None },
			None => {
				return Err(Arc::new(Error::custom(
					ErrorKind::InvalidState,
					"the archive reader was not opened",
				)));
			}
		};
		if let Some(retained) = &self.retained {
			retained.keep_access(ordinal, resolved);
		}
		Ok(resolved)
	}

	/// Where a tar's members lie, as a listing found: the table held, or else the archive
	/// listed again for it; `None` when there is no room to hold it.
	async fn tar_table(&self) -> Result<Option<Arc<HeldIndex>>, Arc<Error>> {
		let held = || {
			self.retained
				.as_ref()
				.and_then(|retained| retained.index())
				.filter(|index| matches!(**index, HeldIndex::Tar(_)))
		};
		if let Some(index) = held() {
			return Ok(Some(index));
		}
		self.list(Unreported, JobControl::default())
			.await
			.map_err(|failed| failed.error)?;
		Ok(held())
	}

	/// Where a stored zip entry's data starts: after its local header at `header`, whose name
	/// and extra field lengths only it gives. Data that would run past `limit` (the next entry's
	/// header, or the central directory) is data two entries share, refused as an extraction
	/// refuses it.
	async fn zip_data(
		&self,
		header: u64,
		limit: u64,
		compressed: u64,
	) -> Result<(u64, u64), Arc<Error>> {
		let corrupt = |what| Arc::new(Error::custom(ErrorKind::ArchiveCorrupt, what));
		let end = header
			.checked_add(LOCAL_HEADER_LEN_U64)
			.filter(|&end| end <= self.archive.size())
			.ok_or_else(|| corrupt("an entry's local header runs past the end of the zip"))?;
		let mut bytes = futures::io::Cursor::new(Vec::with_capacity(LOCAL_HEADER_LEN));
		self.read_archive(header, end, &mut bytes).await?;
		let bytes = bytes.into_inner();
		if bytes.get(..4) != Some(&LOCAL_HEADER_SIG.to_le_bytes()[..]) {
			return Err(corrupt("an entry's local header has no signature"));
		}
		let field = |at: usize| u64::from(u16::from_le_bytes([bytes[at], bytes[at + 1]]));
		let offset = end + field(26) + field(28);
		if offset.saturating_add(compressed) > limit {
			return Err(Arc::new(
				EntryDownloadError::Skipped(ExtractSkipReason::OverlappingData).into(),
			));
		}
		Ok((offset, compressed))
	}

	/// Writes bytes `from..to` of the archive file to `writer`, chunk by chunk: those the reader
	/// keeps from there, the others fetched a few ahead while the client's file-IO budget has
	/// room for them, and kept as the ones read last.
	pub(super) async fn read_archive<W: AsyncWrite + Unpin + MaybeSend>(
		&self,
		from: u64,
		to: u64,
		writer: &mut W,
	) -> Result<(), Arc<Error>> {
		if from >= to {
			return Ok(());
		}
		let (first, last) = (from / CHUNK_SIZE_U64, (to - 1) / CHUNK_SIZE_U64);
		// the call's floor of one chunk, outside the client's budget, as a job has
		let slot = Arc::new(Semaphore::new(1));
		let memory = self.backend.memory();
		let mut pending: FuturesOrdered<MaybeSendBoxFuture<'_, Result<RangeChunk, Arc<Error>>>> =
			FuturesOrdered::new();
		let mut next = first;
		for index in first..=last {
			while next <= last && pending.len() < PREFETCH_CHUNKS {
				let permit = match take_memory(&slot, &memory) {
					Some(permit) => Some(permit),
					// the chunk to write next waits for the floor; one ahead of it waits not at all
					None if pending.is_empty() => Some(
						Arc::clone(&slot)
							.acquire_owned()
							.await
							.expect("the call's own semaphore is never closed"),
					),
					None => break,
				};
				pending.push_back(self.chunk_for_range(next, permit));
				next += 1;
			}
			let (chunk, _permit) = pending
				.next()
				.await
				.expect("the chunk to write is fetched")?;
			self.ensure_open()?;
			let start = index * CHUNK_SIZE_U64;
			// both within the chunk: `whole_chunk` checked its length against the file's
			let within = |at: u64| usize::try_from(at - start).expect("within a chunk");
			let (lo, hi) = (from.max(start), to.min(start + chunk.len() as u64));
			writer
				.write_all(&chunk[within(lo)..within(hi)])
				.await
				.map_err(|error| Arc::new(error.into()))?;
		}
		Ok(())
	}

	/// Chunk `index` of the archive: kept by the reader, or fetched and kept.
	pub(super) fn chunk_for_range(
		&self,
		index: u64,
		permit: Option<OwnedSemaphorePermit>,
	) -> MaybeSendBoxFuture<'_, Result<RangeChunk, Arc<Error>>> {
		Box::pin(async move {
			if let Some(chunk) = self
				.retained
				.as_ref()
				.and_then(|retained| retained.chunk(index))
			{
				return Ok((chunk, None));
			}
			let data = self.backend.fetch_chunk(&self.archive, index).await?;
			let chunk = Arc::new(whole_chunk(&self.archive, index, data)?);
			if let Some(retained) = &self.retained {
				retained.keep_chunk(index, Arc::clone(&chunk));
			}
			Ok((chunk, permit))
		})
	}
}

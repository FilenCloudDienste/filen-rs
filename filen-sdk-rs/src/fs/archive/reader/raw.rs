//! A RAW photo's embedded preview, written from an archive entry read by range: the container is
//! walked as microthumb walks a file in the drive, through a source that holds the chunks of the
//! entry read so far; a walk that reaches a chunk not read yet stops, the chunk is read, and the
//! walk runs again. The walk runs on the CPU pool, and takes the few reads a container needs.

#![warn(unreachable_pub, unused_qualifications)]

use std::{io, sync::Arc};

use futures::{AsyncWrite, AsyncWriteExt};
use microthumb::ByteSource;

use crate::{
	Error, ErrorKind,
	consts::CHUNK_SIZE_U64,
	fs::archive::extract::ArchiveEntryId,
	runtime::do_cpu_intensive,
	thumbnail::{EmbeddedPreview, PreviewSegment, locate_embedded_preview},
	util::MaybeSend,
};

use super::{Reader, Resolved};

/// Most chunks of an entry a walk may read: a real container takes two to four.
const MAX_WALK_CHUNKS: usize = 16;

/// An entry's chunks read so far, as the walk reads them; one it reaches unread is noted, and the
/// read fails.
struct Walked<'c> {
	len: u64,
	chunks: &'c [(u64, Vec<u8>)],
	missing: Option<u64>,
}

impl ByteSource for Walked<'_> {
	fn len(&self) -> u64 {
		self.len
	}

	fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
		if buf.is_empty() || offset >= self.len {
			return Ok(0);
		}
		let index = offset / CHUNK_SIZE_U64;
		let Some((_, chunk)) = self.chunks.iter().find(|(kept, _)| *kept == index) else {
			self.missing = Some(index);
			return Err(io::Error::other("a chunk of the entry not read yet"));
		};
		let within = usize::try_from(offset - index * CHUNK_SIZE_U64).expect("within a chunk");
		let bytes = chunk.get(within..).unwrap_or_default();
		let n = buf.len().min(bytes.len());
		buf[..n].copy_from_slice(&bytes[..n]);
		Ok(n)
	}
}

impl Reader {
	/// Writes the embedded preview of `entry`, a RAW photo read by range, to `writer`, then
	/// closes it; `None` (`writer` closed empty) when it has none.
	pub(crate) async fn write_entry_embedded_preview<W: AsyncWrite + Unpin + MaybeSend>(
		&self,
		entry: ArchiveEntryId,
		writer: &mut W,
	) -> Result<Option<EmbeddedPreview>, Arc<Error>> {
		let len = match self.resolve(entry).await? {
			Resolved::Mapped { size, .. } | Resolved::Seekable { size, .. } => size,
			Resolved::Whole { .. } | Resolved::Sequential { .. } => {
				return Err(Arc::new(Error::custom(
					ErrorKind::ArchiveUnsupported,
					"the entry is only read whole: extract it to open it",
				)));
			}
		};
		let mut chunks: Vec<(u64, Vec<u8>)> = Vec::new();
		let preview = loop {
			let (located, missing) = do_cpu_intensive(|| {
				let mut source = Walked {
					len,
					chunks: &chunks,
					missing: None,
				};
				let located = locate_embedded_preview(&mut source);
				(located, source.missing)
			})
			.await;
			match (located, missing) {
				(Ok(preview), _) => break preview,
				(Err(_), Some(index)) if chunks.len() < MAX_WALK_CHUNKS => {
					let start = index * CHUNK_SIZE_U64;
					let end = (start + CHUNK_SIZE_U64).min(len);
					let mut chunk = futures::io::Cursor::new(Vec::new());
					self.write_entry_range(entry, start, end, &mut chunk)
						.await?;
					chunks.push((index, chunk.into_inner()));
				}
				(Err(_), Some(_)) => {
					return Err(Arc::new(Error::custom(
						ErrorKind::ImageError,
						"the entry's container takes too many reads to walk",
					)));
				}
				(Err(error), None) => return Err(Arc::new(error)),
			}
		};
		drop(chunks);
		if let Some(preview) = &preview {
			for segment in preview.segments() {
				match segment {
					PreviewSegment::Source { start, end } => {
						self.write_entry_range(entry, start, end, writer).await?;
					}
					PreviewSegment::App1(app1) => writer
						.write_all(&app1)
						.await
						.map_err(|error| Arc::new(error.into()))?,
				}
			}
		}
		writer
			.close()
			.await
			.map_err(|error| Arc::new(error.into()))?;
		Ok(preview)
	}
}

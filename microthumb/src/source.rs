use std::io::{self, Read, Seek, SeekFrom};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Random access over the bytes of one image source.
///
/// Implementations may be lazy: a remote-backed source fetches only the ranges
/// actually read, so a pipeline that stops after the header or an embedded
/// preview never pays for the rest of the file. Reads past `len()` return 0.
pub trait ByteSource: Send {
	fn len(&self) -> u64;
	fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize>;

	fn is_empty(&self) -> bool {
		self.len() == 0
	}
}

/// A plain file on disk.
pub struct FileSource {
	file: std::fs::File,
	len: u64,
	cancel: Option<Arc<AtomicBool>>,
}

impl FileSource {
	pub fn new(file: std::fs::File) -> io::Result<Self> {
		Self::with_cancel(file, None)
	}

	/// `cancel` is checked before every read: flipping it makes the next read
	/// fail, unwinding a decode whose awaiting caller is already gone — the
	/// same contract remote sources honor at chunk granularity, so an orphaned
	/// local decode dies at its next read instead of running to completion
	/// under whatever gate serializes decodes.
	///
	/// The failure is `ErrorKind::Other`, never `Interrupted`: std's
	/// `read_exact` retries `Interrupted` forever, and every decoder but png
	/// and gif reads through it, so a cancel answered that way spun the decode
	/// at 100% CPU instead of ending it.
	pub fn with_cancel(file: std::fs::File, cancel: Option<Arc<AtomicBool>>) -> io::Result<Self> {
		let len = file.metadata()?.len();
		Ok(FileSource { file, len, cancel })
	}
}

impl ByteSource for FileSource {
	fn len(&self) -> u64 {
		self.len
	}

	fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
		if self
			.cancel
			.as_ref()
			.is_some_and(|c| c.load(Ordering::Relaxed))
		{
			return Err(io::Error::other("thumbnail cancelled"));
		}
		if offset >= self.len {
			return Ok(0);
		}
		self.file.seek(SeekFrom::Start(offset))?;
		self.file.read(buf)
	}
}

/// An in-memory source, for tests and already-buffered inputs.
pub struct MemSource(pub Vec<u8>);

impl ByteSource for MemSource {
	fn len(&self) -> u64 {
		self.0.len() as u64
	}

	fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
		let Ok(offset) = usize::try_from(offset) else {
			return Ok(0);
		};
		if offset >= self.0.len() {
			return Ok(0);
		}
		let n = buf.len().min(self.0.len() - offset);
		buf[..n].copy_from_slice(&self.0[offset..offset + n]);
		Ok(n)
	}
}

/// A window onto part of another source, addressed from 0.
///
/// Lets a container decoder hand an inner byte range straight to the decoder
/// that owns that format — a RAW file's embedded JPEG preview becomes an
/// ordinary JPEG source, with the same laziness the outer source has: only the
/// bytes the inner decoder actually reads are ever fetched.
pub struct SubSource {
	inner: Box<dyn ByteSource>,
	start: u64,
	len: u64,
}

impl SubSource {
	/// Clamps to what `inner` actually holds, so a forged offset or length in a
	/// container header can only ever produce a shorter window, never a read
	/// past the end.
	pub fn new(inner: Box<dyn ByteSource>, start: u64, len: u64) -> Self {
		let total = inner.len();
		let start = start.min(total);
		let len = len.min(total - start);
		SubSource { inner, start, len }
	}
}

impl ByteSource for SubSource {
	fn len(&self) -> u64 {
		self.len
	}

	fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
		if offset >= self.len {
			return Ok(0);
		}
		let available = self.len - offset;
		let want = (buf.len() as u64).min(available) as usize;
		self.inner.read_at(self.start + offset, &mut buf[..want])
	}
}

/// `Read + Seek` over any [`ByteSource`], for the sequential decoder crates.
pub struct SeqReader {
	src: Box<dyn ByteSource>,
	pos: u64,
}

impl SeqReader {
	pub fn new(src: Box<dyn ByteSource>) -> Self {
		SeqReader { src, pos: 0 }
	}

	pub fn source_len(&self) -> u64 {
		self.src.len()
	}
}

impl Read for SeqReader {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		let n = self.src.read_at(self.pos, buf)?;
		self.pos += n as u64;
		Ok(n)
	}
}

impl Seek for SeqReader {
	fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
		let target = match pos {
			SeekFrom::Start(offset) => Some(offset),
			SeekFrom::End(delta) => self.src.len().checked_add_signed(delta),
			SeekFrom::Current(delta) => self.pos.checked_add_signed(delta),
		};
		match target {
			Some(target) => {
				self.pos = target;
				Ok(target)
			}
			None => Err(io::Error::new(
				io::ErrorKind::InvalidInput,
				"seek to a negative or overflowing position",
			)),
		}
	}
}

/// [`SeqReader`], but borrowing — for decoders that are built, drained, and
/// dropped while the caller keeps owning the source (e.g. a header-only parse
/// followed by a separate full decode over the same bytes).
pub struct BorrowedSeqReader<'a> {
	src: &'a mut dyn ByteSource,
	pos: u64,
}

impl<'a> BorrowedSeqReader<'a> {
	pub fn new(src: &'a mut dyn ByteSource) -> Self {
		BorrowedSeqReader { src, pos: 0 }
	}
}

impl Read for BorrowedSeqReader<'_> {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		let n = self.src.read_at(self.pos, buf)?;
		self.pos += n as u64;
		Ok(n)
	}
}

impl Seek for BorrowedSeqReader<'_> {
	fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
		let target = match pos {
			SeekFrom::Start(offset) => Some(offset),
			SeekFrom::End(delta) => self.src.len().checked_add_signed(delta),
			SeekFrom::Current(delta) => self.pos.checked_add_signed(delta),
		};
		match target {
			Some(target) => {
				self.pos = target;
				Ok(target)
			}
			None => Err(io::Error::new(
				io::ErrorKind::InvalidInput,
				"seek to a negative or overflowing position",
			)),
		}
	}
}

#[cfg(test)]
mod tests {
	use std::io::{Read, Seek, SeekFrom};

	use super::{ByteSource, MemSource, SeqReader};

	#[test]
	fn reads_past_the_end_return_zero() {
		let mut src = MemSource(vec![1, 2, 3]);
		let mut buf = [0u8; 4];
		assert_eq!(src.read_at(3, &mut buf).unwrap(), 0);
		assert_eq!(src.read_at(2, &mut buf).unwrap(), 1);
	}

	#[test]
	fn seq_reader_tracks_position_across_seeks() {
		let mut reader = SeqReader::new(Box::new(MemSource((0u8..10).collect())));
		let mut buf = [0u8; 4];
		reader.read_exact(&mut buf).unwrap();
		assert_eq!(buf, [0, 1, 2, 3]);
		reader.seek(SeekFrom::End(-2)).unwrap();
		let n = reader.read(&mut buf).unwrap();
		assert_eq!(&buf[..n], &[8, 9]);
		assert!(reader.seek(SeekFrom::Current(-100)).is_err());
	}
}

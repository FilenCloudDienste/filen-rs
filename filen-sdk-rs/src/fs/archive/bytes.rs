//! Byte-level reading and writing the archive formats share.

use std::io::{self, Read, Seek, SeekFrom, Write};

/// Reads until `buf` is full or the stream ends; the number of bytes read.
pub(crate) fn read_full(reader: &mut (impl Read + ?Sized), buf: &mut [u8]) -> io::Result<usize> {
	let mut filled = 0;
	while filled < buf.len() {
		match reader.read(&mut buf[filled..]) {
			Ok(0) => break,
			Ok(n) => filled += n,
			Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
			Err(e) => return Err(e),
		}
	}
	Ok(filled)
}

/// The `len` bytes of `source` at `at`. A source that ends before them fails with `ends_early`;
/// its other errors pass through.
pub(crate) fn read_at<R: Read + Seek, E: From<io::Error>>(
	source: &mut R,
	at: u64,
	len: usize,
	ends_early: E,
) -> Result<Vec<u8>, E> {
	source.seek(SeekFrom::Start(at))?;
	let mut bytes = vec![0; len];
	source.read_exact(&mut bytes).map_err(|error| {
		if error.kind() == io::ErrorKind::UnexpectedEof {
			ends_early
		} else {
			error.into()
		}
	})?;
	Ok(bytes)
}

/// Counts the bytes that go through to `inner`.
pub(crate) struct Counting<W> {
	pub(crate) inner: W,
	pub(crate) written: u64,
}

impl<W> Counting<W> {
	/// Counts from `written`, bytes the archive already holds that `inner` never saw.
	pub(crate) fn new(inner: W, written: u64) -> Self {
		Self { inner, written }
	}
}

impl<W: Write> Write for Counting<W> {
	fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
		let n = self.inner.write(buf)?;
		self.written += n as u64;
		Ok(n)
	}

	fn flush(&mut self) -> io::Result<()> {
		self.inner.flush()
	}
}

/// Copies all of `data` into `out`, 64 KiB at a time; how many bytes that was, and their CRC-32.
pub(crate) fn copy_with_crc(
	data: &mut (impl Read + ?Sized),
	out: &mut (impl Write + ?Sized),
) -> io::Result<(u64, u32)> {
	let mut crc = crc32fast::Hasher::new();
	let mut buf = vec![0u8; 64 * 1024];
	let mut copied = 0u64;
	loop {
		let n = match data.read(&mut buf) {
			Ok(0) => return Ok((copied, crc.finalize())),
			Ok(n) => n,
			Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
			Err(error) => return Err(error),
		};
		crc.update(&buf[..n]);
		out.write_all(&buf[..n])?;
		copied += n as u64;
	}
}

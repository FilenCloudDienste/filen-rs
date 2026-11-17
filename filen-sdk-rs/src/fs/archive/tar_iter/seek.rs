//! Walking a bare tar's headers by seeking past its members' data instead of reading it: a
//! listing then fetches the chunks its headers are in, and those of the records that change the
//! next member (PAX and GNU long names and links), which are read as ever, and none of the
//! chunks only member data fills.

use std::io::{self, Read, Seek, SeekFrom};

use super::TarSource;

/// A tar read from a seekable `inner` of `len` bytes, skipped through by seeking.
pub(crate) struct Seeking<R> {
	inner: R,
	len: u64,
	/// Where `inner` is.
	at: u64,
}

impl<R: Read + Seek> Seeking<R> {
	pub(crate) fn new(inner: R, len: u64) -> Self {
		Self { inner, len, at: 0 }
	}
}

impl<R: Read + Seek> Read for Seeking<R> {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		let read = self.inner.read(buf)?;
		self.at += read as u64;
		Ok(read)
	}
}

impl<R: Read + Seek> TarSource for Seeking<R> {
	fn skip(&mut self, bytes: u64) -> io::Result<u64> {
		// no further than the end: seeking past it reads as data that ends early
		let skipped = bytes.min(self.len.saturating_sub(self.at));
		if skipped > 0 {
			self.at += skipped;
			self.inner.seek(SeekFrom::Start(self.at))?;
		}
		Ok(skipped)
	}
}

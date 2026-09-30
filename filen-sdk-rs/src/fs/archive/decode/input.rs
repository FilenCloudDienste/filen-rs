use std::io::{self, BufRead, Read};

use super::{CodecError, SourceError};

/// The buffered input of a decoder: a [`BufRead`] that can also be asked for a minimum number of
/// bytes, which the formats' magic numbers and fixed-size headers need, and that marks its
/// reader's errors as [`SourceError`]s.
pub(super) struct Input<R> {
	inner: R,
	buf: Box<[u8]>,
	pos: usize,
	end: usize,
	eof: bool,
}

impl<R: Read> Input<R> {
	pub(super) fn new(inner: R, capacity: usize) -> Self {
		Self {
			inner,
			buf: vec![0; capacity].into_boxed_slice(),
			pos: 0,
			end: 0,
			eof: false,
		}
	}

	/// Buffers at least `n` bytes (fewer only at the end of the input) and returns everything
	/// buffered; `InvalidInput` when `n` is more than the buffer holds.
	pub(super) fn fill_to(&mut self, n: usize) -> io::Result<&[u8]> {
		// past its capacity the buffer would fill and be taken for the end of the input
		if n > self.buf.len() {
			return Err(io::Error::new(
				io::ErrorKind::InvalidInput,
				format!("{n} bytes asked of a {}-byte input buffer", self.buf.len()),
			));
		}
		if self.end - self.pos < n && self.pos > 0 {
			self.buf.copy_within(self.pos..self.end, 0);
			self.end -= self.pos;
			self.pos = 0;
		}
		while self.end - self.pos < n && !self.eof {
			let read = self.read_inner()?;
			if read == 0 {
				self.eof = true;
			}
		}
		Ok(&self.buf[self.pos..self.end])
	}

	/// Reads exactly `N` bytes, a truncated stream when the input ends first.
	pub(super) fn read_array<const N: usize>(&mut self) -> io::Result<[u8; N]> {
		let head = self.fill_to(N)?;
		let array: [u8; N] = head
			.get(..N)
			.ok_or(CodecError::Corrupt(TRUNCATED))?
			.try_into()
			.expect("sliced to N");
		self.consume(N);
		Ok(array)
	}

	/// Reads the input to its end and returns how many bytes there were from the first non-zero
	/// one on (`prefix`, bytes a decoder took from the input but did not use, comes first):
	/// zero bytes before any data are padding, not data.
	pub(super) fn drain_trailing(&mut self, prefix: &[u8]) -> io::Result<u64> {
		let mut trailing = Trailing::default();
		trailing.push(prefix);
		loop {
			let buffered = self.fill_buf()?;
			if buffered.is_empty() {
				return Ok(trailing.unaccounted());
			}
			let len = buffered.len();
			trailing.push(buffered);
			self.consume(len);
		}
	}

	fn read_inner(&mut self) -> io::Result<usize> {
		loop {
			match self.inner.read(&mut self.buf[self.end..]) {
				Ok(read) => {
					self.end += read;
					return Ok(read);
				}
				Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
				Err(e) => return Err(io::Error::new(e.kind(), SourceError(e))),
			}
		}
	}
}

/// Counts the bytes after the end of what a reader understood: from the first non-zero one to
/// the end, since zero bytes before any data are padding.
#[derive(Debug, Default)]
pub(crate) struct Trailing {
	/// Bytes since the first non-zero one, once there was one.
	since_data: Option<u64>,
}

impl Trailing {
	pub(crate) fn push(&mut self, bytes: &[u8]) {
		match (&mut self.since_data, bytes.iter().position(|&b| b != 0)) {
			(Some(count), _) => *count += bytes.len() as u64,
			(None, Some(first)) => self.since_data = Some((bytes.len() - first) as u64),
			(None, None) => {}
		}
	}

	pub(crate) fn unaccounted(&self) -> u64 {
		self.since_data.unwrap_or(0)
	}
}

/// The message for input that ends inside a header, block or trailer.
pub(super) const TRUNCATED: &str = "the stream is truncated";

impl<R: Read> Read for Input<R> {
	fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
		let buffered = self.fill_buf()?;
		let n = buffered.len().min(out.len());
		out[..n].copy_from_slice(&buffered[..n]);
		self.consume(n);
		Ok(n)
	}
}

impl<R: Read> BufRead for Input<R> {
	fn fill_buf(&mut self) -> io::Result<&[u8]> {
		if self.pos == self.end && !self.eof {
			self.pos = 0;
			self.end = 0;
			if self.read_inner()? == 0 {
				self.eof = true;
			}
		}
		Ok(&self.buf[self.pos..self.end])
	}

	fn consume(&mut self, amount: usize) {
		self.pos = (self.pos + amount).min(self.end);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Hands out its data a few bytes per read, as a network source may.
	struct Trickle<'a>(&'a [u8], usize);

	impl Read for Trickle<'_> {
		fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
			let n = self.0.len().min(buf.len()).min(self.1);
			buf[..n].copy_from_slice(&self.0[..n]);
			self.0 = &self.0[n..];
			Ok(n)
		}
	}

	#[test]
	fn fill_to_gathers_bytes_across_short_reads() {
		let data: Vec<u8> = (0..40).collect();
		let mut input = Input::new(Trickle(&data, 3), 16);
		assert_eq!(input.fill_to(10).unwrap(), &data[..12]);
		input.consume(11);
		// the one byte left is moved to the front before refilling
		assert_eq!(input.fill_to(16).unwrap(), &data[11..27]);
		assert_eq!(input.read_array::<4>().unwrap(), [11, 12, 13, 14]);
	}

	#[test]
	fn fill_to_refuses_more_than_the_buffer_holds_and_keeps_reading() {
		let data: Vec<u8> = (0..40).collect();
		let mut input = Input::new(&data[..], 16);
		assert_eq!(
			input.fill_to(17).unwrap_err().kind(),
			io::ErrorKind::InvalidInput
		);
		// the input was not taken to have ended
		assert_eq!(input.fill_to(16).unwrap(), &data[..16]);
	}

	#[test]
	fn read_array_reports_truncation() {
		let mut input = Input::new(&[1u8, 2][..], 16);
		let error = input.read_array::<3>().unwrap_err();
		assert!(matches!(
			error.get_ref().and_then(|e| e.downcast_ref()),
			Some(CodecError::Corrupt(TRUNCATED))
		));
	}

	#[test]
	fn trailing_zeros_are_padding() {
		let mut input = Input::new(&[0u8; 100][..], 16);
		assert_eq!(input.drain_trailing(&[0, 0]).unwrap(), 0);

		let mut data = [0u8; 100];
		data[70] = 1;
		let mut input = Input::new(&data[..], 16);
		// from the first non-zero byte on
		assert_eq!(input.drain_trailing(&[0, 0]).unwrap(), 30);

		let mut input = Input::new(&[0u8; 4][..], 16);
		assert_eq!(input.drain_trailing(&[9]).unwrap(), 5);
	}

	#[test]
	fn source_errors_are_marked() {
		struct Failing;
		impl Read for Failing {
			fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
				Err(io::Error::other("channel closed"))
			}
		}
		let error = Input::new(Failing, 16).fill_to(1).unwrap_err();
		assert!(error.get_ref().unwrap().is::<SourceError>());
	}
}

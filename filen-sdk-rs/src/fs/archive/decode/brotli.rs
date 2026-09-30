//! Brotli, which has no magic number and no checksum.
//!
//! The window size is read here from the stream's first byte (RFC 7932 §9.1) and charged before
//! the decoder allocates its ring buffer. The large-window extension, which could ask for a 1 GiB
//! window, is refused; the decoder is built with `BrotliState::new_strict`, which refuses it as
//! well (the crate's `Decompressor` accepts it).

use std::{
	io::{self, BufRead, Read},
	mem,
};

use brotli::{BrotliDecompressStream, BrotliResult, BrotliState, enc::StandardAlloc};

use super::{
	Budget, CodecError, Describe, Input, StreamCheck, StreamDecoder, StreamEnd, TRUNCATED,
};

/// The decoder's Huffman tables at their largest: 256 trees in each of the literal,
/// insert-and-copy and distance groups, of up to 1080 four-byte entries, plus the context maps.
const TABLE_BYTES: u64 = 4 * 1024 * 1024;

/// What the ring buffer holds beyond the window: the decoder's write-ahead slack (542) plus the
/// longest dictionary word (24).
const RING_BUFFER_SLACK: u64 = 566;

type Decoder = BrotliState<StandardAlloc, StandardAlloc, StandardAlloc>;

enum State {
	/// The window size is not read yet.
	Start,
	/// Built once the window size has been charged.
	Decoding(Box<Decoder>),
	Done(StreamEnd),
	/// An error ended decoding.
	Failed,
}

pub(super) struct BrotliDecoder<R> {
	input: Input<R>,
	budget: Budget,
	state: State,
}

impl<R: Read> BrotliDecoder<R> {
	pub(super) fn new(input: Input<R>, budget: Budget) -> Self {
		Self {
			input,
			budget,
			state: State::Start,
		}
	}

	fn start(&mut self) -> io::Result<Box<Decoder>> {
		let [first] = *self
			.input
			.fill_to(1)?
			.first_chunk()
			.ok_or(CodecError::Corrupt(TRUNCATED))?;
		let window_bits = window_bits(first)?;
		self.budget
			.charge((1 << window_bits) + RING_BUFFER_SLACK + TABLE_BYTES)?;
		Ok(Box::new(BrotliState::new_strict(
			StandardAlloc::default(),
			StandardAlloc::default(),
			StandardAlloc::default(),
		)))
	}

	/// Decodes into `buf` with `decoder`, which goes back into the state unless the stream
	/// ended or failed.
	fn decode(&mut self, mut decoder: Box<Decoder>, buf: &mut [u8]) -> io::Result<usize> {
		loop {
			let input = self.input.fill_buf()?;
			let at_end = input.is_empty();
			let mut available_in = input.len();
			let mut input_offset = 0;
			let mut available_out = buf.len();
			let mut output_offset = 0;
			let mut total_out = 0;
			let result = BrotliDecompressStream(
				&mut available_in,
				&mut input_offset,
				input,
				&mut available_out,
				&mut output_offset,
				buf,
				&mut total_out,
				&mut decoder,
			);
			self.input.consume(input_offset);
			match result {
				BrotliResult::ResultSuccess => {
					self.state = State::Done(StreamEnd {
						check: StreamCheck::Unverifiable,
						unaccounted_bytes: self.input.drain_trailing(&[])?,
					});
					return Ok(output_offset);
				}
				BrotliResult::NeedsMoreOutput => {
					self.state = State::Decoding(decoder);
					return Ok(output_offset);
				}
				BrotliResult::NeedsMoreInput if output_offset > 0 => {
					self.state = State::Decoding(decoder);
					return Ok(output_offset);
				}
				BrotliResult::NeedsMoreInput if at_end => {
					return Err(CodecError::Corrupt(TRUNCATED).into());
				}
				BrotliResult::NeedsMoreInput => {}
				BrotliResult::ResultFailure => {
					return Err(CodecError::Corrupt(Self::INVALID).into());
				}
			}
		}
	}
}

/// The window size a stream's first byte declares: WBITS as RFC 7932 §9.1 encodes it, in 1, 4
/// or 7 bits read from the least significant bit up.
fn window_bits(first: u8) -> Result<u32, CodecError> {
	if first & 1 == 0 {
		return Ok(16);
	}
	let n = u32::from((first >> 1) & 0x07);
	if n != 0 {
		return Ok(17 + n);
	}
	match u32::from((first >> 4) & 0x07) {
		0 => Ok(17),
		// reserved by the RFC; the large-window extension uses it
		1 => Err(CodecError::Unsupported("large-window brotli")),
		m => Ok(8 + m),
	}
}

impl<R: Read> Read for BrotliDecoder<R> {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		if buf.is_empty() {
			return Ok(0);
		}
		// an error leaves the state `Failed`
		match mem::replace(&mut self.state, State::Failed) {
			State::Start => {
				let decoder = self.start()?;
				self.decode(decoder, buf)
			}
			State::Decoding(decoder) => self.decode(decoder, buf),
			State::Done(end) => {
				self.state = State::Done(end);
				Ok(0)
			}
			State::Failed => Err(CodecError::Corrupt(Self::INVALID).into()),
		}
	}
}

impl<R: Read> StreamDecoder for BrotliDecoder<R> {
	fn end(&self) -> Option<StreamEnd> {
		match self.state {
			State::Done(end) => Some(end),
			_ => None,
		}
	}
}

impl<R> Describe for BrotliDecoder<R> {
	const INVALID: &'static str = "invalid brotli data";
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn window_bits_per_rfc_7932() {
		assert_eq!(window_bits(0b0000_0000).unwrap(), 16);
		// 1 then n = 1..=7
		assert_eq!(window_bits(0b0000_0011).unwrap(), 18);
		assert_eq!(window_bits(0b0000_1111).unwrap(), 24);
		// 1, 000, then m
		assert_eq!(window_bits(0b0000_0001).unwrap(), 17);
		assert_eq!(window_bits(0b0010_0001).unwrap(), 10);
		assert_eq!(window_bits(0b0111_0001).unwrap(), 15);
		assert!(matches!(
			window_bits(0b0001_0001),
			Err(CodecError::Unsupported(_))
		));
	}
}

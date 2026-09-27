//! zstd frames, read here over ruzstd's frame decoder.
//!
//! ruzstd's `StreamingDecoder` reads a single frame. This reader takes the format as RFC 8878
//! gives it: frames one after another, each read to its end, its content checksum and content
//! size checked when its header carries them; skippable frames are skipped, their bytes counted
//! as unaccounted (no reader of the data sees them), and a stream of nothing else is no zstd
//! stream. Frames that need a dictionary are refused.
//!
//! A frame's window sets what it costs: ruzstd refuses a window over the limit it is given
//! before allocating for it, so that limit is the largest window [`Budget`] can hold. That holds
//! only while no block decodes to more than its 128 KiB maximum, which ruzstd 0.9 left
//! unchecked (a few KiB of sequences could fill gigabytes): the workspace patches in a vendored
//! copy that refuses such a block before writing any of it (`filen-sdk-rs/vendor/ruzstd`).

use std::io::{self, Read};

use ruzstd::decoding::{
	BlockDecodingStrategy, FrameDecoder,
	errors::{DecompressBlockError, FrameDecoderError},
};

use super::{
	Budget, CodecError, Describe, Input, SKIPPABLE_FRAME_MAGIC, StreamCheck, StreamDecoder,
	StreamEnd, TRUNCATED, skip_skippable_frame,
};

const MAGIC: u32 = 0xFD2F_B528;

/// The largest block a frame holds, decoded (RFC 8878 §3.1.1.2.4), which the vendored ruzstd
/// enforces.
const MAX_BLOCK_BYTES: u64 = 128 * 1024;

/// The decoder's state besides its window: the literals and block buffers (a block each), the
/// sequences of a block (at most 43690 of 12 bytes, a match copying 3 bytes at least), and the
/// entropy tables. The most measured, 0.99 MB with the ring of a 1 KiB window, is for the block
/// holding the most sequences; Huffman literals stop at their stated size (see the tests).
const STATE_BYTES: u64 = 2 * 1024 * 1024;

/// Fails the build against a ruzstd without the vendored patch that bounds what a block decodes
/// to, which nothing else here would notice: the budget above rests on that bound.
const _: fn(u64) -> DecompressBlockError =
	|at_least| DecompressBlockError::DecompressedSizeTooLarge { at_least };

pub(super) struct ZstdDecoder<R> {
	input: Input<R>,
	budget: Budget,
	decoder: FrameDecoder,
	frame: Option<Frame>,
	/// Frames read so far, skippable ones not included.
	frames: u64,
	/// Bytes of skippable frames, headers included.
	skipped: u64,
	check: StreamCheck,
	end: Option<StreamEnd>,
	failed: bool,
}

struct Frame {
	/// Whether the header states the frame's decoded size, which ruzstd reads but leaves
	/// unchecked.
	states_size: bool,
	decoded: u64,
}

impl<R: Read> ZstdDecoder<R> {
	pub(super) fn new(input: Input<R>, budget: Budget) -> Self {
		let mut decoder = FrameDecoder::new();
		decoder.set_max_window_size(max_window(budget));
		Self {
			input,
			budget,
			decoder,
			frame: None,
			frames: 0,
			skipped: 0,
			check: StreamCheck::Verified,
			end: None,
			failed: false,
		}
	}

	fn decode(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		loop {
			let Some(frame) = &mut self.frame else {
				if self.end.is_some() {
					return Ok(0);
				}
				self.start_frame()?;
				continue;
			};
			if self.decoder.can_collect() > 0 {
				let read = self
					.decoder
					.read(buf)
					.map_err(|_| CodecError::Corrupt(Self::INVALID))?;
				frame.decoded += read as u64;
				return Ok(read);
			}
			if self.decoder.is_finished() {
				self.finish_frame()?;
				continue;
			}
			// a block at a time, so what is decoded but not handed out stays one block
			let mut source = Source::new(&mut self.input);
			let decoded = self
				.decoder
				.decode_blocks(&mut source, BlockDecodingStrategy::UptoBlocks(1));
			if let Err(error) = decoded {
				return Err(source.error(error, self.budget));
			}
		}
	}

	fn start_frame(&mut self) -> io::Result<()> {
		let head = self.input.fill_to(5)?;
		let magic = head
			.first_chunk()
			.map(|magic: &[u8; 4]| u32::from_le_bytes(*magic));
		match magic {
			Some(MAGIC) => {
				let descriptor = *head.get(4).ok_or(CodecError::Corrupt(TRUNCATED))?;
				// either field puts the content size in the header (RFC 8878 §3.1.1.1.1)
				let states_size = descriptor >> 6 != 0 || descriptor & 0x20 != 0;
				let mut source = Source::new(&mut self.input);
				if let Err(error) = self.decoder.reset(&mut source) {
					return Err(source.error(error, self.budget));
				}
				self.frames += 1;
				self.frame = Some(Frame {
					states_size,
					decoded: 0,
				});
			}
			Some(magic) if SKIPPABLE_FRAME_MAGIC.contains(&magic) => {
				let skipped = skip_skippable_frame(&mut self.input)?;
				self.skipped = self.skipped.saturating_add(skipped);
			}
			_ if self.frames == 0 => {
				return Err(CodecError::Corrupt("not a zstd stream").into());
			}
			_ => {
				self.end = Some(StreamEnd {
					check: self.check,
					unaccounted_bytes: self.input.drain_trailing(&[])?.saturating_add(self.skipped),
				});
			}
		}
		Ok(())
	}

	fn finish_frame(&mut self) -> Result<(), CodecError> {
		let frame = self.frame.take().expect("called within a frame");
		match self.decoder.get_checksum_from_data() {
			Some(stored) if Some(stored) != self.decoder.get_calculated_checksum() => {
				return Err(CodecError::Corrupt("zstd content checksum mismatch"));
			}
			Some(_) => {}
			None => self.check = StreamCheck::Unverifiable,
		}
		if frame.states_size && frame.decoded != self.decoder.content_size() {
			return Err(CodecError::Corrupt(
				"a zstd frame's size differs from its header",
			));
		}
		Ok(())
	}
}

/// The largest window whose decoder `budget` holds. The window is kept in a ring buffer that
/// also takes the block being decoded and grows by doubling, so at its largest it is the power
/// of two at or above a window and a block, and while it grows the one before it is held too.
fn max_window(budget: Budget) -> u64 {
	let for_ring = budget.available.saturating_sub(STATE_BYTES);
	// the largest power of two whose ring, with the half-size one before it, fits
	let ring: u64 = match for_ring / 3 * 2 {
		0 => 0,
		fits => 1 << fits.ilog2(),
	};
	ring.saturating_sub(MAX_BLOCK_BYTES)
}

/// ruzstd's view of the input. Its errors do not keep the input's own, so a failing input is
/// kept aside here, and so is the input ending, to tell a truncated stream from a damaged one.
struct Source<'i, R> {
	input: &'i mut Input<R>,
	failure: Option<io::Error>,
	ended: bool,
}

impl<'i, R: Read> Source<'i, R> {
	fn new(input: &'i mut Input<R>) -> Self {
		Self {
			input,
			failure: None,
			ended: false,
		}
	}

	/// What ruzstd's `error` stands for.
	fn error(self, error: FrameDecoderError, budget: Budget) -> io::Error {
		if let Some(failure) = self.failure {
			return failure;
		}
		if self.ended {
			return CodecError::Corrupt(TRUNCATED).into();
		}
		match error {
			FrameDecoderError::WindowSizeTooBig { .. } => budget.exceeded(),
			FrameDecoderError::DictNotProvided { .. } => {
				CodecError::Unsupported("a zstd frame that needs a dictionary")
			}
			_ => CodecError::Corrupt(ZstdDecoder::<R>::INVALID),
		}
		.into()
	}
}

impl<R: Read> Read for Source<'_, R> {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		match self.input.read(buf) {
			Ok(0) if !buf.is_empty() => {
				self.ended = true;
				Ok(0)
			}
			Ok(read) => Ok(read),
			Err(error) => {
				let kind = error.kind();
				self.failure = Some(error);
				Err(kind.into())
			}
		}
	}
}

impl<R: Read> Read for ZstdDecoder<R> {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		if buf.is_empty() {
			return Ok(0);
		}
		if self.failed {
			return Err(CodecError::Corrupt(Self::INVALID).into());
		}
		self.decode(buf).inspect_err(|_| self.failed = true)
	}
}

impl<R: Read> StreamDecoder for ZstdDecoder<R> {
	fn end(&self) -> Option<StreamEnd> {
		self.end
	}
}

impl<R> Describe for ZstdDecoder<R> {
	const INVALID: &'static str = "invalid zstd data";
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_largest_window_leaves_room_for_the_ring_and_the_state() {
		let budget = |mib: u64| Budget::new(mib << 20).unwrap();
		// 128 MiB: a 64 MiB ring and the 32 MiB one before it
		assert_eq!(max_window(budget(128)), (64 << 20) - MAX_BLOCK_BYTES);
		assert_eq!(max_window(budget(64)), (32 << 20) - MAX_BLOCK_BYTES);
		// too little for any ring past its state
		assert_eq!(max_window(budget(2)), 0);
	}
}

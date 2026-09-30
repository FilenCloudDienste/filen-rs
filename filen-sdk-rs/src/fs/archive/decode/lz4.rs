//! lz4 frames, read here over lz4_flex's block decoder.
//!
//! lz4_flex's `FrameDecoder` returns `Ok(0)` at each frame boundary and also where a frame's
//! end mark is missing, so a truncated file would read as a complete, shorter one. This reader
//! takes the frame format as the spec gives it: every frame has to reach its end mark; its header
//! checksum, block checksums, content checksum and content size are checked when present;
//! blocks may be linked (referring back to the previous 64 KiB of the frame's output); no block
//! may exceed the frame's block size, at most 4 MiB, which is what the budget is charged for;
//! skippable frames are skipped, their bytes counted as unaccounted (no reader of the data sees
//! them), and a stream of nothing else is no lz4 stream. Frames that need an external dictionary and the legacy format
//! are refused.

use std::{
	hash::Hasher,
	io::{self, BufRead, Read},
	mem,
};

use lz4_flex::block::{decompress_into, decompress_into_with_dict};
use twox_hash::XxHash32;

use super::{
	Budget, CodecError, Describe, Input, SKIPPABLE_FRAME_MAGIC, StreamCheck, StreamDecoder,
	StreamEnd, skip_skippable_frame,
};

const MAGIC: u32 = 0x184D_2204;
const LEGACY_MAGIC: u32 = 0x184C_2102;

/// How far back a linked block may refer.
const WINDOW: usize = 64 * 1024;

pub(super) struct Lz4Decoder<R> {
	input: Input<R>,
	budget: Budget,
	state: State,
	/// Frames read so far, skippable ones not included.
	frames: u64,
	/// Bytes of skippable frames, headers included.
	skipped: u64,
	compressed: Vec<u8>,
	/// The current block's decoded bytes; `block[pos..len]` are still to be handed out.
	block: Vec<u8>,
	pos: usize,
	len: usize,
	/// The frame's last (up to) 64 KiB of output, for linked blocks.
	window: Vec<u8>,
	check: StreamCheck,
}

enum State {
	/// Expecting a frame, a skippable frame or the end.
	Between,
	Frame(Frame),
	Done(StreamEnd),
	/// An error ended decoding.
	Failed,
}

struct Frame {
	linked: bool,
	block_checksums: bool,
	content_hash: Option<XxHash32>,
	content_size: Option<u64>,
	block_max: usize,
	decoded: u64,
}

impl<R: Read> Lz4Decoder<R> {
	pub(super) fn new(input: Input<R>, budget: Budget) -> Self {
		Self {
			input,
			budget,
			state: State::Between,
			frames: 0,
			skipped: 0,
			compressed: Vec::new(),
			block: Vec::new(),
			pos: 0,
			len: 0,
			window: Vec::new(),
			check: StreamCheck::Verified,
		}
	}

	/// Decodes up to the next block with output, or to the end of the stream.
	fn advance(&mut self) -> io::Result<()> {
		while self.pos == self.len {
			// an error leaves the state `Failed`
			self.state = match mem::replace(&mut self.state, State::Failed) {
				State::Between => self.read_frame_start()?,
				State::Frame(frame) => self.read_block(frame)?,
				State::Done(end) => {
					self.state = State::Done(end);
					break;
				}
				State::Failed => return Err(CodecError::Corrupt(Self::INVALID).into()),
			};
		}
		Ok(())
	}

	/// Reads what comes between frames: the next one's start, or the end of the stream.
	fn read_frame_start(&mut self) -> io::Result<State> {
		let head = self.input.fill_to(4)?;
		let magic = head
			.get(..4)
			.map(|magic| u32::from_le_bytes(magic.try_into().expect("4 bytes")));
		match magic {
			Some(MAGIC) => {
				self.input.consume(4);
				Ok(State::Frame(self.read_frame_descriptor()?))
			}
			Some(magic) if SKIPPABLE_FRAME_MAGIC.contains(&magic) => {
				let skipped = skip_skippable_frame(&mut self.input)?;
				self.skipped = self.skipped.saturating_add(skipped);
				Ok(State::Between)
			}
			Some(LEGACY_MAGIC) => Err(CodecError::Unsupported("the legacy lz4 format").into()),
			_ if self.frames == 0 => Err(CodecError::Corrupt("not an lz4 stream").into()),
			_ => Ok(State::Done(StreamEnd {
				check: self.check,
				unaccounted_bytes: self.input.drain_trailing(&[])?.saturating_add(self.skipped),
			})),
		}
	}

	fn read_frame_descriptor(&mut self) -> io::Result<Frame> {
		let [flags, block_descriptor] = self.input.read_array()?;
		let mut descriptor = [0u8; 14];
		descriptor[..2].copy_from_slice(&[flags, block_descriptor]);
		let mut len = 2;
		if flags >> 6 != 0b01 {
			return Err(CodecError::Unsupported("an lz4 frame version").into());
		}
		if flags & 0x02 != 0 || block_descriptor & 0x8F != 0 {
			return Err(CodecError::Corrupt("reserved lz4 frame bits are set").into());
		}
		let block_max = match (block_descriptor >> 4) & 0x07 {
			4 => 64 * 1024,
			5 => 256 * 1024,
			6 => 1024 * 1024,
			7 => 4 * 1024 * 1024,
			_ => return Err(CodecError::Corrupt("an invalid lz4 block size").into()),
		};
		let content_size = if flags & 0x08 != 0 {
			let size: [u8; 8] = self.input.read_array()?;
			descriptor[len..len + 8].copy_from_slice(&size);
			len += 8;
			Some(u64::from_le_bytes(size))
		} else {
			None
		};
		let needs_dictionary = flags & 0x01 != 0;
		if needs_dictionary {
			let id: [u8; 4] = self.input.read_array()?;
			descriptor[len..len + 4].copy_from_slice(&id);
			len += 4;
		}
		let [header_checksum] = self.input.read_array()?;
		// the checksum is the hash's second byte
		if XxHash32::oneshot(0, &descriptor[..len]).to_le_bytes()[1] != header_checksum {
			return Err(CodecError::Corrupt("lz4 frame header checksum mismatch").into());
		}
		if needs_dictionary {
			return Err(CodecError::Unsupported("an lz4 frame that needs a dictionary").into());
		}

		self.budget.charge(2 * block_max as u64 + WINDOW as u64)?;
		if self.block.len() < block_max {
			self.compressed.resize(block_max, 0);
			self.block.resize(block_max, 0);
		}
		self.window.clear();
		self.frames += 1;
		Ok(Frame {
			linked: flags & 0x20 == 0,
			block_checksums: flags & 0x10 != 0,
			content_hash: (flags & 0x04 != 0).then(XxHash32::default),
			content_size,
			block_max,
			decoded: 0,
		})
	}

	/// Reads `frame`'s next block, or its end mark.
	fn read_block(&mut self, mut frame: Frame) -> io::Result<State> {
		let raw = u32::from_le_bytes(self.input.read_array()?);
		if raw == 0 {
			// the end mark
			match frame.content_hash.take() {
				Some(hash) => {
					if hash.finish_32().to_le_bytes() != self.input.read_array()? {
						return Err(CodecError::Corrupt("lz4 content checksum mismatch").into());
					}
				}
				None => self.check = StreamCheck::Unverifiable,
			}
			if frame.content_size.is_some_and(|size| size != frame.decoded) {
				return Err(
					CodecError::Corrupt("an lz4 frame's size differs from its header").into(),
				);
			}
			return Ok(State::Between);
		}

		let stored_raw = raw & 0x8000_0000 != 0;
		let size = (raw & 0x7FFF_FFFF) as usize;
		if size > frame.block_max {
			return Err(CodecError::Corrupt("an lz4 block over the frame's block size").into());
		}
		// an input that ends first fails as truncated, through `settle`
		self.input.read_exact(&mut self.compressed[..size])?;
		let stored = &self.compressed[..size];
		if frame.block_checksums
			&& XxHash32::oneshot(0, stored).to_le_bytes() != self.input.read_array()?
		{
			return Err(CodecError::Corrupt("lz4 block checksum mismatch").into());
		}
		let output = &mut self.block[..frame.block_max];
		let len = if stored_raw {
			output[..size].copy_from_slice(stored);
			size
		} else if frame.linked {
			decompress_into_with_dict(stored, output, &self.window)
				.map_err(|_| CodecError::Corrupt("invalid lz4 block"))?
		} else {
			decompress_into(stored, output).map_err(|_| CodecError::Corrupt("invalid lz4 block"))?
		};
		let decoded = &self.block[..len];
		frame.decoded += len as u64;
		if let Some(hash) = &mut frame.content_hash {
			hash.write(decoded);
		}
		if frame.linked {
			if len >= WINDOW {
				self.window.clear();
				self.window.extend_from_slice(&decoded[len - WINDOW..]);
			} else {
				let keep = WINDOW - len;
				if self.window.len() > keep {
					self.window.drain(..self.window.len() - keep);
				}
				self.window.extend_from_slice(decoded);
			}
		}
		self.pos = 0;
		self.len = len;
		Ok(State::Frame(frame))
	}
}

impl<R: Read> Read for Lz4Decoder<R> {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		if buf.is_empty() {
			return Ok(0);
		}
		self.advance()?;
		let n = (self.len - self.pos).min(buf.len());
		buf[..n].copy_from_slice(&self.block[self.pos..self.pos + n]);
		self.pos += n;
		Ok(n)
	}
}

impl<R: Read> StreamDecoder for Lz4Decoder<R> {
	fn end(&self) -> Option<StreamEnd> {
		match self.state {
			State::Done(end) => Some(end),
			_ => None,
		}
	}
}

impl<R> Describe for Lz4Decoder<R> {
	const INVALID: &'static str = "invalid lz4 data";
}

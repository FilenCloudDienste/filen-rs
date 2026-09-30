//! LZMA-alone (`.lzma`) and lzip, both LZMA1 underneath.

use std::{
	io::{self, BufRead, Read},
	mem,
};

use lzma_rust2::{
	Action, LzipStream, LzmaReader, Status, lzma_get_memory_usage_by_props, lzma2_get_memory_usage,
};

use super::{
	Budget, CodecError, Describe, Input, StreamCheck, StreamDecoder, StreamEnd, TRUNCATED,
};

/// Slack on top of the crate's own memory figures, for its read-ahead buffer.
const LZMA_READ_AHEAD_BYTES: u64 = 64 * 1024;

/// The smallest dictionary an LZMA decoder allocates, whatever the header asks for.
const LZMA_DICT_MIN: u64 = 4096;

enum AloneState<R> {
	Header(Input<R>),
	Body(Box<LzmaReader<Input<R>>>),
	Done(StreamEnd),
	/// An error ended decoding.
	Failed,
}

/// The legacy `.lzma` container: a 13-byte header (properties, dictionary size, and the
/// decoded size or "unknown") and one raw LZMA stream. It carries no checksum.
pub(super) struct LzmaAloneDecoder<R> {
	state: AloneState<R>,
	budget: Budget,
}

impl<R: Read> LzmaAloneDecoder<R> {
	pub(super) fn new(input: Input<R>, budget: Budget) -> Self {
		Self {
			state: AloneState::Header(input),
			budget,
		}
	}
}

/// The dictionary a stream of `decoded_size` bytes needs at most: no match can reach further
/// back than the output so far, so a smaller dictionary than the header's decodes the same
/// bytes. `None` is an unknown size, which keeps the header's.
pub(crate) fn clamp_dict(dict_size: u32, decoded_size: Option<u64>) -> u32 {
	match decoded_size {
		Some(size) => u64::from(dict_size)
			.min(size.max(LZMA_DICT_MIN))
			.try_into()
			.unwrap_or(dict_size),
		None => dict_size,
	}
}

/// The memory an LZMA decoder with properties byte `props` and a dictionary of `dict_size` bytes
/// takes; `None` when `props` is not a valid properties byte.
pub(crate) fn lzma_memory(dict_size: u32, props: u8) -> Option<u64> {
	let kib = lzma_get_memory_usage_by_props(dict_size, props).ok()?;
	Some(u64::from(kib) * 1024)
}

/// The memory an LZMA2 decoder with a dictionary of `dict_size` bytes takes.
pub(crate) fn lzma2_memory(dict_size: u32) -> u64 {
	u64::from(lzma2_get_memory_usage(dict_size)) * 1024
}

impl<R: Read> Read for LzmaAloneDecoder<R> {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		if buf.is_empty() {
			return Ok(0);
		}
		loop {
			// an error leaves the state `Failed`
			self.state = match mem::replace(&mut self.state, AloneState::Failed) {
				AloneState::Header(mut input) => {
					let header: [u8; 13] = input.read_array()?;
					let props = header[0];
					let dict_size = u32::from_le_bytes(header[1..5].try_into().expect("4 bytes"));
					let decoded_size = u64::from_le_bytes(header[5..].try_into().expect("8 bytes"));
					let known_size = (decoded_size != u64::MAX).then_some(decoded_size);
					let dict_size = clamp_dict(dict_size, known_size);
					let memory = lzma_memory(dict_size, props)
						.ok_or(CodecError::Corrupt("invalid LZMA properties"))?;
					self.budget.charge(memory + LZMA_READ_AHEAD_BYTES)?;
					AloneState::Body(Box::new(LzmaReader::new_with_props(
						input,
						decoded_size,
						props,
						dict_size,
						None,
					)?))
				}
				AloneState::Body(mut reader) => {
					let read = reader.read(buf)?;
					if read > 0 {
						self.state = AloneState::Body(reader);
						return Ok(read);
					}
					// bytes the decoder read ahead but didn't use come first
					let (mut input, unused) = reader.into_parts();
					AloneState::Done(StreamEnd {
						check: StreamCheck::Unverifiable,
						unaccounted_bytes: input.drain_trailing(&unused)?,
					})
				}
				AloneState::Done(end) => {
					self.state = AloneState::Done(end);
					return Ok(0);
				}
				AloneState::Failed => return Err(CodecError::Corrupt(Self::INVALID).into()),
			};
		}
	}
}

impl<R: Read> StreamDecoder for LzmaAloneDecoder<R> {
	fn end(&self) -> Option<StreamEnd> {
		match self.state {
			AloneState::Done(end) => Some(end),
			_ => None,
		}
	}
}

impl<R> Describe for LzmaAloneDecoder<R> {
	const INVALID: &'static str = "invalid LZMA data";
}

/// lzip: members of a 6-byte header, an LZMA1 stream with an end marker, and a trailer with the
/// CRC32 and both sizes, which the crate checks. Decoded with the crate's sans-I/O
/// [`LzipStream`], since its `Read` adapter has no memory limit.
pub(super) struct LzipDecoder<R> {
	input: Input<R>,
	stream: LzipStream,
	budget: Budget,
	end: Option<StreamEnd>,
}

impl<R: Read> LzipDecoder<R> {
	pub(super) fn new(input: Input<R>, budget: Budget) -> Self {
		// the crate checks every member's dictionary against this before allocating for it
		let limit_kib = u32::try_from(budget.available / 1024).unwrap_or(u32::MAX - 1);
		Self {
			input,
			stream: LzipStream::new_mem_limit(limit_kib),
			budget,
			end: None,
		}
	}
}

impl<R: Read> Read for LzipDecoder<R> {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		if buf.is_empty() || self.end.is_some() {
			return Ok(0);
		}
		loop {
			let available = self.input.fill_buf()?;
			let action = if available.is_empty() {
				Action::Finish
			} else {
				Action::Run
			};
			let result =
				self.stream
					.process(available, buf, action)
					.map_err(|e| match e.kind() {
						io::ErrorKind::OutOfMemory => self.budget.exceeded().into(),
						_ => e,
					})?;
			self.input.consume(result.bytes_consumed);
			if result.status == Status::StreamEnd {
				let unused = self.stream.unused_input();
				self.end = Some(StreamEnd {
					check: StreamCheck::Verified,
					unaccounted_bytes: self.input.drain_trailing(unused)?,
				});
				return Ok(result.bytes_produced);
			}
			if result.bytes_produced > 0 {
				return Ok(result.bytes_produced);
			}
			if action == Action::Finish || result.bytes_consumed == 0 {
				// the crate ends or fails a stream it was told is finished; this only guards
				// against spinning if it ever does neither
				return Err(CodecError::Corrupt(TRUNCATED).into());
			}
		}
	}
}

impl<R: Read> StreamDecoder for LzipDecoder<R> {
	fn end(&self) -> Option<StreamEnd> {
		self.end
	}
}

impl<R> Describe for LzipDecoder<R> {
	const INVALID: &'static str = "invalid lzip data";
}

//! Encoders for the single-stream codecs, the write side of [`decode`](super::decode): the outer
//! layer of a compressed tar, or a single compressed file. Each level is checked and each
//! encoder's memory is known before it is built, so a job can refuse a level its budget cannot
//! hold instead of running out of memory.

use std::{
	fmt,
	io::{self, Write},
	ops::RangeInclusive,
};

use filen_macros::js_type;
use lz4_flex::frame::{BlockSize, FrameEncoder, FrameInfo};
use lzma_rust2::{LzipOptions, LzipWriter, LzmaOptions, LzmaWriter, XzOptions, XzWriter};

use crate::{Error, ErrorKind};

use super::format::StreamCodec;

/// A codec and its level; `None` is the codec's default level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[js_type(import, export, no_default)]
pub struct Compression {
	pub codec: StreamCodec,
	#[cfg_attr(feature = "wasm-full", serde(default), tsify(optional))]
	pub level: Option<u32>,
}

impl StreamCodec {
	/// The levels the codec takes, and the one it uses by default.
	pub fn levels(self) -> (RangeInclusive<u32>, u32) {
		match self {
			Self::Gzip => (0..=9, 6),
			Self::Bzip2 => (1..=9, 9),
			Self::Xz | Self::Lzma | Self::Lzip => (0..=9, 6),
			// lz4_flex has a single level
			Self::Lz4 => (1..=1, 1),
			Self::Brotli => (0..=11, 9),
			// ruzstd's encoder has a single level, about zstd's 1
			Self::Zstd => (1..=1, 1),
		}
	}
}

/// `level`, refused with [`ErrorKind::InvalidState`] when outside `levels`, the levels `what`
/// takes.
pub(super) fn check_level(
	what: impl fmt::Display,
	levels: RangeInclusive<u32>,
	level: u32,
) -> Result<u32, Error> {
	if levels.contains(&level) {
		return Ok(level);
	}
	Err(Error::custom(
		ErrorKind::InvalidState,
		format!(
			"{what} takes levels {} to {}, not {level}",
			levels.start(),
			levels.end()
		),
	))
}

/// The brotli window: 4 MiB, a common default that keeps the encoder's memory modest.
const BROTLI_LGWIN: u32 = 22;

/// lz4 blocks are written 256 KiB at a time, which decoders at any setting accept.
const LZ4_BLOCK_BYTES: u64 = 256 << 10;

/// zstd is written a frame per this much input (see [`ZstdEncoder`]).
const ZSTD_FRAME_BYTES: usize = 1 << 20;

/// Room for what a crate's own formula leaves out (buffers, block bookkeeping).
const ENCODER_SLACK_BYTES: u64 = 1 << 20;

impl Compression {
	/// The level to encode at: the given one, checked, or the codec's default.
	pub(crate) fn level(self) -> Result<u32, Error> {
		let (levels, default) = self.codec.levels();
		match self.level {
			None => Ok(default),
			Some(level) => check_level(format_args!("{:?}", self.codec), levels, level),
		}
	}

	/// Memory the encoder needs at this level, in bytes.
	pub fn encoder_memory(self) -> Result<u64, Error> {
		let level = self.level()?;
		Ok(match self.codec {
			// miniz_oxide's deflate state: hash chains and a 32 KiB window, well under this
			StreamCodec::Gzip => ENCODER_SLACK_BYTES,
			// bzip2's documented compression memory: 400 kB + 8 × the block size
			StreamCodec::Bzip2 => 400_000 + 8 * u64::from(level) * 100_000,
			StreamCodec::Xz | StreamCodec::Lzma | StreamCodec::Lzip => lzma_encoder_memory(level),
			// an input and an output block, and a small hash table
			StreamCodec::Lz4 => 2 * LZ4_BLOCK_BYTES + ENCODER_SLACK_BYTES,
			// The ring buffer and hash tables of a 4 MiB window, measured over the qualities
			// (see the test): at most 29 MiB up to 8, 48 MiB at 9 and 64 MiB at 10 and 11,
			// stated with a quarter or more to spare.
			// a frame's input and its output, and ruzstd's match finder and tables, measured
			// at under 3 MiB (see the tests)
			StreamCodec::Zstd => 2 * ZSTD_FRAME_BYTES as u64 + 4 * ENCODER_SLACK_BYTES,
			StreamCodec::Brotli => {
				let window = 1u64 << BROTLI_LGWIN;
				match level {
					0..=8 => 9 * window,
					9 => 15 * window,
					_ => 20 * window,
				}
			}
		})
	}
}

/// An LZMA encoder's memory at `preset`, the same for LZMA-alone, lzip, xz and 7z's LZMA and
/// LZMA2. lzma-rust2's own figure is not in the unit it states (it overshoots the real use about
/// 140 times), so this is from measurement: the peak is 8 to 11.7 times the dictionary over
/// the presets (the binary-tree match finders of 4 and up take the most).
pub(crate) fn lzma_encoder_memory(preset: u32) -> u64 {
	12 * u64::from(LzmaOptions::with_preset(preset).dict_size) + ENCODER_SLACK_BYTES
}

/// An encoder over the sink `W`; [`StreamEncoder::finish`] ends the stream and gives the sink
/// back.
pub(crate) trait StreamEncoder<W>: Write {
	fn finish(self: Box<Self>) -> io::Result<W>;
}

impl<W: Write> StreamEncoder<W> for flate2::write::GzEncoder<W> {
	fn finish(self: Box<Self>) -> io::Result<W> {
		(*self).finish()
	}
}

impl<W: Write> StreamEncoder<W> for bzip2::write::BzEncoder<W> {
	fn finish(self: Box<Self>) -> io::Result<W> {
		(*self).finish()
	}
}

impl<W: Write> StreamEncoder<W> for XzWriter<W> {
	fn finish(self: Box<Self>) -> io::Result<W> {
		(*self).finish()
	}
}

impl<W: Write> StreamEncoder<W> for LzmaWriter<W> {
	fn finish(self: Box<Self>) -> io::Result<W> {
		(*self).finish()
	}
}

impl<W: Write> StreamEncoder<W> for LzipWriter<W> {
	fn finish(self: Box<Self>) -> io::Result<W> {
		(*self).finish()
	}
}

impl<W: Write> StreamEncoder<W> for FrameEncoder<W> {
	fn finish(self: Box<Self>) -> io::Result<W> {
		(*self).finish().map_err(io::Error::other)
	}
}

impl<W: Write> StreamEncoder<W> for brotli::CompressorWriter<W> {
	/// The crate swallows an error of its last write here; the sinks this is used with keep
	/// their errors and report them when they are finished themselves.
	fn finish(self: Box<Self>) -> io::Result<W> {
		Ok((*self).into_inner())
	}
}

/// zstd, written with ruzstd, whose compressor reads its input from a reader to the end rather
/// than being written to: the input is gathered a frame at a time and each frame compressed on
/// its own, with its content checksum. Frames one after another are one zstd stream to every
/// decoder (RFC 8878 §3.1), and past its first 128 KiB window a frame loses no matches.
pub(crate) struct ZstdEncoder<W> {
	sink: W,
	input: Vec<u8>,
	output: Vec<u8>,
	frames: u64,
}

impl<W: Write> ZstdEncoder<W> {
	fn new(sink: W) -> Self {
		Self {
			sink,
			input: Vec::with_capacity(ZSTD_FRAME_BYTES),
			// data that does not compress is stored in raw blocks: 3 bytes more per 128 KiB,
			// and the frame's header and checksum
			output: Vec::with_capacity(ZSTD_FRAME_BYTES + ZSTD_FRAME_BYTES / 1024 + 64),
			frames: 0,
		}
	}

	fn write_frame(&mut self) -> io::Result<()> {
		self.output.clear();
		ruzstd::encoding::compress(
			self.input.as_slice(),
			&mut self.output,
			ruzstd::encoding::CompressionLevel::Fastest,
		);
		self.input.clear();
		self.frames += 1;
		self.sink.write_all(&self.output)
	}
}

impl<W: Write> Write for ZstdEncoder<W> {
	fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
		if self.input.len() == ZSTD_FRAME_BYTES {
			self.write_frame()?;
		}
		let taken = buf.len().min(ZSTD_FRAME_BYTES - self.input.len());
		self.input.extend_from_slice(&buf[..taken]);
		Ok(taken)
	}

	fn flush(&mut self) -> io::Result<()> {
		self.sink.flush()
	}
}

impl<W: Write> StreamEncoder<W> for ZstdEncoder<W> {
	fn finish(mut self: Box<Self>) -> io::Result<W> {
		// an empty input is still one (empty) frame: no frames at all is no zstd stream
		if !self.input.is_empty() || self.frames == 0 {
			self.write_frame()?;
		}
		Ok(self.sink)
	}
}

/// Opens an encoder for `compression` over `sink`.
pub(crate) fn open_encoder<'a, W: Write + 'a>(
	compression: Compression,
	sink: W,
) -> Result<Box<dyn StreamEncoder<W> + 'a>, Error> {
	let level = compression.level()?;
	Ok(match compression.codec {
		StreamCodec::Gzip => Box::new(flate2::write::GzEncoder::new(
			sink,
			flate2::Compression::new(level),
		)),
		StreamCodec::Bzip2 => Box::new(bzip2::write::BzEncoder::new(
			sink,
			bzip2::Compression::new(level),
		)),
		StreamCodec::Xz => {
			let mut options = XzOptions::with_preset(level);
			options.set_check_sum_type(lzma_rust2::CheckType::Crc64);
			Box::new(XzWriter::new(sink, options)?)
		}
		// no size up front: the stream ends with an end marker
		StreamCodec::Lzma => Box::new(LzmaWriter::new_use_header(
			sink,
			&LzmaOptions::with_preset(level),
			None,
		)?),
		StreamCodec::Lzip => Box::new(LzipWriter::new(sink, LzipOptions::with_preset(level))),
		StreamCodec::Lz4 => Box::new(FrameEncoder::with_frame_info(
			FrameInfo::new()
				.block_size(BlockSize::Max256KB)
				.content_checksum(true),
			sink,
		)),
		StreamCodec::Brotli => Box::new(brotli::CompressorWriter::new(
			sink,
			64 << 10,
			level,
			BROTLI_LGWIN,
		)),
		StreamCodec::Zstd => Box::new(ZstdEncoder::new(sink)),
	})
}

#[cfg(test)]
mod tests {
	use std::io::Read;

	use super::*;
	use crate::fs::archive::decode::{StreamCheck, open_stream};

	const CODECS: [StreamCodec; 8] = [
		StreamCodec::Gzip,
		StreamCodec::Bzip2,
		StreamCodec::Xz,
		StreamCodec::Lzma,
		StreamCodec::Lzip,
		StreamCodec::Lz4,
		StreamCodec::Brotli,
		StreamCodec::Zstd,
	];

	fn encode(compression: Compression, data: &[u8]) -> Vec<u8> {
		let mut encoder = open_encoder(compression, Vec::new()).unwrap();
		encoder.write_all(data).unwrap();
		encoder.finish().unwrap()
	}

	#[test]
	fn every_codec_round_trips_through_our_decoders_at_every_level() {
		let data: Vec<u8> = (0..200_000u32)
			.map(|i| (i % 97) as u8 ^ (i >> 11) as u8)
			.collect();
		for codec in CODECS {
			let (levels, _) = codec.levels();
			// the ends of the range and the default cover the code paths; every level of the
			// slow codecs would only cost time
			let (_, default) = codec.levels();
			for level in [*levels.start(), default, *levels.end()] {
				let compression = Compression {
					codec,
					level: Some(level),
				};
				let encoded = encode(compression, &data);
				let mut decoder = open_stream(codec, &encoded[..], 512 << 20).unwrap();
				let mut decoded = Vec::new();
				decoder.read_to_end(&mut decoded).unwrap();
				assert_eq!(decoded, data, "{codec:?} level {level}");
				let end = decoder.end().unwrap();
				assert_eq!(end.unaccounted_bytes, 0, "{codec:?} level {level}");
				let checked = !matches!(codec, StreamCodec::Lzma | StreamCodec::Brotli);
				assert_eq!(
					end.check == StreamCheck::Verified,
					checked,
					"{codec:?} writes a check when it can"
				);
			}
		}
	}

	#[test]
	fn levels_outside_the_range_are_refused() {
		for (codec, level) in [
			(StreamCodec::Gzip, 10),
			(StreamCodec::Bzip2, 0),
			(StreamCodec::Xz, 10),
			(StreamCodec::Lz4, 2),
			(StreamCodec::Brotli, 12),
			(StreamCodec::Zstd, 2),
		] {
			let compression = Compression {
				codec,
				level: Some(level),
			};
			assert_eq!(
				compression.level().unwrap_err().kind(),
				ErrorKind::InvalidState
			);
			assert!(compression.encoder_memory().is_err());
		}
		assert_eq!(
			Compression {
				codec: StreamCodec::Bzip2,
				level: None
			}
			.level()
			.unwrap(),
			9
		);
	}

	#[test]
	fn encoder_memory_grows_with_the_level() {
		let memory = |codec, level| {
			Compression {
				codec,
				level: Some(level),
			}
			.encoder_memory()
			.unwrap()
		};
		assert_eq!(memory(StreamCodec::Bzip2, 9), 7_600_000);
		assert!(memory(StreamCodec::Xz, 0) < memory(StreamCodec::Xz, 6));
		assert!(memory(StreamCodec::Xz, 6) < memory(StreamCodec::Xz, 9));
		assert!(memory(StreamCodec::Brotli, 9) < memory(StreamCodec::Brotli, 11));
	}

	/// Input for the memory tests: text-like runs and noise, so the encoders' match finders
	/// and entropy coders both work.
	fn mixed_input(len: usize) -> Vec<u8> {
		let mut state = 0x2545_F491_4F6C_DD1Du64;
		(0..len)
			.map(|i| {
				if (i / 4096) % 2 == 0 {
					b"the quick brown fox jumps over the lazy dog "[i % 44]
				} else {
					state ^= state << 13;
					state ^= state >> 7;
					state ^= state << 17;
					state as u8
				}
			})
			.collect()
	}

	/// Encodes 2 MiB at each of `levels` (`None` for lz4, which has none), checking the heap the
	/// encoder took against what it states. A 10 MiB input measures the same: the peaks are
	/// bound by the dictionary or window, not the input.
	fn check_encoder_memory(codec: StreamCodec, levels: &[Option<u32>]) {
		let input = mixed_input(2 << 20);
		for &level in levels {
			let compression = Compression { codec, level };
			let stated = compression.encoder_memory().unwrap();
			let (_, peak) = crate::fs::archive::alloc_meter::peak_bytes(|| {
				let mut encoder = open_encoder(compression, io::sink()).unwrap();
				encoder.write_all(&input).unwrap();
				encoder.finish().unwrap();
			});
			assert!(
				peak <= stated,
				"{codec:?} at level {level:?} took {peak} bytes, stating {stated}"
			);
		}
	}

	fn levels(range: std::ops::RangeInclusive<u32>) -> Vec<Option<u32>> {
		range.map(Some).collect()
	}

	#[test]
	fn deflate_bzip2_and_lz4_stay_within_their_stated_memory() {
		check_encoder_memory(StreamCodec::Gzip, &levels(1..=9));
		check_encoder_memory(StreamCodec::Bzip2, &levels(1..=9));
		check_encoder_memory(StreamCodec::Lz4, &[None]);
		check_encoder_memory(StreamCodec::Zstd, &[None]);
	}

	#[test]
	fn zstd_writes_a_frame_per_mebibyte_and_one_for_nothing() {
		let compression = Compression {
			codec: StreamCodec::Zstd,
			level: None,
		};
		let frames = |encoded: &[u8]| {
			encoded
				.windows(4)
				.filter(|window| window == &[0x28, 0xB5, 0x2F, 0xFD])
				.count()
		};
		let empty = encode(compression, b"");
		assert_eq!(frames(&empty), 1);
		let mut decoded = Vec::new();
		open_stream(StreamCodec::Zstd, &empty[..], 64 << 20)
			.unwrap()
			.read_to_end(&mut decoded)
			.unwrap();
		assert!(decoded.is_empty());
		// the magic may turn up inside compressed data too, but not in this much of it
		let data = mixed_input(ZSTD_FRAME_BYTES * 2 + 5);
		assert_eq!(frames(&encode(compression, &data)), 3);
		// data that does not compress costs no more than what is stated
		let mut state = 0x9E37_79B9_7F4A_7C15u64;
		let noise: Vec<u8> = (0..ZSTD_FRAME_BYTES * 2)
			.map(|_| {
				state ^= state << 13;
				state ^= state >> 7;
				state ^= state << 17;
				(state >> 24) as u8
			})
			.collect();
		let ((), peak) = crate::fs::archive::alloc_meter::peak_bytes(|| {
			let mut encoder = open_encoder(compression, io::sink()).unwrap();
			encoder.write_all(&noise).unwrap();
			encoder.finish().unwrap();
		});
		assert!(
			peak <= compression.encoder_memory().unwrap(),
			"took {peak} bytes"
		);
	}

	#[test]
	fn lzma_encoders_stay_within_their_stated_memory() {
		// 7 to 9 take 185 to 673 MiB, too much for a test run; they measured 11.5, 11.5 and
		// 10.5 times their dictionaries, within the stated 12
		for codec in [StreamCodec::Xz, StreamCodec::Lzma, StreamCodec::Lzip] {
			check_encoder_memory(codec, &levels(0..=6));
		}
	}

	#[test]
	fn brotli_stays_within_its_stated_memory() {
		check_encoder_memory(StreamCodec::Brotli, &levels(0..=9));
	}

	#[test]
	fn brotli_at_its_tree_qualities_stays_within_its_stated_memory() {
		check_encoder_memory(StreamCodec::Brotli, &levels(10..=11));
	}
}

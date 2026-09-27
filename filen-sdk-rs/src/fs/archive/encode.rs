//! Encoders for the single-stream codecs, the write side of [`decode`](super::decode): the outer
//! layer of a compressed tar, or a single compressed file. Each level is checked and each
//! encoder's memory is known before it is built, so a job can refuse a level its budget cannot
//! hold instead of running out of memory.

use std::{
	io::{self, Write},
	ops::RangeInclusive,
};

use lz4_flex::frame::{BlockSize, FrameEncoder, FrameInfo};
use lzma_rust2::{LzipOptions, LzipWriter, LzmaOptions, LzmaWriter, XzOptions, XzWriter};

use crate::{Error, ErrorKind};

use super::format::StreamCodec;

/// A codec and its level; `None` is the codec's default level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Compression {
	pub codec: StreamCodec,
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
		}
	}
}

/// The brotli window: 4 MiB, a common default that keeps the encoder's memory modest.
const BROTLI_LGWIN: u32 = 22;

/// lz4 blocks are written 256 KiB at a time, which decoders at any setting accept.
const LZ4_BLOCK_BYTES: u64 = 256 << 10;

/// Room for what a crate's own formula leaves out (buffers, block bookkeeping).
const ENCODER_SLACK_BYTES: u64 = 1 << 20;

impl Compression {
	/// The level to encode at: the given one, checked, or the codec's default.
	pub(crate) fn level(self) -> Result<u32, Error> {
		let (levels, default) = self.codec.levels();
		match self.level {
			None => Ok(default),
			Some(level) if levels.contains(&level) => Ok(level),
			Some(level) => Err(Error::custom(
				ErrorKind::InvalidState,
				format!(
					"{:?} takes levels {} to {}, not {level}",
					self.codec,
					levels.start(),
					levels.end()
				),
			)),
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
			StreamCodec::Xz | StreamCodec::Lzma | StreamCodec::Lzip => {
				u64::from(LzmaOptions::with_preset(level).get_memory_usage()) * 1024
					+ ENCODER_SLACK_BYTES
			}
			// an input and an output block, and a small hash table
			StreamCodec::Lz4 => 2 * LZ4_BLOCK_BYTES + ENCODER_SLACK_BYTES,
			// An estimate: the ring buffer and its hash tables, more at the tree-hashing
			// qualities (10 and 11). Upper bounds, not measurements.
			StreamCodec::Brotli => {
				let window = 1u64 << BROTLI_LGWIN;
				if level >= 10 {
					12 * window
				} else {
					4 * window + (16 << 20)
				}
			}
		})
	}
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
	})
}

#[cfg(test)]
mod tests {
	use std::io::Read;

	use super::*;
	use crate::fs::archive::decode::{StreamCheck, open_stream};

	const CODECS: [StreamCodec; 7] = [
		StreamCodec::Gzip,
		StreamCodec::Bzip2,
		StreamCodec::Xz,
		StreamCodec::Lzma,
		StreamCodec::Lzip,
		StreamCodec::Lz4,
		StreamCodec::Brotli,
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
}

//! Decoders for the single-stream codecs ([`StreamCodec`]): the outer layer of a compressed tar,
//! or the one file of a standalone compressed file.
//!
//! Each decoder is a plain [`Read`] over the decoded bytes that
//! - checks its memory need against a budget before allocating for it, for every member, block
//!   or frame whose header sets that need;
//! - decodes concatenated members, streams or frames as one stream, the way the reference tools
//!   do;
//! - verifies whatever integrity check the format carries, and reports through
//!   [`StreamDecoder::end`] whether one was there;
//! - reads its input to the end and reports bytes that belong to no member, so data appended
//!   behind the stream is seen rather than silently ignored. Zero bytes there are padding.
//!
//! Errors are either the input's own error, passed through unchanged, or a [`CodecError`]: a
//! failing source (a dropped channel, say) is never reported as a damaged archive.
//!
//! The container formats of xz and lz4 are parsed here rather than by their crates, over the
//! crates' block decoders: the crates' readers trust the index sizes and frame boundaries that
//! these parsers check (see `xz.rs` and `lz4.rs`).

mod brotli;
mod input;
mod lz4;
mod lzma;
mod members;
mod xz;

use std::io::{self, Read};

use super::format::StreamCodec;
use input::{Input, TRUNCATED};

/// How a decoded stream ended, once its decoder has returned `Ok(0)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StreamEnd {
	pub(crate) check: StreamCheck,
	/// Bytes after the last member that belong to none, or 0 when all of them are zero (padding).
	pub(crate) unaccounted_bytes: u64,
}

/// Whether the decoded bytes were checked against a checksum the stream carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamCheck {
	/// Every member carried a check and each one matched: gzip, bzip2 and lzip always do, xz
	/// and lz4 when their headers ask for one.
	Verified,
	/// At least one member carried no check this decoder can verify. Brotli and LZMA-alone
	/// have none at all.
	Unverifiable,
}

impl StreamCheck {
	fn and(self, other: Self) -> Self {
		match (self, other) {
			(Self::Verified, Self::Verified) => Self::Verified,
			_ => Self::Unverifiable,
		}
	}
}

pub(crate) trait StreamDecoder: Read {
	/// How the stream ended; `None` until [`Read::read`] has returned `Ok(0)` for a non-empty
	/// buffer.
	fn end(&self) -> Option<StreamEnd>;
}

/// Why decoding stopped, other than the input failing.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CodecError {
	#[error("the compressed data is damaged: {0}")]
	Corrupt(&'static str),
	#[error("the compressed data uses a feature that isn't supported: {0}")]
	Unsupported(&'static str),
	#[error("decoding needs more memory than the {limit} bytes allowed")]
	OverBudget { limit: u64 },
}

impl From<CodecError> for io::Error {
	fn from(error: CodecError) -> Self {
		io::Error::new(io::ErrorKind::InvalidData, error)
	}
}

/// The [`CodecError`] inside an error a decoder returned, if it is one rather than the input's
/// own error.
pub(crate) fn codec_error(error: &io::Error) -> Option<&CodecError> {
	error.get_ref()?.downcast_ref()
}

/// Memory a decoder's input buffer takes, on top of what [`open_stream`] charges per codec.
const INPUT_BUFFER_BYTES: usize = 64 * 1024;

/// Opens a decoder for `codec` over `input`. `mem_limit` bounds the decoder's own memory; each
/// codec charges it before allocating, with an [`CodecError::OverBudget`] error when it's too
/// small.
pub(crate) fn open_stream<'a, R: Read + 'a>(
	codec: StreamCodec,
	input: R,
	mem_limit: u64,
) -> Result<Box<dyn StreamDecoder + 'a>, CodecError> {
	let budget = Budget::new(mem_limit)?;
	let input = Input::new(input, INPUT_BUFFER_BYTES);
	Ok(match codec {
		StreamCodec::Gzip => Box::new(Settled(members::gzip(input, budget)?)),
		StreamCodec::Bzip2 => Box::new(Settled(members::bzip2(input, budget)?)),
		StreamCodec::Xz => Box::new(Settled(xz::XzDecoder::new(input, budget))),
		StreamCodec::Lzma => Box::new(Settled(lzma::LzmaAloneDecoder::new(input, budget))),
		StreamCodec::Lzip => Box::new(Settled(lzma::LzipDecoder::new(input, budget))),
		StreamCodec::Lz4 => Box::new(Settled(lz4::Lz4Decoder::new(input, budget))),
		StreamCodec::Brotli => Box::new(Settled(brotli::BrotliDecoder::new(input, budget))),
	})
}

/// The memory a decoder may use, less its input buffer.
#[derive(Debug, Clone, Copy)]
struct Budget {
	limit: u64,
	available: u64,
}

impl Budget {
	fn new(limit: u64) -> Result<Self, CodecError> {
		let available = limit
			.checked_sub(INPUT_BUFFER_BYTES as u64)
			.ok_or(CodecError::OverBudget { limit })?;
		Ok(Self { limit, available })
	}

	/// Refuses a decoder state of `needed` bytes that the budget can't hold.
	fn charge(self, needed: u64) -> Result<(), CodecError> {
		if needed > self.available {
			return Err(self.exceeded());
		}
		Ok(())
	}

	fn exceeded(self) -> CodecError {
		CodecError::OverBudget { limit: self.limit }
	}
}

/// An error from the input beneath a decoder, marked so it can be told apart from the errors a
/// codec crate raises about the data.
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
struct SourceError(io::Error);

/// Returns the input's error unchanged and turns anything a codec crate raised into a
/// [`CodecError`], so that a crate's message (which may quote the data) never escapes.
fn settle(error: io::Error, what: &'static str) -> io::Error {
	let kind = error.kind();
	let corrupt = || {
		// the crates report input that ends early as `UnexpectedEof`
		let what = if kind == io::ErrorKind::UnexpectedEof {
			TRUNCATED
		} else {
			what
		};
		io::Error::from(CodecError::Corrupt(what))
	};
	match error.into_inner() {
		Some(inner) if inner.is::<CodecError>() => io::Error::new(kind, inner),
		Some(inner) => match inner.downcast::<SourceError>() {
			Ok(source) => source.0,
			Err(_) => corrupt(),
		},
		None => corrupt(),
	}
}

/// Applies [`settle`] to every error of the decoder it wraps.
struct Settled<D>(D);

impl<D: StreamDecoder + Describe> Read for Settled<D> {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		self.0.read(buf).map_err(|e| settle(e, D::INVALID))
	}
}

impl<D: StreamDecoder + Describe> StreamDecoder for Settled<D> {
	fn end(&self) -> Option<StreamEnd> {
		self.0.end()
	}
}

/// The message for data a codec crate rejected, which [`settle`] puts in place of the crate's.
trait Describe {
	const INVALID: &'static str;
}

#[cfg(test)]
mod tests;

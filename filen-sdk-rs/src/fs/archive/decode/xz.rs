//! xz, framed here over the crate's LZMA2, BCJ and Delta readers.
//!
//! The crate's own `XzReader` reserves memory for the record count the index declares before it
//! reads a single record, and its `XzStream` keeps one record per block. This parser checks the
//! index in constant memory instead: the record count has to equal the number of blocks
//! decoded, and a hash over the records' sizes has to equal the same hash taken while decoding.
//! Each block's filter chain is charged against the budget before it is built.

use std::{
	io::{self, BufRead, Read},
	mem,
};

use lzma_rust2::{
	Lzma2Reader,
	filter::{bcj::BcjReader, delta::DeltaReader},
	lzma2_get_memory_usage,
};
use sha2::{Digest, Sha256};

use crate::fs::archive::format::XZ_MAGIC;

use super::{
	Budget, CodecError, Describe, Input, StreamCheck, StreamDecoder, StreamEnd, TRUNCATED,
	lzma::clamp_dict,
};

const FOOTER_MAGIC: [u8; 2] = *b"YZ";

/// Memory charged per BCJ or Delta filter; their buffers are 4 KiB and 256 bytes.
const FILTER_BYTES: u64 = 64 * 1024;

pub(super) struct XzDecoder<'a, R> {
	state: State<'a, R>,
	budget: Budget,
	stream: Option<StreamState>,
	check: StreamCheck,
	end: Option<StreamEnd>,
}

enum State<'a, R> {
	StreamHeader(Input<R>),
	/// Expecting a block header, or the index that follows the last block.
	BlockHeader(Input<R>),
	Block(Box<Block<'a, R>>),
	/// Between streams: zero padding, then another stream or the end.
	StreamPadding(Input<R>),
	Done,
	/// An error ended decoding.
	Failed,
}

/// What a stream's blocks were, to check its index against.
struct StreamState {
	flags: [u8; 2],
	blocks: u64,
	/// A hash over each block's unpadded and decoded size, in order.
	records: blake3::Hasher,
}

impl StreamState {
	fn check_id(&self) -> u8 {
		self.flags[1] & 0x0F
	}
}

struct Block<'a, R> {
	chain: Box<dyn Chain<R> + 'a>,
	header_size: u64,
	compressed_size: Option<u64>,
	decoded_size: Option<u64>,
	decoded: u64,
	check: Check,
}

impl<'a, R: Read + 'a> XzDecoder<'a, R> {
	pub(super) fn new(input: Input<R>, budget: Budget) -> Self {
		Self {
			state: State::StreamHeader(input),
			budget,
			stream: None,
			check: StreamCheck::Verified,
			end: None,
		}
	}

	fn stream(&mut self) -> &mut StreamState {
		self.stream
			.as_mut()
			.expect("a stream header was read first")
	}

	fn read_stream_header(&mut self, input: &mut Input<R>) -> io::Result<()> {
		let header: [u8; 12] = input.read_array()?;
		if header[..6] != XZ_MAGIC {
			return Err(CodecError::Corrupt("not an xz stream").into());
		}
		let flags = [header[6], header[7]];
		if crc32fast::hash(&flags).to_le_bytes() != header[8..] {
			return Err(CodecError::Corrupt("stream header checksum mismatch").into());
		}
		if flags[0] != 0 || flags[1] & 0xF0 != 0 {
			return Err(CodecError::Unsupported("xz stream flags").into());
		}
		self.stream = Some(StreamState {
			flags,
			blocks: 0,
			records: blake3::Hasher::new(),
		});
		Ok(())
	}

	fn open_block(&self, mut input: Input<R>, size_byte: u8) -> io::Result<Block<'a, R>> {
		let header_size = (usize::from(size_byte) + 1) * 4;
		let mut header = [0u8; 1024];
		header[0] = size_byte;
		let rest = input.fill_to(header_size - 1)?;
		header[1..header_size].copy_from_slice(
			rest.get(..header_size - 1)
				.ok_or(CodecError::Corrupt(TRUNCATED))?,
		);
		input.consume(header_size - 1);

		let (fields, crc) = header[..header_size].split_at(header_size - 4);
		if crc32fast::hash(fields).to_le_bytes() != crc {
			return Err(CodecError::Corrupt("block header checksum mismatch").into());
		}
		let flags = fields[1];
		if flags & 0x3C != 0 {
			return Err(CodecError::Unsupported("xz block flags").into());
		}
		let mut fields = &fields[2..];
		let compressed_size = (flags & 0x40 != 0).then(|| vli(&mut fields)).transpose()?;
		let decoded_size = (flags & 0x80 != 0).then(|| vli(&mut fields)).transpose()?;
		if compressed_size == Some(0) {
			return Err(CodecError::Corrupt("a block declares no compressed data").into());
		}
		let filter_count = usize::from(flags & 0x03) + 1;
		let mut filters = Vec::with_capacity(filter_count);
		for _ in 0..filter_count {
			filters.push(Filter::parse(&mut fields)?);
		}
		if fields.iter().any(|&b| b != 0) {
			return Err(CodecError::Corrupt("block header padding isn't zero").into());
		}
		let Some((Filter::Lzma2 { dict_size }, before)) = filters.split_last() else {
			return Err(CodecError::Corrupt("an xz filter chain not ending in LZMA2").into());
		};
		let before = before
			.iter()
			.map(|filter| match filter {
				Filter::Pre(filter) => Ok(*filter),
				Filter::Lzma2 { .. } => Err(CodecError::Corrupt(
					"LZMA2 before the end of a filter chain",
				)),
			})
			.collect::<Result<Vec<_>, _>>()?;

		let dict_size = clamp_dict(*dict_size, decoded_size);
		self.budget.charge(
			u64::from(lzma2_get_memory_usage(dict_size)) * 1024
				+ FILTER_BYTES * before.len() as u64,
		)?;
		let check_id = self
			.stream
			.as_ref()
			.expect("a stream header was read first")
			.check_id();
		let block_input = BlockInput {
			input,
			limit: compressed_size,
			read: 0,
		};
		let mut chain: Box<dyn Chain<R> + 'a> =
			Box::new(Lzma2Reader::new(block_input, dict_size, None));
		// the header lists the filters in the order they were applied when encoding
		for filter in before.iter().rev() {
			chain = filter.wrap(chain);
		}
		Ok(Block {
			chain,
			header_size: header_size as u64,
			compressed_size,
			decoded_size,
			decoded: 0,
			check: Check::new(check_id),
		})
	}

	fn finish_block(&mut self, block: Block<'a, R>) -> io::Result<Input<R>> {
		let BlockInput {
			mut input,
			read: compressed,
			..
		} = block.chain.into_input();
		if block.compressed_size.is_some_and(|size| size != compressed)
			|| block.decoded_size.is_some_and(|size| size != block.decoded)
		{
			return Err(CodecError::Corrupt("a block's size differs from its header").into());
		}
		let padding = (4 - (block.header_size + compressed) % 4) % 4;
		for _ in 0..padding {
			if input.read_array::<1>()? != [0] {
				return Err(CodecError::Corrupt("block padding isn't zero").into());
			}
		}
		let check_size = block.check.size();
		let stored = input.fill_to(check_size)?;
		let stored = stored
			.get(..check_size)
			.ok_or(CodecError::Corrupt(TRUNCATED))?;
		let outcome = block.check.verify(stored)?;
		input.consume(check_size);
		self.check = self.check.and(outcome);

		let unpadded = block.header_size + compressed + check_size as u64;
		let stream = self.stream();
		stream.blocks += 1;
		stream.records.update(&unpadded.to_le_bytes());
		stream.records.update(&block.decoded.to_le_bytes());
		Ok(input)
	}

	/// Reads the index (its indicator byte already consumed) and the stream footer.
	fn read_index_and_footer(&mut self, input: &mut Input<R>) -> io::Result<()> {
		let stream = self
			.stream
			.as_ref()
			.expect("a stream header was read first");
		let mut index = IndexReader {
			input,
			crc: crc32fast::Hasher::new(),
			len: 0,
		};
		index.crc.update(&[0]);
		index.len = 1;
		if index.vli()? != stream.blocks {
			return Err(CodecError::Corrupt("the index lists a different number of blocks").into());
		}
		let mut records = blake3::Hasher::new();
		for _ in 0..stream.blocks {
			records.update(&index.vli()?.to_le_bytes());
			records.update(&index.vli()?.to_le_bytes());
		}
		if records.finalize() != stream.records.finalize() {
			return Err(CodecError::Corrupt("the index doesn't match the blocks").into());
		}
		while !index.len.is_multiple_of(4) {
			if index.byte()? != 0 {
				return Err(CodecError::Corrupt("index padding isn't zero").into());
			}
		}
		let crc = index.crc.finalize();
		let index_size = index.len + 4;
		if input.read_array::<4>()? != crc.to_le_bytes() {
			return Err(CodecError::Corrupt("index checksum mismatch").into());
		}

		let footer: [u8; 12] = input.read_array()?;
		if footer[10..] != FOOTER_MAGIC {
			return Err(CodecError::Corrupt("the stream footer is missing").into());
		}
		if crc32fast::hash(&footer[4..10]).to_le_bytes() != footer[..4] {
			return Err(CodecError::Corrupt("stream footer checksum mismatch").into());
		}
		let backward_size = (u64::from(u32::from_le_bytes(
			footer[4..8].try_into().expect("4 bytes"),
		)) + 1) * 4;
		if backward_size != index_size || footer[8..10] != stream.flags {
			return Err(CodecError::Corrupt("the stream footer doesn't match the stream").into());
		}
		Ok(())
	}
}

impl<'a, R: Read + 'a> Read for XzDecoder<'a, R> {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		if buf.is_empty() {
			return Ok(0);
		}
		loop {
			// an error leaves the state `Failed`
			self.state = match mem::replace(&mut self.state, State::Failed) {
				State::StreamHeader(mut input) => {
					self.read_stream_header(&mut input)?;
					State::BlockHeader(input)
				}
				State::BlockHeader(mut input) => match input.read_array::<1>()? {
					[0] => {
						self.read_index_and_footer(&mut input)?;
						State::StreamPadding(input)
					}
					[size_byte] => State::Block(Box::new(self.open_block(input, size_byte)?)),
				},
				State::Block(mut block) => {
					let read = block.chain.read(buf)?;
					if read == 0 {
						State::BlockHeader(self.finish_block(*block)?)
					} else {
						block.decoded += read as u64;
						if block.decoded_size.is_some_and(|size| block.decoded > size) {
							return Err(CodecError::Corrupt(
								"a block decodes to more than its header says",
							)
							.into());
						}
						block.check.update(&buf[..read]);
						self.state = State::Block(block);
						return Ok(read);
					}
				}
				State::StreamPadding(mut input) => {
					while input.fill_to(4)?.starts_with(&[0; 4]) {
						input.consume(4);
					}
					if input.fill_to(XZ_MAGIC.len())?.starts_with(&XZ_MAGIC) {
						State::StreamHeader(input)
					} else {
						self.end = Some(StreamEnd {
							check: self.check,
							unaccounted_bytes: input.drain_trailing(&[])?,
						});
						State::Done
					}
				}
				State::Done => {
					self.state = State::Done;
					return Ok(0);
				}
				State::Failed => return Err(CodecError::Corrupt(Self::INVALID).into()),
			};
		}
	}
}

impl<'a, R: Read + 'a> StreamDecoder for XzDecoder<'a, R> {
	fn end(&self) -> Option<StreamEnd> {
		self.end
	}
}

impl<R> Describe for XzDecoder<'_, R> {
	const INVALID: &'static str = "invalid xz data";
}

/// A block's compressed data: the input, cut off at the size the block header declares, if it
/// declares one.
struct BlockInput<R> {
	input: Input<R>,
	limit: Option<u64>,
	read: u64,
}

impl<R: Read> Read for BlockInput<R> {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		let len = match self.limit {
			Some(limit) => buf
				.len()
				.min(usize::try_from(limit - self.read).unwrap_or(usize::MAX)),
			None => buf.len(),
		};
		let read = self.input.read(&mut buf[..len])?;
		self.read += read as u64;
		Ok(read)
	}
}

/// A block's decoder chain, from which the input can be taken back once the block has ended.
trait Chain<R>: Read {
	fn into_input(self: Box<Self>) -> BlockInput<R>;
}

impl<R: Read> Chain<R> for Lzma2Reader<BlockInput<R>> {
	fn into_input(self: Box<Self>) -> BlockInput<R> {
		(*self).into_inner()
	}
}

impl<'a, R: Read> Chain<R> for BcjReader<Box<dyn Chain<R> + 'a>> {
	fn into_input(self: Box<Self>) -> BlockInput<R> {
		(*self).into_inner().into_input()
	}
}

impl<'a, R: Read> Chain<R> for DeltaReader<Box<dyn Chain<R> + 'a>> {
	fn into_input(self: Box<Self>) -> BlockInput<R> {
		(*self).into_inner().into_input()
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Filter {
	/// Always the last filter of a chain.
	Lzma2 {
		dict_size: u32,
	},
	Pre(PreFilter),
}

/// A filter that comes before LZMA2 in a chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreFilter {
	Delta { distance: usize },
	Bcj { arch: BcjArch, start: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BcjArch {
	X86,
	PowerPc,
	Ia64,
	Arm,
	ArmThumb,
	Sparc,
	Arm64,
	RiscV,
}

impl Filter {
	/// Parses one filter's flags: its id, the length of its properties, and the properties.
	fn parse(fields: &mut &[u8]) -> Result<Self, CodecError> {
		let id = vli(fields)?;
		let props_len = usize::try_from(vli(fields)?).unwrap_or(usize::MAX);
		if props_len > fields.len() {
			return Err(CodecError::Corrupt("malformed block header"));
		}
		let (props, rest) = fields.split_at(props_len);
		*fields = rest;
		let bcj = |arch| match props {
			[] => Ok(Self::Pre(PreFilter::Bcj { arch, start: 0 })),
			&[a, b, c, d] => Ok(Self::Pre(PreFilter::Bcj {
				arch,
				start: u32::from_le_bytes([a, b, c, d]),
			})),
			_ => Err(CodecError::Corrupt("malformed BCJ filter properties")),
		};
		match id {
			0x21 => match props {
				&[bits @ 0..=40] => Ok(Self::Lzma2 {
					dict_size: lzma2_dict_size(bits),
				}),
				_ => Err(CodecError::Corrupt("malformed LZMA2 filter properties")),
			},
			0x03 => match props {
				&[distance] => Ok(Self::Pre(PreFilter::Delta {
					distance: usize::from(distance) + 1,
				})),
				_ => Err(CodecError::Corrupt("malformed delta filter properties")),
			},
			0x04 => bcj(BcjArch::X86),
			0x05 => bcj(BcjArch::PowerPc),
			0x06 => bcj(BcjArch::Ia64),
			0x07 => bcj(BcjArch::Arm),
			0x08 => bcj(BcjArch::ArmThumb),
			0x09 => bcj(BcjArch::Sparc),
			0x0A => bcj(BcjArch::Arm64),
			0x0B => bcj(BcjArch::RiscV),
			_ => Err(CodecError::Unsupported("an xz filter")),
		}
	}
}

impl PreFilter {
	fn wrap<'a, R: Read + 'a>(self, inner: Box<dyn Chain<R> + 'a>) -> Box<dyn Chain<R> + 'a> {
		let (arch, start) = match self {
			Self::Delta { distance } => return Box::new(DeltaReader::new(inner, distance)),
			Self::Bcj { arch, start } => (arch, start as usize),
		};
		Box::new(match arch {
			BcjArch::X86 => BcjReader::new_x86(inner, start),
			BcjArch::PowerPc => BcjReader::new_ppc(inner, start),
			BcjArch::Ia64 => BcjReader::new_ia64(inner, start),
			BcjArch::Arm => BcjReader::new_arm(inner, start),
			BcjArch::ArmThumb => BcjReader::new_arm_thumb(inner, start),
			BcjArch::Sparc => BcjReader::new_sparc(inner, start),
			BcjArch::Arm64 => BcjReader::new_arm64(inner, start),
			BcjArch::RiscV => BcjReader::new_riscv(inner, start),
		})
	}
}

/// The dictionary size an LZMA2 properties byte (0 to 40) stands for.
fn lzma2_dict_size(bits: u8) -> u32 {
	if bits == 40 {
		return u32::MAX;
	}
	(2 | u32::from(bits & 1)) << (bits / 2 + 11)
}

/// Reads one of xz's variable-length integers: up to nine bytes of seven bits each, least
/// significant first, the last one without its top bit set, and no redundant zero byte.
fn vli(bytes: &mut &[u8]) -> Result<u64, CodecError> {
	let mut value = 0;
	for i in 0..9 {
		let (&byte, rest) = bytes
			.split_first()
			.ok_or(CodecError::Corrupt("malformed block header"))?;
		*bytes = rest;
		value |= u64::from(byte & 0x7F) << (i * 7);
		if byte & 0x80 == 0 {
			if i > 0 && byte == 0 {
				return Err(CodecError::Corrupt("a non-minimal variable-length integer"));
			}
			return Ok(value);
		}
	}
	Err(CodecError::Corrupt(
		"a variable-length integer over nine bytes",
	))
}

/// Reads the index byte by byte, keeping its CRC32 and length.
struct IndexReader<'i, R> {
	input: &'i mut Input<R>,
	crc: crc32fast::Hasher,
	len: u64,
}

impl<R: Read> IndexReader<'_, R> {
	fn byte(&mut self) -> io::Result<u8> {
		let [byte] = self.input.read_array()?;
		self.crc.update(&[byte]);
		self.len += 1;
		Ok(byte)
	}

	fn vli(&mut self) -> io::Result<u64> {
		// at most nine bytes, collected first so the same parser reads them
		let mut bytes = [0u8; 9];
		for (i, slot) in bytes.iter_mut().enumerate() {
			*slot = self.byte()?;
			if *slot & 0x80 == 0 {
				return Ok(vli(&mut &bytes[..=i])?);
			}
		}
		Err(CodecError::Corrupt("a variable-length integer over nine bytes").into())
	}
}

/// The integrity check of a stream's blocks, by the check id in its flags.
enum Check {
	None,
	Crc32(crc32fast::Hasher),
	Crc64(u64),
	Sha256(Box<Sha256>),
	/// A check id reserved by the format: its size is known, so it can be skipped.
	Unknown {
		size: usize,
	},
}

impl Check {
	fn new(id: u8) -> Self {
		match id {
			0x00 => Self::None,
			0x01 => Self::Crc32(crc32fast::Hasher::new()),
			0x04 => Self::Crc64(!0),
			0x0A => Self::Sha256(Box::default()),
			id => Self::Unknown {
				size: [0, 4, 4, 4, 8, 8, 8, 16, 16, 16, 32, 32, 32, 64, 64, 64]
					[usize::from(id & 0x0F)],
			},
		}
	}

	fn size(&self) -> usize {
		match self {
			Self::None => 0,
			Self::Crc32(_) => 4,
			Self::Crc64(_) => 8,
			Self::Sha256(_) => 32,
			Self::Unknown { size } => *size,
		}
	}

	fn update(&mut self, data: &[u8]) {
		match self {
			Self::None | Self::Unknown { .. } => {}
			Self::Crc32(hasher) => hasher.update(data),
			Self::Crc64(crc) => *crc = crc64_update(*crc, data),
			Self::Sha256(hasher) => hasher.update(data),
		}
	}

	/// Compares the check with the one stored after the block.
	fn verify(self, stored: &[u8]) -> Result<StreamCheck, CodecError> {
		let matches = match self {
			Self::None | Self::Unknown { .. } => return Ok(StreamCheck::Unverifiable),
			Self::Crc32(hasher) => hasher.finalize().to_le_bytes() == stored,
			Self::Crc64(crc) => (!crc).to_le_bytes() == stored,
			Self::Sha256(hasher) => hasher.finalize()[..] == *stored,
		};
		if !matches {
			return Err(CodecError::Corrupt("block checksum mismatch"));
		}
		Ok(StreamCheck::Verified)
	}
}

/// CRC-64 as xz uses it (ECMA-182, reflected), one table lookup per byte.
fn crc64_update(mut crc: u64, data: &[u8]) -> u64 {
	for &byte in data {
		crc = CRC64_TABLE[usize::from(crc.to_le_bytes()[0] ^ byte)] ^ (crc >> 8);
	}
	crc
}

const CRC64_TABLE: [u64; 256] = {
	const POLY: u64 = 0xC96C_5795_D787_0F42;
	let mut table = [0; 256];
	let mut i = 0;
	while i < 256 {
		let mut crc = i as u64;
		let mut bit = 0;
		while bit < 8 {
			crc = if crc & 1 == 1 {
				(crc >> 1) ^ POLY
			} else {
				crc >> 1
			};
			bit += 1;
		}
		table[i] = crc;
		i += 1;
	}
	table
};

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn crc64_matches_the_catalogue_check_value() {
		// CRC-64/XZ of "123456789"
		assert_eq!(!crc64_update(!0, b"123456789"), 0x995D_C9BB_DF19_39FA);
	}

	#[test]
	fn lzma2_dict_sizes() {
		assert_eq!(lzma2_dict_size(0), 4096);
		assert_eq!(lzma2_dict_size(1), 6144);
		assert_eq!(lzma2_dict_size(18), 2 << 20);
		assert_eq!(lzma2_dict_size(39), 3 << 30);
		assert_eq!(lzma2_dict_size(40), u32::MAX);
	}

	#[test]
	fn every_block_is_counted_against_the_index() {
		use std::{io::Write, num::NonZeroU64};

		let data: Vec<u8> = (0..300_000u32)
			.map(|i| (i % 251).to_le_bytes()[0] ^ (i >> 12).to_le_bytes()[0])
			.collect();
		let mut options = lzma_rust2::XzOptions::with_preset(1);
		options.lzma_options.dict_size = 64 * 1024;
		options.set_block_size(NonZeroU64::new(64 * 1024));
		let mut writer = lzma_rust2::XzWriter::new(Vec::new(), options).unwrap();
		writer.write_all(&data).unwrap();
		let bytes = writer.finish().unwrap();

		let budget = Budget::new(64 << 20).unwrap();
		let mut decoder = XzDecoder::new(Input::new(&bytes[..], 64 * 1024), budget);
		let mut out = Vec::new();
		decoder.read_to_end(&mut out).unwrap();
		assert_eq!(out, data);
		// ⌈300 000 / 65 536⌉
		assert_eq!(decoder.stream.unwrap().blocks, 5);
	}

	#[test]
	fn vli_rejects_non_minimal_and_overlong_encodings() {
		assert_eq!(vli(&mut &[0x7F][..]).unwrap(), 0x7F);
		assert_eq!(vli(&mut &[0x80, 0x01][..]).unwrap(), 0x80);
		assert!(vli(&mut &[0x80, 0x00][..]).is_err());
		assert!(vli(&mut &[0xFF; 10][..]).is_err());
		assert!(vli(&mut &[0x80][..]).is_err());
	}
}

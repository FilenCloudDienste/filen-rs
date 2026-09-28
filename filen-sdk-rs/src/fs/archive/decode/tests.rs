//! Round trips through reference encoders, and the damage each decoder has to notice.

use std::{
	io::{self, Read, Write},
	num::NonZeroU64,
};

use lz4_flex::frame::{BlockMode, BlockSize, FrameEncoder, FrameInfo};
use lzma_rust2::{
	CheckType, FilterConfig, LzipOptions, LzipWriter, LzmaOptions, LzmaWriter, XzOptions, XzWriter,
};

use ruzstd::encoding::{CompressionLevel, compress_to_vec};

use super::*;
use crate::fs::archive::{
	alloc_meter::peak_bytes,
	test_support::{damaged_copies, gzip},
};

const MIB_USIZE: usize = 1024 * 1024;
const MIB: u64 = MIB_USIZE as u64;
const BUDGET: u64 = 64 * MIB;

/// Compressible but not trivial: runs of repeated phrases mixed with pseudo-random bytes.
fn sample(len: usize) -> Vec<u8> {
	let mut state = 0x2545_F491_u32;
	let mut out = Vec::with_capacity(len);
	while out.len() < len {
		state ^= state << 13;
		state ^= state >> 17;
		state ^= state << 5;
		if state.is_multiple_of(3) {
			out.extend_from_slice(b"the quick brown fox jumps over the lazy dog ");
		} else {
			out.extend_from_slice(&state.to_le_bytes());
		}
	}
	out.truncate(len);
	out
}

fn decode(codec: StreamCodec, bytes: &[u8], budget: u64) -> io::Result<(Vec<u8>, StreamEnd)> {
	let mut decoder = open_stream(codec, bytes, budget)?;
	let mut out = Vec::new();
	decoder.read_to_end(&mut out)?;
	let end = decoder.end().expect("read to the end");
	Ok((out, end))
}

fn codec_err(result: io::Result<(Vec<u8>, StreamEnd)>) -> CodecError {
	let error = result.expect_err("decoding should fail");
	match codec_error(&error) {
		Some(CodecError::Corrupt(what)) => CodecError::Corrupt(what),
		Some(CodecError::Unsupported(what)) => CodecError::Unsupported(what),
		Some(CodecError::OverBudget { limit }) => CodecError::OverBudget { limit: *limit },
		None => panic!("not a codec error: {error}"),
	}
}

fn corrupt(result: io::Result<(Vec<u8>, StreamEnd)>) -> &'static str {
	match codec_err(result) {
		CodecError::Corrupt(what) => what,
		other => panic!("expected corrupt data, got {other}"),
	}
}

const VERIFIED: StreamEnd = StreamEnd {
	check: StreamCheck::Verified,
	unaccounted_bytes: 0,
};

const UNVERIFIABLE: StreamEnd = StreamEnd {
	check: StreamCheck::Unverifiable,
	unaccounted_bytes: 0,
};

fn bzip2(data: &[u8]) -> Vec<u8> {
	let mut encoder = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
	encoder.write_all(data).unwrap();
	encoder.finish().unwrap()
}

fn xz(data: &[u8], configure: impl FnOnce(&mut XzOptions)) -> Vec<u8> {
	let mut options = XzOptions::with_preset(1);
	configure(&mut options);
	let mut writer = XzWriter::new(Vec::new(), options).unwrap();
	writer.write_all(data).unwrap();
	writer.finish().unwrap()
}

fn lz4(data: &[u8], info: FrameInfo) -> Vec<u8> {
	let mut encoder = FrameEncoder::with_frame_info(info, Vec::new());
	encoder.write_all(data).unwrap();
	encoder.finish().unwrap()
}

/// What the zstd decoder reports damaged data as.
const ZSTD_INVALID: &str = "invalid zstd data";

fn zstd(data: &[u8]) -> Vec<u8> {
	compress_to_vec(data, CompressionLevel::Fastest)
}

/// A zstd frame of raw blocks, written by hand: a window of `1 << window_log` bytes, the content
/// size in the header when `states_size`, and a dictionary id when `dictionary` is given.
fn zstd_raw_frame(
	data: &[u8],
	window_log: u8,
	states_size: bool,
	dictionary: Option<u8>,
) -> Vec<u8> {
	let mut frame = vec![0x28, 0xB5, 0x2F, 0xFD];
	// FCS_flag 2 (4 bytes) when stating the size, Dictionary_ID_flag 1 (1 byte) with one
	let descriptor = if states_size { 2 << 6 } else { 0 } | u8::from(dictionary.is_some());
	frame.push(descriptor);
	frame.push((window_log - 10) << 3);
	frame.extend(dictionary);
	if states_size {
		frame.extend_from_slice(&(u32::try_from(data.len()).unwrap()).to_le_bytes());
	}
	let block_max = (1usize << window_log).min(128 << 10);
	let mut blocks = data.chunks(block_max).peekable();
	if blocks.peek().is_none() {
		frame.extend_from_slice(&[1, 0, 0]);
	}
	while let Some(block) = blocks.next() {
		// raw blocks: the last-block bit, type 0, and the size above them
		let header =
			u32::from(blocks.peek().is_none()) | (u32::try_from(block.len()).unwrap()) << 3;
		frame.extend_from_slice(&header.to_le_bytes()[..3]);
		frame.extend_from_slice(block);
	}
	frame
}

fn brotli(data: &[u8]) -> Vec<u8> {
	let mut writer = ::brotli::CompressorWriter::new(Vec::new(), 4096, 9, 22);
	writer.write_all(data).unwrap();
	writer.into_inner()
}

#[test]
fn gzip_members_are_one_stream() {
	let (a, b) = (sample(100_000), sample(3_000));
	let mut bytes = [gzip(&a), gzip(&b)].concat();
	bytes.extend_from_slice(&[0; 3]);
	let (out, end) = decode(StreamCodec::Gzip, &bytes, BUDGET).unwrap();
	assert_eq!(out, [a, b].concat());
	assert_eq!(end, VERIFIED);
}

#[test]
fn gzip_damage() {
	let data = sample(50_000);
	let bytes = gzip(&data);

	let mut with_junk = bytes.clone();
	with_junk.extend_from_slice(b"junk");
	let (out, end) = decode(StreamCodec::Gzip, &with_junk, BUDGET).unwrap();
	assert_eq!(out, data);
	assert_eq!(end.unaccounted_bytes, 4);

	// the CRC32 is the trailer's first four bytes
	let mut bad_crc = bytes.clone();
	let at = bad_crc.len() - 8;
	bad_crc[at] ^= 1;
	assert_eq!(
		corrupt(decode(StreamCodec::Gzip, &bad_crc, BUDGET)),
		"invalid gzip data"
	);

	assert_eq!(
		corrupt(decode(StreamCodec::Gzip, &bytes[..bytes.len() - 3], BUDGET)),
		TRUNCATED
	);
	assert_eq!(
		corrupt(decode(StreamCodec::Gzip, b"plain text", BUDGET)),
		"not a gzip stream"
	);
}

#[test]
fn bzip2_members_and_budget() {
	let (a, b) = (sample(200_000), sample(10));
	let bytes = [bzip2(&a), bzip2(&b)].concat();
	let (out, end) = decode(StreamCodec::Bzip2, &bytes, BUDGET).unwrap();
	assert_eq!(out, [a, b].concat());
	assert_eq!(end, VERIFIED);

	assert!(matches!(
		codec_err(decode(StreamCodec::Bzip2, &bytes, 2 * MIB)),
		CodecError::OverBudget { limit } if limit == 2 * MIB
	));
}

#[test]
fn xz_checks() {
	let data = sample(100_000);
	for (check, expected) in [
		(CheckType::None, UNVERIFIABLE),
		(CheckType::Crc32, VERIFIED),
		(CheckType::Crc64, VERIFIED),
		(CheckType::Sha256, VERIFIED),
	] {
		let bytes = xz(&data, |options| options.set_check_sum_type(check));
		let (out, end) = decode(StreamCodec::Xz, &bytes, BUDGET).unwrap();
		assert_eq!(out, data, "{check:?}");
		assert_eq!(end, expected, "{check:?}");
	}
}

#[test]
fn xz_blocks_filters_and_streams() {
	let data = sample(300_000);
	let many_blocks = xz(&data, |options| {
		options.lzma_options.dict_size = 64 * 1024;
		options.set_block_size(NonZeroU64::new(64 * 1024));
	});
	assert_eq!(
		decode(StreamCodec::Xz, &many_blocks, BUDGET).unwrap(),
		(data.clone(), VERIFIED)
	);

	for filters in [
		vec![FilterConfig::new_bcj_x86(0)],
		vec![FilterConfig::new_bcj_arm64(0)],
		vec![FilterConfig::new_delta(4), FilterConfig::new_bcj_x86(0)],
	] {
		let bytes = xz(&data, |options| options.filters = filters);
		assert_eq!(
			decode(StreamCodec::Xz, &bytes, BUDGET).unwrap(),
			(data.clone(), VERIFIED)
		);
	}

	// two streams, with stream padding between them and after the last
	let small = sample(1000);
	let mut two = xz(&data, |_| {});
	two.extend_from_slice(&[0; 4]);
	two.extend(xz(&small, |_| {}));
	two.extend_from_slice(&[0; 8]);
	assert_eq!(
		decode(StreamCodec::Xz, &two, BUDGET).unwrap(),
		([data.clone(), small].concat(), VERIFIED)
	);
}

#[test]
fn xz_damage() {
	let data = sample(100_000);
	let bytes = xz(&data, |_| {});

	// the backward size in the footer: its checksum no longer matches
	let mut footer = bytes.clone();
	let at = footer.len() - 8;
	footer[at] ^= 1;
	assert_eq!(
		corrupt(decode(StreamCodec::Xz, &footer, BUDGET)),
		"stream footer checksum mismatch"
	);

	assert_eq!(
		corrupt(decode(StreamCodec::Xz, &bytes[..bytes.len() - 12], BUDGET)),
		TRUNCATED
	);

	let mut junk = bytes.clone();
	junk.extend_from_slice(&[0, 0, 0, 0, 7]);
	assert_eq!(
		decode(StreamCodec::Xz, &junk, BUDGET)
			.unwrap()
			.1
			.unaccounted_bytes,
		1
	);

	// preset 6 asks for an 8 MiB dictionary
	let big_dict = xz(&data, |options| *options = XzOptions::with_preset(6));
	assert!(matches!(
		codec_err(decode(StreamCodec::Xz, &big_dict, 2 * MIB)),
		CodecError::OverBudget { .. }
	));
}

#[test]
fn xz_index_must_match_the_blocks() {
	// an empty stream: header, the index (indicator, zero records, padding, CRC32), footer
	let bytes = xz(&[], |options| options.set_check_sum_type(CheckType::Crc32));
	assert_eq!(
		decode(StreamCodec::Xz, &bytes, BUDGET).unwrap(),
		(vec![], VERIFIED)
	);
	assert_eq!(bytes[12..14], [0x00, 0x00]);

	// the same index claiming one record, its CRC32 recomputed
	let mut claims_one = bytes.clone();
	claims_one[13] = 0x01;
	let crc = crc32fast::hash(&claims_one[12..16]);
	claims_one[16..20].copy_from_slice(&crc.to_le_bytes());
	assert_eq!(
		corrupt(decode(StreamCodec::Xz, &claims_one, BUDGET)),
		"the index lists a different number of blocks"
	);
}

#[test]
fn lzma_alone_with_and_without_a_size() {
	let data = sample(80_000);
	for size in [Some(data.len() as u64), None] {
		let mut writer =
			LzmaWriter::new_use_header(Vec::new(), &LzmaOptions::with_preset(6), size).unwrap();
		writer.write_all(&data).unwrap();
		let mut bytes = writer.finish().unwrap();
		assert_eq!(
			decode(StreamCodec::Lzma, &bytes, BUDGET).unwrap(),
			(data.clone(), UNVERIFIABLE),
			"{size:?}"
		);
		bytes.extend_from_slice(b"tail");
		assert_eq!(
			decode(StreamCodec::Lzma, &bytes, BUDGET)
				.unwrap()
				.1
				.unaccounted_bytes,
			4
		);
	}
}

#[test]
fn lzma_alone_dictionary_is_clamped_to_the_size() {
	// preset 9 asks for 64 MiB, but the stream says it decodes to 1000 bytes
	let data = sample(1000);
	let mut writer =
		LzmaWriter::new_use_header(Vec::new(), &LzmaOptions::with_preset(9), Some(1000)).unwrap();
	writer.write_all(&data).unwrap();
	let bytes = writer.finish().unwrap();
	assert_eq!(decode(StreamCodec::Lzma, &bytes, 2 * MIB).unwrap().0, data);
}

#[test]
fn lzip_members() {
	let data = sample(300_000);
	let mut options = LzipOptions::with_preset(1);
	options.member_size = NonZeroU64::new(64 * 1024);
	let mut writer = LzipWriter::new(Vec::new(), options);
	writer.write_all(&data).unwrap();
	let mut bytes = writer.finish().unwrap();
	bytes.extend_from_slice(&[0; 5]);
	assert_eq!(
		decode(StreamCodec::Lzip, &bytes, BUDGET).unwrap(),
		(data.clone(), VERIFIED)
	);

	let mut bad = bytes.clone();
	// the data size in the last member's trailer
	let at = bad.len() - 5 - 16;
	bad[at] ^= 1;
	assert_eq!(
		corrupt(decode(StreamCodec::Lzip, &bad, BUDGET)),
		"invalid lzip data"
	);

	let mut writer = LzipWriter::new(Vec::new(), LzipOptions::with_preset(6));
	writer.write_all(&data).unwrap();
	let big_dict = writer.finish().unwrap();
	assert!(matches!(
		codec_err(decode(StreamCodec::Lzip, &big_dict, 2 * MIB)),
		CodecError::OverBudget { limit } if limit == 2 * MIB
	));
}

#[test]
fn lz4_frames() {
	let data = sample(300_000);
	let linked = FrameInfo::new()
		.block_size(BlockSize::Max64KB)
		.block_mode(BlockMode::Linked)
		.block_checksums(true)
		.content_checksum(true)
		.content_size(Some(data.len() as u64));
	let independent = FrameInfo::new().block_size(BlockSize::Max256KB);
	assert_eq!(
		decode(StreamCodec::Lz4, &lz4(&data, linked.clone()), BUDGET).unwrap(),
		(data.clone(), VERIFIED)
	);
	assert_eq!(
		decode(StreamCodec::Lz4, &lz4(&data, independent), BUDGET).unwrap(),
		(data.clone(), UNVERIFIABLE)
	);

	// a skippable frame ahead of the data, a second frame, and zero padding
	let small = sample(10);
	let mut bytes = vec![0x50, 0x2A, 0x4D, 0x18, 3, 0, 0, 0, 1, 2, 3];
	bytes.extend(lz4(&data, linked));
	bytes.extend(lz4(&small, FrameInfo::new().content_checksum(true)));
	bytes.extend_from_slice(&[0; 2]);
	assert_eq!(
		decode(StreamCodec::Lz4, &bytes, BUDGET).unwrap(),
		(
			[data.clone(), small].concat(),
			StreamEnd {
				unaccounted_bytes: 11,
				..VERIFIED
			}
		)
	);
	// skippable frames alone hold no lz4 data
	assert_eq!(
		corrupt(decode(
			StreamCodec::Lz4,
			&[0x50, 0x2A, 0x4D, 0x18, 3, 0, 0, 0, 1, 2, 3],
			BUDGET
		)),
		"not an lz4 stream"
	);
}

#[test]
fn lz4_damage() {
	let data = sample(100_000);
	let info = FrameInfo::new().content_checksum(true);
	let bytes = lz4(&data, info);

	// without the end mark and content checksum
	assert_eq!(
		corrupt(decode(StreamCodec::Lz4, &bytes[..bytes.len() - 8], BUDGET)),
		TRUNCATED
	);

	let mut bad_checksum = bytes.clone();
	*bad_checksum.last_mut().unwrap() ^= 1;
	assert_eq!(
		corrupt(decode(StreamCodec::Lz4, &bad_checksum, BUDGET)),
		"lz4 content checksum mismatch"
	);

	let mut bad_header = bytes.clone();
	bad_header[6] ^= 1;
	assert_eq!(
		corrupt(decode(StreamCodec::Lz4, &bad_header, BUDGET)),
		"lz4 frame header checksum mismatch"
	);

	let mut legacy = bytes.clone();
	legacy[..4].copy_from_slice(&[0x02, 0x21, 0x4C, 0x18]);
	assert!(matches!(
		codec_err(decode(StreamCodec::Lz4, &legacy, BUDGET)),
		CodecError::Unsupported("the legacy lz4 format")
	));
}

#[test]
fn brotli_round_trip_and_damage() {
	let data = sample(200_000);
	let bytes = brotli(&data);
	assert_eq!(
		decode(StreamCodec::Brotli, &bytes, BUDGET).unwrap(),
		(data.clone(), UNVERIFIABLE)
	);

	let mut junk = bytes.clone();
	junk.extend_from_slice(b"xy");
	assert_eq!(
		decode(StreamCodec::Brotli, &junk, BUDGET)
			.unwrap()
			.1
			.unaccounted_bytes,
		2
	);

	assert_eq!(
		corrupt(decode(
			StreamCodec::Brotli,
			&bytes[..bytes.len() / 2],
			BUDGET
		)),
		TRUNCATED
	);

	// a 4 MiB window (22 bits) and the tables need more than 4 MiB
	assert!(matches!(
		codec_err(decode(StreamCodec::Brotli, &bytes, 4 * MIB)),
		CodecError::OverBudget { .. }
	));
}

/// Decodes `bytes` with every bit of `flips` flipped at each position in turn, and truncated at
/// each length: a damaged stream is refused or decodes to something, never panics, and every
/// refusal is a [`CodecError`].
fn damage_never_panics(codec: StreamCodec, bytes: &[u8]) {
	let check = |damaged: &[u8], what: &str| {
		let result = std::panic::catch_unwind(|| decode(codec, damaged, BUDGET))
			.unwrap_or_else(|_| panic!("{codec:?}: {what} panicked"));
		if let Err(error) = result {
			assert!(codec_error(&error).is_some(), "{codec:?}: {what}: {error}");
		}
	};
	for (at, bit, damaged) in damaged_copies(bytes, 0..bytes.len()) {
		check(&damaged, &format!("a flip of {bit:#x} at {at}"));
	}
	for at in 0..bytes.len() {
		check(&bytes[..at], &format!("a cut at {at}"));
	}
}

#[test]
fn a_damaged_xz_never_panics() {
	let data = sample(3_000);
	// a filter chain and a second stream, so the damage reaches every part of the framing
	let mut bytes = xz(&data, |options| {
		options.filters = vec![FilterConfig::new_delta(2), FilterConfig::new_bcj_x86(0)];
		options.set_check_sum_type(CheckType::Sha256);
	});
	bytes.extend_from_slice(&[0; 4]);
	bytes.extend(xz(&data[..100], |options| {
		options.set_check_sum_type(CheckType::Crc32)
	}));
	damage_never_panics(StreamCodec::Xz, &bytes);
}

#[test]
fn a_damaged_lz4_never_panics() {
	let data = sample(3_000);
	let mut bytes = vec![0x5F, 0x2A, 0x4D, 0x18, 2, 0, 0, 0, 7, 7];
	bytes.extend(lz4(
		&data,
		FrameInfo::new()
			.block_size(BlockSize::Max64KB)
			.block_mode(BlockMode::Linked)
			.block_checksums(true)
			.content_checksum(true)
			.content_size(Some(data.len() as u64)),
	));
	bytes.extend(lz4(&data[..100], FrameInfo::new()));
	damage_never_panics(StreamCodec::Lz4, &bytes);
}

#[test]
fn zstd_frames() {
	let data = sample(300_000);
	assert_eq!(
		decode(StreamCodec::Zstd, &zstd(&data), BUDGET).unwrap(),
		(data.clone(), VERIFIED)
	);
	// a skippable frame ahead of the data, one between frames, a frame without a checksum
	// that states its size, and zero padding
	let small = sample(10);
	let mut bytes = vec![0x50, 0x2A, 0x4D, 0x18, 3, 0, 0, 0, 1, 2, 3];
	bytes.extend(zstd(&data));
	bytes.extend_from_slice(&[0x5F, 0x2A, 0x4D, 0x18, 0, 0, 0, 0]);
	bytes.extend(zstd_raw_frame(&small, 17, true, None));
	bytes.extend_from_slice(&[0; 2]);
	assert_eq!(
		decode(StreamCodec::Zstd, &bytes, BUDGET).unwrap(),
		(
			[data.clone(), small].concat(),
			StreamEnd {
				unaccounted_bytes: 19,
				..UNVERIFIABLE
			}
		)
	);
	// data after the last frame
	let mut junk = zstd(&data);
	junk.extend_from_slice(b"junk");
	assert_eq!(
		decode(StreamCodec::Zstd, &junk, BUDGET).unwrap().1,
		StreamEnd {
			unaccounted_bytes: 4,
			..VERIFIED
		}
	);
	// skippable frames alone hold no zstd data, and neither does nothing
	assert_eq!(
		corrupt(decode(
			StreamCodec::Zstd,
			&[0x50, 0x2A, 0x4D, 0x18, 3, 0, 0, 0, 1, 2, 3],
			BUDGET
		)),
		"not a zstd stream"
	);
	assert_eq!(
		corrupt(decode(StreamCodec::Zstd, &[], BUDGET)),
		"not a zstd stream"
	);
}

#[test]
fn zstd_damage() {
	let data = sample(100_000);
	let bytes = zstd(&data);
	assert_eq!(
		corrupt(decode(StreamCodec::Zstd, &bytes[..bytes.len() - 2], BUDGET)),
		TRUNCATED
	);
	assert_eq!(
		corrupt(decode(StreamCodec::Zstd, &bytes[..bytes.len() / 2], BUDGET)),
		TRUNCATED
	);
	let mut bad_checksum = bytes.clone();
	*bad_checksum.last_mut().unwrap() ^= 1;
	assert_eq!(
		corrupt(decode(StreamCodec::Zstd, &bad_checksum, BUDGET)),
		"zstd content checksum mismatch"
	);
	// a header stating a size the frame does not decode to
	let mut wrong_size = zstd_raw_frame(&data[..100], 17, true, None);
	wrong_size[6] += 1;
	assert_eq!(
		corrupt(decode(StreamCodec::Zstd, &wrong_size, BUDGET)),
		"a zstd frame's size differs from its header"
	);
	assert!(matches!(
		codec_err(decode(
			StreamCodec::Zstd,
			&zstd_raw_frame(b"x", 17, false, Some(7)),
			BUDGET
		)),
		CodecError::Unsupported("a zstd frame that needs a dictionary")
	));
}

#[test]
fn a_zstd_window_is_charged_before_it_is_allocated() {
	// a 64 MiB window's ring needs 192 MiB at its peak: refused under a 64 MiB budget, without
	// allocating for it
	let frame = zstd_raw_frame(b"small", 26, false, None);
	let (result, peak) = peak_bytes(|| decode(StreamCodec::Zstd, &frame, BUDGET));
	assert!(matches!(
		codec_err(result),
		CodecError::OverBudget { limit } if limit == BUDGET
	));
	assert!(peak < MIB, "took {peak} bytes");
	assert_eq!(
		decode(StreamCodec::Zstd, &frame, 256 * MIB).unwrap().0,
		b"small"
	);
}

/// Hashes what is written to it, without keeping it.
#[derive(Default)]
struct Hashed {
	len: u64,
	crc: crc32fast::Hasher,
}

impl Write for Hashed {
	fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
		self.len += buf.len() as u64;
		self.crc.update(buf);
		Ok(buf.len())
	}

	fn flush(&mut self) -> io::Result<()> {
		Ok(())
	}
}

/// Decodes `bytes` under the heap meter: the decoded length and CRC-32, and the decoder's peak.
fn zstd_peak(bytes: &[u8]) -> ((u64, u32), u64) {
	let mut hashed = Hashed::default();
	let ((), peak) = peak_bytes(|| {
		let mut decoder = open_stream(StreamCodec::Zstd, bytes, 512 * MIB).unwrap();
		io::copy(&mut decoder, &mut hashed).unwrap();
	});
	((hashed.len, hashed.crc.finalize()), peak)
}

#[test]
fn a_zstd_decoder_stays_within_what_its_window_is_charged() {
	// what `max_window` holds a budget to: the ring at its peak (the power of two above a window
	// and a block, and the half-size one before it) and the decoder's state
	let charged = |window: u64| (window + 128 * 1024).next_power_of_two() * 3 / 2 + 2 * MIB;
	for (window_log, len) in [(23, 9 * MIB_USIZE), (17, 2 * MIB_USIZE)] {
		let data = sample(len);
		let (decoded, peak) = zstd_peak(&zstd_raw_frame(&data, window_log, true, None));
		assert_eq!(decoded, (len as u64, crc32fast::hash(&data)));
		assert!(
			peak <= charged(1 << window_log),
			"a {window_log}-bit window took {peak} bytes"
		);
	}
	// compressed blocks fill the literal, sequence and table buffers the state is charged for
	let data = sample(4 * MIB_USIZE);
	let (decoded, peak) = zstd_peak(&zstd(&data));
	assert_eq!(decoded, (data.len() as u64, crc32fast::hash(&data)));
	assert!(peak <= charged(128 * 1024), "took {peak} bytes");
	// the most sequences a block holds, 43690 matches of 3 bytes: the sequence buffer at its
	// largest, in a frame of the smallest window a full block fits
	let count: u32 = 128 * 1024 / 3;
	let mut block = vec![0, 255];
	block.extend_from_slice(&u16::try_from(count - 0x7F00).unwrap().to_le_bytes());
	block.extend_from_slice(&[0b0101_0100, 0, 0, 0, 1]);
	let ((decoded, _), peak) = zstd_peak(&zstd_crafted_frame(17, &block));
	assert_eq!(decoded, 8 + 3 * u64::from(count));
	assert!(peak <= charged(1 << 17), "took {peak} bytes");
}

/// A zstd frame of a 1 KiB window (`window_log` 10) or larger, without checksum or size: an
/// 8-byte raw block for matches to copy from, then `block`, a compressed block's content.
fn zstd_crafted_frame(window_log: u8, block: &[u8]) -> Vec<u8> {
	let mut frame = vec![0x28, 0xB5, 0x2F, 0xFD, 0, (window_log - 10) << 3];
	frame.extend_from_slice(&[8 << 3, 0, 0]);
	frame.extend_from_slice(b"12345678");
	// the last block, compressed (type 2), of the content's size
	let header = 1 | 2 << 1 | u32::try_from(block.len()).unwrap() << 3;
	frame.extend_from_slice(&header.to_le_bytes()[..3]);
	frame.extend_from_slice(block);
	frame
}

/// A compressed block's content of no literals and `count` sequences, all alike in RLE mode:
/// literal length 0, the second repeated offset, and a match length of `131_074 - spare`
/// (match length code 52, whose 16 extra bits are all ones but for `spare`).
fn zstd_sequences_block(count: u32, spare: u16) -> Vec<u8> {
	// raw literals of size 0
	let mut block = vec![0];
	match count {
		0..128 => block.push(u8::try_from(count).unwrap()),
		128..0x7F00 => {
			let [low, high] = u16::try_from(count).unwrap().to_le_bytes();
			block.extend_from_slice(&[high + 128, low]);
		}
		_ => {
			let [low, high] = u16::try_from(count - 0x7F00).unwrap().to_le_bytes();
			block.extend_from_slice(&[255, low, high]);
		}
	}
	// literal lengths, offsets and match lengths each in RLE mode, then their one symbol
	block.extend_from_slice(&[0b0101_0100, 0, 0, 52]);
	// each sequence reads its 16 extra bits, backwards from the end, after a marker bit
	let extra = u16::MAX - spare;
	let mut bits = Vec::with_capacity(count as usize * 2 + 1);
	for _ in 0..count {
		bits.extend_from_slice(&extra.to_le_bytes());
	}
	bits.push(1);
	block.extend(bits);
	block
}

#[test]
fn a_zstd_block_decoding_past_its_maximum_is_refused_before_it_is_written() {
	// 1000 sequences of 131074 bytes each in one block: 131 MB from a 2 KiB frame, where a block
	// may decode to 128 KiB at most
	let frame = zstd_crafted_frame(10, &zstd_sequences_block(1000, 0));
	assert_eq!(frame.len(), 2028);
	let (result, peak) = peak_bytes(|| decode(StreamCodec::Zstd, &frame, BUDGET));
	assert_eq!(corrupt(result), ZSTD_INVALID);
	assert!(peak < 4 * MIB, "took {peak} bytes");

	// past 32768 of them the lengths' sum overflowed a u32 and ruzstd panicked
	let frame = zstd_crafted_frame(10, &zstd_sequences_block(33_000, 0));
	let result =
		std::panic::catch_unwind(|| decode(StreamCodec::Zstd, &frame, BUDGET)).expect("no panic");
	assert_eq!(corrupt(result), ZSTD_INVALID);

	// more sequences than a block has room for, each copying the least a match can (3 bytes)
	let mut many = vec![0, 255, 0xAB, 0x2A, 0b0101_0100, 0, 0, 0, 1];
	many[2..4]
		.copy_from_slice(&(u16::try_from(128 * 1024 / 3 + 1 - 0x7F00).unwrap()).to_le_bytes());
	assert_eq!(
		corrupt(decode(
			StreamCodec::Zstd,
			&zstd_crafted_frame(10, &many),
			BUDGET
		)),
		ZSTD_INVALID
	);

	// literals stated at 1 MiB, the most their header holds: RLE, a 3-byte size of 2^20 - 1
	let size = (1u32 << 20) - 1;
	let rle = 1 | 0b11 << 2 | (size << 4);
	let mut block = rle.to_le_bytes()[..3].to_vec();
	block.extend_from_slice(&[b'x', 0]);
	let (result, peak) =
		peak_bytes(|| decode(StreamCodec::Zstd, &zstd_crafted_frame(10, &block), BUDGET));
	assert_eq!(corrupt(result), ZSTD_INVALID);
	assert!(peak < MIB, "took {peak} bytes");
}

#[test]
fn zstd_huffman_literals_stop_at_their_stated_size() {
	// 4 literal words stated, then 4 streams of ones under a 1-bit Huffman table: each bit is a
	// literal, nearly a million of them in a block
	const STREAM: usize = 29_998;
	let tree = [0x80, 0x10];
	let compressed = tree.len() + 6 + 4 * STREAM;
	// compressed literals (type 2), 18-bit sizes (format 3): regenerated, then compressed
	let header = 2 | 3 << 2 | 4u64 << 4 | (compressed as u64) << 22;
	let mut block = header.to_le_bytes()[..5].to_vec();
	block.extend_from_slice(&tree);
	for _ in 0..3 {
		block.extend_from_slice(&u16::try_from(STREAM).unwrap().to_le_bytes());
	}
	block.extend(std::iter::repeat_n(0xFF, 4 * STREAM));
	// no sequences
	block.push(0);
	let (result, peak) =
		peak_bytes(|| decode(StreamCodec::Zstd, &zstd_crafted_frame(10, &block), BUDGET));
	assert_eq!(corrupt(result), ZSTD_INVALID);
	assert!(peak < 512 * 1024, "took {peak} bytes");
}

#[test]
fn a_zstd_block_of_exactly_its_maximum_decodes() {
	// literals and matches of 128 KiB in all: 131074 - 2
	let frame = zstd_crafted_frame(17, &zstd_sequences_block(1, 2));
	let (decoded, _) = decode(StreamCodec::Zstd, &frame, BUDGET).unwrap();
	assert_eq!(decoded.len(), 8 + 128 * 1024);
	// the second repeated offset starts as 4: the raw block's last four bytes, over and over
	assert!(
		decoded[8..]
			.chunks(4)
			.all(|chunk| chunk == &b"5678"[..chunk.len()])
	);
}

#[test]
fn a_damaged_zstd_never_panics() {
	let data = sample(3_000);
	let mut bytes = vec![0x5F, 0x2A, 0x4D, 0x18, 2, 0, 0, 0, 7, 7];
	bytes.extend(zstd(&data));
	bytes.extend(zstd_raw_frame(&data[..100], 17, true, None));
	damage_never_panics(StreamCodec::Zstd, &bytes);
}

#[test]
fn input_errors_pass_through_unchanged() {
	/// Hands out a valid stream's first bytes, then fails.
	struct FailsAfter<'a>(&'a [u8]);

	impl Read for FailsAfter<'_> {
		fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
			if self.0.is_empty() {
				return Err(io::Error::new(io::ErrorKind::BrokenPipe, "channel closed"));
			}
			let n = self.0.len().min(buf.len());
			buf[..n].copy_from_slice(&self.0[..n]);
			self.0 = &self.0[n..];
			Ok(n)
		}
	}

	let data = sample(100_000);
	for (codec, bytes) in [
		(StreamCodec::Gzip, gzip(&data)),
		(StreamCodec::Bzip2, bzip2(&data)),
		(StreamCodec::Xz, xz(&data, |_| {})),
		(StreamCodec::Lz4, lz4(&data, FrameInfo::new())),
		(StreamCodec::Brotli, brotli(&data)),
		(StreamCodec::Zstd, zstd(&data)),
	] {
		let mut decoder =
			open_stream(codec, FailsAfter(&bytes[..bytes.len() / 2]), BUDGET).unwrap();
		let error = decoder.read_to_end(&mut Vec::new()).unwrap_err();
		assert_eq!(error.kind(), io::ErrorKind::BrokenPipe, "{codec:?}");
		assert_eq!(error.to_string(), "channel closed", "{codec:?}");
		assert!(codec_error(&error).is_none(), "{codec:?}");
	}
}

#[test]
fn a_budget_below_the_input_buffer_is_refused() {
	assert!(matches!(
		open_stream(StreamCodec::Gzip, &[][..], 1024).err(),
		Some(CodecError::OverBudget { limit: 1024 })
	));
}

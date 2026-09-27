//! Round trips through reference encoders, and the damage each decoder has to notice.

use std::{
	io::{self, Read, Write},
	num::NonZeroU64,
};

use lz4_flex::frame::{BlockMode, BlockSize, FrameEncoder, FrameInfo};
use lzma_rust2::{
	CheckType, FilterConfig, LzipOptions, LzipWriter, LzmaOptions, LzmaWriter, XzOptions, XzWriter,
};

use super::*;

const MIB: u64 = 1024 * 1024;
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

fn gzip(data: &[u8]) -> Vec<u8> {
	let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
	encoder.write_all(data).unwrap();
	encoder.finish().unwrap()
}

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

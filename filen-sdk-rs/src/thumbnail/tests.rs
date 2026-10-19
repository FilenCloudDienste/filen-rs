use std::{cell::Cell, time::Duration};

use futures::future::join_all;
use microthumb::MemSource;

use super::{
	DEFAULT_THUMBNAIL_MEM_BUDGET, REMOTE_SOURCE_RESIDENT_BYTES, RemoteChunkSource, ThumbSpec,
	ThumbnailFit, ThumbnailOutcome, WEBP_MAX_LOSSY_DIMENSION, make_thumbnail_from_source,
	remote_chunks::{ChunkAsk, chunk_range},
	write_webp,
};
use crate::{
	Error, ErrorKind,
	auth::http::{ClientConfig, SharedClientState},
	consts::{CHUNK_SIZE, CHUNK_SIZE_U64, FULL_CHUNK_BYTES},
	fs::{drive_job::test_support::full_chunk, file::chunk::Chunk},
};

fn png_bytes(width: u32, height: u32) -> Vec<u8> {
	use image::{ImageFormat, RgbImage};
	let image = RgbImage::from_fn(width, height, |x, y| {
		image::Rgb([(x & 0xFF) as u8, (y & 0xFF) as u8, 0])
	});
	let mut bytes = Vec::new();
	image
		.write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Png)
		.unwrap();
	bytes
}

/// A PNG of deterministic noise over a gradient — photo-like in the one way
/// an encoder cares about, nothing lossless can predict it away — with the
/// alpha `alpha(x, y)` gives each pixel.
fn noise_png_bytes(width: u32, height: u32, alpha: impl Fn(u32, u32) -> u8) -> Vec<u8> {
	// A small LCG: the same bytes on every run and every target.
	let mut state = 0x2545_f491_u32;
	let image = image::RgbaImage::from_fn(width, height, |x, y| {
		state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
		let noise = (state >> 26) as u8;
		image::Rgba([
			((x & 0xFF) as u8).wrapping_add(noise),
			((y & 0xFF) as u8).wrapping_add(noise),
			noise,
			alpha(x, y),
		])
	});
	let mut bytes = Vec::new();
	image
		.write_to(
			&mut std::io::Cursor::new(&mut bytes),
			image::ImageFormat::Png,
		)
		.unwrap();
	bytes
}

/// The first chunk's FourCC: `VP8L` lossless, `VP8 ` simple lossy, `VP8X`
/// extended (alpha, among others).
fn webp_fourcc(webp: &[u8]) -> &[u8] {
	assert_eq!((&webp[..4], &webp[8..12]), (&b"RIFF"[..], &b"WEBP"[..]));
	&webp[12..16]
}

/// The shape every caller uses: bytes in, webp out, plus a verdict.
fn thumbnail(
	bytes: Vec<u8>,
	target: u32,
	mem_budget: usize,
	fit: ThumbnailFit,
) -> (ThumbnailOutcome, Vec<u8>) {
	thumbnail_with(
		bytes,
		&ThumbSpec::new(target, target, mem_budget),
		fit,
		None,
	)
	.unwrap()
}

/// [`thumbnail`] with the spec and the encoding spelled out, and the error
/// kept.
fn thumbnail_with(
	bytes: Vec<u8>,
	spec: &ThumbSpec,
	fit: ThumbnailFit,
	lossy_quality: Option<u8>,
) -> Result<(ThumbnailOutcome, Vec<u8>), Error> {
	let mut out = Vec::new();
	let outcome = make_thumbnail_from_source(
		Box::new(MemSource(bytes)),
		spec,
		fit,
		lossy_quality,
		&mut out,
	)?;
	Ok((outcome, out))
}

#[test]
fn lossless_unless_a_lossy_quality_is_asked_for() {
	let (_, webp) = thumbnail(
		noise_png_bytes(64, 64, |_, _| u8::MAX),
		32,
		DEFAULT_THUMBNAIL_MEM_BUDGET,
		ThumbnailFit::Cover,
	);
	assert_eq!(webp_fourcc(&webp), b"VP8L");
}

#[test]
fn a_lossy_opaque_thumbnail_is_simple_lossy_and_smaller() {
	let spec = ThumbSpec::new(128, 128, DEFAULT_THUMBNAIL_MEM_BUDGET);
	// Cloned: the pipeline owns its source, and both encodings need one.
	let source = noise_png_bytes(256, 256, |_, _| u8::MAX);
	let (_, lossless) = thumbnail_with(source.clone(), &spec, ThumbnailFit::Contain, None).unwrap();
	let (outcome, lossy) = thumbnail_with(source, &spec, ThumbnailFit::Contain, Some(75)).unwrap();
	let ThumbnailOutcome::Thumbnail(info) = outcome else {
		panic!("expected a thumbnail, got {outcome:?}");
	};
	// `VP8 `, not `VP8X`: an opaque thumbnail carries no alpha plane.
	assert_eq!(webp_fourcc(&lossy), b"VP8 ");
	let decoded = image::load_from_memory(&lossy).unwrap();
	assert_eq!(
		(decoded.width(), decoded.height()),
		(info.width, info.height)
	);
	assert!(
		lossy.len() < lossless.len(),
		"lossy {} B is not smaller than lossless {} B",
		lossy.len(),
		lossless.len()
	);
}

#[test]
fn a_lossy_thumbnail_keeps_real_transparency_exactly() {
	let spec = ThumbSpec::new(64, 64, DEFAULT_THUMBNAIL_MEM_BUDGET);
	let source = noise_png_bytes(64, 64, |x, y| ((x * 4 + y) & 0xFF) as u8);
	let (_, lossless) = thumbnail_with(source.clone(), &spec, ThumbnailFit::Cover, None).unwrap();
	let (outcome, lossy) = thumbnail_with(source, &spec, ThumbnailFit::Cover, Some(75)).unwrap();
	assert!(
		matches!(outcome, ThumbnailOutcome::Thumbnail(_)),
		"got {outcome:?}"
	);
	assert_eq!(webp_fourcc(&lossy), b"VP8X");
	// The VP8X chunk is 18 bytes after the 12-byte RIFF header, and ALPH
	// must follow it directly when there is no ICC profile.
	assert_eq!(&lossy[30..34], b"ALPH");
	let alpha = |webp: &[u8]| -> Vec<u8> {
		image::load_from_memory(webp)
			.unwrap()
			.to_rgba8()
			.pixels()
			.map(|pixel| pixel[3])
			.collect()
	};
	let expected = alpha(&lossless);
	// Else this would be comparing two opaque images.
	assert!(expected.iter().any(|&a| a != u8::MAX));
	assert_eq!(alpha(&lossy), expected);
}

#[test]
fn a_zero_sided_cover_box_is_an_error_not_a_panic() {
	// A `Cover` fit into a box with a 0 side makes a thumbnail with a 0 side.
	// zenwebp panics on a 0 width and "encodes" a 0 height into a file no
	// decoder opens. Both encodings must refuse either.
	for (width, height) in [(0, 32), (32, 0)] {
		let spec = ThumbSpec::new(width, height, DEFAULT_THUMBNAIL_MEM_BUDGET);
		for lossy_quality in [None, Some(75)] {
			let result = thumbnail_with(
				noise_png_bytes(64, 64, |_, _| u8::MAX),
				&spec,
				ThumbnailFit::Cover,
				lossy_quality,
			);
			let Err(e) = result else {
				panic!("{width}x{height} {lossy_quality:?}: expected an error, got {result:?}");
			};
			assert_eq!(
				e.kind(),
				ErrorKind::ImageError,
				"{width}x{height} {lossy_quality:?}: {e}"
			);
		}
	}
}

#[test]
fn a_lossy_thumbnail_past_webps_dimension_limit_is_refused() {
	// zenwebp "encodes" 16384 into a header no decoder opens.
	let mut out = Vec::new();
	let too_wide = image::DynamicImage::new_rgb8(WEBP_MAX_LOSSY_DIMENSION + 1, 1);
	let e = write_webp(too_wide, Some(75), &mut out).unwrap_err();
	assert_eq!(e.kind(), ErrorKind::ImageError);
	assert!(out.is_empty());

	let too_tall = image::DynamicImage::new_rgb8(1, WEBP_MAX_LOSSY_DIMENSION + 1);
	let e = write_webp(too_tall, Some(75), &mut out).unwrap_err();
	assert_eq!(e.kind(), ErrorKind::ImageError);
	assert!(out.is_empty());

	let widest = image::DynamicImage::new_rgb8(WEBP_MAX_LOSSY_DIMENSION, 1);
	write_webp(widest, Some(75), &mut out).unwrap();
	let decoded = image::load_from_memory(&out).unwrap();
	assert_eq!(decoded.width(), WEBP_MAX_LOSSY_DIMENSION);
}

#[test]
fn a_lossy_quality_above_100_is_100() {
	let spec = ThumbSpec::new(32, 32, DEFAULT_THUMBNAIL_MEM_BUDGET);
	let source = noise_png_bytes(64, 64, |_, _| u8::MAX);
	let encode = |quality| {
		thumbnail_with(source.clone(), &spec, ThumbnailFit::Cover, Some(quality))
			.unwrap()
			.1
	};
	assert_eq!(encode(250), encode(100));
}

#[test]
fn refuses_sources_above_the_memory_budget() {
	// A budget too small even for PNG's row buffers: the pipeline must
	// refuse rather than decode past its ceiling — and say that the FORMAT
	// was fine, which is what makes the verdict retryable elsewhere.
	let (outcome, out) = thumbnail(png_bytes(100, 80), 32, 1024, ThumbnailFit::Cover);
	assert_eq!(outcome, ThumbnailOutcome::OverBudget);
	assert!(out.is_empty());
}

#[test]
fn makes_a_thumbnail_within_the_default_budget() {
	let (outcome, out) = thumbnail(
		png_bytes(100, 80),
		32,
		DEFAULT_THUMBNAIL_MEM_BUDGET,
		ThumbnailFit::Cover,
	);
	let ThumbnailOutcome::Thumbnail(info) = outcome else {
		panic!("expected a thumbnail, got {outcome:?}");
	};
	assert_eq!((info.width, info.height), (32, 32));
	assert!(!out.is_empty());
}

#[test]
fn cover_crops_to_the_box_and_contain_keeps_the_aspect() {
	// The one thing the two wrappers ask for differently: a grid tile wants
	// an exact square, an in-memory preview wants the picture's shape.
	let (cover, _) = thumbnail(
		png_bytes(400, 200),
		64,
		DEFAULT_THUMBNAIL_MEM_BUDGET,
		ThumbnailFit::Cover,
	);
	let ThumbnailOutcome::Thumbnail(cover) = cover else {
		panic!("expected a thumbnail");
	};
	assert_eq!((cover.width, cover.height), (64, 64));

	let (contain, _) = thumbnail(
		png_bytes(400, 200),
		64,
		DEFAULT_THUMBNAIL_MEM_BUDGET,
		ThumbnailFit::Contain,
	);
	let ThumbnailOutcome::Thumbnail(contain) = contain else {
		panic!("expected a thumbnail");
	};
	assert_eq!((contain.width, contain.height), (64, 32));
}

#[test]
fn never_upscales_small_sources() {
	let (outcome, _) = thumbnail(
		png_bytes(16, 16),
		64,
		DEFAULT_THUMBNAIL_MEM_BUDGET,
		ThumbnailFit::Cover,
	);
	let ThumbnailOutcome::Thumbnail(info) = outcome else {
		panic!("expected a thumbnail, got {outcome:?}");
	};
	assert_eq!((info.width, info.height), (16, 16));
}

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// A [`microthumb::ByteSource`] with [`super::RemoteChunkSource`]'s
/// observable behavior, minus the network: short reads at every chunk
/// boundary, and a cancel flag answered with an io error — optionally
/// flipped by the source itself after N reads, standing in for a batch
/// cancel landing mid-decode.
struct ChunkySource {
	data: Vec<u8>,
	chunk: usize,
	cancel: Arc<AtomicBool>,
	cancel_after_reads: Option<usize>,
	reads: AtomicUsize,
}

impl microthumb::ByteSource for ChunkySource {
	fn len(&self) -> u64 {
		self.data.len() as u64
	}

	fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
		let reads = self.reads.fetch_add(1, Ordering::Relaxed) + 1;
		if self
			.cancel_after_reads
			.is_some_and(|threshold| reads > threshold)
		{
			self.cancel.store(true, Ordering::Relaxed);
		}
		if self.cancel.load(Ordering::Relaxed) {
			return Err(std::io::Error::other("thumbnail cancelled"));
		}
		let Ok(offset) = usize::try_from(offset) else {
			return Ok(0);
		};
		if offset >= self.data.len() {
			return Ok(0);
		}
		let to_boundary = self.chunk - (offset % self.chunk);
		let n = buf.len().min(to_boundary).min(self.data.len() - offset);
		buf[..n].copy_from_slice(&self.data[offset..offset + n]);
		Ok(n)
	}
}

#[test]
fn the_bulk_hint_turns_every_later_miss_into_a_stream_ask() {
	use std::sync::Mutex;

	use microthumb::ByteSource;

	use crate::consts::CHUNK_SIZE_U64;

	let asks = Arc::new(Mutex::new(Vec::new()));
	let len = 5 * CHUNK_SIZE_U64 + 10;
	let mut source = RemoteChunkSource::with_fetcher(len, None, {
		let asks = asks.clone();
		Box::new(move |ask| {
			asks.lock().unwrap().push(ask);
			let (start, end) = chunk_range(ask.index, len);
			let fill = u8::try_from(ask.index).unwrap();
			Ok(vec![fill; usize::try_from(end - start).unwrap()])
		})
	});
	let mut buf = [0u8; 16];
	// The header phase: on-demand range asks, and a read up against the
	// chunk boundary stays inside the resident chunk.
	source.read_at(0, &mut buf).unwrap();
	source.read_at(CHUNK_SIZE_U64 - 8, &mut buf).unwrap();
	// The decode commits. A hit is still a hit; every miss is now a bulk
	// ask, in the order the decoder reads.
	source.hint_bulk_sequential();
	source.read_at(0, &mut buf).unwrap();
	source.read_at(CHUNK_SIZE_U64, &mut buf).unwrap();
	source.read_at(2 * CHUNK_SIZE_U64 + 100, &mut buf).unwrap();
	assert_eq!(buf[0], 2, "chunk bytes must come from the asked chunk");
	assert_eq!(
		*asks.lock().unwrap(),
		vec![
			ChunkAsk {
				index: 0,
				bulk: false
			},
			ChunkAsk {
				index: 1,
				bulk: true
			},
			ChunkAsk {
				index: 2,
				bulk: true
			},
		]
	);
}

/// Chunk `index` of a `len`-byte file, as a fake fetcher answers it: every byte the index.
fn chunk_of(index: u64, len: u64) -> Vec<u8> {
	let (start, end) = chunk_range(index, len);
	let fill = u8::try_from(index).expect("a test file has few chunks");
	vec![fill; usize::try_from(end - start).expect("a chunk fits in memory")]
}

#[test]
fn a_seeded_first_chunk_is_never_asked_for() {
	let len = 2 * CHUNK_SIZE_U64 + 10;
	let asks = Arc::new(std::sync::Mutex::new(Vec::new()));
	let mut source = RemoteChunkSource::with_fetcher(len, None, {
		let asks = Arc::clone(&asks);
		Box::new(move |ask: ChunkAsk| {
			asks.lock().expect("not poisoned").push(ask.index);
			Ok(chunk_of(ask.index, len))
		})
	})
	.with_first_chunk(chunk_of(0, len));
	let mut buf = [9u8; 16];

	// the header reads, then the decode committing and reading on from the top
	assert_eq!(source.read_at(0, &mut buf).unwrap(), 16);
	assert_eq!(buf, [0; 16]);
	source.hint_bulk_sequential();
	source.read_at(0, &mut buf).unwrap();
	source.read_at(CHUNK_SIZE_U64, &mut buf).unwrap();
	source.read_at(2 * CHUNK_SIZE_U64, &mut buf).unwrap();
	assert_eq!(buf[0], 2);
	assert_eq!(*asks.lock().expect("not poisoned"), vec![1, 2]);
}

#[test]
fn a_seeded_first_chunk_is_cut_to_the_files_declared_size() {
	// the file says 100 bytes, its first chunk decrypts to 300: nothing past 100 is read
	let mut source = RemoteChunkSource::with_fetcher(
		100,
		None,
		Box::new(|ask: ChunkAsk| -> std::io::Result<Vec<u8>> {
			panic!("a 100-byte file has no chunk {} to fetch", ask.index)
		}),
	)
	.with_first_chunk(vec![7; 300]);
	let mut buf = [0u8; 300];
	assert_eq!(source.read_at(0, &mut buf).unwrap(), 100);
	assert_eq!(source.read_at(100, &mut buf).unwrap(), 0);

	// a first chunk past a chunk's size: a read up to the boundary stops there, and the
	// bytes after it are chunk 1's, not the overlong chunk 0's
	let len = CHUNK_SIZE_U64 + 10;
	let mut source = RemoteChunkSource::with_fetcher(
		len,
		None,
		Box::new(move |ask: ChunkAsk| Ok(chunk_of(ask.index, len))),
	)
	.with_first_chunk(vec![0; CHUNK_SIZE + 50]);
	let mut buf = [9u8; 8];
	assert_eq!(source.read_at(CHUNK_SIZE_U64 - 2, &mut buf).unwrap(), 2);
	assert_eq!(source.read_at(CHUNK_SIZE_U64, &mut buf).unwrap(), 8);
	assert_eq!(buf, [1; 8]);
}

#[test]
fn short_reads_at_chunk_boundaries_still_thumbnail() {
	// A 1 KiB chunk size forces hundreds of boundary-shortened reads
	// through the same read_at contract RemoteChunkSource answers with.
	let source = ChunkySource {
		data: png_bytes(300, 200),
		chunk: 1024,
		cancel: Arc::new(AtomicBool::new(false)),
		cancel_after_reads: None,
		reads: AtomicUsize::new(0),
	};
	let mut out = Vec::new();
	let outcome = make_thumbnail_from_source(
		Box::new(source),
		&ThumbSpec::new(32, 32, DEFAULT_THUMBNAIL_MEM_BUDGET),
		ThumbnailFit::Cover,
		None,
		&mut out,
	)
	.unwrap();
	let ThumbnailOutcome::Thumbnail(info) = outcome else {
		panic!("expected a thumbnail, got {outcome:?}");
	};
	assert_eq!((info.width, info.height), (32, 32));
	assert!(!out.is_empty());
}

#[test]
fn a_cancel_mid_decode_is_a_hard_error_not_a_cached_verdict() {
	// The flag flips after the header reads succeed, the way a batch
	// cancel lands mid-decode. A cancel must surface as Err — never as a
	// settled verdict, which the platform would cache forever.
	let source = ChunkySource {
		data: png_bytes(300, 200),
		chunk: 1024,
		cancel: Arc::new(AtomicBool::new(false)),
		cancel_after_reads: Some(3),
		reads: AtomicUsize::new(0),
	};
	let mut out = Vec::new();
	let result = make_thumbnail_from_source(
		Box::new(source),
		&ThumbSpec::new(32, 32, DEFAULT_THUMBNAIL_MEM_BUDGET),
		ThumbnailFit::Cover,
		None,
		&mut out,
	);
	assert!(result.is_err(), "expected a hard error, got {result:?}");
}

/// The real remote source, minus the network: one fetch serves the whole
/// JPEG, and the cancel flag flips after the header reads. Reads are
/// counted so a decoder that keeps retrying the refusal fails here rather
/// than spinning the test: `jpeg-decoder` reads through std's
/// `read_exact`, which retries `ErrorKind::Interrupted` forever.
#[test]
fn a_cancelled_remote_source_stops_a_jpeg_decode() {
	struct CancelAfter {
		inner: RemoteChunkSource,
		cancel: Arc<AtomicBool>,
		after: usize,
		reads: usize,
	}
	impl ByteSource for CancelAfter {
		fn len(&self) -> u64 {
			self.inner.len()
		}
		fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
			self.reads += 1;
			assert!(
				self.reads <= self.after + 8,
				"the decoder kept reading after the cancel: {} reads",
				self.reads
			);
			if self.reads > self.after {
				self.cancel.store(true, Ordering::Relaxed);
			}
			self.inner.read_at(offset, buf)
		}
	}
	let data = jpeg_bytes(600, 400);
	let len = data.len() as u64;
	let cancel = Arc::new(AtomicBool::new(false));
	let inner = RemoteChunkSource::with_fetcher(
		len,
		Some(cancel.clone()),
		Box::new(move |ask| {
			let (start, end) = chunk_range(ask.index, len);
			let range = usize::try_from(start).unwrap()..usize::try_from(end).unwrap();
			Ok(data[range].to_vec())
		}),
	);
	let mut out = Vec::new();
	let result = make_thumbnail_from_source(
		Box::new(CancelAfter {
			inner,
			cancel,
			after: 3,
			reads: 0,
		}),
		&ThumbSpec::new(32, 32, DEFAULT_THUMBNAIL_MEM_BUDGET),
		ThumbnailFit::Cover,
		None,
		&mut out,
	);
	assert!(result.is_err(), "expected a hard error, got {result:?}");
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[test]
fn svg_rasterises_through_the_bounded_pipeline() {
	let bytes = br#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100"><rect width="100" height="100" fill="red"/></svg>"#.to_vec();
	let (outcome, out) = thumbnail(bytes, 32, DEFAULT_THUMBNAIL_MEM_BUDGET, ThumbnailFit::Cover);
	let ThumbnailOutcome::Thumbnail(info) = outcome else {
		panic!("expected an svg thumbnail, got {outcome:?}");
	};
	assert_eq!((info.width, info.height), (32, 32));
	assert!(!out.is_empty());
}

#[test]
fn corrupt_bytes_are_a_settled_verdict_not_an_error() {
	// Sniffable as JPEG, undecodable past the marker: a verdict the caller
	// may cache, not an error it retries forever — and distinguishable
	// from "we do not decode this format".
	let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE0];
	bytes.extend_from_slice(&[0x00; 64]);
	let (outcome, out) = thumbnail(bytes, 32, DEFAULT_THUMBNAIL_MEM_BUDGET, ThumbnailFit::Cover);
	assert!(
		matches!(outcome, ThumbnailOutcome::Corrupt(_)),
		"got {outcome:?}"
	);
	assert!(out.is_empty());
}

#[test]
fn a_truncated_image_is_a_verdict_too_even_when_its_decoder_calls_it_io() {
	// `png` reports running off the end as an io error, not a decode one.
	// That must not be mistaken for the network dropping: only the sources
	// can speak for the transport, and none of them produce UnexpectedEof.
	let mut bytes = png_bytes(300, 200);
	bytes.truncate(bytes.len() / 3);
	let (outcome, out) = thumbnail(bytes, 32, DEFAULT_THUMBNAIL_MEM_BUDGET, ThumbnailFit::Cover);
	assert!(
		matches!(outcome, ThumbnailOutcome::Corrupt(_)),
		"got {outcome:?}"
	);
	assert!(out.is_empty());
}

/// The point of the whole config: what a host configures is what the decode runs under.
///
/// A real `Client` needs auth material a unit test has no business minting, so this stops one
/// layer short at `SharedClientState` — which is where the config actually lands and what
/// `Client::thumbnails()` hands straight back.
#[test]
fn a_configured_budget_reaches_the_decode() {
	let bytes = png_bytes(300, 200);
	let decode_under = |config: ClientConfig| {
		let state = SharedClientState::new(config).expect("valid config");
		let mut out = Vec::new();
		let outcome = make_thumbnail_from_source(
			Box::new(MemSource(bytes.clone())),
			&state.thumbnails().spec_local(32, 32),
			ThumbnailFit::Cover,
			None,
			&mut out,
		)
		.unwrap();
		(outcome, out)
	};

	// Same image, same call path — only the configured budget differs.
	let (outcome, out) = decode_under(ClientConfig::default());
	let ThumbnailOutcome::Thumbnail(info) = outcome else {
		panic!("expected a thumbnail under the default budget, got {outcome:?}");
	};
	assert_eq!((info.width, info.height), (32, 32));
	assert!(!out.is_empty());

	let (outcome, out) = decode_under(ClientConfig::default().with_thumbnail_mem_budget(1024));
	assert_eq!(outcome, ThumbnailOutcome::OverBudget);
	assert!(out.is_empty());
}

/// The two corrections `spec_remote` absorbed from its (formerly duplicated) callers.
#[test]
fn spec_remote_pays_for_the_sources_resident_chunks_and_caps_the_stream() {
	let state = SharedClientState::new(ClientConfig::default()).expect("valid config");
	let thumbs = state.thumbnails();

	assert_eq!(
		thumbs.spec_remote(32, 32, 1).mem_budget,
		thumbs.mem_budget - REMOTE_SOURCE_RESIDENT_BYTES
	);
	assert!(
		thumbs
			.spec_remote(32, 32, thumbs.max_source_bytes)
			.allow_full_decode
	);
	assert!(
		!thumbs
			.spec_remote(32, 32, thumbs.max_source_bytes + 1)
			.allow_full_decode,
		"past the cap only the embedded-preview probe is worth the bytes"
	);

	// A budget under the subtraction saturates to 0 rather than wrapping to a huge one.
	let starved = SharedClientState::new(ClientConfig::default().with_thumbnail_mem_budget(1024))
		.expect("valid config");
	assert_eq!(starved.thumbnails().spec_remote(32, 32, 1).mem_budget, 0);
}

/// The same correction for a buffered source, whose residency is the whole file rather than
/// two chunk slots — without it one call costs the buffer PLUS the whole budget.
#[test]
fn spec_in_memory_pays_for_the_buffer_the_caller_is_holding() {
	let state = SharedClientState::new(ClientConfig::default()).expect("valid config");
	let thumbs = state.thumbnails();

	assert_eq!(
		thumbs.spec_in_memory(32, 32, 8 * 1024 * 1024).mem_budget,
		thumbs.mem_budget - 8 * 1024 * 1024
	);
	// A buffer at the source cap eats the whole budget rather than wrapping to a huge one.
	assert_eq!(
		thumbs
			.spec_in_memory(32, 32, thumbs.mem_budget + 1)
			.mem_budget,
		0
	);
}

/// The permit is `acquire_owned`ed so it can be MOVED INTO the detached blocking closure and
/// released when the decode really ends. This pins the other half of that contract: an
/// ordinary drop does give the slot back, so a cancelled caller cannot leak the gate shut.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn a_dropped_decode_permit_gives_its_slot_back() {
	let state =
		SharedClientState::new(ClientConfig::default().with_thumbnail_decode_concurrency(1))
			.expect("valid config");
	let thumbs = state.thumbnails();

	let held = thumbs.decode_permit().await;
	// Paused time auto-advances once every task is idle, so a gate that never opens elapses
	// this deterministically instead of sleeping.
	assert!(
		tokio::time::timeout(Duration::from_secs(30), thumbs.decode_permit())
			.await
			.is_err(),
		"the only slot is held; a second decode must wait"
	);

	drop(held);
	let _reacquired = tokio::time::timeout(Duration::from_secs(30), thumbs.decode_permit())
		.await
		.expect("dropping a decode permit must return it to the gate");
}

/// How long a test waits for what a working gate gives at once. Paused time runs it out the
/// moment nothing else can move, so a wait that never ends fails at once instead of hanging.
const PATIENCE: Duration = Duration::from_secs(30);

/// One chunk downloaded the way a driver or a decode downloads it: its memory taken from the
/// file-io budget while it arrives, given back once its bytes are in hand.
async fn fetch_chunk(state: &SharedClientState) -> Vec<u8> {
	let chunk = Chunk::acquire(full_chunk(), state).await;
	// arrives on a later poll, as a download does
	tokio::task::yield_now().await;
	chunk.into_parts().0
}

#[tokio::test(start_paused = true)]
async fn a_driver_with_a_free_slot_fetches_nothing_ahead() {
	let state =
		SharedClientState::new(ClientConfig::default().with_thumbnail_decode_concurrency(1))
			.expect("valid config");
	let polled = Cell::new(false);

	let (_permit, prefetched) = state
		.thumbnails()
		.decode_permit_prefetching(async { polled.set(true) })
		.await;
	assert_eq!(prefetched, None);
	assert!(
		!polled.get(),
		"a decode that can start now fetches for itself"
	);
}

#[tokio::test(start_paused = true)]
async fn a_waiting_driver_fetches_ahead_and_starts_once_a_slot_frees() {
	let state =
		SharedClientState::new(ClientConfig::default().with_thumbnail_decode_concurrency(1))
			.expect("valid config");
	let thumbs = state.thumbnails();
	let prefetched = Cell::new(false);

	let decoding = thumbs.decode_permit().await;
	let waiting = thumbs.decode_permit_prefetching(async {
		prefetched.set(true);
		7
	});
	tokio::pin!(waiting);
	assert!(
		tokio::time::timeout(PATIENCE, &mut waiting).await.is_err(),
		"no slot until the decode ahead ends"
	);
	assert!(prefetched.get(), "the wait was spent fetching");

	drop(decoding);
	let (_permit, first) = tokio::time::timeout(PATIENCE, waiting)
		.await
		.expect("the freed slot is the waiting driver's");
	assert_eq!(first, Some(7));
}

#[tokio::test(start_paused = true)]
async fn a_dropped_wait_gives_back_its_place_and_the_memory_its_fetch_held() {
	let budget = 2 * FULL_CHUNK_BYTES;
	let state = SharedClientState::new(
		ClientConfig::default()
			.with_thumbnail_decode_concurrency(1)
			.with_memory_budget(budget),
	)
	.expect("valid config");
	let thumbs = state.thumbnails();
	let reserved = Cell::new(false);

	let decoding = thumbs.decode_permit().await;
	// a fetch still downloading, its chunk's memory taken, when its caller goes away
	let abandoned = thumbs.decode_permit_prefetching(async {
		let _chunk = Chunk::acquire(full_chunk(), &state).await;
		reserved.set(true);
		std::future::pending::<()>().await;
	});
	assert!(
		tokio::time::timeout(PATIENCE, abandoned).await.is_err(),
		"no slot until the decode ahead ends"
	);
	assert!(reserved.get());
	assert_eq!(state.memory_semaphore().available_permits(), budget);

	// the place to fetch ahead is the next waiting driver's
	let next_fetched = Cell::new(false);
	let next = thumbs.decode_permit_prefetching(async { next_fetched.set(true) });
	tokio::pin!(next);
	assert!(tokio::time::timeout(PATIENCE, &mut next).await.is_err());
	assert!(next_fetched.get());
	drop(decoding);
	let (_permit, first) = tokio::time::timeout(PATIENCE, next)
		.await
		.expect("the abandoned wait left the slot to the next driver");
	assert_eq!(first, Some(()));
}

/// Decodes waiting for the gate, the decodes they wait for and transfers all draw on one
/// file-io budget of two chunks. A waiting driver holds no budget while it waits, only its
/// fetched chunk's bytes, so the decodes ahead always get the memory they need to end; and
/// no more drivers hold a chunk fetched ahead than the gate admits decodes.
#[tokio::test(start_paused = true)]
async fn drivers_fetching_ahead_never_starve_the_decodes_they_wait_for() {
	const GATE: usize = 2;
	let state = SharedClientState::new(
		ClientConfig::default()
			.with_thumbnail_decode_concurrency(GATE)
			.with_memory_budget(2 * FULL_CHUNK_BYTES),
	)
	.expect("valid config");
	let (state, thumbs) = (&state, state.thumbnails());
	// drivers holding a chunk fetched ahead, from the fetch starting to their decode starting
	let (holding, most_holding) = (&Cell::new(0usize), &Cell::new(0usize));

	let decodes = (0..8).map(|_| async move {
		let (_permit, first) = thumbs
			.decode_permit_prefetching(async {
				holding.set(holding.get() + 1);
				most_holding.set(most_holding.get().max(holding.get()));
				fetch_chunk(state).await
			})
			.await;
		if first.is_some() {
			holding.set(holding.get() - 1);
		}
		// the rest of the file, under the permit
		for _ in 0..3 {
			fetch_chunk(state).await;
		}
	});
	let transfers = (0..3).map(|_| async move {
		for _ in 0..4 {
			// waits for one chunk's memory, and reads ahead into a second only if it is free
			let chunk = Chunk::acquire(full_chunk(), state).await;
			let read_ahead = Chunk::try_acquire(full_chunk(), state);
			tokio::task::yield_now().await;
			drop((chunk, read_ahead));
		}
	});
	tokio::time::timeout(
		PATIENCE,
		futures::future::join(join_all(decodes), join_all(transfers)),
	)
	.await
	.expect("every decode and transfer ends");
	assert_eq!(most_holding.get(), GATE);
}

/// On the web the gate's width is also how many of the page's decode workers a client's jobs
/// spread over, so it is the clamped width the gate really has, never a 0 that would leave
/// a job no worker.
#[test]
fn the_decode_concurrency_is_the_gates_clamped_width() {
	let width = |concurrency| {
		SharedClientState::new(
			ClientConfig::default().with_thumbnail_decode_concurrency(concurrency),
		)
		.expect("valid config")
		.thumbnails()
		.decode_concurrency()
	};
	assert_eq!(width(0), 1);
	assert_eq!(width(3), 3);
}

#[test]
fn bytes_that_are_not_an_image_are_unsupported() {
	let (outcome, out) = thumbnail(
		b"this is not an image at all, not even close".to_vec(),
		32,
		DEFAULT_THUMBNAIL_MEM_BUDGET,
		ThumbnailFit::Cover,
	);
	assert_eq!(outcome, ThumbnailOutcome::Unsupported);
	assert!(out.is_empty());
}

// -----------------------------------------------------------------------
// The embedded preview, written out as it is.
// -----------------------------------------------------------------------

use super::{
	EmbeddedPreview, MAX_PREVIEW_BYTES, ORIENTATION_APP1_LEN, locate_embedded_preview,
	write_embedded_preview,
};
use microthumb::ByteSource;

fn jpeg_bytes(width: u32, height: u32) -> Vec<u8> {
	use image::{ImageFormat, RgbImage};
	let image = RgbImage::from_fn(width, height, |x, y| {
		image::Rgb([(x & 0xFF) as u8, (y & 0xFF) as u8, 0])
	});
	let mut bytes = Vec::new();
	image
		.write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Jpeg)
		.unwrap();
	bytes
}

/// Where a SubIFD JPEG sits in [`raw_with`]: past the two directories.
const PREVIEW_AT: u16 = 200;

/// A RAW-shaped container: a little-endian TIFF whose IFD0 carries
/// `orientation` and whose SubIFD points at a JPEG of `declared` bytes at
/// [`PREVIEW_AT`] — the shape every TIFF-family camera uses, and the one
/// microthumb's own tests prove it walks.
fn raw_with(jpeg: &[u8], orientation: u32, declared: u64) -> Vec<u8> {
	fn entry(tag: u16, typ: u16, count: u32, value: u32) -> Vec<u8> {
		let mut out = tag.to_le_bytes().to_vec();
		out.extend_from_slice(&typ.to_le_bytes());
		out.extend_from_slice(&count.to_le_bytes());
		out.extend_from_slice(&value.to_le_bytes());
		out
	}
	let mut file = b"II\x2a\x00\x08\x00\x00\x00".to_vec();
	file.extend_from_slice(&2u16.to_le_bytes());
	file.extend_from_slice(&entry(0x0112, 3, 1, orientation));
	file.extend_from_slice(&entry(0x014A, 4, 1, 60));
	file.extend_from_slice(&0u32.to_le_bytes());
	file.resize(60, 0);
	file.extend_from_slice(&2u16.to_le_bytes());
	file.extend_from_slice(&entry(0x0201, 4, 1, u32::from(PREVIEW_AT)));
	file.extend_from_slice(&entry(0x0202, 4, 1, u32::try_from(declared).unwrap()));
	file.extend_from_slice(&0u32.to_le_bytes());
	file.resize(usize::from(PREVIEW_AT), 0);
	file.extend_from_slice(jpeg);
	file
}

fn write(container: Vec<u8>) -> (EmbeddedPreview, Vec<u8>) {
	let mut src = MemSource(container);
	let preview = locate_embedded_preview(&mut src)
		.unwrap()
		.expect("a preview");
	let mut out = Vec::new();
	let written = write_embedded_preview(&mut src, &preview, &mut out).unwrap();
	assert_eq!(written, out.len() as u64);
	assert_eq!(written, preview.written_len());
	(preview, out)
}

/// The one test that fails if anything starts transcoding: what comes out
/// is byte for byte what the camera stored.
#[test]
fn the_written_preview_is_the_stored_jpeg_and_nothing_else() {
	let jpeg = jpeg_bytes(640, 480);
	let (preview, out) = write(raw_with(&jpeg, 1, jpeg.len() as u64));
	assert_eq!(out, jpeg);
	assert_eq!(preview.app1, None);
	assert_eq!((preview.located.width, preview.located.height), (640, 480));
}

/// A rotation only the container knows is spliced in as the smallest
/// legal EXIF segment, behind the JFIF APP0 the encoder wrote, and a
/// decoder reads it back as the rotation it is. Everything either side of
/// those 36 bytes is untouched.
#[test]
fn a_container_rotation_is_spliced_in_as_exif() {
	use image::{ImageDecoder, metadata::Orientation};

	let jpeg = jpeg_bytes(640, 480);
	let (preview, out) = write(raw_with(&jpeg, 6, jpeg.len() as u64));
	let at = usize::from(preview.located.exif_insert_at.expect("a splice point"));
	// SOI, then the 16-byte JFIF APP0 with its marker and length.
	assert_eq!(at, 2 + 2 + 16);
	let app1 = preview.app1.expect("the container knows a rotation");
	assert_eq!(out.len(), jpeg.len() + ORIENTATION_APP1_LEN);
	assert_eq!(&out[..at], &jpeg[..at]);
	assert_eq!(&out[at..at + ORIENTATION_APP1_LEN], &app1);
	assert_eq!(&out[at + ORIENTATION_APP1_LEN..], &jpeg[at..]);

	let mut decoder = image::ImageReader::new(std::io::Cursor::new(&out))
		.with_guessed_format()
		.unwrap()
		.into_decoder()
		.unwrap();
	assert_eq!(decoder.dimensions(), (640, 480));
	assert_eq!(decoder.orientation().unwrap(), Orientation::Rotate90);
}

/// A stream that carries an EXIF of its own — Fuji, Panasonic — is left
/// alone whatever the container says, and the orientation reported is the
/// one that EXIF declares, because that is what a viewer will apply.
#[test]
fn a_stream_with_its_own_exif_is_left_alone_and_speaks_for_itself() {
	let plain = jpeg_bytes(640, 480);
	let mut jpeg = vec![0xFF, 0xD8];
	jpeg.extend_from_slice(&super::orientation_app1(6));
	jpeg.extend_from_slice(&plain[2..]);
	// The container claims 8; the stream says 6 and the stream wins.
	let (preview, out) = write(raw_with(&jpeg, 8, jpeg.len() as u64));
	assert_eq!(out, jpeg);
	assert_eq!(preview.app1, None);
	assert_eq!(preview.located.stream_orientation, Some(6));
	assert_eq!(preview.viewer_orientation(), 6);
	// Neither side declares anything: upright, and nothing spliced.
	let (preview, out) = write(raw_with(&plain, 1, plain.len() as u64));
	assert_eq!(out, plain);
	assert_eq!(preview.viewer_orientation(), 1);
	// Only the container knows: spliced, and reported.
	let (preview, _) = write(raw_with(&plain, 3, plain.len() as u64));
	assert!(preview.app1.is_some());
	assert_eq!(preview.viewer_orientation(), 3);
}

/// A Fujifilm HIF's camera JPEG is its preview. A portrait shot's is bare
/// and stored sideways, so the HIF's quarter turn is spliced in; every
/// other byte is the item's own.
#[cfg(feature = "heif-decoder")]
#[test]
fn a_portrait_fujifilm_hifs_camera_jpeg_goes_out_turned_upright() {
	use image::{ImageDecoder, metadata::Orientation};

	let hif = include_bytes!("../../../microthumb/tests/fixtures/heif/fuji-irot1.heic");
	let (preview, out) = write(hif.to_vec());
	let start = usize::try_from(preview.located.offset).unwrap();
	let jpeg = &hif[start..start + usize::try_from(preview.located.len).unwrap()];
	let at = usize::from(preview.located.exif_insert_at.expect("a splice point"));
	assert_eq!(at, 2, "nothing ahead of a bare JPEG's tables");
	assert_eq!(out.len(), jpeg.len() + ORIENTATION_APP1_LEN);
	assert_eq!(&out[..at], &jpeg[..at]);
	assert_eq!(&out[at + ORIENTATION_APP1_LEN..], &jpeg[at..]);

	let mut decoder = image::ImageReader::new(std::io::Cursor::new(&out))
		.with_guessed_format()
		.unwrap()
		.into_decoder()
		.unwrap();
	assert_eq!(decoder.dimensions(), (768, 512));
	assert_eq!(decoder.orientation().unwrap(), Orientation::Rotate270);
}

/// A source that hands out a fixed prefix and claims to be much longer:
/// every read past the prefix answers zeros, and the furthest offset asked
/// for is recorded.
struct Sparse {
	prefix: Vec<u8>,
	len: u64,
	furthest: Arc<AtomicUsize>,
}

impl ByteSource for Sparse {
	fn len(&self) -> u64 {
		self.len
	}

	fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
		if offset >= self.len {
			return Ok(0);
		}
		let rest = usize::try_from(self.len - offset).unwrap();
		let offset = usize::try_from(offset).unwrap();
		self.furthest
			.fetch_max(offset + buf.len(), Ordering::Relaxed);
		let n = rest.min(buf.len());
		for (i, b) in buf[..n].iter_mut().enumerate() {
			*b = self.prefix.get(offset + i).copied().unwrap_or(0);
		}
		Ok(n)
	}
}

/// `jpeg` with its SOF0 rewritten to claim `w`x`h`; nothing here decodes
/// it, and microthumb's own per-pixel cap scales with what the SOF says.
fn with_sof_dims(mut jpeg: Vec<u8>, w: u16, h: u16) -> Vec<u8> {
	let at = jpeg
		.windows(2)
		.position(|m| m == [0xFF, 0xC0])
		.expect("a baseline SOF");
	jpeg[at + 5..at + 7].copy_from_slice(&h.to_be_bytes());
	jpeg[at + 7..at + 9].copy_from_slice(&w.to_be_bytes());
	jpeg
}

/// A forged length is refused on the container's word, before a single
/// payload byte is read — what stops a hostile file turning one preview
/// into a 60 MB stream. The SOF claims 48 Mpx so that microthumb's own
/// per-pixel cap (4 B/px) clears and this ceiling is the one deciding;
/// both sides of it are pinned.
#[test]
fn a_forged_length_is_refused_before_any_payload_is_read() {
	let jpeg = with_sof_dims(jpeg_bytes(640, 480), 8000, 6000);
	let sparse = |declared: u64| {
		let furthest = Arc::new(AtomicUsize::new(0));
		let src = Sparse {
			prefix: raw_with(&jpeg, 1, declared),
			len: declared + u64::from(PREVIEW_AT) + 1024,
			furthest: furthest.clone(),
		};
		(src, furthest)
	};
	let (mut src, furthest) = sparse(MAX_PREVIEW_BYTES + 1);
	assert_eq!(locate_embedded_preview(&mut src).unwrap(), None);
	assert!(
		furthest.load(Ordering::Relaxed) < 64 * 1024,
		"read {} bytes into a preview it was going to refuse",
		furthest.load(Ordering::Relaxed)
	);
	// One byte under the ceiling is served — and locating it reads no
	// payload either.
	let (mut src, furthest) = sparse(MAX_PREVIEW_BYTES);
	assert!(locate_embedded_preview(&mut src).unwrap().is_some());
	assert!(furthest.load(Ordering::Relaxed) < 64 * 1024);
}

/// A source that runs out inside the range it declared is an error: a
/// short file at the consumer's path would be cached as the whole preview.
#[test]
fn a_source_that_runs_short_is_an_error_not_a_short_file() {
	let jpeg = jpeg_bytes(640, 480);
	let declared = jpeg.len() as u64 + 4096;
	let container = raw_with(&jpeg, 1, declared);
	let real_len = container.len() as u64;
	struct Truncated(MemSource, u64);
	impl ByteSource for Truncated {
		fn len(&self) -> u64 {
			self.1
		}
		fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
			self.0.read_at(offset, buf)
		}
	}
	let mut src = Truncated(MemSource(container), real_len + 4096);
	let preview = locate_embedded_preview(&mut src)
		.unwrap()
		.expect("the declared range fits the claimed length");
	let mut out = Vec::new();
	assert!(write_embedded_preview(&mut src, &preview, &mut out).is_err());
}

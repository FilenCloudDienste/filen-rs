//! End-to-end pipeline tests over fixtures generated at runtime, so no binary
//! fixture files land here. The one exception is AVIF: nothing in the tree can
//! encode AV1, so that case reads the browser suite's committed `parrot.avif`
//! and says so at the test.

use std::{
	io::Cursor,
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	},
};

use image::{ImageFormat, Rgb, RgbImage};
use microthumb::{
	APP_PROCESS_MEM_BUDGET, ByteSource, DEFAULT_MEM_BUDGET, FileSource, MemSource, ThumbError,
	ThumbOutcome, ThumbSpec, generate,
};

/// Most tests care only about the pixels. The ones that care about WHICH path
/// produced them call `generate` directly and inspect `Thumbnail::source`.
fn thumb(
	src: Box<dyn microthumb::ByteSource>,
	spec: &ThumbSpec,
) -> Result<Option<microthumb::SmallImage>, microthumb::ThumbError> {
	Ok(generate(src, spec)?.thumbnail().map(|t| t.image))
}

fn spec(target: u32) -> ThumbSpec {
	ThumbSpec::new(target, target, DEFAULT_MEM_BUDGET)
}

fn encode(image: &RgbImage, format: ImageFormat) -> Vec<u8> {
	let mut bytes = Vec::new();
	image
		.write_to(&mut Cursor::new(&mut bytes), format)
		.unwrap();
	bytes
}

fn checkerboard(w: u32, h: u32) -> RgbImage {
	RgbImage::from_fn(w, h, |x, y| {
		if (x / 2 + y / 2) % 2 == 0 {
			Rgb([0, 0, 0])
		} else {
			Rgb([255, 255, 255])
		}
	})
}

fn mean_channel(rgba: &[u8], channel: usize) -> u32 {
	let mut sum = 0u64;
	let mut n = 0u64;
	for px in rgba.chunks_exact(4) {
		sum += u64::from(px[channel]);
		n += 1;
	}
	(sum / n.max(1)) as u32
}

#[test]
fn jpeg_baseline_downscales_a_checkerboard_to_grey() {
	let bytes = encode(&checkerboard(512, 512), ImageFormat::Jpeg);
	let result = thumb(Box::new(MemSource(bytes)), &spec(64))
		.unwrap()
		.expect("baseline jpeg must thumbnail");
	assert!(result.width >= 64 && result.width <= 512);
	assert_eq!(result.width, result.height);
	let grey = mean_channel(&result.rgba, 0);
	assert!(
		(107..=147).contains(&grey),
		"checkerboard should average to ~127, got {grey}"
	);
}

#[test]
fn png_rows_stream_and_preserve_aspect() {
	let image = RgbImage::from_fn(1000, 500, |x, _| Rgb([(x % 256) as u8, 0, 0]));
	let bytes = encode(&image, ImageFormat::Png);
	let result = thumb(Box::new(MemSource(bytes)), &spec(100))
		.unwrap()
		.expect("png must thumbnail");
	// 2:1 aspect survives the canvas.
	let ratio = f64::from(result.width) / f64::from(result.height);
	assert!(
		(1.8..=2.2).contains(&ratio),
		"aspect drifted: {}x{}",
		result.width,
		result.height
	);
}

#[test]
fn small_sources_are_never_upscaled() {
	let bytes = encode(&checkerboard(16, 16), ImageFormat::Png);
	let result = thumb(Box::new(MemSource(bytes)), &spec(64))
		.unwrap()
		.expect("small png must thumbnail");
	assert_eq!((result.width, result.height), (16, 16));
}

#[test]
fn webp_over_budget_is_refused_but_streaming_png_is_not() {
	// Same pixel count, tiny budget: the full-frame format is refused, the
	// row-streaming one sails through — the whole point of per-format peaks.
	let image = checkerboard(600, 600);
	let tiny = ThumbSpec {
		target_width: 64,
		target_height: 64,
		mem_budget: 1024 * 1024,
		allow_full_decode: true,
	};
	let webp = encode(&image, ImageFormat::WebP);
	assert_eq!(thumb(Box::new(MemSource(webp)), &tiny).unwrap(), None);
	let png = encode(&image, ImageFormat::Png);
	assert!(thumb(Box::new(MemSource(png)), &tiny).unwrap().is_some());
}

#[test]
fn unsupported_bytes_are_ok_none_not_an_error() {
	let bytes = b"this is not an image at all, not even close".to_vec();
	assert_eq!(
		thumb(Box::new(MemSource(bytes.clone())), &spec(64)).unwrap(),
		None
	);
	// And it is the FINAL kind of "no": no budget will ever make these bytes
	// decode, which is what separates this from `OverBudget`.
	assert_eq!(
		generate(Box::new(MemSource(bytes)), &spec(64)).unwrap(),
		ThumbOutcome::Unsupported
	);
}

#[test]
fn truncated_jpeg_is_an_error_not_a_thumbnail() {
	let mut bytes = encode(&checkerboard(512, 512), ImageFormat::Jpeg);
	bytes.truncate(bytes.len() / 3);
	assert!(thumb(Box::new(MemSource(bytes)), &spec(64)).is_err());
}

/// A [`FileSource`] whose cancel flag flips after `after` reads, the way a
/// batch cancel lands mid-decode. Every read is counted so a decoder that
/// keeps retrying the refusal fails here instead of spinning the test.
struct CancelAfter {
	inner: FileSource,
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

#[test]
fn a_cancelled_file_source_stops_every_decoder() {
	// std's `read_exact` retries `ErrorKind::Interrupted` forever, and every
	// decoder but png and gif reads through it. A cancel answered with that
	// kind spun the decode at 100% CPU, holding its decode permit, instead of
	// dying at the next read.
	let dir = std::env::temp_dir();
	for (format, ext) in [
		(ImageFormat::Jpeg, "jpg"),
		(ImageFormat::WebP, "webp"),
		(ImageFormat::Png, "png"),
	] {
		let path = dir.join(format!("microthumb-cancel-{}.{ext}", std::process::id()));
		std::fs::write(&path, encode(&checkerboard(512, 512), format)).unwrap();
		let cancel = Arc::new(AtomicBool::new(false));
		let file = std::fs::File::open(&path).unwrap();
		let source = CancelAfter {
			inner: FileSource::with_cancel(file, Some(cancel.clone())).unwrap(),
			cancel,
			after: 3,
			reads: 0,
		};
		let result = generate(Box::new(source), &spec(64));
		let _ = std::fs::remove_file(&path);
		assert!(
			matches!(result, Err(ThumbError::Io(_))),
			"{ext}: expected a hard io error, got {result:?}"
		);
	}
}

/// SOI + SOF2 (progressive) header for an image of the given size — enough
/// for `read_info`, no entropy data. Assembled by hand because no pure-Rust
/// encoder writes progressive JPEG.
fn progressive_jpeg_header(width: u16, height: u16) -> Vec<u8> {
	let mut bytes = vec![0xFF, 0xD8]; // SOI
	bytes.extend_from_slice(&[0xFF, 0xDB, 0x00, 0x43, 0x00]); // DQT, table 0
	bytes.extend_from_slice(&[16u8; 64]);
	bytes.extend_from_slice(&[0xFF, 0xC2, 0x00, 0x0B, 0x08]); // SOF2, 1 component
	bytes.extend_from_slice(&height.to_be_bytes());
	bytes.extend_from_slice(&width.to_be_bytes());
	bytes.extend_from_slice(&[0x01, 0x01, 0x11, 0x00]);
	bytes
}

#[test]
fn a_large_progressive_jpeg_routes_to_the_dc_parser() {
	// 8000×6000 progressive: the full decode's coefficient buffer (~3 B/px =
	// 144 MB) dwarfs the budget, so the DC-scan path takes over. This header
	// has no scan data, so the observable is the DC parser's own truncation
	// error — proof of the routing (the old behavior was a silent None).
	let bytes = progressive_jpeg_header(8000, 6000);
	let err = thumb(Box::new(MemSource(bytes)), &spec(512)).unwrap_err();
	assert!(err.to_string().contains("dc-scan"), "got: {err}");
}

#[test]
fn a_small_progressive_jpeg_is_at_least_attempted() {
	// Same header at 400×300 fits the budget, so the pipeline proceeds into
	// the decode — which then fails on the truncated entropy data. The error
	// (vs None above) is what distinguishes "attempted" from "refused".
	let bytes = progressive_jpeg_header(400, 300);
	assert!(thumb(Box::new(MemSource(bytes)), &spec(512)).is_err());
}

/// A JPEG whose EXIF APP1 carries orientation 6 and an embedded thumbnail of
/// a *green* image, while the main image is red — so tests can tell exactly
/// which path produced the pixels.
fn jpeg_with_exif_thumbnail(orientation_: u16) -> Vec<u8> {
	jpeg_with_exif_thumbnail_sized(orientation_, 300, 200)
}

/// As above, with a main image big enough that reading it is measurable — the
/// preview-only path must not touch it.
fn jpeg_with_exif_thumbnail_sized(orientation_: u16, main_w: u32, main_h: u32) -> Vec<u8> {
	let thumb = encode(
		&RgbImage::from_pixel(160, 120, Rgb([0, 255, 0])),
		ImageFormat::Jpeg,
	);
	// TIFF payload: IFD0 {orientation, next → IFD1}, IFD1 {thumb offset/len}.
	let mut tiff = Vec::new();
	tiff.extend_from_slice(b"II");
	tiff.extend_from_slice(&42u16.to_le_bytes());
	tiff.extend_from_slice(&8u32.to_le_bytes());
	tiff.extend_from_slice(&1u16.to_le_bytes());
	tiff.extend_from_slice(&0x0112u16.to_le_bytes());
	tiff.extend_from_slice(&3u16.to_le_bytes());
	tiff.extend_from_slice(&1u32.to_le_bytes());
	tiff.extend_from_slice(&u32::from(orientation_).to_le_bytes());
	tiff.extend_from_slice(&26u32.to_le_bytes());
	tiff.extend_from_slice(&2u16.to_le_bytes());
	tiff.extend_from_slice(&0x0201u16.to_le_bytes());
	tiff.extend_from_slice(&4u16.to_le_bytes());
	tiff.extend_from_slice(&1u32.to_le_bytes());
	tiff.extend_from_slice(&56u32.to_le_bytes());
	tiff.extend_from_slice(&0x0202u16.to_le_bytes());
	tiff.extend_from_slice(&4u16.to_le_bytes());
	tiff.extend_from_slice(&1u32.to_le_bytes());
	tiff.extend_from_slice(&(thumb.len() as u32).to_le_bytes());
	tiff.extend_from_slice(&0u32.to_le_bytes());
	assert_eq!(tiff.len(), 56);
	tiff.extend_from_slice(&thumb);

	let main = encode(
		&RgbImage::from_fn(main_w, main_h, |x, y| {
			Rgb([255, (x % 256) as u8, (y % 256) as u8])
		}),
		ImageFormat::Jpeg,
	);
	let mut exif_payload = b"Exif\0\0".to_vec();
	exif_payload.extend_from_slice(&tiff);
	let app1_len = (exif_payload.len() + 2) as u16;
	let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE1];
	bytes.extend_from_slice(&app1_len.to_be_bytes());
	bytes.extend_from_slice(&exif_payload);
	bytes.extend_from_slice(&main[2..]); // main image sans its SOI
	bytes
}

#[test]
fn the_exif_thumbnail_serves_small_requests_without_a_full_decode() {
	let bytes = jpeg_with_exif_thumbnail(1);
	let result = thumb(Box::new(MemSource(bytes)), &spec(64))
		.unwrap()
		.expect("the embedded thumbnail must serve this");
	// Green means the preview path; red would mean the main image decoded.
	assert!(
		mean_channel(&result.rgba, 1) > 200,
		"expected the green thumb"
	);
	assert!(mean_channel(&result.rgba, 0) < 50);
}

#[test]
fn exif_orientation_rotates_the_result() {
	// Orientation 6 = rotate 90° clockwise: the 300×200 red main image must
	// come out portrait. Target large enough that the 160px thumb cannot
	// serve it (160*2 < 512), forcing the main decode + rotation.
	let bytes = jpeg_with_exif_thumbnail(6);
	let result = thumb(Box::new(MemSource(bytes)), &spec(512))
		.unwrap()
		.expect("must decode the main image");
	assert!(mean_channel(&result.rgba, 0) > 200, "expected the red main");
	assert!(
		result.height > result.width,
		"orientation 6 must rotate to portrait, got {}x{}",
		result.width,
		result.height
	);
}

#[test]
fn gif_and_bmp_and_tiff_thumbnail_within_their_caps() {
	for format in [ImageFormat::Gif, ImageFormat::Bmp, ImageFormat::Tiff] {
		let bytes = encode(&checkerboard(200, 100), format);
		let result = thumb(Box::new(MemSource(bytes)), &spec(50))
			.unwrap()
			.unwrap_or_else(|| panic!("{format:?} must thumbnail"));
		assert!(result.width >= 50, "{format:?}");
	}
}

// ---- progressive DC-scan path ----

fn progressive_jpeg(image: &RgbImage, sampling: jpeg_encoder::SamplingFactor) -> Vec<u8> {
	let mut out = Vec::new();
	let mut enc = jpeg_encoder::Encoder::new(&mut out, 90);
	enc.set_progressive(true);
	enc.set_sampling_factor(sampling);
	enc.encode(
		image.as_raw(),
		image.width() as u16,
		image.height() as u16,
		jpeg_encoder::ColorType::Rgb,
	)
	.unwrap();
	out
}

/// A ByteSource that remembers the furthest byte it was ever asked for —
/// how the prefix-only property gets asserted.
struct CountingSource(MemSource, std::sync::Arc<std::sync::atomic::AtomicU64>);

impl microthumb::ByteSource for CountingSource {
	fn len(&self) -> u64 {
		self.0.len()
	}

	fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
		let n = self.0.read_at(offset, buf)?;
		self.1
			.fetch_max(offset + n as u64, std::sync::atomic::Ordering::Relaxed);
		Ok(n)
	}
}

/// Per-channel mean absolute error between the DC-path output and the block
/// means of the original image, over blocks fully inside the image.
fn dc_mae(original: &RgbImage, dc: &microthumb::SmallImage) -> f64 {
	let bw = original.width() / 8;
	let bh = original.height() / 8;
	assert!(dc.width >= bw && dc.height >= bh);
	let mut total = 0f64;
	let mut n = 0f64;
	for by in 0..bh {
		for bx in 0..bw {
			let mut mean = [0f64; 3];
			for y in 0..8 {
				for x in 0..8 {
					let px = original.get_pixel(bx * 8 + x, by * 8 + y);
					for (m, v) in mean.iter_mut().zip(px.0) {
						*m += f64::from(v);
					}
				}
			}
			let o = ((by * dc.width + bx) * 4) as usize;
			for (c, m) in mean.iter().enumerate() {
				total += (m / 64.0 - f64::from(dc.rgba[o + c])).abs();
				n += 1.0;
			}
		}
	}
	total / n
}

fn smooth_image(w: u32, h: u32) -> RgbImage {
	RgbImage::from_fn(w, h, |x, y| {
		Rgb([
			(x * 255 / w.max(1)) as u8,
			(y * 255 / h.max(1)) as u8,
			((x + y) * 127 / (w + h).max(1)) as u8,
		])
	})
}

/// Budget that the full progressive decode cannot fit but the DC path can —
/// forcing the routing without needing a huge fixture.
fn dc_forcing_spec(target: u32) -> ThumbSpec {
	ThumbSpec::new(target, target, 400_000)
}

#[test]
fn dc_parse_matches_the_full_decode_within_tolerance() {
	let image = smooth_image(512, 384);
	for (sampling, tolerance) in [
		(jpeg_encoder::SamplingFactor::F_1_1, 6.0),
		// 4:2:0 chroma covers 16×16 per block; a bit more drift is expected.
		(jpeg_encoder::SamplingFactor::F_2_2, 10.0),
	] {
		let bytes = progressive_jpeg(&image, sampling);
		let result = thumb(Box::new(MemSource(bytes)), &dc_forcing_spec(64))
			.unwrap()
			.expect("dc path must produce a preview");
		assert_eq!((result.width, result.height), (64, 48));
		let mae = dc_mae(&image, &result);
		assert!(
			mae <= tolerance,
			"MAE {mae} over tolerance {tolerance} for {sampling:?}"
		);
	}
}

#[test]
fn dc_parse_handles_grayscale() {
	let mut out = Vec::new();
	let mut enc = jpeg_encoder::Encoder::new(&mut out, 90);
	enc.set_progressive(true);
	let luma: Vec<u8> = (0..200u32 * 120)
		.map(|i| ((i % 200) * 255 / 200) as u8)
		.collect();
	enc.encode(&luma, 200, 120, jpeg_encoder::ColorType::Luma)
		.unwrap();
	let result = thumb(Box::new(MemSource(out)), &dc_forcing_spec(32))
		.unwrap()
		.expect("grayscale dc path must produce a preview");
	// Grey means R==G==B everywhere.
	for px in result.rgba.chunks_exact(4) {
		assert_eq!(px[0], px[1]);
		assert_eq!(px[1], px[2]);
	}
}

#[test]
fn dc_parse_reads_only_a_prefix_of_the_file() {
	// Real-entropy content so the AC scans (which the DC path must never
	// read) dominate the file.
	let image = RgbImage::from_fn(2000, 1500, |x, y| {
		Rgb([
			(x * 7 % 251) as u8,
			(y * 13 % 249) as u8,
			((x ^ y) % 256) as u8,
		])
	});
	let bytes = progressive_jpeg(&image, jpeg_encoder::SamplingFactor::F_2_2);
	let len = bytes.len() as u64;
	let watermark = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
	let source = CountingSource(MemSource(bytes), watermark.clone());
	let result = thumb(Box::new(source), &spec(512)).unwrap();
	result.expect("large progressive must produce a dc preview");
	let read = watermark.load(std::sync::atomic::Ordering::Relaxed);
	eprintln!(
		"dc prefix: read {read} of {len} bytes ({:.1}%)",
		read as f64 * 100.0 / len as f64
	);
	assert!(
		read * 2 < len,
		"dc path read {read} of {len} bytes — not a prefix decode"
	);
}

#[test]
fn hostile_progressive_streams_error_cleanly() {
	// Truncated mid-scan: routed to the DC parser, which must error, not hang
	// or panic.
	let image = smooth_image(512, 384);
	let mut bytes = progressive_jpeg(&image, jpeg_encoder::SamplingFactor::F_2_2);
	bytes.truncate(bytes.len() / 20);
	assert!(thumb(Box::new(MemSource(bytes)), &dc_forcing_spec(64)).is_err());

	// A DHT whose counts exceed 256 symbols.
	let mut bad_dht = progressive_jpeg_header(4000, 3000);
	bad_dht.extend_from_slice(&[0xFF, 0xC4, 0x01, 0x15, 0x00]);
	bad_dht.extend_from_slice(&[16u8; 16]);
	bad_dht.extend_from_slice(&[0u8; 256]);
	bad_dht.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00]);
	assert!(thumb(Box::new(MemSource(bad_dht)), &spec(512)).is_err());
}

/// A progressive JPEG built to overflow the DC dequantiser: a 16-bit DQT with
/// the largest quantizer, a DC scan at the largest point transform the probe
/// admits (`Al` = 7 — jpeg-decoder rejects a shift past the frame precision
/// before our parser ever sees the file), and every block carrying the largest
/// DC magnitude 8-bit precision allows, so the predictor accumulates across
/// 4096 blocks. The product runs to ~1e14, far outside i32, and
/// `[profile.test]` keeps overflow-checks on: before the widening this
/// panicked with "attempt to multiply with overflow", breaking the module's
/// no-panic contract on hostile input. Sized and starved so the routing sends
/// it to the DC parser rather than a full decode — an image small enough to
/// decode normally never reaches this code at all.
#[test]
fn a_dc_scan_with_a_huge_quantizer_and_shift_does_not_panic() {
	const DIM: u16 = 512;
	const AL: u8 = 7;
	const CAT: u8 = 11;

	let mut bytes = vec![0xFF, 0xD8]; // SOI
	// DQT, 16-bit precision (Pq=1), table 0, DC quantizer = 65535.
	bytes.extend_from_slice(&[0xFF, 0xDB]);
	bytes.extend_from_slice(&(2u16 + 1 + 128).to_be_bytes());
	bytes.push(0x10);
	for _ in 0..64 {
		bytes.extend_from_slice(&65535u16.to_be_bytes());
	}
	// SOF2: one component, quant table 0.
	bytes.extend_from_slice(&[0xFF, 0xC2, 0x00, 0x0B, 0x08]);
	bytes.extend_from_slice(&DIM.to_be_bytes());
	bytes.extend_from_slice(&DIM.to_be_bytes());
	bytes.extend_from_slice(&[0x01, 0x01, 0x11, 0x00]);
	// DHT, DC table 0: a single one-bit code "0" meaning category CAT.
	bytes.extend_from_slice(&[0xFF, 0xC4]);
	bytes.extend_from_slice(&(2u16 + 1 + 16 + 1).to_be_bytes());
	bytes.push(0x00); // class 0 (DC), table 0
	bytes.push(1); // one code of length 1
	bytes.extend_from_slice(&[0u8; 15]);
	bytes.push(CAT);
	// SOS: one component, Ss=0, Se=0, Ah=0, Al=AL.
	bytes.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x00, AL]);

	// Entropy: per block, code "0" then CAT one-bits (maximum positive
	// magnitude). 0xFF is byte-stuffed as required inside a scan.
	let blocks = u64::from(DIM.div_ceil(8)).pow(2);
	let (mut acc, mut n) = (0u32, 0u32);
	let mut scan = Vec::new();
	let push = |bit: u32, acc: &mut u32, n: &mut u32, out: &mut Vec<u8>| {
		*acc = (*acc << 1) | bit;
		*n += 1;
		if *n == 8 {
			let byte = *acc as u8;
			out.push(byte);
			if byte == 0xFF {
				out.push(0);
			}
			*acc = 0;
			*n = 0;
		}
	};
	for _ in 0..blocks {
		push(0, &mut acc, &mut n, &mut scan);
		for _ in 0..CAT {
			push(1, &mut acc, &mut n, &mut scan);
		}
	}
	while n != 0 {
		push(1, &mut acc, &mut n, &mut scan);
	}
	bytes.extend_from_slice(&scan);
	bytes.extend_from_slice(&[0xFF, 0xD9]); // EOI

	let starved = ThumbSpec {
		target_width: 64,
		target_height: 64,
		mem_budget: 400 * 1024,
		allow_full_decode: true,
	};
	let result = thumb(Box::new(MemSource(bytes)), &starved)
		.expect("a hostile DC scan must not error out here")
		.expect("the DC parser must still produce a preview");
	// Saturated rather than wrapped: the clamp is what the widening buys.
	assert!(result.rgba.chunks_exact(4).all(|px| px[0] == 255));
}

/// Zero dimensions in a progressive SOF2 must be refused, not divided by.
/// The DC parser guards `width == 0 || height == 0` explicitly; nothing
/// exercised it until now, so the guard shipped untested.
#[test]
fn a_progressive_jpeg_with_zero_dimensions_is_refused() {
	for (w, h) in [(0u16, 300u16), (300, 0), (0, 0)] {
		let bytes = progressive_jpeg_header(w, h);
		let result = thumb(Box::new(MemSource(bytes)), &spec(512));
		assert!(
			matches!(result, Ok(None) | Err(_)),
			"{w}x{h} must not produce a thumbnail"
		);
	}
}

// ---- phase 2: streaming gif / tiff / bmp ----

#[test]
fn gif_thumbnails_the_first_frame_of_an_animation() {
	let (w, h) = (64u16, 32u16);
	let mut bytes = Vec::new();
	{
		let palette = [255, 0, 0, 0, 255, 0];
		let mut enc = gif::Encoder::new(&mut bytes, w, h, &palette).unwrap();
		let px = w as usize * h as usize;
		enc.write_frame(&gif::Frame::from_indexed_pixels(w, h, vec![0u8; px], None))
			.unwrap();
		enc.write_frame(&gif::Frame::from_indexed_pixels(w, h, vec![1u8; px], None))
			.unwrap();
	}
	let result = thumb(Box::new(MemSource(bytes)), &spec(32))
		.unwrap()
		.expect("gif must thumbnail");
	assert!(
		mean_channel(&result.rgba, 0) > 200,
		"expected the RED first frame"
	);
	assert!(mean_channel(&result.rgba, 1) < 50);
}

#[test]
fn interlaced_gif_rows_land_at_their_display_positions() {
	// 16 distinct row colors, rows written in interlace pass order with the
	// interlace flag set — the decode must land each at its display y.
	let (w, h) = (16u16, 16u16);
	let mut palette = Vec::new();
	for i in 0..16u8 {
		palette.extend_from_slice(&[i * 17, i * 17, i * 17]);
	}
	let pass_order = [0u8, 8, 4, 12, 2, 6, 10, 14, 1, 3, 5, 7, 9, 11, 13, 15];
	let mut pixels = Vec::new();
	for y in pass_order {
		pixels.extend(std::iter::repeat_n(y, w as usize));
	}
	let mut bytes = Vec::new();
	{
		let mut enc = gif::Encoder::new(&mut bytes, w, h, &palette).unwrap();
		let mut frame = gif::Frame::from_indexed_pixels(w, h, pixels, None);
		frame.interlaced = true;
		enc.write_frame(&frame).unwrap();
	}
	let result = thumb(Box::new(MemSource(bytes)), &spec(16))
		.unwrap()
		.expect("interlaced gif must thumbnail");
	assert_eq!((result.width, result.height), (16, 16));
	for y in 0..16u32 {
		let v = result.rgba[(y * 16 * 4) as usize];
		assert_eq!(v, y as u8 * 17, "row {y} landed wrong");
	}
}

#[test]
fn tiff_gray16_streams_with_depth_downscale() {
	let (w, h) = (300u32, 200u32);
	let data: Vec<u16> = (0..w * h).map(|i| ((i % w) * 65535 / w) as u16).collect();
	let mut bytes = Vec::new();
	{
		let mut enc = tiff::encoder::TiffEncoder::new(std::io::Cursor::new(&mut bytes)).unwrap();
		let mut img = enc
			.new_image::<tiff::encoder::colortype::Gray16>(w, h)
			.unwrap();
		img.rows_per_strip(8).unwrap();
		img.write_data(&data).unwrap();
	}
	let result = thumb(Box::new(MemSource(bytes)), &spec(64))
		.unwrap()
		.expect("gray16 tiff must thumbnail");
	for px in result.rgba.chunks_exact(4) {
		assert_eq!(px[0], px[1]);
		assert_eq!(px[1], px[2]);
	}
	let mean = mean_channel(&result.rgba, 0);
	assert!((100..=155).contains(&mean), "gradient mean drifted: {mean}");
}

#[test]
fn a_single_huge_strip_tiff_is_refused_but_striped_is_not() {
	let (w, h) = (2000u32, 1500u32);
	let data: Vec<u8> = (0..w * h * 3).map(|i| (i % 251) as u8).collect();
	let encode = |rows: u32| {
		let mut bytes = Vec::new();
		let mut enc = tiff::encoder::TiffEncoder::new(std::io::Cursor::new(&mut bytes)).unwrap();
		let mut img = enc
			.new_image::<tiff::encoder::colortype::RGB8>(w, h)
			.unwrap();
		img.rows_per_strip(rows).unwrap();
		img.write_data(&data).unwrap();
		bytes
	};
	// Same pixels, same budget, two strip layouts — the whole point of pricing
	// a decode by its real per-chunk peak. One 9 MB strip must be decoded whole
	// and cannot fit a 4 MB budget; 8-row strips peak at ~48 KB and sail
	// through. (Under the default 12 MB budget the single strip DOES fit and is
	// decoded — affording what the budget genuinely allows is the point.)
	let tight = ThumbSpec {
		target_width: 512,
		target_height: 512,
		mem_budget: 4 * 1024 * 1024,
		allow_full_decode: true,
	};
	let huge = encode(h);
	assert_eq!(
		thumb(Box::new(MemSource(huge.clone())), &tight).unwrap(),
		None
	);
	let striped = encode(8);
	assert!(
		thumb(Box::new(MemSource(striped.clone())), &tight)
			.unwrap()
			.is_some()
	);
	// Both are affordable once the budget can actually hold the strip.
	assert!(
		thumb(Box::new(MemSource(huge)), &spec(512))
			.unwrap()
			.is_some()
	);
	assert!(
		thumb(Box::new(MemSource(striped)), &spec(512))
			.unwrap()
			.is_some()
	);
}

/// Hand-assembled uncompressed BMP (BITMAPINFOHEADER). `rows` are display
/// order (top first), indices for 8bpp or BGR triples for 24bpp.
fn bmp_bytes(
	w: u32,
	h: u32,
	bpp: u16,
	top_down: bool,
	palette: &[[u8; 4]],
	rows: &[&[u8]],
) -> Vec<u8> {
	let stride = ((w as usize * bpp as usize).div_ceil(32)) * 4;
	let data_offset = 14 + 40 + palette.len() * 4;
	let mut bytes = Vec::new();
	bytes.extend_from_slice(b"BM");
	bytes.extend_from_slice(&(data_offset as u32 + (stride as u32) * h).to_le_bytes());
	bytes.extend_from_slice(&0u32.to_le_bytes());
	bytes.extend_from_slice(&(data_offset as u32).to_le_bytes());
	bytes.extend_from_slice(&40u32.to_le_bytes());
	bytes.extend_from_slice(&(w as i32).to_le_bytes());
	let h_field = if top_down { -(h as i32) } else { h as i32 };
	bytes.extend_from_slice(&h_field.to_le_bytes());
	bytes.extend_from_slice(&1u16.to_le_bytes());
	bytes.extend_from_slice(&bpp.to_le_bytes());
	bytes.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
	bytes.extend_from_slice(&0u32.to_le_bytes());
	bytes.extend_from_slice(&[0u8; 8]); // ppm
	bytes.extend_from_slice(&(palette.len() as u32).to_le_bytes());
	bytes.extend_from_slice(&0u32.to_le_bytes());
	for entry in palette {
		bytes.extend_from_slice(entry);
	}
	let disk_rows: Vec<&&[u8]> = if top_down {
		rows.iter().collect()
	} else {
		rows.iter().rev().collect()
	};
	for row in disk_rows {
		let mut padded = row.to_vec();
		padded.resize(stride, 0);
		bytes.extend_from_slice(&padded);
	}
	bytes
}

#[test]
fn bmp_palette_and_row_orders_stream_correctly() {
	// 8bpp bottom-up: two rows, distinct palette colors; display top row must
	// be palette 0 (red) even though it is LAST on disk.
	let palette = [[0u8, 0, 255, 0], [0u8, 255, 0, 0]]; // BGRA: red, green
	let bytes = bmp_bytes(3, 2, 8, false, &palette, &[&[0, 0, 0], &[1, 1, 1]]);
	let result = thumb(Box::new(MemSource(bytes)), &spec(4))
		.unwrap()
		.expect("8bpp bmp must thumbnail");
	assert_eq!((result.width, result.height), (3, 2));
	assert_eq!(&result.rgba[..4], &[255, 0, 0, 255], "top row must be red");
	assert_eq!(&result.rgba[3 * 4..3 * 4 + 4], &[0, 255, 0, 255]);

	// 24bpp top-down: BGR on disk, first disk row IS the top row.
	let bytes = bmp_bytes(
		2,
		2,
		24,
		true,
		&[],
		&[&[255, 0, 0, 255, 0, 0], &[0, 0, 255, 0, 0, 255]],
	);
	let result = thumb(Box::new(MemSource(bytes)), &spec(4))
		.unwrap()
		.expect("top-down bmp must thumbnail");
	assert_eq!(&result.rgba[..4], &[0, 0, 255, 255], "top row must be blue");
	assert_eq!(&result.rgba[2 * 4..2 * 4 + 4], &[255, 0, 0, 255]);
}

#[test]
fn rle_bmp_falls_back_to_the_capped_image_path() {
	// Minimal valid RLE8 2x2 (compression = 1): our streamer refuses it and
	// hands the source to the whole-frame fallback, which decodes it.
	let mut bytes = Vec::new();
	bytes.extend_from_slice(b"BM");
	bytes.extend_from_slice(&([0u32; 2].map(|_| 0u32))[0].to_le_bytes()); // size (unused)
	bytes.extend_from_slice(&0u32.to_le_bytes());
	bytes.extend_from_slice(&(14u32 + 40 + 8).to_le_bytes());
	bytes.extend_from_slice(&40u32.to_le_bytes());
	bytes.extend_from_slice(&2i32.to_le_bytes());
	bytes.extend_from_slice(&2i32.to_le_bytes());
	bytes.extend_from_slice(&1u16.to_le_bytes());
	bytes.extend_from_slice(&8u16.to_le_bytes());
	bytes.extend_from_slice(&1u32.to_le_bytes()); // BI_RLE8
	bytes.extend_from_slice(&0u32.to_le_bytes());
	bytes.extend_from_slice(&[0u8; 8]);
	bytes.extend_from_slice(&2u32.to_le_bytes());
	bytes.extend_from_slice(&0u32.to_le_bytes());
	bytes.extend_from_slice(&[0, 0, 255, 0]); // palette 0: red (BGRA)
	bytes.extend_from_slice(&[0, 255, 0, 0]); // palette 1: green
	// bottom row: 2 px of idx 0, EOL; top row: 2 px of idx 1, EOS.
	bytes.extend_from_slice(&[2, 0, 0, 0, 2, 1, 0, 1]);
	let result = thumb(Box::new(MemSource(bytes)), &spec(4))
		.unwrap()
		.expect("rle bmp must fall back and decode");
	assert_eq!((result.width, result.height), (2, 2));
	assert_eq!(&result.rgba[..4], &[0, 255, 0, 255], "top row green");
}

// ---- review-fix pass: hostile shapes and the work ceiling ----

/// GIF89a + a bare logical screen descriptor + an immediate trailer + one
/// trailing byte. The decoder's trailer state consumes nothing and reports
/// nothing, so feeding it that byte used to spin forever INSIDE update();
/// the trailer bail must fire first.
#[test]
fn a_trailer_before_any_frame_errors_instead_of_spinning() {
	let mut bytes = Vec::new();
	bytes.extend_from_slice(b"GIF89a");
	bytes.extend_from_slice(&1u16.to_le_bytes());
	bytes.extend_from_slice(&1u16.to_le_bytes());
	bytes.extend_from_slice(&[0, 0, 0]);
	bytes.push(0x3B);
	bytes.push(0x00);
	assert!(thumb(Box::new(MemSource(bytes)), &spec(64)).is_err());
}

fn crc32(data: &[u8]) -> u32 {
	let mut crc = 0xFFFF_FFFFu32;
	for &b in data {
		crc ^= u32::from(b);
		for _ in 0..8 {
			crc = (crc >> 1) ^ (0xEDB8_8320 & (0u32.wrapping_sub(crc & 1)));
		}
	}
	!crc
}

fn png_chunk(out: &mut Vec<u8>, tag: &[u8; 4], data: &[u8]) {
	out.extend_from_slice(&(data.len() as u32).to_be_bytes());
	out.extend_from_slice(tag);
	out.extend_from_slice(data);
	let mut crc_input = tag.to_vec();
	crc_input.extend_from_slice(data);
	out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

#[test]
fn png_honours_the_exif_orientation_chunk() {
	// PNGs carry EXIF in an `eXIf` chunk — a converted phone screenshot keeps
	// its orientation there, and the row streamer used to ignore it and
	// thumbnail the picture on its side.
	let mut exif = Vec::new();
	exif.extend_from_slice(b"II");
	exif.extend_from_slice(&42u16.to_le_bytes());
	exif.extend_from_slice(&8u32.to_le_bytes());
	exif.extend_from_slice(&1u16.to_le_bytes()); // one entry
	exif.extend_from_slice(&0x0112u16.to_le_bytes()); // Orientation
	exif.extend_from_slice(&3u16.to_le_bytes()); // SHORT
	exif.extend_from_slice(&1u32.to_le_bytes());
	exif.extend_from_slice(&6u16.to_le_bytes()); // rotate 90° clockwise
	exif.extend_from_slice(&0u16.to_le_bytes()); // value padding
	exif.extend_from_slice(&0u32.to_le_bytes()); // no next IFD

	// Splice the chunk in behind IHDR (8-byte signature + a 25-byte IHDR).
	let landscape = encode(&checkerboard(300, 200), ImageFormat::Png);
	let mut bytes = landscape[..33].to_vec();
	png_chunk(&mut bytes, b"eXIf", &exif);
	bytes.extend_from_slice(&landscape[33..]);

	let upright = thumb(Box::new(MemSource(landscape)), &spec(128))
		.unwrap()
		.expect("the plain png must thumbnail");
	assert!(upright.width > upright.height, "fixture must be landscape");
	let rotated = thumb(Box::new(MemSource(bytes)), &spec(128))
		.unwrap()
		.expect("the eXIf png must thumbnail");
	assert!(
		rotated.height > rotated.width,
		"orientation 6 must rotate to portrait, got {}x{}",
		rotated.width,
		rotated.height
	);
}

/// A 1×2³⁰ PNG: per-row memory passes any budget — the row COUNT is the
/// attack. The source-side ceiling refuses it before a single row decodes.
/// (Hand-built header; actually encoding one would itself be the DoS.)
#[test]
fn a_degenerate_png_ribbon_refuses_before_any_decode_work() {
	let mut bytes = Vec::new();
	bytes.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
	let mut ihdr = Vec::new();
	ihdr.extend_from_slice(&1u32.to_be_bytes());
	ihdr.extend_from_slice(&(1u32 << 30).to_be_bytes());
	ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit RGB, no interlace
	png_chunk(&mut bytes, b"IHDR", &ihdr);
	png_chunk(&mut bytes, b"IDAT", &[0x78, 0x9C]);
	png_chunk(&mut bytes, b"IEND", &[]);
	assert_eq!(thumb(Box::new(MemSource(bytes)), &spec(64)).unwrap(), None);
}

/// A forged `RowsPerStrip=1` TIFF declaring 2²⁶ rows: every per-strip peak is
/// tiny, the strip count is the attack. Hand-built IFD — inline values only.
#[test]
fn a_forged_strip_count_tiff_refuses_before_any_decode_work() {
	let mut t = Vec::new();
	t.extend_from_slice(b"II");
	t.extend_from_slice(&42u16.to_le_bytes());
	t.extend_from_slice(&8u32.to_le_bytes());
	let entries: &[(u16, u16, u32)] = &[
		(256, 3, 1),        // ImageWidth = 1
		(257, 4, 1 << 26),  // ImageLength
		(258, 3, 8),        // BitsPerSample
		(259, 3, 1),        // Compression = none
		(262, 3, 1),        // Photometric = BlackIsZero
		(273, 4, 0x10_000), // StripOffsets (bogus, never read)
		(277, 3, 1),        // SamplesPerPixel
		(278, 4, 1),        // RowsPerStrip = 1
		(279, 4, 1),        // StripByteCounts
	];
	t.extend_from_slice(&(entries.len() as u16).to_le_bytes());
	for &(tag, kind, value) in entries {
		t.extend_from_slice(&tag.to_le_bytes());
		t.extend_from_slice(&kind.to_le_bytes());
		t.extend_from_slice(&1u32.to_le_bytes());
		if kind == 3 {
			t.extend_from_slice(&(value as u16).to_le_bytes());
			t.extend_from_slice(&0u16.to_le_bytes());
		} else {
			t.extend_from_slice(&value.to_le_bytes());
		}
	}
	t.extend_from_slice(&0u32.to_le_bytes());
	let result = thumb(Box::new(MemSource(t)), &spec(64));
	// Refused on the DECLARED count, before `Decoder::new` reads a single
	// offset entry — the whole point, since that constructor materialises the
	// per-strip tables eagerly (~48 B each, so 2²⁶ strips is gigabytes) under
	// the tiff crate's own limits rather than ours. A generic Err would also
	// have passed here before the gate existed, so pin the reason.
	match result {
		Err(ThumbError::Decode(msg)) => assert!(
			msg.contains("strip/tile count"),
			"expected the pre-decode strip-count refusal, got {msg}"
		),
		other => panic!("expected a pre-decode refusal, got {other:?}"),
	}
}

/// An embedded preview that does not fit the budget (with its rotation copy)
/// must not be served through the fast path — pre-fix it skipped the budget
/// check entirely.
#[test]
fn an_oversized_embedded_preview_is_not_served_unbudgeted() {
	let bytes = jpeg_with_exif_thumbnail(1);
	let starved = ThumbSpec {
		target_width: 16,
		target_height: 16,
		mem_budget: 64,
		allow_full_decode: true,
	};
	// The preview would suffice for a 16px target, but 64 bytes cannot hold
	// it twice over — and the starved budget refuses the real decode too.
	assert_eq!(thumb(Box::new(MemSource(bytes)), &starved).unwrap(), None);
}

/// #11: a first frame smaller than the logical screen — the surrounding
/// border must carry the background color, exercised by no other fixture.
#[test]
fn a_small_first_frame_fills_the_border_with_the_background() {
	let (w, h) = (16u16, 16u16);
	let mut bytes = Vec::new();
	{
		// Palette index 0 (the background) is blue, index 1 is red.
		let palette = [0, 0, 255, 255, 0, 0];
		let mut enc = gif::Encoder::new(&mut bytes, w, h, &palette).unwrap();
		let mut frame = gif::Frame::from_indexed_pixels(8, 8, vec![1u8; 64], None);
		frame.left = 4;
		frame.top = 4;
		enc.write_frame(&frame).unwrap();
	}
	let result = thumb(Box::new(MemSource(bytes)), &spec(16))
		.unwrap()
		.expect("gif with a small frame must thumbnail");
	assert_eq!((result.width, result.height), (16, 16));
	assert_eq!(&result.rgba[..4], &[0, 0, 255, 255], "corner is background");
	let center = ((8 * 16 + 8) * 4) as usize;
	assert_eq!(
		&result.rgba[center..center + 4],
		&[255, 0, 0, 255],
		"frame interior is the frame's color"
	);
}

#[test]
fn the_reported_source_distinguishes_the_two_paths() {
	// The whole point of reporting it: an embedded preview costs a couple of
	// container reads, a decode can cost the whole file, and nothing else in
	// the result tells them apart. A JPEG carrying an EXIF thumbnail serves a
	// small request from it...
	let with_thumb = jpeg_with_exif_thumbnail(1);
	let served = generate(Box::new(MemSource(with_thumb.clone())), &spec(64))
		.unwrap()
		.thumbnail()
		.expect("the embedded thumbnail must serve this");
	assert_eq!(served.source, microthumb::ThumbSource::EmbeddedPreview);

	// ...and the same file decodes for real once the request outgrows the
	// 160 px thumb (preview_suffices needs half the requested long side).
	let decoded = generate(Box::new(MemSource(with_thumb)), &spec(512))
		.unwrap()
		.thumbnail()
		.expect("the main image must decode");
	assert_eq!(decoded.source, microthumb::ThumbSource::Decoded);

	// A file with no embedded thumbnail at all can only ever be decoded.
	let plain = encode(&checkerboard(256, 256), ImageFormat::Png);
	let plain = generate(Box::new(MemSource(plain)), &spec(64))
		.unwrap()
		.thumbnail()
		.expect("png must thumbnail");
	assert_eq!(plain.source, microthumb::ThumbSource::Decoded);
}

#[test]
fn a_progressive_jpeg_past_the_dc_block_cap_still_serves_its_embedded_thumbnail() {
	// The real case this comes from: a 188 MP progressive library scan whose DC
	// planes (4.4M blocks) exceed the parser's pre-allocation cap. `open` refuses
	// — and used to propagate that refusal out of the pipeline, discarding the
	// EXIF thumbnail it had already parsed, because the orchestrator only looks
	// at the preview AFTER `open` returns. A refusal must degrade to the
	// cheapest path, not abort the whole request.
	//
	// 20000x20000 at 4:2:0 is 6.25M DC blocks, three times the cap.
	let thumb = encode(
		&RgbImage::from_pixel(160, 120, Rgb([0, 255, 0])),
		ImageFormat::Jpeg,
	);
	let mut tiff = Vec::new();
	tiff.extend_from_slice(b"II");
	tiff.extend_from_slice(&42u16.to_le_bytes());
	tiff.extend_from_slice(&8u32.to_le_bytes());
	tiff.extend_from_slice(&1u16.to_le_bytes());
	tiff.extend_from_slice(&0x0112u16.to_le_bytes());
	tiff.extend_from_slice(&3u16.to_le_bytes());
	tiff.extend_from_slice(&1u32.to_le_bytes());
	tiff.extend_from_slice(&1u32.to_le_bytes());
	tiff.extend_from_slice(&26u32.to_le_bytes());
	tiff.extend_from_slice(&2u16.to_le_bytes());
	tiff.extend_from_slice(&0x0201u16.to_le_bytes());
	tiff.extend_from_slice(&4u16.to_le_bytes());
	tiff.extend_from_slice(&1u32.to_le_bytes());
	tiff.extend_from_slice(&56u32.to_le_bytes());
	tiff.extend_from_slice(&0x0202u16.to_le_bytes());
	tiff.extend_from_slice(&4u16.to_le_bytes());
	tiff.extend_from_slice(&1u32.to_le_bytes());
	tiff.extend_from_slice(&(thumb.len() as u32).to_le_bytes());
	tiff.extend_from_slice(&0u32.to_le_bytes());
	assert_eq!(tiff.len(), 56);
	tiff.extend_from_slice(&thumb);

	let mut exif_payload = b"Exif\0\0".to_vec();
	exif_payload.extend_from_slice(&tiff);
	let app1_len = (exif_payload.len() + 2) as u16;
	let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE1];
	bytes.extend_from_slice(&app1_len.to_be_bytes());
	bytes.extend_from_slice(&exif_payload);
	// SOF2 with 4:2:0 sampling, far past the DC parser's block cap.
	bytes.extend_from_slice(&[0xFF, 0xDB, 0x00, 0x43, 0x00]);
	bytes.extend_from_slice(&[16u8; 64]);
	bytes.extend_from_slice(&[0xFF, 0xC2, 0x00, 0x11, 0x08]);
	bytes.extend_from_slice(&20000u16.to_be_bytes()); // height
	bytes.extend_from_slice(&20000u16.to_be_bytes()); // width
	bytes.push(3);
	bytes.extend_from_slice(&[0x01, 0x22, 0x00]); // Y  h=2 v=2
	bytes.extend_from_slice(&[0x02, 0x11, 0x00]); // Cb
	bytes.extend_from_slice(&[0x03, 0x11, 0x00]); // Cr

	let result = generate(Box::new(MemSource(bytes)), &spec(64))
		.unwrap()
		.thumbnail()
		.expect("the embedded thumbnail must survive the DC parser's refusal");
	assert_eq!(result.source, microthumb::ThumbSource::EmbeddedPreview);
	assert!(
		mean_channel(&result.image.rgba, 1) > 200,
		"expected the green embedded thumb"
	);
}

#[test]
fn preview_only_serves_the_embedded_thumbnail_without_reading_the_image() {
	// The case this exists for: a file too large to stream over the network for
	// one small thumbnail. Refusing on size alone threw away the cheap path
	// too — an embedded thumbnail lives in the header region and costs a chunk
	// or two, however big the file is.
	// A main image large enough that reading it would be obvious: a buffered
	// header read cannot mask the difference the way it would on a 3 KB file.
	let bytes = jpeg_with_exif_thumbnail_sized(1, 1600, 1200);
	let len = bytes.len() as u64;
	let reach = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
	let source = CountingSource(MemSource(bytes), reach.clone());

	let spec = ThumbSpec::preview_only(64, 64, DEFAULT_MEM_BUDGET);
	let result = generate(Box::new(source), &spec)
		.unwrap()
		.thumbnail()
		.expect("the embedded thumbnail must still be served");
	assert_eq!(result.source, microthumb::ThumbSource::EmbeddedPreview);
	assert!(
		mean_channel(&result.image.rgba, 1) > 200,
		"expected the green embedded thumb, not the red main image"
	);
	// The main image was never touched: the thumbnail sits in the APP1
	// segment near the start, so the read never reaches the end of the file.
	let furthest = reach.load(std::sync::atomic::Ordering::Relaxed);
	assert!(
		furthest * 4 < len,
		"preview-only read to {furthest} of {len} bytes — it should stop in the header region"
	);
}

#[test]
fn preview_only_without_an_embedded_thumbnail_refuses_instead_of_decoding() {
	// No embedded preview and no permission to decode: the honest answer is
	// "no thumbnail", NOT a full decode of a file we were told not to stream.
	let bytes = encode(&checkerboard(600, 600), ImageFormat::Png);
	let preview_only = ThumbSpec::preview_only(64, 64, DEFAULT_MEM_BUDGET);
	// And it says WHICH kind of "no": the format WAS recognised, so this is a
	// verdict about the spec, not about the bytes.
	assert_eq!(
		generate(Box::new(MemSource(bytes)), &preview_only).unwrap(),
		ThumbOutcome::OverBudget
	);

	// The very same source decodes fine when it IS allowed to.
	let bytes = encode(&checkerboard(600, 600), ImageFormat::Png);
	assert!(
		generate(Box::new(MemSource(bytes)), &spec(64))
			.unwrap()
			.thumbnail()
			.is_some()
	);
}

// ---- heif ----

/// The one fixture-FILE case in this suite, and deliberately so: AVIF cannot
/// be generated here — the vendored libheif is decode-only and `image` has no
/// AV1 encoder — so this reads the committed browser fixture rather than
/// inventing a binary. It is what proves the `avif` brand claimed in
/// `formats/heif.rs` reaches a decoder that actually answers.
#[cfg(feature = "heif")]
#[test]
fn avif_thumbnails_through_the_heif_path() {
	let path = concat!(
		env!("CARGO_MANIFEST_DIR"),
		"/../filen-sdk-rs/web/test-assets/imgs/parrot.avif"
	);
	let bytes = std::fs::read(path).expect("committed parrot.avif fixture");
	let result = thumb(Box::new(MemSource(bytes)), &spec(64))
		.unwrap()
		.expect("avif must thumbnail");
	// `generate` returns at least the requested box (the caller does the final
	// resize); the fixture is 1000x667, so the aspect must stay landscape.
	assert!(result.width >= 64 && result.width <= 1000);
	assert!(result.width > result.height);
	assert_eq!(
		result.rgba.len(),
		(result.width * result.height * 4) as usize
	);
	assert!(
		result.rgba.iter().any(|&b| b != 0),
		"decoded avif thumbnail is entirely zero"
	);
}

// ---- svg ----

#[cfg(feature = "svg")]
fn svg_doc(attrs: &str, body: &str) -> Vec<u8> {
	format!(r#"<svg xmlns="http://www.w3.org/2000/svg" {attrs}>{body}</svg>"#).into_bytes()
}

#[cfg(feature = "svg")]
#[test]
fn svg_sniffs_through_a_full_xml_prologue() {
	// BOM, declaration, comment, DOCTYPE with an (entity-free) internal
	// subset — everything a real exporter piles in front of the root.
	let mut bytes = Vec::new();
	bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
	bytes.extend_from_slice(
		b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"no\"?>\n\
		  <!-- Created with Inkscape (http://www.inkscape.org/) -->\n\
		  <!DOCTYPE svg PUBLIC \"-//W3C//DTD SVG 1.1//EN\" \
		  \"http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd\" [ <!ATTLIST svg id ID #IMPLIED> ]>\n",
	);
	bytes.extend_from_slice(&svg_doc(
		r#"width="100" height="100""#,
		r#"<rect width="100" height="100" fill="red"/>"#,
	));
	let result = thumb(Box::new(MemSource(bytes)), &spec(64))
		.unwrap()
		.expect("prologue-heavy svg must thumbnail");
	assert_eq!(&result.rgba[..4], &[255, 0, 0, 255]);
}

#[cfg(feature = "svg")]
#[test]
fn svg_sniff_rejects_non_svg_text_and_gzip() {
	// Well-formed XML whose root is not svg.
	let xml = b"<?xml version=\"1.0\"?><note><to>Tove</to><from>Jani</from></note>".to_vec();
	assert_eq!(thumb(Box::new(MemSource(xml)), &spec(64)).unwrap(), None);
	// HTML — even when an inline <svg> appears later in the body.
	let html =
		b"<!DOCTYPE html><html><body><p>hi</p><svg xmlns=\"http://www.w3.org/2000/svg\"/></body></html>"
			.to_vec();
	assert_eq!(thumb(Box::new(MemSource(html)), &spec(64)).unwrap(), None);
	// .svgz: gzip bytes are refused by design — no inflate dependency.
	let svgz = vec![0x1F, 0x8B, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00];
	assert_eq!(thumb(Box::new(MemSource(svgz)), &spec(64)).unwrap(), None);
}

#[cfg(feature = "svg")]
#[test]
fn svg_aspect_comes_from_width_height_then_view_box() {
	// Absolute width/height decide, even in mixed units (200pt × 100pt = 2:1).
	let bytes = svg_doc(
		r#"width="200pt" height="100pt" viewBox="0 0 50 400""#,
		r#"<rect width="50" height="400" fill="lime"/>"#,
	);
	let result = thumb(Box::new(MemSource(bytes)), &spec(100))
		.unwrap()
		.expect("sized svg must thumbnail");
	let ratio = f64::from(result.width) / f64::from(result.height);
	assert!(
		(1.9..=2.1).contains(&ratio),
		"got {}x{}",
		result.width,
		result.height
	);

	// A percentage width carries no absolute size: the viewBox (1:4) decides.
	let bytes = svg_doc(
		r#"width="100%" viewBox="0 0 100 400""#,
		r#"<rect width="100" height="400" fill="lime"/>"#,
	);
	let result = thumb(Box::new(MemSource(bytes)), &spec(100))
		.unwrap()
		.expect("viewBox svg must thumbnail");
	let ratio = f64::from(result.height) / f64::from(result.width);
	assert!(
		(3.8..=4.2).contains(&ratio),
		"got {}x{}",
		result.width,
		result.height
	);

	// Neither: a square raster, not a panic.
	let bytes = svg_doc("", r#"<circle cx="50" cy="50" r="40" fill="blue"/>"#);
	let result = thumb(Box::new(MemSource(bytes)), &spec(64))
		.unwrap()
		.expect("unsized svg must thumbnail");
	assert_eq!(result.width, result.height);
}

#[cfg(feature = "svg")]
#[test]
fn svg_renders_a_flat_colour_and_upscales_small_nominal_sizes() {
	// A 16px nominal size still rasterises at the target — scaling up is what
	// vectors are for — and a full-cover rect lands exactly that colour.
	let bytes = svg_doc(
		r#"width="16" height="16""#,
		r#"<rect width="16" height="16" fill="red"/>"#,
	);
	let result = thumb(Box::new(MemSource(bytes)), &spec(256))
		.unwrap()
		.expect("small svg must thumbnail");
	assert!(
		result.width >= 256,
		"vector must render at the target, got {}",
		result.width
	);
	for px in result.rgba.chunks_exact(4) {
		assert_eq!(px, &[255, 0, 0, 255]);
	}
}

#[cfg(feature = "svg")]
#[test]
fn svg_over_budget_is_refused() {
	let bytes = svg_doc(
		r#"width="100" height="100""#,
		r#"<rect width="100" height="100" fill="red"/>"#,
	);
	let starved = ThumbSpec {
		target_width: 64,
		target_height: 64,
		mem_budget: 1024,
		allow_full_decode: true,
	};
	assert_eq!(thumb(Box::new(MemSource(bytes)), &starved).unwrap(), None);
}

#[cfg(feature = "svg")]
#[test]
fn svg_hostile_documents_are_refused_not_expanded() {
	// Entity declarations: roxmltree bounds billion-laughs expansion, but
	// plain internal entities still amplify quadratically — refused outright.
	let mut laughs = b"<?xml version=\"1.0\"?><!DOCTYPE svg [<!ENTITY a \"aaaaaaaaaa\">\
		<!ENTITY b \"&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;\">]>"
		.to_vec();
	laughs.extend_from_slice(&svg_doc(r#"width="100" height="100""#, "<desc>&b;</desc>"));
	assert!(thumb(Box::new(MemSource(laughs)), &spec(64)).is_err());

	// Patterns: resvg sizes the tile pixmap straight from the pattern rect.
	let pattern = svg_doc(
		r#"width="100" height="100""#,
		r#"<defs><pattern id="p" width="60000" height="60000"><rect width="1" height="1"/></pattern></defs>
		<rect width="100" height="100" fill="url(#p)"/>"#,
	);
	assert!(thumb(Box::new(MemSource(pattern)), &spec(64)).is_err());
	// The same pattern behind a long namespace prefix. The byte scan reads the
	// local name out of a fixed 64-byte window, so a prefix long enough to push
	// `pattern` past it used to hide the element from the refusal above — while
	// usvg, which resolves by namespace URI, still saw a real `<pattern>` and
	// tried to allocate its tile. That was an ABORT, not an error: a failed
	// allocation of 163 TB, uncatchable, taking the whole process with it.
	let prefix = "n".repeat(70);
	let prefixed = svg_doc(
		r#"width="100" height="100""#,
		&format!(
			r#"<defs><{p}:pattern xmlns:{p}="http://www.w3.org/2000/svg" id="p"
			 width="60000" height="60000"><rect width="1" height="1"/></{p}:pattern></defs>
			<rect width="100" height="100" fill="url(#p)"/>"#,
			p = prefix
		),
	);
	let err = thumb(Box::new(MemSource(prefixed)), &spec(64))
		.expect_err("a namespace-prefixed <pattern> must be refused, not aborted on");
	assert!(
		err.to_string().contains("<pattern> is refused"),
		"expected the pattern refusal, got {err}"
	);

	// A filter chain past the primitive cap.
	let mut filters = String::from(r#"<filter id="f">"#);
	for _ in 0..80 {
		filters.push_str(r#"<feGaussianBlur stdDeviation="2"/>"#);
	}
	filters.push_str("</filter>");
	let heavy = svg_doc(r#"width="100" height="100""#, &filters);
	assert!(thumb(Box::new(MemSource(heavy)), &spec(64)).is_err());

	// Nested full-canvas opacity groups: every level is an isolated layer
	// with its own sub-pixmap, concurrent with all its ancestors' — the
	// walk must refuse the stack before resvg allocates it.
	let mut body = String::new();
	for _ in 0..24 {
		body.push_str(r#"<g opacity="0.5">"#);
	}
	body.push_str(r#"<rect width="100" height="100" fill="red"/>"#);
	for _ in 0..24 {
		body.push_str("</g>");
	}
	let stack = svg_doc(r#"width="100" height="100""#, &body);
	assert!(thumb(Box::new(MemSource(stack)), &spec(64)).is_err());
}

#[cfg(feature = "svg")]
#[test]
fn svg_use_sprites_decode_and_use_bombs_do_not() {
	// The idiom `<use>` exists for: define once in <defs>, place by reference.
	// It used to be refused outright, which cost every icon sprite in the
	// world its thumbnail.
	let icon = svg_doc(
		r#"width="100" height="100""#,
		r##"<defs><g id="i"><rect width="100" height="100" fill="red"/></g></defs>
		<use href="#i"/>"##,
	);
	let result = thumb(Box::new(MemSource(icon)), &spec(64))
		.unwrap()
		.expect("a use/defs sprite must thumbnail");
	assert_eq!(&result.rgba[..4], &[255, 0, 0, 255]);

	// A non-recursive doubling chain: each level copies the previous one
	// twice, so 24 levels of ~40 bytes expand to 2^24 nodes. usvg's only
	// backstop (1M tree nodes) fires hundreds of megabytes too late, so the
	// refusal has to come from the pre-pass — assert on ITS message, or
	// usvg's own late error would satisfy this test without proving anything.
	let mut body = String::from(r#"<g id="g0"><rect width="8" height="8"/></g>"#);
	for level in 1..24 {
		let prev = level - 1;
		body.push_str(&format!(
			r##"<g id="g{level}"><use href="#g{prev}"/><use href="#g{prev}"/></g>"##
		));
	}
	body.push_str(r##"<use href="#g23"/>"##);
	let bomb = svg_doc(r#"width="100" height="100""#, &body);
	let err = thumb(Box::new(MemSource(bomb)), &spec(64)).expect_err("a use bomb must be refused");
	assert!(
		err.to_string().contains("expansion past the node budget"),
		"expected the pre-pass, got {err}"
	);

	// A cycle has no fixed point at all.
	let loopy = svg_doc(
		r#"width="100" height="100""#,
		r##"<g id="a"><g id="b"><use href="#a"/></g></g><use href="#b"/>"##,
	);
	let err =
		thumb(Box::new(MemSource(loopy)), &spec(64)).expect_err("recursive use must be refused");
	assert!(
		err.to_string().contains("recursive <use>"),
		"expected the cycle guard, got {err}"
	);
}

/// Every reference edge but `<use>`'s href used to reach usvg unguarded, and
/// usvg's own defence only reaches self-links and 2-cycles.
#[cfg(feature = "svg")]
#[test]
fn svg_reference_cycles_are_refused_before_usvg_sees_them() {
	// Three masks in a ring: 285 bytes, and usvg's `fix_recursive_links` —
	// which walks a node's descendants plus one hop — does not see it. This
	// died with `fatal runtime error: stack overflow`, an ABORT rather than a
	// catchable panic, so a regression here takes the test binary with it
	// exactly the way it takes an iOS file-provider extension.
	let cycle = |element: &str, attr: &str, body: &str| {
		let mut defs = String::new();
		for (id, next) in [("a", "b"), ("b", "c"), ("c", "a")] {
			defs.push_str(&format!(
				r##"<{element} id="{id}" {attr}="url(#{next})">{body}</{element}>"##
			));
		}
		svg_doc(
			r#"width="100" height="100""#,
			&format!(r##"<defs>{defs}</defs><rect width="99" height="99" {attr}="url(#a)"/>"##),
		)
	};
	for (element, attr, body) in [
		(
			"mask",
			"mask",
			r##"<rect width="99" height="99" fill="#fff"/>"##,
		),
		("clipPath", "clip-path", r#"<rect width="99" height="99"/>"#),
	] {
		let err = thumb(Box::new(MemSource(cycle(element, attr, body))), &spec(64))
			.expect_err("a three-element reference cycle must be refused");
		assert!(
			err.to_string().contains("recursive reference"),
			"expected the reference-cycle guard, got {err}"
		);
	}

	// Paint servers chain through `href`, and usvg's `HrefIter` rejects only
	// the first and current node — a cycle among the ones between spins at
	// 100% CPU and never returns.
	let gradients = svg_doc(
		r#"xmlns:xlink="http://www.w3.org/1999/xlink" width="100" height="100""#,
		r##"<defs><linearGradient id="a" xlink:href="#b"/><linearGradient id="b" xlink:href="#c"/>
		<linearGradient id="c" xlink:href="#b"/></defs>
		<rect width="99" height="99" fill="url(#a)"/>"##,
	);
	let err = thumb(Box::new(MemSource(gradients)), &spec(64))
		.expect_err("a gradient href cycle must be refused");
	assert!(
		err.to_string().contains("recursive reference"),
		"expected the reference-cycle guard, got {err}"
	);

	// The same ring written as CSS aborts identically, and no walk of the
	// attributes can see it — usvg applies the stylesheet before anything
	// here does.
	let styled = svg_doc(
		r#"width="100" height="100""#,
		r##"<style>#a{mask:url(#b)}#b{mask:url(#c)}#c{mask:url(#a)}</style>
		<defs><mask id="a"><rect width="99" height="99" fill="#fff"/></mask>
		<mask id="b"><rect width="99" height="99" fill="#fff"/></mask>
		<mask id="c"><rect width="99" height="99" fill="#fff"/></mask></defs>
		<rect width="99" height="99" mask="url(#a)"/>"##,
	);
	let err = thumb(Box::new(MemSource(styled)), &spec(64))
		.expect_err("a stylesheet-declared mask cycle must be refused");
	assert!(
		err.to_string().contains("stylesheet"),
		"expected the stylesheet guard, got {err}"
	);

	// No cycle at all, and just as fatal: usvg resolves each link recursively,
	// so a long CHAIN is a stack depth. 5000 links aborted; the threshold on
	// the 2 MiB stack a decode thread gets sits between 400 and 800.
	let mut defs = String::new();
	for level in 0..600 {
		defs.push_str(&format!(
			r##"<mask id="m{level}"><rect width="9" height="9" fill="#fff"
			 mask="url(#m{})"/></mask>"##,
			level + 1
		));
	}
	defs.push_str(r##"<mask id="m600"><rect width="9" height="9" fill="#fff"/></mask>"##);
	let chain = svg_doc(
		r#"width="100" height="100""#,
		&format!(r##"<defs>{defs}</defs><rect width="99" height="99" mask="url(#m0)"/>"##),
	);
	let err = thumb(Box::new(MemSource(chain)), &spec(64))
		.expect_err("a 600-link mask chain must be refused");
	assert!(
		err.to_string().contains("chained deeper"),
		"expected the chain-depth guard, got {err}"
	);

	// And the shape every export in the world has — a full-canvas clip, a
	// mask, a gradient that inherits from another gradient through `href`, and
	// a `<use>` — is not a cycle and must still thumbnail.
	let export = svg_doc(
		r#"xmlns:xlink="http://www.w3.org/1999/xlink" width="200" height="200"
		 viewBox="0 0 200 200""#,
		r##"<g clip-path="url(#clip0)"><rect width="200" height="200" fill="url(#paint0)"/>
		<g mask="url(#mask0)"><path d="M10 10L190 190" stroke="url(#paint1)"/></g>
		<use xlink:href="#icon" x="20"/></g>
		<defs><g id="icon"><circle cx="10" cy="10" r="8" fill="red"/></g>
		<clipPath id="clip0"><rect width="200" height="200"/></clipPath>
		<mask id="mask0"><rect width="200" height="200" fill="#fff"/></mask>
		<linearGradient id="paint0"><stop stop-color="#f00"/><stop offset="1" stop-color="#00f"/>
		</linearGradient>
		<linearGradient id="paint1" xlink:href="#paint0" x1="0" y1="0" x2="1" y2="1"/></defs>"##,
	);
	thumb(Box::new(MemSource(export)), &spec(256))
		.unwrap()
		.expect("an ordinary export must thumbnail");

	// A stylesheet naming a GRADIENT is the common case and stays untouched:
	// gradients carry no mask or clip-path to recurse through.
	let css_paint = svg_doc(
		r#"width="200" height="200""#,
		r##"<style>.a{fill:url(#g);stroke:url(#g)}</style>
		<defs><linearGradient id="g"><stop stop-color="#0f0"/>
		<stop offset="1" stop-color="#00f"/></linearGradient></defs>
		<rect class="a" width="200" height="200"/>"##,
	);
	thumb(Box::new(MemSource(css_paint)), &spec(64))
		.unwrap()
		.expect("a stylesheet naming a gradient must thumbnail");
}

/// A percentage resolves against the viewport of the `<svg>` it sits in, and
/// usvg swaps that for every NESTED `<svg>` — so the pre-pass reading only the
/// root's handed the radius guard a viewport 34 orders of magnitude too small.
#[cfg(feature = "svg")]
#[test]
fn svg_nested_viewports_cannot_smuggle_a_giant_radius() {
	// 120 bytes, and the identical viewBox on the ROOT was already refused.
	let smuggled = svg_doc(
		r#"width="100" height="100""#,
		r#"<svg viewBox="0 0 1e36 1e36"><ellipse rx="50%" ry="50%"/></svg>"#,
	);
	let err = thumb(Box::new(MemSource(smuggled)), &spec(64))
		.expect_err("a nested viewBox must not smuggle a radius past the guard");
	assert!(
		err.to_string().contains("shape radius"),
		"expected the arc-vertex guard, got {err}"
	);

	// `width`/`height` on the nested `<svg>` is the same viewport by another
	// spelling.
	let sized = svg_doc(
		r#"width="100" height="100""#,
		r#"<svg width="1e34" height="1e34"><ellipse rx="50%" ry="50%"/></svg>"#,
	);
	assert!(
		thumb(Box::new(MemSource(sized)), &spec(64)).is_err(),
		"a nested width/height must not smuggle a radius past the guard"
	);

	// An ordinary nested viewport — the idiom for placing a sub-drawing —
	// still thumbnails.
	let ordinary = svg_doc(
		r#"width="200" height="200""#,
		r##"<svg x="10" y="10" width="100" height="100" viewBox="0 0 50 50">
		<ellipse cx="25" cy="25" rx="50%" ry="40%" fill="orange"/></svg>
		<rect width="20" height="20" fill="green"/>"##,
	);
	thumb(Box::new(MemSource(ordinary)), &spec(64))
		.unwrap()
		.expect("an ordinary nested viewport must thumbnail");
}

/// usvg copies the RESOLVED filter chain into every group it applies to, so a
/// `<use>` fan-out multiplies the primitives the byte scan counted once.
#[cfg(feature = "svg")]
#[test]
fn svg_filter_chains_are_charged_per_copy() {
	let primitives: String = (0..64)
		.map(|i| format!(r#"<feGaussianBlur stdDeviation="{}"/>"#, 1 + i % 3))
		.collect();

	// 130 KB of document: 8000 copies of one filtered group. It returned a
	// thumbnail, at a 164 MiB peak — and raising the target until the
	// post-parse layer walk fired refused it only after peaking at 40 MiB,
	// because that walk runs on the PARSED tree. Only the pre-pass is early
	// enough.
	for placement in ["filtered group", "filter on the use"] {
		let (on_group, on_use) = match placement {
			"filtered group" => (r#" filter="url(#f)""#, ""),
			_ => ("", r#" filter="url(#f)""#),
		};
		let body = format!(
			r##"<defs><filter id="f">{primitives}</filter>
			<g id="p"{on_group}><rect width="9" height="9" fill="red"/></g></defs>{}"##,
			format!(r##"<use href="#p"{on_use}/>"##).repeat(8000)
		);
		let bomb = svg_doc(r#"width="100" height="100""#, &body);
		let err = thumb(Box::new(MemSource(bomb)), &spec(64))
			.expect_err("a filter fan-out must be refused");
		assert!(
			err.to_string().contains("expansion past the node budget"),
			"expected the pre-pass ({placement}), got {err}"
		);
	}

	// A drop shadow on fifty shapes is what filters are for, as an attribute
	// and as a stylesheet rule.
	let shadow = r#"<feDropShadow dx="2" dy="2" stdDeviation="2"/><feGaussianBlur
		stdDeviation="1"/><feOffset dx="1"/><feMerge><feMergeNode/></feMerge>"#;
	let rects = |attrs: &str| {
		(0..50)
			.map(|i: u32| {
				format!(
					r#"<rect x="{}" y="{}" width="18" height="18" fill="teal"{attrs}/>"#,
					i % 10 * 20,
					i / 10 * 20
				)
			})
			.collect::<String>()
	};
	for (what, style, attrs) in [
		("attribute", "", r#" filter="url(#f)""#),
		("stylesheet", r#"<style>rect{filter:url(#f)}</style>"#, ""),
	] {
		let ordinary = svg_doc(
			r#"width="200" height="200""#,
			&format!(
				r##"{style}<defs><filter id="f">{shadow}</filter></defs>{}"##,
				rects(attrs)
			),
		);
		thumb(Box::new(MemSource(ordinary)), &spec(64))
			.unwrap()
			.unwrap_or_else(|| panic!("fifty drop shadows ({what}) must thumbnail"));
	}
}

#[cfg(feature = "svg")]
#[test]
fn svg_marker_bombs_are_refused_and_arrow_diagrams_are_not() {
	// usvg copies a marker's subtree once per path vertex, DURING the parse,
	// with no cap — a sub-megabyte document peaked at a quarter of a gigabyte.
	// With `overflow="visible"` on the marker the post-parse layer walk does
	// not even fire, so the pre-pass is the only thing standing there.
	for marker_attrs in ["", r#" overflow="visible""#] {
		let mut d = String::from("M0,0");
		for i in 0..50_000u32 {
			d.push_str(&format!("L{},{}", 10 + i % 80, 10 + i % 70));
		}
		let bomb = svg_doc(
			r#"width="100" height="100""#,
			&format!(
				r##"<defs><marker id="m" markerWidth="9" markerHeight="9"{marker_attrs}>
				<circle cx="4" cy="4" r="4" fill="red"/></marker></defs>
				<path d="{d}" fill="none" stroke="black" marker-mid="url(#m)"/>"##
			),
		);
		let err =
			thumb(Box::new(MemSource(bomb)), &spec(64)).expect_err("a marker bomb must be refused");
		assert!(
			err.to_string().contains("expansion past the node budget"),
			"expected the pre-pass, got {err}"
		);
	}

	// The whole point of markers: a five-vertex arrow diagram still renders.
	// (usvg wraps every marker copy in a clipped group, so this also pins the
	// layer allowance being charged honestly rather than by keyword.)
	let arrows = svg_doc(
		r#"width="100" height="100""#,
		r##"<defs><marker id="a" markerWidth="6" markerHeight="6" refX="3" refY="3" orient="auto">
		<path d="M0,0 L6,3 L0,6 z" fill="red"/></marker></defs>
		<path d="M10,10 L30,30 L50,10 L70,30 L90,10" fill="none" stroke="red" stroke-width="3"
		 marker-start="url(#a)" marker-mid="url(#a)" marker-end="url(#a)"/>"##,
	);
	let result = thumb(Box::new(MemSource(arrows)), &spec(64))
		.unwrap()
		.expect("an arrow diagram must thumbnail");
	assert!(
		mean_channel(&result.rgba, 0) > 0,
		"the arrows rendered as nothing"
	);
}

/// A `<use>` doubling chain — 24 levels, 2^24 nodes — whose links are spelled
/// by `tmpl`. Only the spelling varies; every one of these resolves for usvg.
#[cfg(feature = "svg")]
fn svg_use_chain(tmpl: &str) -> Vec<u8> {
	let mut body = String::from(r#"<g id="tiny"/><g id="g0"><rect width="8" height="8"/></g>"#);
	for level in 1..24 {
		let link = tmpl.replace("{PREV}", &format!("g{}", level - 1));
		body.push_str(&format!(r#"<g id="g{level}">{link}{link}</g>"#));
	}
	body.push_str(&tmpl.replace("{PREV}", "g23"));
	svg_doc(
		r#"xmlns:xlink="http://www.w3.org/1999/xlink" xmlns:z="http://example.com/z"
		 width="100" height="100""#,
		&body,
	)
}

#[cfg(feature = "svg")]
#[test]
fn svg_use_links_are_resolved_the_way_usvg_resolves_them() {
	// roxmltree matches an attribute by LOCAL name and answers whichever comes
	// first, and `strip_prefix('#')` is not what `svgtypes::IRI` does. Each of
	// these four spellings resolves for usvg while the pre-pass saw no link at
	// all: 1.4 KB of document, 91 MiB of expansion, and only usvg's own
	// (far too late) node limit to end it. Pin the PRE-PASS message — usvg's
	// backstop would satisfy an `is_err` without proving anything.
	for tmpl in [
		// An xlink decoy in front of the real, unprefixed href.
		r##"<use xlink:href="#tiny" href="#{PREV}"/>"##,
		// The same trick with a foreign namespace, which usvg drops entirely.
		r##"<use z:href="#tiny" href="#{PREV}"/>"##,
		// `IRI` skips leading whitespace; XML normalisation turns the newline
		// of a pretty-printed sprite into exactly this space.
		r##"<use href=" #{PREV}"/>"##,
		// And the same space spelled as a character reference.
		r##"<use href="&#32;#{PREV}"/>"##,
		// The plain spelling, as a control: it was already costed.
		r##"<use href="#{PREV}"/>"##,
	] {
		let err = thumb(Box::new(MemSource(svg_use_chain(tmpl))), &spec(64))
			.expect_err("a use bomb must be refused however its href is spelled");
		assert!(
			err.to_string().contains("expansion past the node budget"),
			"expected the pre-pass for {tmpl}, got {err}"
		);
	}

	// The flip side: those spellings are how real sprites are written, and
	// they still have to resolve to a picture.
	let sprite = svg_doc(
		r#"xmlns:xlink="http://www.w3.org/1999/xlink" width="100" height="100""#,
		r##"<defs><g id="i"><rect width="100" height="100" fill="red"/></g></defs>
		<use xlink:href="#i"/><use href=" #i"/>"##,
	);
	let result = thumb(Box::new(MemSource(sprite)), &spec(64))
		.unwrap()
		.expect("an xlink/whitespace sprite must thumbnail");
	assert_eq!(&result.rgba[..4], &[255, 0, 0, 255]);
}

#[cfg(feature = "svg")]
#[test]
fn svg_marker_copies_are_costed_per_copy_and_per_shape() {
	// (a) usvg re-runs marker expansion for every `<use>` copy of a path, so
	// the cost is the PRODUCT of the fan-out and the vertices — charging the
	// vertices once per source element missed 168 MiB of it.
	let mut d = String::from("M0,0");
	for i in 0..2000u32 {
		d.push_str(&format!("L{},{}", 1 + i % 9, 1 + i % 7));
	}
	let mut body = String::from(
		r##"<defs><marker id="m" markerWidth="9" markerHeight="9" overflow="visible">
		<circle cx="4" cy="4" r="4" fill="red"/></marker></defs>"##,
	);
	body.push_str(&format!(
		r##"<g id="p"><path d="{d}" fill="none" stroke="black" marker-mid="url(#m)"/></g>"##
	));
	body.push_str(&r##"<use href="#p"/>"##.repeat(100));
	let err = thumb(
		Box::new(MemSource(svg_doc(r#"width="100" height="100""#, &body))),
		&spec(64),
	)
	.expect_err("marker copies per <use> copy must be costed");
	assert!(
		err.to_string().contains("expansion past the node budget"),
		"expected the pre-pass, got {err}"
	);

	// (b) usvg blocks only a marker that is its OWN ancestor, so a marker
	// whose path carries a SECOND marker is copied once per vertex per vertex
	// — 4 KB of document, 169 MiB, perfectly quadratic. There is no fixed
	// point to solve for, so the document is refused.
	let mut inner = String::from("M0,0");
	for i in 0..450u32 {
		inner.push_str(&format!("L{},{}", 1 + i % 9, 1 + i % 7));
	}
	let chain = format!(
		r##"<defs>
		<marker id="m2" markerWidth="9" markerHeight="9" overflow="visible">
		<circle cx="1" cy="1" r="1" fill="red"/></marker>
		<marker id="m1" markerWidth="9" markerHeight="9" overflow="visible">
		<path d="{inner}" fill="none" stroke="black" marker-mid="url(#m2)"/></marker></defs>
		<path d="{inner}" fill="none" stroke="black" marker-mid="url(#m1)"/>"##
	);
	let err = thumb(
		Box::new(MemSource(svg_doc(r#"width="100" height="100""#, &chain))),
		&spec(64),
	)
	.expect_err("a marker chain must be refused");
	assert!(
		err.to_string().contains("can itself carry a marker"),
		"expected the chain guard, got {err}"
	);

	// (c) markers are not a path-only affair: usvg routes `<rect>`, `<circle>`
	// and `<ellipse>` through the same conversion, and a marker inherited from
	// an ancestor group reaches every one of them. Charging those zero let
	// 120 KB of circles expand to 70 MiB.
	for (count, shape) in [
		(12_000, r#"<circle r="9"/>"#),
		(5_000, r#"<rect width="9" height="9" rx="2"/>"#),
		(5_000, r#"<ellipse rx="9" ry="5"/>"#),
	] {
		let mut body = String::from(
			r##"<defs><marker id="m" markerWidth="9" markerHeight="9" overflow="visible">
			<circle cx="4" cy="4" r="4" fill="red"/></marker></defs>
			<g marker-mid="url(#m)" marker-start="url(#m)" marker-end="url(#m)" stroke="black">"##,
		);
		body.push_str(&shape.repeat(count));
		body.push_str("</g>");
		let err = thumb(
			Box::new(MemSource(svg_doc(r#"width="100" height="100""#, &body))),
			&spec(64),
		)
		.expect_err("a built-shape marker bomb must be refused");
		assert!(
			err.to_string().contains("expansion past the node budget"),
			"expected the pre-pass for {shape}, got {err}"
		);
	}

	// (d) and one `<circle>` is enough on its own: kurbo subdivides an arc by
	// its RADIUS, so `r="1e30"` — four characters — flattens into millions of
	// segments, with or without a marker to copy onto them.
	for body in [
		r#"<circle r="1e30" stroke="black" fill="none"/>"#,
		r##"<defs><marker id="m" markerWidth="9" markerHeight="9"><circle r="1"/></marker></defs>
		<circle r="1e30" stroke="black" fill="none" marker-mid="url(#m)"/>"##,
	] {
		let err = thumb(
			Box::new(MemSource(svg_doc(r#"width="100" height="100""#, body))),
			&spec(64),
		)
		.expect_err("an absurd radius must be refused");
		assert!(
			err.to_string().contains("shape radius"),
			"expected the radius guard, got {err}"
		);
	}

	// (e) the same arc, struck from a `<path>` instead of a built shape.
	// usvg hands `d`'s `A` commands to the SAME kurbo flattener, so charging
	// a path only its digit runs let an eleven-character arc buy millions of
	// segments. The chord has to scale with the radius or f64 rounding
	// collapses the arc to nothing, which is why the naive spelling looks
	// harmless.
	for body in [
		r#"<path d="M0,0A1e30,1e30 0 1 1 1e25,1e25" fill="none" stroke="black"/>"#,
		r#"<path d="m0,0a1e30,1e30 0 1 1 1e25,1e25" fill="none" stroke="black"/>"#,
	] {
		let err = thumb(
			Box::new(MemSource(svg_doc(r#"width="100" height="100""#, body))),
			&spec(64),
		)
		.expect_err("an absurd path arc must be refused");
		assert!(
			err.to_string().contains("shape radius"),
			"expected the radius guard for a path arc, got {err}"
		);
	}

	// (f) a radius whose flattened vertex count saturates the counter. The
	// count is only ever compared against a budget, so it must saturate
	// rather than wrap: wrapping to a small number turned the guard above
	// into a no-op, and the intermediate `+` panicked in debug builds.
	for body in [
		r#"<circle r="1e307" stroke="black" fill="none"/>"#,
		r#"<ellipse rx="1e307" ry="1e307" stroke="black" fill="none"/>"#,
		r#"<rect width="9" height="9" rx="1e307" stroke="black" fill="none"/>"#,
		r#"<path d="M0,0A1e307,1e307 0 1 1 1e300,1e300" fill="none" stroke="black"/>"#,
	] {
		let err = thumb(
			Box::new(MemSource(svg_doc(r#"width="100" height="100""#, body))),
			&spec(64),
		)
		.expect_err("a saturating radius must be refused, not wrapped past the guard");
		assert!(
			err.to_string().contains("shape radius"),
			"expected the radius guard, got {err}"
		);
	}

	// (g) and none of that may cost a real icon its thumbnail. A detailed
	// path carries far more than MAX_SHAPE_ARC_VERTICES coordinates, which is
	// why only the ARC half of the charge may be measured against that cap —
	// and a minified `d` runs its numbers together (`10-5` is two numbers,
	// `1.2.3` is two more), which the scanner has to read as written rather
	// than as one unparseable token.
	let mut detailed = String::from(r#"<path fill="none" stroke="black" d="M0,0"#);
	for n in 0..3000 {
		detailed.push_str(&format!("l{}-{}", n % 7 + 1, n % 5 + 1));
	}
	detailed.push_str(r#"a10,10 0 1 1 5,5l1.2.3z"/>"#);
	let result = thumb(
		Box::new(MemSource(svg_doc(r#"width="100" height="100""#, &detailed))),
		&spec(64),
	)
	.unwrap()
	.expect("a detailed icon path with an ordinary arc must still thumbnail");
	assert!(result.width > 0 && result.height > 0);

	// None of which may cost an ordinary diagram its thumbnail: markers on a
	// path, a percentage-sized background rect and a plain circle together.
	let benign = svg_doc(
		r#"width="100" height="100""#,
		r##"<defs><marker id="a" markerWidth="6" markerHeight="6" orient="auto">
		<path d="M0,0 L6,3 L0,6 z" fill="red"/></marker></defs>
		<rect width="100%" height="100%" fill="white"/>
		<circle cx="50" cy="50" r="20" fill="none" stroke="green"/>
		<path d="M10,10 L30,30 L50,10" fill="none" stroke="red" stroke-width="3"
		 marker-start="url(#a)" marker-mid="url(#a)" marker-end="url(#a)"/>"##,
	);
	let result = thumb(Box::new(MemSource(benign)), &spec(64))
		.unwrap()
		.expect("an ordinary marker diagram must thumbnail");
	// The background alone is white, so a green mean above zero proves
	// nothing — assert on what only the drawn elements can produce: the red
	// marker/stroke pulls the red mean clear of the green one.
	assert!(
		mean_channel(&result.rgba, 0) > mean_channel(&result.rgba, 1),
		"the diagram rendered as a bare background"
	);
}

#[cfg(feature = "svg")]
#[test]
fn svg_dash_bombs_are_refused_and_dashed_borders_are_not() {
	// tiny-skia gives up past a million dash segments per path — but it
	// BUILDS up to that many first, and these ~200-byte documents get there:
	// ~125 MB, and then a thumbnail as if nothing had happened. Both spellings
	// reach the same stroke.
	for dash in [
		r#" stroke-dasharray="0.001 0.001""#,
		r#" style="stroke-dasharray:0.001 0.001""#,
	] {
		let bomb = svg_doc(
			r#"width="100" height="100""#,
			&format!(r#"<path d="M0 0 L1000000 0" stroke="black" stroke-width="1"{dash}/>"#),
		);
		let err =
			thumb(Box::new(MemSource(bomb)), &spec(64)).expect_err("a dash bomb must be refused");
		assert!(
			err.to_string().contains("stroke-dasharray"),
			"expected the dash guard, got {err}"
		);
	}

	// A dashed border is a few dozen dashes and has to keep working.
	let border = svg_doc(
		r#"width="100" height="100""#,
		r#"<rect x="5" y="5" width="90" height="90" fill="none" stroke="red" stroke-width="6"
		 stroke-dasharray="6 4"/>"#,
	);
	let result = thumb(Box::new(MemSource(border)), &spec(64))
		.unwrap()
		.expect("a dashed border must thumbnail");
	assert!(
		mean_channel(&result.rgba, 0) > 0,
		"the dashed border rendered as nothing"
	);
}

#[cfg(feature = "svg")]
#[test]
fn svg_ordinary_layered_documents_are_not_refused() {
	// A full-canvas clipPath is in essentially every Figma/Illustrator/
	// Inkscape export, and it costs three pixmaps — layer, clip pixmap, mask.
	// It used to be refused outright whenever the text happened not to
	// mention an isolation keyword, and charged against a two-pixmap
	// allowance when it did.
	let clipped = svg_doc(
		r#"width="100" height="100""#,
		r##"<defs><clipPath id="c"><rect width="100" height="100"/></clipPath></defs>
		<g clip-path="url(#c)"><rect width="100" height="100" fill="red"/></g>"##,
	);
	let result = thumb(Box::new(MemSource(clipped)), &spec(64))
		.unwrap()
		.expect("a clipPath document must thumbnail");
	assert_eq!(&result.rgba[..4], &[255, 0, 0, 255]);

	// Three nested full-canvas opacity groups: three concurrent layers.
	let mut body = r#"<g opacity="0.8">"#.repeat(3);
	body.push_str(r#"<rect width="100" height="100" fill="red"/>"#);
	body.push_str(&"</g>".repeat(3));
	let nested = svg_doc(r#"width="100" height="100""#, &body);
	let result = thumb(Box::new(MemSource(nested)), &spec(64))
		.unwrap()
		.expect("three nested opacity groups must thumbnail");
	assert!(mean_channel(&result.rgba, 0) > 100);

	// And the verdict no longer turns on a word appearing in the text: the
	// same flat document decodes identically with and without `opacity`
	// written into a <desc>.
	let plain = svg_doc(
		r#"width="100" height="100""#,
		r#"<rect width="100" height="100" fill="red"/>"#,
	);
	let with_word = svg_doc(
		r#"width="100" height="100""#,
		r#"<desc>exported at full opacity</desc><rect width="100" height="100" fill="red"/>"#,
	);
	assert_eq!(
		thumb(Box::new(MemSource(plain)), &spec(64)).unwrap(),
		thumb(Box::new(MemSource(with_word)), &spec(64)).unwrap(),
		"a word in a <desc> changed the decode"
	);
}

#[cfg(feature = "svg")]
#[test]
fn svg_deep_nesting_is_refused_before_anything_parses_it() {
	// The XML layer under usvg recurses per level and overflows a 2 MiB
	// worker stack (an abort, not a panic) long before usvg's own depth cap
	// can answer — so the byte scan has to refuse first, and cheaply.
	for depth in [4000usize, 20_000] {
		let mut body = "<g>".repeat(depth);
		body.push_str(r#"<rect width="10" height="10" fill="red"/>"#);
		body.push_str(&"</g>".repeat(depth));
		let bytes = svg_doc(r#"width="100" height="100""#, &body);
		let err = thumb(Box::new(MemSource(bytes)), &spec(64))
			.expect_err("a {depth}-deep nest must be refused");
		assert!(
			err.to_string().contains("nesting"),
			"expected the depth guard, got {err}"
		);
	}
	// Right up against the cap still renders, ON THE STACK THE DECODE REALLY
	// GETS: 2 MiB is what a `spawn_blocking` worker (the mobile cache's decode
	// thread) has, and the cap is only honest if the deepest document it lets
	// through survives there. Test threads get 8 MiB, which would hide it.
	let mut body = "<g>".repeat(1022);
	body.push_str(r#"<rect width="100" height="100" fill="red"/>"#);
	body.push_str(&"</g>".repeat(1022));
	let bytes = svg_doc(r#"width="100" height="100""#, &body);
	let rgba = std::thread::Builder::new()
		.stack_size(2 * 1024 * 1024)
		.spawn(move || {
			thumb(Box::new(MemSource(bytes)), &spec(64))
				.unwrap()
				.expect("a nest at the cap is within what usvg accepts")
				.rgba
		})
		.unwrap()
		.join()
		.unwrap();
	assert_eq!(&rgba[..4], &[255, 0, 0, 255]);
}

#[cfg(feature = "svg")]
#[test]
fn svg_sizes_with_multi_byte_units_do_not_panic() {
	// `rfind`-based unit splitting landed mid-char on these and panicked the
	// slice — which on wasm is a module-killing trap. Each is unparseable, so
	// the aspect falls through to the viewBox, then to square.
	for attrs in [
		r#"width="1é" height="5µ" viewBox="0 0 100 400""#,
		r#"width="١٠" height="١٠" viewBox="0 0 100 400""#,
		r#"width="10é""#,
	] {
		let bytes = svg_doc(attrs, r#"<rect width="100" height="400" fill="lime"/>"#);
		let result = thumb(Box::new(MemSource(bytes)), &spec(64))
			.unwrap()
			.expect("an unparseable size must fall through, not panic");
		assert!(result.width >= 1 && result.height >= 1);
	}
	// The viewBox really is what decided the first two (1:4, not square).
	let bytes = svg_doc(
		r#"width="1é" height="5µ" viewBox="0 0 100 400""#,
		r#"<rect width="100" height="400" fill="lime"/>"#,
	);
	let result = thumb(Box::new(MemSource(bytes)), &spec(100))
		.unwrap()
		.expect("viewBox svg must thumbnail");
	let ratio = f64::from(result.height) / f64::from(result.width);
	assert!(
		(3.8..=4.2).contains(&ratio),
		"got {}x{}",
		result.width,
		result.height
	);
}

#[cfg(feature = "svg")]
#[test]
fn svg_filter_primitives_are_counted_once_per_element() {
	// Non-self-closing primitives used to be counted twice (the closing tag
	// matched the `fe` prefix too), so 40 real primitives read as 80 and
	// tripped the cap of 64.
	let mut filters = String::from(r#"<defs><filter id="f">"#);
	for _ in 0..40 {
		filters.push_str(r#"<feGaussianBlur stdDeviation="2"></feGaussianBlur>"#);
	}
	filters.push_str("</filter></defs>");
	let bytes = svg_doc(r#"width="100" height="100""#, &filters);
	assert!(
		thumb(Box::new(MemSource(bytes)), &spec(64))
			.unwrap()
			.is_some(),
		"40 primitives are under the cap"
	);

	// Past the cap it is still refused, paired tags or not.
	let mut filters = String::from(r#"<defs><filter id="f">"#);
	for _ in 0..70 {
		filters.push_str(r#"<feGaussianBlur stdDeviation="2"></feGaussianBlur>"#);
	}
	filters.push_str("</filter></defs>");
	let bytes = svg_doc(r#"width="100" height="100""#, &filters);
	let err = thumb(Box::new(MemSource(bytes)), &spec(64)).expect_err("70 primitives are refused");
	assert!(
		err.to_string().contains("filter primitives"),
		"expected the primitive cap, got {err}"
	);
}

#[cfg(feature = "svg")]
#[test]
fn svg_never_resolves_external_or_embedded_images() {
	// A real red PNG on disk, referenced by absolute path: usvg's DEFAULT
	// string resolver would read it — ours must not, so the render is the
	// blue background alone. (Belt and braces: raster-images is off too.)
	let png = encode(
		&RgbImage::from_pixel(8, 8, Rgb([255, 0, 0])),
		ImageFormat::Png,
	);
	let path = std::env::temp_dir().join("microthumb-svg-external-ref-test.png");
	std::fs::write(&path, &png).unwrap();
	let body = format!(
		r#"<rect width="100" height="100" fill="blue"/><image href="{}" width="100" height="100"/>"#,
		path.display()
	);
	let bytes = svg_doc(r#"width="100" height="100""#, &body);
	let result = thumb(Box::new(MemSource(bytes)), &spec(64))
		.unwrap()
		.expect("svg with an unresolvable image must still thumbnail");
	std::fs::remove_file(&path).ok();
	assert!(
		mean_channel(&result.rgba, 0) < 10,
		"the external file leaked into the render"
	);
	assert!(mean_channel(&result.rgba, 2) > 245);

	// Same for an embedded data URI (a valid red 1×1 PNG, base64).
	let body = r#"<rect width="100" height="100" fill="blue"/>
		<image href="data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==" width="100" height="100"/>"#;
	let bytes = svg_doc(r#"width="100" height="100""#, body);
	let result = thumb(Box::new(MemSource(bytes)), &spec(64))
		.unwrap()
		.expect("svg with a data uri must still thumbnail");
	assert!(
		mean_channel(&result.rgba, 0) < 10,
		"the data uri leaked into the render"
	);
}

#[cfg(feature = "svg")]
#[test]
fn svg_text_renders_as_nothing() {
	// No font database ships in this build; text elements drop silently
	// rather than pulling system fonts into an untrusted decode.
	let bytes = svg_doc(
		r#"width="100" height="100""#,
		r#"<rect width="100" height="100" fill="blue"/><text x="10" y="50" font-size="40" fill="red">HELLO</text>"#,
	);
	let result = thumb(Box::new(MemSource(bytes)), &spec(64))
		.unwrap()
		.expect("svg with text must thumbnail");
	assert!(
		mean_channel(&result.rgba, 0) < 10,
		"text was rendered from somewhere"
	);
}

/// Every design tool exports the same shape: a group clipped to the frame,
/// holding a masked group. Both cover the canvas, so both are charged at
/// canvas size — and the verdict has to be the same at every target, because
/// nothing about the DOCUMENT changed between them. It was not: the allowance
/// was priced in unpadded output pixmaps while `layer_peak` charges the
/// padding resvg rounds each layer out by, an absolute cost that is ~13% of
/// this document at a 64 px target and ~2% at 512 — so this export was
/// refused small and rendered large.
#[cfg(feature = "svg")]
#[test]
fn svg_design_tool_exports_thumbnail_at_every_target() {
	let export = svg_doc(
		r#"width="200" height="200" viewBox="0 0 200 200" fill="none""#,
		r##"<g clip-path="url(#clip0)">
		<rect width="200" height="200" fill="#fff"/>
		<g mask="url(#mask0)"><path d="M10 10L190 190" stroke="#000" stroke-width="4"/></g>
		</g>
		<defs><clipPath id="clip0"><rect width="200" height="200"/></clipPath>
		<mask id="mask0" maskUnits="userSpaceOnUse" x="0" y="0" width="200" height="200">
		<rect width="200" height="200" fill="#fff"/></mask></defs>"##,
	);
	// Both budgets at every shipped target: an `OverBudget` here is the
	// estimate — allowance included — not fitting `mem_budget`. 512 on the
	// 12 MB budget is the tightest of the six at half the budget spent, so it
	// catches an allowance that grew past roughly double.
	for budget in [DEFAULT_MEM_BUDGET, APP_PROCESS_MEM_BUDGET] {
		for target in [64, 128, 512] {
			let spec = ThumbSpec::new(target, target, budget);
			let outcome = generate(Box::new(MemSource(export.clone())), &spec)
				.unwrap_or_else(|e| panic!("an ordinary export must decode at {target}: {e}"));
			assert!(
				matches!(outcome, ThumbOutcome::Thumbnail(_)),
				"an ordinary export must thumbnail at {target} on a {budget}-byte budget, \
				 got {outcome:?}"
			);
		}
	}

	// And the allowance still binds: nested full-canvas opacity groups are
	// concurrent sub-pixmaps, and twenty-four of them are refused at the
	// largest target on the roomiest budget, not merely at the smallest.
	let mut body = String::new();
	for _ in 0..24 {
		body.push_str(r#"<g opacity="0.5">"#);
	}
	body.push_str(r#"<rect width="200" height="200" fill="red"/>"#);
	for _ in 0..24 {
		body.push_str("</g>");
	}
	let stack = svg_doc(r#"width="200" height="200""#, &body);
	let spec = ThumbSpec::new(512, 512, APP_PROCESS_MEM_BUDGET);
	let err = thumb(Box::new(MemSource(stack)), &spec)
		.expect_err("a twenty-four-deep layer stack must still be refused");
	assert!(
		err.to_string().contains("isolated layer stack"),
		"expected the layer allowance, got {err}"
	);
}

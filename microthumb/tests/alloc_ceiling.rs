//! The memory contract, enforced: a counting allocator measures the high-water
//! mark of `thumb()` on large fixtures and asserts it stays inside the
//! budget. On macOS the meter is libmalloc's `malloc_logger` hook, which sees
//! every malloc/realloc/free in the process — including the C side
//! (libheif/libde265), which a Rust `#[global_allocator]` cannot observe.
//! NOT visible to either meter: thread stacks (libde265 spawns workers) and
//! direct mmap/vm_allocate — the numbers here are heap high-water marks, not
//! total process footprint. Elsewhere the global allocator swap meters the
//! pure-Rust formats only. This lives in its own test binary because both
//! meters are process-wide, and runs the cases in ONE #[test] so no parallel
//! test pollutes the counters.

use std::io::Cursor;
use std::sync::atomic::{AtomicUsize, Ordering};

use image::{ImageFormat, Rgb, RgbImage};
use microthumb::{DEFAULT_MEM_BUDGET, FileSource, MemSource, ThumbSpec, generate};

mod raw_fixtures;

/// Most tests care only about the pixels. The ones that care about WHICH path
/// produced them call `generate` directly and inspect `Thumbnail::source`.
fn thumb(
	src: Box<dyn microthumb::ByteSource>,
	spec: &ThumbSpec,
) -> Result<Option<microthumb::SmallImage>, microthumb::ThumbError> {
	Ok(generate(src, spec)?.thumbnail().map(|t| t.image))
}

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn on_alloc(size: usize) {
	let now = CURRENT.fetch_add(size, Ordering::Relaxed) + size;
	PEAK.fetch_max(now, Ordering::Relaxed);
}

#[cfg(not(target_os = "macos"))]
mod meter {
	use std::alloc::{GlobalAlloc, Layout, System};
	use std::sync::atomic::Ordering;

	use super::{CURRENT, on_alloc};

	struct Counting;

	unsafe impl GlobalAlloc for Counting {
		unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
			on_alloc(layout.size());
			unsafe { System.alloc(layout) }
		}

		unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
			CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
			unsafe { System.dealloc(ptr, layout) }
		}

		unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
			if new_size > layout.size() {
				on_alloc(new_size - layout.size());
			} else {
				CURRENT.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
			}
			unsafe { System.realloc(ptr, layout, new_size) }
		}
	}

	#[global_allocator]
	static ALLOC: Counting = Counting;

	pub fn install() {}

	pub fn dropped() -> usize {
		0
	}

	pub fn healed() -> usize {
		0
	}
}

/// libmalloc's logging hook. Installing a `malloc_logger` gets a callback for
/// every malloc/realloc/free across all zones in this process — Rust's
/// `System` allocator funnels through libmalloc too, so this single meter
/// subsumes the global-allocator one AND sees libheif/libde265.
///
/// Free and realloc events do not carry the old block's size, so the callback
/// keeps a fixed-size open-addressing table of live (ptr, size) pairs. The
/// callback runs inside malloc itself and therefore must never allocate: the
/// table is static, insertion is CAS-only, and overflow is counted (never
/// dropped silently) — an untracked block inflates CURRENT forever, which only
/// over-reports the peak, and `dropped()` is asserted zero regardless.
///
/// The table is exact only under exactly-once event delivery. Nightly CI has
/// produced overshoots that are whole multiples of one decode buffer with no
/// repo change touching the path — consistent with the runner's libmalloc
/// duplicating an alloc event or losing a free, either of which strands a live
/// entry that inflates CURRENT permanently. `insert` heals the detectable
/// half (a second alloc for a still-live pointer), and `measured_peak` prints
/// per-window drift so a future failure tells the two apart.
#[cfg(target_os = "macos")]
mod meter {
	use std::sync::Once;
	use std::sync::atomic::{AtomicUsize, Ordering};

	use super::{CURRENT, on_alloc};

	// stack_logging.h: event type bits.
	const TYPE_ALLOC: u32 = 2;
	const TYPE_DEALLOC: u32 = 4;
	const FLAG_ZONE: u32 = 8;

	type Logger = unsafe extern "C" fn(u32, usize, usize, usize, usize, u32);

	unsafe extern "C" {
		static mut malloc_logger: Option<Logger>;
	}

	const SLOTS: usize = 1 << 20;
	const PROBE_LIMIT: usize = 1024;
	const TOMBSTONE: usize = usize::MAX;

	static PTRS: [AtomicUsize; SLOTS] = [const { AtomicUsize::new(0) }; SLOTS];
	static SIZES: [AtomicUsize; SLOTS] = [const { AtomicUsize::new(0) }; SLOTS];
	static DROPPED: AtomicUsize = AtomicUsize::new(0);
	static HEALED: AtomicUsize = AtomicUsize::new(0);

	fn slot(ptr: usize, step: usize) -> usize {
		(((ptr >> 4).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) + step) & (SLOTS - 1)
	}

	/// The malloc callback for `ptr` runs before malloc returns it, so no other
	/// thread can race an insert of the same pointer with its own free.
	fn insert(ptr: usize, size: usize) {
		// A second alloc for a pointer the table still holds live is physically
		// impossible for real memory: the logger duplicated this alloc event, or
		// lost the free that let the address be reused. Both would strand the old
		// entry and inflate CURRENT by its size forever — so replace it and refund
		// the stale bytes. This scan must come before claiming a slot: taking a
		// tombstone that precedes the stale entry would leave both live.
		for step in 0..PROBE_LIMIT {
			let idx = slot(ptr, step);
			let found = PTRS[idx].load(Ordering::Relaxed);
			if found == ptr {
				// Counted, not silent: healing hides the duplicated-event anomaly
				// from CURRENT, so the counter is the only remaining witness that
				// the meter saw one at all.
				HEALED.fetch_add(1, Ordering::Relaxed);
				let stale = SIZES[idx].swap(size, Ordering::AcqRel);
				CURRENT.fetch_sub(stale, Ordering::Relaxed);
				return;
			}
			if found == 0 {
				break;
			}
		}
		for step in 0..PROBE_LIMIT {
			let s = &PTRS[slot(ptr, step)];
			let found = s.load(Ordering::Relaxed);
			if (found == 0 || found == TOMBSTONE)
				&& s.compare_exchange(found, ptr, Ordering::AcqRel, Ordering::Relaxed)
					.is_ok()
			{
				SIZES[slot(ptr, step)].store(size, Ordering::Release);
				return;
			}
		}
		DROPPED.fetch_add(1, Ordering::Relaxed);
	}

	/// Size the pointer was tracked with, or 0 for blocks allocated before the
	/// hook was installed (their free is a no-op on the counters).
	fn remove(ptr: usize) -> usize {
		for step in 0..PROBE_LIMIT {
			let idx = slot(ptr, step);
			let found = PTRS[idx].load(Ordering::Relaxed);
			if found == ptr {
				let size = SIZES[idx].load(Ordering::Acquire);
				PTRS[idx].store(TOMBSTONE, Ordering::Release);
				return size;
			}
			if found == 0 {
				return 0;
			}
		}
		0
	}

	unsafe extern "C" fn logger(ty: u32, p1: usize, p2: usize, p3: usize, result: usize, _n: u32) {
		// With the zone flag, p1 is the zone and the payload shifts to p2/p3.
		let (a, b) = if ty & FLAG_ZONE != 0 {
			(p2, p3)
		} else {
			(p1, p2)
		};
		match ty & (TYPE_ALLOC | TYPE_DEALLOC) {
			t if t == TYPE_ALLOC | TYPE_DEALLOC => {
				// realloc: a = old ptr, b = new size, result = new ptr.
				let old = remove(a);
				if result != 0 {
					insert(result, b);
					on_alloc(b);
				}
				CURRENT.fetch_sub(old, Ordering::Relaxed);
			}
			TYPE_ALLOC => {
				// malloc/calloc/valloc: a = size, result = the block.
				if result != 0 {
					insert(result, a);
					on_alloc(a);
				}
			}
			TYPE_DEALLOC => {
				// free: a = the block.
				let old = remove(a);
				CURRENT.fetch_sub(old, Ordering::Relaxed);
			}
			_ => {}
		}
	}

	pub fn install() {
		static ONCE: Once = Once::new();
		ONCE.call_once(|| unsafe {
			malloc_logger = Some(logger);
		});
	}

	pub fn dropped() -> usize {
		DROPPED.load(Ordering::Relaxed)
	}

	pub fn healed() -> usize {
		HEALED.load(Ordering::Relaxed)
	}
}

/// Peak extra bytes allocated while `f` ran, on top of what was already live.
fn measured_peak<T>(f: impl FnOnce() -> T) -> (T, usize) {
	meter::install();
	let baseline = CURRENT.load(Ordering::Relaxed);
	let healed_before = meter::healed();
	PEAK.store(baseline, Ordering::Relaxed);
	let out = f();
	let peak = PEAK.load(Ordering::Relaxed);
	// By now everything the closure allocated is dead, so CURRENT should sit at
	// the baseline minus whatever baseline-counted bytes the closure consumed
	// (typically the moved-in source Vec). A drift far above that is phantom
	// accounting — a duplicated alloc event or a lost free in the meter — and
	// this line, read next to the case's own peak line, is what tells a real
	// overshoot from a metering artifact on the next CI failure. `healed` is the
	// duplicated-event half's only witness: healing keeps it out of CURRENT.
	let end = CURRENT.load(Ordering::Relaxed);
	eprintln!(
		"  window drift: {} bytes (baseline {baseline}, end {end}, healed {})",
		end as i64 - baseline as i64,
		meter::healed() - healed_before
	);
	assert_eq!(meter::dropped(), 0, "the live-block table overflowed");
	(out, peak.saturating_sub(baseline))
}

fn spec(target: u32) -> ThumbSpec {
	ThumbSpec::new(target, target, DEFAULT_MEM_BUDGET)
}

/// Any real photograph downscales to more than one colour; a decode bug that
/// pushes nothing (or one tile) into the canvas leaves it flat.
#[cfg(feature = "heif")]
fn assert_not_flat(image: &microthumb::SmallImage, what: &str) {
	let first = &image.rgba[..4];
	assert!(
		image.rgba.chunks_exact(4).any(|px| px != first),
		"{what}: decoded image is a single flat colour"
	);
}

#[test]
fn generate_stays_inside_the_budget_for_large_sources() {
	// 24 MP — the class of source the old full-frame pipeline died on
	// (96 MB of RGBA). Fixture encoding happens OUTSIDE the measurement.
	let gradient = RgbImage::from_fn(6000, 4000, |x, y| {
		Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
	});

	let mut jpeg = Vec::new();
	gradient
		.write_to(&mut Cursor::new(&mut jpeg), ImageFormat::Jpeg)
		.unwrap();
	let (result, peak) = measured_peak(|| thumb(Box::new(MemSource(jpeg)), &spec(512)));
	// The input Vec (a few MB) is owned by the source and counted at the
	// baseline of the closure via the move — subtract nothing, just assert
	// the whole thing stays under budget + the moved-in source itself.
	let result = result.unwrap().expect("24 MP baseline jpeg must thumbnail");
	assert!(result.width >= 500 || result.height >= 500);
	eprintln!("24 MP baseline jpeg peak: {peak} bytes");
	assert!(
		peak <= DEFAULT_MEM_BUDGET,
		"24 MP jpeg peaked at {peak} bytes (budget {DEFAULT_MEM_BUDGET})"
	);

	let mut png = Vec::new();
	gradient
		.write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
		.unwrap();
	drop(gradient);
	let (result, peak) = measured_peak(|| thumb(Box::new(MemSource(png)), &spec(512)));
	result.unwrap().expect("24 MP png must thumbnail");
	eprintln!("24 MP png peak: {peak} bytes");
	assert!(
		peak <= DEFAULT_MEM_BUDGET,
		"24 MP png peaked at {peak} bytes (budget {DEFAULT_MEM_BUDGET})"
	);

	// A 60 MP baseline JPEG — beyond any phone sensor — still fits: the IDCT
	// scale keeps the decode output proportional to the target, not the file.
	let big = RgbImage::from_pixel(10000, 6000, Rgb([90, 120, 30]));
	let mut jpeg = Vec::new();
	big.write_to(&mut Cursor::new(&mut jpeg), ImageFormat::Jpeg)
		.unwrap();
	drop(big);
	let (result, peak) = measured_peak(|| thumb(Box::new(MemSource(jpeg)), &spec(512)));
	result.unwrap().expect("60 MP baseline jpeg must thumbnail");
	eprintln!("60 MP baseline jpeg peak: {peak} bytes");
	assert!(
		peak <= DEFAULT_MEM_BUDGET,
		"60 MP jpeg peaked at {peak} bytes (budget {DEFAULT_MEM_BUDGET})"
	);

	// 24 MP PROGRESSIVE — the class the full decode can never fit (a ~72 MB
	// coefficient buffer): served by the DC-scan parser, which also must not
	// read past the DC scans at the head of the file.
	let gradient = RgbImage::from_fn(6000, 4000, |x, y| {
		Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
	});
	let mut jpeg = Vec::new();
	{
		let mut enc = jpeg_encoder::Encoder::new(&mut jpeg, 85);
		enc.set_progressive(true);
		enc.encode(gradient.as_raw(), 6000, 4000, jpeg_encoder::ColorType::Rgb)
			.unwrap();
	}
	drop(gradient);
	let len = jpeg.len() as u64;
	let watermark = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
	let source = CountingSource(MemSource(jpeg), watermark.clone());
	let (result, peak) = measured_peak(|| thumb(Box::new(source), &spec(512)));
	result
		.unwrap()
		.expect("24 MP progressive jpeg must thumbnail via DC scans");
	let read = watermark.load(Ordering::Relaxed);
	eprintln!(
		"24 MP progressive jpeg peak: {peak} bytes; prefix read: {read} of {len} bytes ({:.1}%)",
		read as f64 * 100.0 / len as f64
	);
	assert!(
		peak <= DEFAULT_MEM_BUDGET,
		"24 MP progressive peaked at {peak} bytes (budget {DEFAULT_MEM_BUDGET})"
	);
	assert!(read * 2 < len, "dc path read {read} of {len} bytes");

	// 12 MP GIF — the row streamer's peak is palettes + two rows.
	let mut gif_bytes = Vec::new();
	{
		let mut palette = Vec::new();
		for i in 0..=255u8 {
			palette.extend_from_slice(&[i, i.wrapping_mul(3), i.wrapping_mul(7)]);
		}
		let mut enc = gif::Encoder::new(&mut gif_bytes, 4000, 3000, &palette).unwrap();
		let pixels: Vec<u8> = (0..4000usize * 3000).map(|i| (i % 256) as u8).collect();
		enc.write_frame(&gif::Frame::from_indexed_pixels(4000, 3000, pixels, None))
			.unwrap();
	}
	let (result, peak) = measured_peak(|| thumb(Box::new(MemSource(gif_bytes)), &spec(512)));
	result.unwrap().expect("12 MP gif must thumbnail");
	eprintln!("12 MP gif peak: {peak} bytes");
	assert!(
		peak <= DEFAULT_MEM_BUDGET,
		"12 MP gif peaked at {peak} bytes (budget {DEFAULT_MEM_BUDGET})"
	);

	// 24 MP striped TIFF — peak is one 16-row strip.
	let mut tiff_bytes = Vec::new();
	{
		let data: Vec<u8> = (0..6000usize * 4000 * 3).map(|i| (i % 253) as u8).collect();
		let mut enc = tiff::encoder::TiffEncoder::new(Cursor::new(&mut tiff_bytes)).unwrap();
		let mut img = enc
			.new_image::<tiff::encoder::colortype::RGB8>(6000, 4000)
			.unwrap();
		img.rows_per_strip(16).unwrap();
		img.write_data(&data).unwrap();
	}
	let (result, peak) = measured_peak(|| thumb(Box::new(MemSource(tiff_bytes)), &spec(512)));
	result.unwrap().expect("24 MP striped tiff must thumbnail");
	eprintln!("24 MP tiff peak: {peak} bytes");
	assert!(
		peak <= DEFAULT_MEM_BUDGET,
		"24 MP tiff peaked at {peak} bytes (budget {DEFAULT_MEM_BUDGET})"
	);

	// 24 MP BMP (24bpp, 72 MB of pixel data) — the seek-rows path reads one
	// row at a time; the fixture Vec sits at the baseline, not in the peak.
	let mut bmp_bytes = Vec::with_capacity(6000 * 3 * 4000 + 64);
	bmp_bytes.extend_from_slice(b"BM");
	bmp_bytes.extend_from_slice(&0u32.to_le_bytes());
	bmp_bytes.extend_from_slice(&0u32.to_le_bytes());
	bmp_bytes.extend_from_slice(&54u32.to_le_bytes());
	bmp_bytes.extend_from_slice(&40u32.to_le_bytes());
	bmp_bytes.extend_from_slice(&6000i32.to_le_bytes());
	bmp_bytes.extend_from_slice(&4000i32.to_le_bytes());
	bmp_bytes.extend_from_slice(&1u16.to_le_bytes());
	bmp_bytes.extend_from_slice(&24u16.to_le_bytes());
	bmp_bytes.extend_from_slice(&0u32.to_le_bytes());
	bmp_bytes.extend_from_slice(&0u32.to_le_bytes());
	bmp_bytes.extend_from_slice(&[0u8; 8]);
	bmp_bytes.extend_from_slice(&0u32.to_le_bytes());
	bmp_bytes.extend_from_slice(&0u32.to_le_bytes());
	// 6000*3 = 18000 bytes/row, already 4-aligned; identical rows are fine
	// for a memory test.
	let row: Vec<u8> = (0..6000usize * 3).map(|i| (i % 251) as u8).collect();
	for _ in 0..4000 {
		bmp_bytes.extend_from_slice(&row);
	}
	let (result, peak) = measured_peak(|| thumb(Box::new(MemSource(bmp_bytes)), &spec(512)));
	result.unwrap().expect("24 MP bmp must thumbnail");
	eprintln!("24 MP bmp peak: {peak} bytes");
	assert!(
		peak <= DEFAULT_MEM_BUDGET,
		"24 MP bmp peaked at {peak} bytes (budget {DEFAULT_MEM_BUDGET})"
	);

	// The whole-frame path (`formats::simple`) has no streaming to fall back
	// on: its estimate IS the guard, so what it charges has to stay above what
	// the decode really takes. 7 MP is just under what the browser budget buys
	// at 8 bytes a source pixel; the peak must fit that budget AND stay under
	// the charge, or the estimate is a fiction on the host that has no jetsam
	// to catch it.
	let px = 3200usize * 2200;
	let frame = RgbImage::from_fn(3200, 2200, |x, y| {
		Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
	});
	for format in [ImageFormat::WebP, ImageFormat::Qoi] {
		let mut bytes = Vec::new();
		frame
			.write_to(&mut Cursor::new(&mut bytes), format)
			.unwrap();
		// At a real request the accumulator canvas is a big share of the peak
		// and is budgeted SEPARATELY from the decoder's estimate, so the
		// budget is what this run pins.
		let browser = ThumbSpec::new(512, 512, microthumb::APP_PROCESS_MEM_BUDGET);
		// Cloned OUTSIDE the window: the source bytes belong to the baseline,
		// not to the decode being measured.
		let copy = bytes.clone();
		let (result, peak) = measured_peak(|| thumb(Box::new(MemSource(copy)), &browser));
		result
			.unwrap()
			.unwrap_or_else(|| panic!("7 MP {format:?} must thumbnail inside the browser budget"));
		eprintln!("7 MP {format:?} peak at 512: {peak} bytes");
		assert!(
			peak <= microthumb::APP_PROCESS_MEM_BUDGET,
			"7 MP {format:?} peaked at {peak} bytes (budget {})",
			microthumb::APP_PROCESS_MEM_BUDGET
		);
		// And at a small request, where the canvas is a rounding error, what
		// is left IS the decode — which must stay under what it was charged.
		let (result, peak) = measured_peak(|| {
			thumb(
				Box::new(MemSource(bytes)),
				&ThumbSpec::new(64, 64, microthumb::APP_PROCESS_MEM_BUDGET),
			)
		});
		result.unwrap().expect("7 MP must thumbnail at 64 px too");
		eprintln!(
			"7 MP {format:?} decode peak: {peak} bytes ({} B/px, charged 8)",
			peak / px
		);
		assert!(
			peak <= px * 8,
			"7 MP {format:?} decode peaked at {peak} bytes, past the {} its estimate charges",
			px * 8
		);
	}
	drop(frame);

	#[cfg(feature = "svg")]
	svg_cases();

	#[cfg(feature = "heif")]
	heif_cases();

	raw_cases();

	assert_eq!(meter::dropped(), 0, "the live-block table overflowed");
}

/// Vector documents: the pixmap, the parse tree and the held text are the
/// costs, all priced by the svg decoder's estimate.
#[cfg(feature = "svg")]
fn svg_cases() {
	// A busy document — thousands of shapes plus an isolated layer.
	let mut body = String::new();
	for i in 0..3000u32 {
		let (x, y) = (i % 100 * 20, i / 100 * 27);
		body.push_str(&format!(
			r#"<circle cx="{x}" cy="{y}" r="9" fill="rgb({},{},{})"/>"#,
			i % 256,
			(i * 3) % 256,
			(i * 7) % 256
		));
	}
	body.push_str(r#"<g opacity="0.5"><rect width="2000" height="800" fill="red"/></g>"#);
	let svg_bytes = format!(
		r#"<svg xmlns="http://www.w3.org/2000/svg" width="2000" height="800">{body}</svg>"#
	)
	.into_bytes();
	let (result, peak) = measured_peak(|| thumb(Box::new(MemSource(svg_bytes)), &spec(512)));
	result.unwrap().expect("busy svg must thumbnail");
	eprintln!("busy svg peak: {peak} bytes");
	assert!(
		peak <= DEFAULT_MEM_BUDGET,
		"busy svg peaked at {peak} bytes (budget {DEFAULT_MEM_BUDGET})"
	);

	// Marker bomb: usvg deep-copies the marker subtree once per path vertex,
	// during the PARSE. Refusing after `Tree::from_str` returns is too late —
	// this document peaked at a quarter of a gigabyte doing it, and with
	// `overflow="visible"` on the marker it then answered Ok. The refusal has
	// to come from the pre-pass, and the meter is what proves it does.
	let mut d = String::from("M0,0");
	for i in 0..50_000u32 {
		d.push_str(&format!("L{},{}", 10 + i % 80, 10 + i % 70));
	}
	for marker_attrs in ["", r#" overflow="visible""#] {
		let bomb = format!(
			r##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100">
			<defs><marker id="m" markerWidth="9" markerHeight="9"{marker_attrs}>
			<circle cx="4" cy="4" r="4" fill="red"/></marker></defs>
			<path d="{d}" fill="none" stroke="black" marker-mid="url(#m)"/></svg>"##
		)
		.into_bytes();
		let (result, peak) = measured_peak(|| thumb(Box::new(MemSource(bomb)), &spec(512)));
		assert!(result.is_err(), "the marker bomb must be refused");
		eprintln!("marker bomb peak: {peak} bytes");
		assert!(
			peak <= DEFAULT_MEM_BUDGET,
			"marker bomb peaked at {peak} bytes (budget {DEFAULT_MEM_BUDGET})"
		);
	}

	// `<use>` doubling chain: 24 levels of ~40 bytes, 2^24 nodes if resolved.
	let mut body = String::from(r#"<g id="g0"><rect width="8" height="8"/></g>"#);
	for level in 1..24 {
		let prev = level - 1;
		body.push_str(&format!(
			r##"<g id="g{level}"><use href="#g{prev}"/><use href="#g{prev}"/></g>"##
		));
	}
	body.push_str(r##"<use href="#g23"/>"##);
	let bomb =
		format!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100">{body}</svg>"#)
			.into_bytes();
	let (result, peak) = measured_peak(|| thumb(Box::new(MemSource(bomb)), &spec(512)));
	assert!(result.is_err(), "the use bomb must be refused");
	eprintln!("use bomb peak: {peak} bytes");
	assert!(
		peak <= DEFAULT_MEM_BUDGET,
		"use bomb peaked at {peak} bytes (budget {DEFAULT_MEM_BUDGET})"
	);

	// The same chain with an href usvg resolves and the pre-pass did not: an
	// xlink decoy in front of the real link, and a leading space that
	// `svgtypes::IRI` skips. Both peaked at 91 MiB before the pre-pass learned
	// usvg's resolution rules.
	for link in [
		r##"<use xlink:href="#tiny" href="#{PREV}"/>"##,
		r##"<use href=" #{PREV}"/>"##,
	] {
		let mut body = String::from(r#"<g id="tiny"/><g id="g0"><rect width="8" height="8"/></g>"#);
		for level in 1..24 {
			let one = link.replace("{PREV}", &format!("g{}", level - 1));
			body.push_str(&format!(r#"<g id="g{level}">{one}{one}</g>"#));
		}
		body.push_str(&link.replace("{PREV}", "g23"));
		let bomb = format!(
			r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink"
			 width="100" height="100">{body}</svg>"#
		)
		.into_bytes();
		let (result, peak) = measured_peak(|| thumb(Box::new(MemSource(bomb)), &spec(512)));
		assert!(result.is_err(), "the disguised use bomb must be refused");
		eprintln!("disguised use bomb peak: {peak} bytes");
		assert!(
			peak <= DEFAULT_MEM_BUDGET,
			"disguised use bomb peaked at {peak} bytes (budget {DEFAULT_MEM_BUDGET})"
		);
	}

	// Marker copies multiply with the `<use>` fan-out (usvg re-expands them
	// per copy: 168 MiB), chain through a second marker (169 MiB), and land on
	// built shapes the whitelist used to skip (70 MiB of `<circle>`).
	let mut d = String::from("M0,0");
	for i in 0..2000u32 {
		d.push_str(&format!("L{},{}", 1 + i % 9, 1 + i % 7));
	}
	let marker = r##"<defs><marker id="m" markerWidth="9" markerHeight="9" overflow="visible">
		<circle cx="4" cy="4" r="4" fill="red"/></marker></defs>"##;
	let mut fan_out = format!(
		r##"{marker}<g id="p"><path d="{d}" fill="none" stroke="black" marker-mid="url(#m)"/></g>"##
	);
	fan_out.push_str(&r##"<use href="#p"/>"##.repeat(100));
	let mut inner = String::from("M0,0");
	for i in 0..450u32 {
		inner.push_str(&format!("L{},{}", 1 + i % 9, 1 + i % 7));
	}
	let chained = format!(
		r##"<defs>
		<marker id="m2" markerWidth="9" markerHeight="9" overflow="visible">
		<circle cx="1" cy="1" r="1" fill="red"/></marker>
		<marker id="m1" markerWidth="9" markerHeight="9" overflow="visible">
		<path d="{inner}" fill="none" stroke="black" marker-mid="url(#m2)"/></marker></defs>
		<path d="{inner}" fill="none" stroke="black" marker-mid="url(#m1)"/>"##
	);
	let shapes = format!(
		r##"{marker}<g marker-mid="url(#m)" marker-start="url(#m)" marker-end="url(#m)"
		 stroke="black">{}</g>"##,
		r#"<circle r="9"/>"#.repeat(12_000)
	);
	for (what, body) in [
		("use x marker", fan_out),
		("marker chain", chained),
		("markers on circles", shapes),
	] {
		let bomb = format!(
			r#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100">{body}</svg>"#
		)
		.into_bytes();
		let (result, peak) = measured_peak(|| thumb(Box::new(MemSource(bomb)), &spec(512)));
		assert!(result.is_err(), "the {what} bomb must be refused");
		eprintln!("{what} bomb peak: {peak} bytes");
		assert!(
			peak <= DEFAULT_MEM_BUDGET,
			"{what} bomb peaked at {peak} bytes (budget {DEFAULT_MEM_BUDGET})"
		);
	}

	// Nested viewport: 120 bytes. The pre-pass resolved percentages against
	// the ROOT viewport, while usvg swaps `state.view_box` for each nested
	// `<svg>` — so a 50% radius inside a 1e36 viewBox walked past the arc
	// guard and rendered at a 65.89 MiB peak. Fifty of them peaked at 1.7 GiB.
	let mut body = String::new();
	for _ in 0..50 {
		body.push_str(r#"<svg viewBox="0 0 1e36 1e36"><ellipse rx="50%" ry="50%"/></svg>"#);
	}
	for nested in [
		r#"<svg viewBox="0 0 1e36 1e36"><ellipse rx="50%" ry="50%"/></svg>"#.to_string(),
		body,
	] {
		let bomb = format!(
			r#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100">{nested}</svg>"#
		)
		.into_bytes();
		let (result, peak) = measured_peak(|| thumb(Box::new(MemSource(bomb)), &spec(512)));
		assert!(result.is_err(), "the nested-viewport bomb must be refused");
		eprintln!("nested viewport bomb peak: {peak} bytes");
		assert!(
			peak <= DEFAULT_MEM_BUDGET,
			"nested viewport bomb peaked at {peak} bytes (budget {DEFAULT_MEM_BUDGET})"
		);
	}

	// Filter fan-out: usvg copies the resolved chain into every group, so 8000
	// `<use>`s of one 64-primitive filter — 130 KB — peaked at 164 MiB and
	// still answered Ok. The byte scan counts an `fe*` tag once per physical
	// occurrence and the layer walk runs after the parse; only the expansion
	// pre-pass sees the product.
	let primitives: String = (0..64)
		.map(|i| format!(r#"<feGaussianBlur stdDeviation="{}"/>"#, 1 + i % 3))
		.collect();
	let bomb = format!(
		r##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100">
		<defs><filter id="f">{primitives}</filter>
		<g id="p" filter="url(#f)"><rect width="9" height="9" fill="red"/></g></defs>{}</svg>"##,
		r##"<use href="#p"/>"##.repeat(8000)
	)
	.into_bytes();
	let (result, peak) = measured_peak(|| thumb(Box::new(MemSource(bomb)), &spec(512)));
	assert!(result.is_err(), "the filter fan-out bomb must be refused");
	eprintln!("filter fan-out bomb peak: {peak} bytes");
	assert!(
		peak <= DEFAULT_MEM_BUDGET,
		"filter fan-out bomb peaked at {peak} bytes (budget {DEFAULT_MEM_BUDGET})"
	);

	// And the charge is calibrated, not merely present: the largest fan-out
	// the estimate still accepts has to render inside the budget it was
	// measured against.
	let allowed = format!(
		r##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100">
		<defs><filter id="f">{primitives}</filter>
		<g id="p" filter="url(#f)"><rect width="9" height="9" fill="red"/></g></defs>{}</svg>"##,
		r##"<use href="#p"/>"##.repeat(340)
	)
	.into_bytes();
	let (result, peak) = measured_peak(|| thumb(Box::new(MemSource(allowed)), &spec(64)));
	result
		.unwrap()
		.expect("the largest accepted filter fan-out must thumbnail");
	eprintln!("accepted filter fan-out peak: {peak} bytes");
	assert!(
		peak <= DEFAULT_MEM_BUDGET,
		"accepted filter fan-out peaked at {peak} bytes (budget {DEFAULT_MEM_BUDGET})"
	);

	// Dash bomb: 200 bytes of document, and tiny-skia builds dash segments
	// until its own million-per-path ceiling — ~125 MB — and then returns a
	// thumbnail. Both spellings of the same stroke.
	for dash in [
		r#" stroke-dasharray="0.001 0.001""#,
		r#" style="stroke-dasharray:0.001 0.001""#,
	] {
		let bomb = format!(
			r#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100">
			<path d="M0 0 L1000000 0" stroke="black" stroke-width="1"{dash}/></svg>"#
		)
		.into_bytes();
		let (result, peak) = measured_peak(|| thumb(Box::new(MemSource(bomb)), &spec(512)));
		assert!(result.is_err(), "the dash bomb must be refused");
		eprintln!("dash bomb peak: {peak} bytes");
		assert!(
			peak <= DEFAULT_MEM_BUDGET,
			"dash bomb peaked at {peak} bytes (budget {DEFAULT_MEM_BUDGET})"
		);
	}
}

/// See tests/pipeline.rs — duplicated because each test binary is its own
/// crate and ten lines beat a shared test-support module.
struct CountingSource(MemSource, std::sync::Arc<std::sync::atomic::AtomicU64>);

impl microthumb::ByteSource for CountingSource {
	fn len(&self) -> u64 {
		self.0.len()
	}

	fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
		let n = self.0.read_at(offset, buf)?;
		self.1.fetch_max(offset + n as u64, Ordering::Relaxed);
		Ok(n)
	}
}

/// The C side of the pipeline — which only the macOS malloc_logger meter can
/// see; the global-allocator meter under-counts every case here, and their
/// ceilings are asserted for the C-aware meter's benefit. Neither codec can be
/// encoded at test time (the vendored libheif is decode-only and nothing in
/// the tree encodes AV1), so every case here is a fixture file: the committed
/// browser parrots cover both codecs' whole-frame decode, and the grid and
/// embedded-preview paths need a real device HEIC on top.
#[cfg(feature = "heif")]
fn heif_cases() {
	committed_codec_cases();
	committed_grid_cases();

	// `MICROTHUMB_HEIF_FIXTURE` is the fixture's whole availability story: no
	// device HEIC is committed (a megabyte of HEVC, against this suite's
	// generate-your-own-fixtures rule) and none can be generated. Absence used
	// to skip with a printed warning that libtest swallows, so a run that
	// proved neither of these two ceilings looked identical to one that did —
	// the same hole `raw_cases` closed. It fails now, and `=offline` is the one
	// spelling that says the omission was deliberate, matching the sentinel
	// `MICROTHUMB_RAW_FIXTURES=offline` already uses.
	let var = std::env::var("MICROTHUMB_HEIF_FIXTURE");
	if var.as_deref() == Ok("offline") {
		eprintln!(
			"SKIP heif cases: MICROTHUMB_HEIF_FIXTURE=offline — the HEIF \
			 embedded-preview and tile-decode memory ceilings are NOT proven by \
			 this run"
		);
		return;
	}
	// Set-but-wrong is a caller error and fails; simply absent is not. No device
	// HEIC is committed and CI never sets the variable, so panicking on absence
	// would red every matrix leg that enables `microthumb/heif` — and it would
	// buy nothing, because `committed_codec_cases` above has already metered
	// both codecs on this run. What is unproven without it is narrower than the
	// old message claimed: the grid and embedded-preview paths, not the ceiling.
	let bytes = match &var {
		Ok(path) => Some(std::fs::read(path).unwrap_or_else(|e| {
			panic!(
				"MICROTHUMB_HEIF_FIXTURE points at {path}, which cannot be read \
				 ({e}). Fix the path, or set it to `offline` to run without the \
				 grid and embedded-preview ceilings on purpose."
			)
		})),
		Err(_) => {
			let path = format!(
				"{}/../iphone_img.heic",
				env!("CARGO_MANIFEST_DIR").replace('\\', "/")
			);
			std::fs::read(&path).ok()
		}
	};
	let Some(bytes) = bytes else {
		eprintln!(
			"SKIP heif grid/preview cases: no device HEIC found — those two \
			 ceilings are NOT proven by this run (the committed parrots above \
			 were still metered)"
		);
		return;
	};

	// Embedded-thumbnail path: a device HEIC carries a `thmb` item (~320–685 px
	// depending on iOS vintage), which suffices for a 512 target — no tile is
	// ever decoded, and the result IS the preview, whose dimensions anchor the
	// tile-path proof below.
	let (result, peak) = measured_peak(|| thumb(Box::new(MemSource(bytes.clone())), &spec(512)));
	let preview = result.unwrap().expect("device heic must thumbnail at 512");
	assert_not_flat(&preview, "heif embedded preview");
	eprintln!("heif embedded-preview peak: {peak} bytes");
	assert!(
		peak <= DEFAULT_MEM_BUDGET,
		"heif preview peaked at {peak} bytes (budget {DEFAULT_MEM_BUDGET})"
	);

	// Tile path. The requested target alone can no longer force it: an
	// oversized request is clamped to what the budget can serve, so asking for
	// 2048 under the default budget just serves the embedded preview again.
	// A budget large enough to make the clamped target exceed twice the
	// preview's long side is what sends the decode through the grid — and the
	// peak is then asserted against THAT budget, since the canvas is entitled
	// to spend it. A strictly larger long side than the 512-target run is the
	// proof the tiles actually ran, whatever thumb this fixture carries.
	const TILE_BUDGET: usize = 96 * 1024 * 1024;
	let tile_spec = ThumbSpec {
		target_width: 2048,
		target_height: 2048,
		mem_budget: TILE_BUDGET,
		allow_full_decode: true,
	};
	let (result, peak) = measured_peak(|| thumb(Box::new(MemSource(bytes)), &tile_spec));
	let result = result.unwrap().expect("device heic must thumbnail at 2048");
	assert!(
		result.width.max(result.height) > preview.width.max(preview.height),
		"expected the tile-decoded canvas to out-resolve the {}x{} preview, got {}x{}",
		preview.width,
		preview.height,
		result.width,
		result.height
	);
	assert_not_flat(&result, "heif tile decode");
	eprintln!("heif tile-decode peak: {peak} bytes");
	assert!(
		peak <= TILE_BUDGET,
		"heif tile decode peaked at {peak} bytes (budget {TILE_BUDGET})"
	);
}

/// The two committed HEIF fixtures, metered where nothing else meters them:
/// the AV1 and HEVC decodes happen inside dav1d and libde265, whose buffers
/// are C allocations no `#[global_allocator]` can see, so only the macOS
/// meter above prices them. Nothing in this tree encodes either codec, so a
/// real file has to come from disk, and these two 1000×667 parrots are the
/// ones there are — `pipeline.rs` reads the AVIF for the same reason. Neither
/// is tiled and neither carries a `thmb`, so what they pin is the whole-frame
/// decode; the grid and embedded-preview paths still need the device HEIC
/// `heif_cases` asks for.
#[cfg(feature = "heif")]
fn committed_codec_cases() {
	for name in ["parrot.avif", "parrot.heif"] {
		let path = format!(
			"{}/../filen-sdk-rs/web/test-assets/imgs/{name}",
			env!("CARGO_MANIFEST_DIR").replace('\\', "/")
		);
		let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("committed {name}: {e}"));
		let px = 1000usize * 667;

		// The consequence, not the accounting: at the extension budget the
		// charge is what decides how much canvas is left, and the whole run
		// has to land inside the budget it was admitted under. (This used to
		// assert against APP_PROCESS_MEM_BUDGET, 3.2× the peak it saw, and so
		// could not fail — while the same decode was 1.27× OVER the default
		// budget that admits it in the extension.)
		let copy = bytes.clone();
		let (result, peak) = measured_peak(|| thumb(Box::new(MemSource(copy)), &spec(512)));
		let image = result
			.unwrap()
			.unwrap_or_else(|| panic!("committed {name} must thumbnail"));
		assert_not_flat(&image, name);
		eprintln!(
			"{name} peak at 512: {peak} bytes ({}x{} canvas)",
			image.width, image.height
		);
		assert!(
			peak <= DEFAULT_MEM_BUDGET,
			"{name} peaked at {peak} bytes (budget {DEFAULT_MEM_BUDGET})"
		);

		// And at a small request, where the canvas is a rounding error, what
		// is left IS the decode — which must stay under what it was charged.
		let charged = heif_charge(px, HEIF_BYTES_PER_PIXEL, bytes.len(), 1);
		let (result, peak) = measured_peak(|| thumb(Box::new(MemSource(bytes)), &spec(64)));
		result
			.unwrap()
			.unwrap_or_else(|| panic!("{name} must thumbnail at 64 px too"));
		eprintln!(
			"{name} decode peak: {peak} bytes ({} B/px, charged {charged})",
			peak / px
		);
		assert!(
			peak <= charged,
			"{name} decode peaked at {peak} bytes, past the {charged} its estimate charges"
		);
	}
}

/// The committed HEVC grids — 64x48 tiles, 10-bit 4:2:2 like a camera HIF,
/// 8-bit 4:2:0 like a phone and 10-bit 4:2:0 — metered one tile at a time
/// against the rate for their depth and chroma. Tiny tiles, so what this pins is mostly the
/// setup charge; the per-pixel rates were fitted on camera files (see
/// `formats::heif`), which are far too large to commit.
#[cfg(feature = "heif")]
fn committed_grid_cases() {
	for (name, bytes, bytes_per_pixel) in [
		(
			"grid-irot1.heic",
			&include_bytes!("fixtures/heif/grid-irot1.heic")[..],
			HEIF_BYTES_PER_PIXEL_DEEP_CHROMA,
		),
		(
			"grid-8bit.heic",
			&include_bytes!("fixtures/heif/grid-8bit.heic")[..],
			HEIF_BYTES_PER_PIXEL,
		),
		(
			"grid-10bit-420.heic",
			&include_bytes!("fixtures/heif/grid-10bit-420.heic")[..],
			HEIF_BYTES_PER_PIXEL,
		),
	] {
		let charged = heif_charge(64 * 48, bytes_per_pixel, bytes.len(), 4);
		let copy = bytes.to_vec();
		let (result, peak) = measured_peak(|| thumb(Box::new(MemSource(copy)), &spec(64)));
		result
			.unwrap()
			.unwrap_or_else(|| panic!("committed {name} must thumbnail"));
		eprintln!("{name} tile decode peak: {peak} bytes (charged {charged})");
		assert!(
			peak <= charged,
			"{name} decode peaked at {peak} bytes, past the {charged} its estimate charges"
		);
	}
}

/// What `formats::heif::peak_estimate` charges for one decode of `pixels`
/// out of a file of `source_len` bytes split into `units` decodes. Mirrored
/// rather than called: the estimate is crate-private, and a test that read
/// the real number would pass however wrong the number was. Keep in step with
/// `DECODE_SETUP_BYTES` / `DECODE_BYTES_PER_PIXEL` /
/// `DECODE_BYTES_PER_PIXEL_DEEP_CHROMA` / `input_bytes`.
#[cfg(feature = "heif")]
fn heif_charge(pixels: usize, bytes_per_pixel: usize, source_len: usize, units: usize) -> usize {
	1024 * 1024 + pixels * bytes_per_pixel + source_len + 2 * source_len.div_ceil(units)
}

#[cfg(feature = "heif")]
const HEIF_BYTES_PER_PIXEL: usize = 12;
#[cfg(feature = "heif")]
const HEIF_BYTES_PER_PIXEL_DEEP_CHROMA: usize = 20;

/// Real camera RAW files, metered. Most of them now yield a thumbnail — the
/// embedded preview, decoded through the JPEG path (see
/// tests/raw_characterisation.rs for the per-format outcome) — and that
/// decode is the point of metering them: a RAW file is tens of megabytes, its
/// preview can be a 6480x4320 JPEG, its IFD0 can declare an enormous strip
/// table, and every one of those paths has to stay inside the ceiling.
///
/// Downloads what it needs: `raw_fixtures::wanted` picks one sample per
/// container format (~115 MiB, once, kept outside `target/`), every pin is
/// hash-verified on every run, and anything already cached is metered too even
/// when it is outside that set. `MICROTHUMB_RAW_FIXTURES=all` meters the whole
/// ~1 GiB table; `=offline` meters only what is already on disk.
///
/// Measuring nothing is a failure, not a quiet pass — that is the entire
/// reason this function exists. `=offline` is the way to say so deliberately.
fn raw_cases() {
	let mut worst = 0;
	let mut worst_name = "";
	let mut measured = 0;
	let mut unavailable = 0;

	for fixture in raw_fixtures::RAW_FIXTURES {
		let path = if raw_fixtures::wanted(fixture) {
			match raw_fixtures::locate(fixture) {
				raw_fixtures::Fixture::Ready(path) => path,
				raw_fixtures::Fixture::Unavailable(why) => {
					eprintln!("SKIP {}: {why}", fixture.cache_name);
					unavailable += 1;
					continue;
				}
			}
		} else {
			// Not in the default set, but if a previous `=all` run left it
			// cached it is free to meter.
			match raw_fixtures::cached(fixture) {
				Some(path) => path,
				None => continue,
			}
		};
		let file = std::fs::File::open(&path).expect("cached fixture opens");
		let source = FileSource::new(file).expect("file source");
		let (result, peak) = measured_peak(|| thumb(Box::new(source), &spec(512)));
		// The outcome itself is characterised elsewhere; here we only care that
		// whichever path ran did so inside the budget. An error is a legitimate
		// outcome and must be metered like any other.
		drop(result);
		measured += 1;
		if peak > worst {
			worst = peak;
			worst_name = fixture.cache_name;
		}
		assert!(
			peak <= DEFAULT_MEM_BUDGET,
			"{} ({}) peaked at {peak} bytes (budget {DEFAULT_MEM_BUDGET})",
			fixture.cache_name,
			fixture.format
		);
	}

	if measured == 0 {
		// A pass here would prove nothing about RAW memory while looking green,
		// and a printed warning would be swallowed by libtest's capture. So it
		// fails; the message says exactly how to make an offline run legitimate.
		assert!(
			raw_fixtures::offline(),
			"raw cases measured NOTHING: {unavailable} pinned samples could not be \
			 fetched into {} (see the SKIP lines above), so the RAW memory ceilings \
			 are unproven. Set MICROTHUMB_RAW_FIXTURES=offline to run without them \
			 on purpose.",
			raw_fixtures::cache_dir().display()
		);
		eprintln!(
			"SKIP raw cases: MICROTHUMB_RAW_FIXTURES=offline and nothing cached in {} \
			 — the RAW memory ceilings are NOT proven by this run",
			raw_fixtures::cache_dir().display()
		);
		return;
	}
	eprintln!(
		"raw peak over {measured} samples ({unavailable} unavailable): \
		 {worst} bytes ({worst_name})"
	);
}

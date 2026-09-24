//! What the pipeline does with real HEIF files.
//!
//! A characterisation suite, like `raw_characterisation`: the outcome for every
//! pinned file is written down in [`EXPECTED`], so a change in HEIF handling
//! has to rewrite a baseline instead of moving silently. The files are camera
//! and phone originals from the test corpora of open-source projects (see
//! `raw_fixtures/heif_pins.rs`) — about 11 MB, fetched once into the same cache
//! as the RAW samples, so unlike those this suite runs by default.
//!
//! Beyond the per-file outcomes it pins that a real rotated grid decodes the
//! way libheif composites it, and that a motion photo's video is never read.
//!
//! No Fujifilm HIF is among them: none exists under a usable licence. The
//! camera-JPEG path is pinned by the synthetic fixtures in `pipeline.rs`.
#![cfg(feature = "heif")]

mod raw_fixtures;

use std::{
	collections::BTreeSet,
	io::BufReader,
	path::{Path, PathBuf},
	sync::{
		Arc, OnceLock,
		atomic::{AtomicU64, Ordering},
	},
};

use heif_decoder::HeifSession;
use image::{RgbaImage, imageops};
use microthumb::{
	APP_PROCESS_MEM_BUDGET, ByteSource, DEFAULT_MEM_BUDGET, FileSource, ThumbOutcome, ThumbSource,
	ThumbSpec, generate, locate_preview,
};
use raw_fixtures::heif_pins::{HEIF_FIXTURES, HeifFixture};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
	/// From an embedded thumbnail, at these dimensions.
	Preview(u32, u32),
	/// Decoded from the image itself.
	Decoded(u32, u32),
	/// No thumbnail the spec could afford.
	Nothing,
	/// An error.
	Failed,
}

use Outcome::{Decoded, Failed, Nothing, Preview};

/// The file-provider extension on a local file: 12 MiB, a 256 px request.
fn extension() -> ThumbSpec {
	ThumbSpec::new(256, 256, DEFAULT_MEM_BUDGET)
}

/// The extension on a remote file too large to stream: no full decode.
fn extension_remote() -> ThumbSpec {
	ThumbSpec::preview_only(256, 256, DEFAULT_MEM_BUDGET)
}

/// The app on a local file, asking for as much as it can get.
fn app() -> ThumbSpec {
	ThumbSpec::new(2048, 2048, APP_PROCESS_MEM_BUDGET)
}

struct Expected {
	cache_name: &'static str,
	extension: Outcome,
	extension_remote: Outcome,
	app: Outcome,
}

const EXPECTED: &[Expected] = &[
	// Sony HIF has no thumbnail any of these budgets admits. Its only JPEG is a
	// 160x120 stamp, under the 512 px a camera JPEG must reach to stand in for
	// the shot; its 1616x1080 HEVC thumbnail is over the 1 Mpx thumbnail cap;
	// and its tiles, 5.5 Mpx at 10 bits, are over every budget. Moving these is
	// a deliberate change to HIF support.
	Expected {
		cache_name: "heif-exiv2-Sony.HIF",
		extension: Nothing,
		extension_remote: Nothing,
		app: Nothing,
	},
	Expected {
		cache_name: "heif-pillow_heif-guitar_cw90.hif",
		extension: Nothing,
		extension_remote: Nothing,
		app: Nothing,
	},
	// Canon's thumbnail is coded 320x320 against a declared 320x214, which
	// libheif refuses — an error wherever no full decode may follow it. The
	// grid's 1216x832 tiles fit the app budget.
	Expected {
		cache_name: "heif-exiv2-Canon.HIF",
		extension: Nothing,
		extension_remote: Failed,
		app: Decoded(1600, 1066),
	},
	Expected {
		cache_name: "heif-pillow_heif-arrow.heic",
		extension: Preview(240, 320),
		extension_remote: Preview(240, 320),
		app: Decoded(1200, 1600),
	},
	Expected {
		cache_name: "heif-sm_motion_photo-sg22-ultra.heic",
		extension: Preview(384, 512),
		extension_remote: Preview(384, 512),
		app: Decoded(1200, 1600),
	},
	Expected {
		cache_name: "heif-ts-heic-iphone-grid.heic",
		extension: Preview(312, 416),
		extension_remote: Preview(312, 416),
		app: Decoded(1200, 1600),
	},
	// One untiled 12 Mpx AV1 frame is over every budget here.
	Expected {
		cache_name: "heif-libvips-avif-orientation-6.avif",
		extension: Nothing,
		extension_remote: Nothing,
		app: Nothing,
	},
];

/// The licences a pinned file may carry: this repository's own AGPL-3.0 and
/// those compatible with it.
const USABLE_LICENCES: &[&str] = &[
	"AGPL-3.0-only",
	"AGPL-3.0-or-later",
	"GPL-2.0-or-later",
	"GPL-3.0-only",
	"GPL-3.0-or-later",
	"LGPL-2.1-or-later",
	"LGPL-3.0-or-later",
	"MIT",
	"BSD-2-Clause",
	"BSD-3-Clause",
	"Apache-2.0",
	"CC0-1.0",
	"CC-BY-4.0",
	"CC-BY-SA-4.0",
];

/// Every pinned file that could be obtained, fetched once per test binary —
/// the tests run in parallel and must not download the same file twice.
/// Obtaining nothing is a failure rather than a quiet pass, unless
/// `MICROTHUMB_RAW_FIXTURES=offline` says the omission is deliberate.
fn available() -> &'static [(&'static HeifFixture, PathBuf)] {
	static AVAILABLE: OnceLock<Vec<(&'static HeifFixture, PathBuf)>> = OnceLock::new();
	AVAILABLE.get_or_init(|| {
		let mut found = Vec::new();
		raw_fixtures::for_each_available(HEIF_FIXTURES, |fixture, path| {
			found.push((fixture, path.to_path_buf()));
		});
		if found.is_empty() {
			assert!(
				raw_fixtures::offline(),
				"not one pinned HEIF file could be fetched into {} (see the SKIP lines \
				 above), so real HEIF handling is unproven. Set \
				 MICROTHUMB_RAW_FIXTURES=offline to run without it on purpose.",
				raw_fixtures::cache_dir().display()
			);
			eprintln!("SKIP heif characterisation: MICROTHUMB_RAW_FIXTURES=offline");
		}
		found
	})
}

fn available_named(cache_name: &str) -> Option<&'static Path> {
	available()
		.iter()
		.find(|(fixture, _)| fixture.cache_name == cache_name)
		.map(|(_, path)| path.as_path())
}

/// A file source that records the furthest byte read.
struct Counting(FileSource, Arc<AtomicU64>);

impl ByteSource for Counting {
	fn len(&self) -> u64 {
		self.0.len()
	}

	fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
		let n = self.0.read_at(offset, buf)?;
		self.1.fetch_max(offset + n as u64, Ordering::Relaxed);
		Ok(n)
	}
}

/// The outcome of `spec` on the file at `path`, and the furthest byte read.
fn run(path: &Path, spec: &ThumbSpec) -> (Outcome, u64) {
	let furthest = Arc::new(AtomicU64::new(0));
	let file = std::fs::File::open(path).expect("fixture opens");
	let src = Counting(
		FileSource::new(file).expect("file source"),
		furthest.clone(),
	);
	let outcome = match generate(Box::new(src), spec) {
		Ok(ThumbOutcome::Thumbnail(thumb)) => {
			let (width, height) = (thumb.image.width, thumb.image.height);
			match thumb.source {
				ThumbSource::EmbeddedPreview => Preview(width, height),
				ThumbSource::Decoded => Decoded(width, height),
			}
		}
		Ok(ThumbOutcome::Unsupported | ThumbOutcome::OverBudget) => Nothing,
		Err(_) => Failed,
	};
	(outcome, furthest.load(Ordering::Relaxed))
}

/// The pins are internally consistent and each is usable under this
/// repository's licence. Needs no fixture bytes.
#[test]
fn heif_pins_are_well_formed_and_usably_licensed() {
	let mut names = BTreeSet::new();
	let mut hashes = BTreeSet::new();
	for f in HEIF_FIXTURES {
		assert!(f.len > 0, "{} pinned with zero length", f.cache_name);
		assert!(
			f.blake3.len() == 64 && f.blake3.bytes().all(|b| b.is_ascii_hexdigit()),
			"{} has a malformed BLAKE3 pin",
			f.cache_name
		);
		// raw.githubusercontent.com/<owner>/<repo>/<commit>/...: the bytes are
		// those of one commit, and so is the licence that covers them.
		let segments: Vec<&str> = f
			.url
			.strip_prefix("https://raw.githubusercontent.com/")
			.unwrap_or_else(|| panic!("{} is pinned to an unexpected host", f.cache_name))
			.splitn(4, '/')
			.collect();
		let [owner, repo, commit, _] = segments[..] else {
			panic!("{} has no path under its commit", f.cache_name);
		};
		assert!(
			commit.len() == 40 && commit.bytes().all(|b| b.is_ascii_hexdigit()),
			"{} is not pinned to a commit: {}",
			f.cache_name,
			f.url
		);
		assert!(
			f.licence_source.starts_with(&format!(
				"https://raw.githubusercontent.com/{owner}/{repo}/{commit}/"
			)),
			"{} cites a licence from somewhere other than its own commit",
			f.cache_name
		);
		assert!(
			USABLE_LICENCES.contains(&f.licence),
			"{} is pinned under {}, which is not compatible with AGPL-3.0",
			f.cache_name,
			f.licence
		);
		assert!(
			names.insert(f.cache_name),
			"duplicate cache name {}",
			f.cache_name
		);
		assert!(hashes.insert(f.blake3), "duplicate pin on {}", f.cache_name);
		assert!(
			EXPECTED.iter().any(|e| e.cache_name == f.cache_name),
			"{} has no baseline",
			f.cache_name
		);
	}
	for e in EXPECTED {
		assert!(
			names.contains(e.cache_name),
			"baseline for {}, which is not pinned",
			e.cache_name
		);
	}
}

/// Every pinned file thumbnails — or does not — exactly as its baseline says,
/// on the three specs the callers use, and none offers a preview: no real
/// HEIF here carries a camera JPEG large enough to stand in for the shot.
#[test]
fn real_heif_files_characterise_as_pinned() {
	for (fixture, path) in available() {
		let expected = EXPECTED
			.iter()
			.find(|e| e.cache_name == fixture.cache_name)
			.expect("every pin has a baseline");
		let got = (
			run(path, &extension()).0,
			run(path, &extension_remote()).0,
			run(path, &app()).0,
		);
		eprintln!(
			"{:<40} {} {}: {got:?}",
			fixture.cache_name, fixture.make, fixture.model
		);
		assert_eq!(
			got,
			(expected.extension, expected.extension_remote, expected.app),
			"{} ({} {}): extension, extension-remote, app",
			fixture.cache_name,
			fixture.make,
			fixture.model
		);
		let file = std::fs::File::open(path).expect("fixture opens");
		assert_eq!(
			locate_preview(&mut FileSource::new(file).expect("file source")).unwrap(),
			None,
			"{} offers a preview",
			fixture.cache_name
		);
	}
}

/// Real grids stored turned — the iPhone 8 Plus and Samsung portraits, their
/// overhang on the leading edge once turned — and a current iPhone's native
/// portrait come out of the tile path as libheif composites the whole frame.
/// Compared at the canvas size by mean absolute difference per channel: these
/// measure under 1, while the same canvas shifted by the 14 px the overhang
/// once displaced it by measures 8 to 14.
#[test]
fn real_grids_decode_as_libheif_composites_them() {
	for name in [
		"heif-pillow_heif-arrow.heic",
		"heif-sm_motion_photo-sg22-ultra.heic",
		"heif-ts-heic-iphone-grid.heic",
	] {
		let Some(path) = available_named(name) else {
			continue;
		};
		let ThumbOutcome::Thumbnail(thumb) = generate(
			Box::new(FileSource::new(std::fs::File::open(path).unwrap()).unwrap()),
			&app(),
		)
		.unwrap() else {
			panic!("{name} must thumbnail at the app budget");
		};
		assert_eq!(thumb.source, ThumbSource::Decoded, "{name}");
		let tiled = RgbaImage::from_raw(thumb.image.width, thumb.image.height, thumb.image.rgba)
			.expect("a whole canvas");

		let file = std::fs::File::open(path).unwrap();
		let len = file.metadata().unwrap().len();
		let whole = HeifSession::new(BufReader::new(file), len)
			.unwrap()
			.decode_primary_rgba()
			.unwrap();
		let whole = imageops::resize(
			&whole,
			tiled.width(),
			tiled.height(),
			imageops::FilterType::Triangle,
		);
		let difference = tiled
			.as_raw()
			.iter()
			.zip(whole.as_raw())
			.enumerate()
			.filter(|(i, _)| i % 4 != 3)
			.map(|(_, (a, b))| f64::from(a.abs_diff(*b)))
			.sum::<f64>()
			/ (f64::from(tiled.width()) * f64::from(tiled.height()) * 3.0);
		assert!(
			difference < 3.0,
			"{name}: the tile path differs from libheif's whole frame by {difference:.2} per channel"
		);
	}
}

/// A Samsung motion photo carries its video in a trailing `mpvd` box. Neither
/// the thumbnail nor the full decode reads past that box's header: the still
/// is all any of them needs.
#[test]
fn a_motion_photos_video_is_never_read() {
	let Some(path) = available_named("heif-sm_motion_photo-sg22-ultra.heic") else {
		return;
	};
	let bytes = std::fs::read(path).unwrap();
	let mut at = 0usize;
	let video = loop {
		let size = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
		if &bytes[at + 4..at + 8] == b"mpvd" {
			break at;
		}
		at += size;
	};
	for spec in [extension(), extension_remote(), app()] {
		let (outcome, furthest) = run(path, &spec);
		assert!(matches!(outcome, Preview(..) | Decoded(..)), "{outcome:?}");
		assert!(
			furthest <= (video + 8) as u64,
			"read to byte {furthest}, past the video's header at {video}"
		);
	}
}

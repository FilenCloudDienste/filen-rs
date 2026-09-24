//! Pinned real HEIF files: camera and phone originals committed to the test
//! corpora of open-source projects whose licence is compatible with this
//! repository's AGPL-3.0. See `regenerate.md` in this directory for how they
//! were found, checked and chosen.
//!
//! Every URL is pinned to a commit, so the bytes cannot move under the pin, and
//! `blake3` is checked on every run exactly as for the RAW table. `licence` is
//! the SPDX identifier the file is distributed under, and `licence_source` the
//! document at that same commit which says so.
//!
//! No Fujifilm HIF is here: none exists under a usable licence anywhere the
//! search reached. The camera-JPEG path Fujifilm files take is pinned by the
//! synthetic `tests/fixtures/heif/fuji*.heic` instead.

pub struct HeifFixture {
	/// Exact URL this file is fetched from, pinned to a commit.
	pub url: &'static str,
	/// BLAKE3 of the whole file. Verified on every run.
	pub blake3: &'static str,
	/// Exact byte length.
	pub len: u64,
	/// File name inside the fixture cache directory.
	pub cache_name: &'static str,
	pub make: &'static str,
	pub model: &'static str,
	/// SPDX identifier.
	pub licence: &'static str,
	/// The licence document at the pinned commit.
	pub licence_source: &'static str,
}

impl super::Pinned for HeifFixture {
	fn url(&self) -> &'static str {
		self.url
	}

	fn blake3(&self) -> &'static str {
		self.blake3
	}

	fn byte_len(&self) -> u64 {
		self.len
	}

	fn cache_name(&self) -> &'static str {
		self.cache_name
	}
}

pub const HEIF_FIXTURES: &[HeifFixture] = &[
	// 8640x5760 as a 3x3 grid of 10-bit 4:2:0 tiles; thumbnails 1616x1080 and
	// 320x212 HEVC plus a 160x120 JPEG. The pixels are an all-black frame.
	HeifFixture {
		url: "https://raw.githubusercontent.com/Exiv2/exiv2/c69c2e9fca21cab6d16c6fd58eb70ab0b697c06b/test/data/Sony.HIF",
		blake3: "f11505905bf3ff993222b06e25ed2f652bd9baa5cbfd060690997341945f74f5",
		len: 151_552,
		cache_name: "heif-exiv2-Sony.HIF",
		make: "SONY",
		model: "ILCE-1",
		licence: "GPL-2.0-or-later",
		licence_source: "https://raw.githubusercontent.com/Exiv2/exiv2/c69c2e9fca21cab6d16c6fd58eb70ab0b697c06b/LICENSE.txt",
	},
	// A portrait shot: 7008x4672 as 10-bit 4:2:2 tiles turned a quarter
	// counter-clockwise, its 160x120 JPEG thumbnail carrying no transform of
	// its own — the layout a portrait Fujifilm HIF has.
	HeifFixture {
		url: "https://raw.githubusercontent.com/bigcat88/pillow_heif/4ce712961deece4507463c83de1a41f486d303b8/tests/images/heif_special/guitar_cw90.hif",
		blake3: "7e5e974c45020d5cf9cdf6ef528dcd978ce4d58f4a4a416c152147db2dafb1dd",
		len: 5_156_864,
		cache_name: "heif-pillow_heif-guitar_cw90.hif",
		make: "SONY",
		model: "ILCE-7M4",
		licence: "BSD-3-Clause",
		licence_source: "https://raw.githubusercontent.com/bigcat88/pillow_heif/4ce712961deece4507463c83de1a41f486d303b8/LICENSE.txt",
	},
	// The R5's in-camera 2400x1600 size, a 2x2 grid of 10-bit 4:2:2 BT.2100 PQ
	// tiles. Its 320x214 thumbnail is coded as a 320x320 picture, which libheif
	// refuses. The pixels are a near-black frame.
	HeifFixture {
		url: "https://raw.githubusercontent.com/Exiv2/exiv2/c69c2e9fca21cab6d16c6fd58eb70ab0b697c06b/test/data/Canon.HIF",
		blake3: "32790bbc0874d1fa266ab915cdc3e30d4c22816f892021e5bf4d641f22f647ee",
		len: 1_253_392,
		cache_name: "heif-exiv2-Canon.HIF",
		make: "Canon",
		model: "EOS R5",
		licence: "GPL-2.0-or-later",
		licence_source: "https://raw.githubusercontent.com/Exiv2/exiv2/c69c2e9fca21cab6d16c6fd58eb70ab0b697c06b/LICENSE.txt",
	},
	// A portrait capture: 8-bit 4:2:0 512x512 tiles stored landscape and turned
	// 270 counter-clockwise, overhanging on the leading edge once turned. A
	// hand-drawn arrow pointing up.
	HeifFixture {
		url: "https://raw.githubusercontent.com/bigcat88/pillow_heif/4ce712961deece4507463c83de1a41f486d303b8/tests/images/heif_other/arrow.heic",
		blake3: "f02bab9a7279a1a0f6e8a5a012472e3d26589eee623f87509539ba576c8728c0",
		len: 792_052,
		cache_name: "heif-pillow_heif-arrow.heic",
		make: "Apple",
		model: "iPhone 8 Plus",
		licence: "BSD-3-Clause",
		licence_source: "https://raw.githubusercontent.com/bigcat88/pillow_heif/4ce712961deece4507463c83de1a41f486d303b8/LICENSE.txt",
	},
	// A motion photo: the still's mdat first, then meta, then a 2.3 MB MP4 in a
	// trailing `mpvd` box. The grid is turned 270 counter-clockwise.
	HeifFixture {
		url: "https://raw.githubusercontent.com/g0ddest/sm_motion_photo/e5f9d6b96d9f93e7dfd42dd06e4bd11077c84b59/tests/data/photo-sg22-ultra.heic",
		blake3: "818f5a746e798d7ed5fe67d4f8ca7a211affb71f210fc16c362efc358a8446df",
		len: 2_849_914,
		cache_name: "heif-sm_motion_photo-sg22-ultra.heic",
		make: "samsung",
		model: "SM-S9080",
		licence: "MIT",
		licence_source: "https://raw.githubusercontent.com/g0ddest/sm_motion_photo/e5f9d6b96d9f93e7dfd42dd06e4bd11077c84b59/LICENSE",
	},
	// A current iPhone: a native portrait grid, a depth auxiliary image, the
	// HDR gain-map brand and a 36 KB meta box.
	HeifFixture {
		url: "https://raw.githubusercontent.com/stacksjs/ts-heic/59a1ac4b1910b8cb6bcef7c68b2b2fb2792f82f2/test/fixtures/iphone-grid.heic",
		blake3: "31b4b8af73d4473af2e94ec467f62bcdfd6ada4210b6a6a427ed8e011ca95505",
		len: 576_966,
		cache_name: "heif-ts-heic-iphone-grid.heic",
		make: "Apple",
		model: "iPhone 15 Pro",
		licence: "MIT",
		licence_source: "https://raw.githubusercontent.com/stacksjs/ts-heic/59a1ac4b1910b8cb6bcef7c68b2b2fb2792f82f2/LICENSE.md",
	},
	// A phone photo as one untiled 12 Mpx AV1 image. The only AVIF of device
	// origin under a usable licence: its bitstream is a re-encode, its EXIF the
	// phone's own.
	HeifFixture {
		url: "https://raw.githubusercontent.com/libvips/libvips/2f397a4a31f7f11986eecbd2b7c5afc248892b2f/test/test-suite/images/avif-orientation-6.avif",
		blake3: "0180ebb44f314d4fcc775f4933ebd10ebe5c61ed3a8faf6abed034fc59f99920",
		len: 162_011,
		cache_name: "heif-libvips-avif-orientation-6.avif",
		make: "Apple",
		model: "iPhone XS",
		licence: "LGPL-2.1-or-later",
		licence_source: "https://raw.githubusercontent.com/libvips/libvips/2f397a4a31f7f11986eecbd2b7c5afc248892b2f/LICENSE",
	},
];

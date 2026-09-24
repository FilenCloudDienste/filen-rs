//! Fetch-and-verify for the pinned sample files: the RAW samples in [`pins`]
//! and the real HEIF files in [`heif_pins`].
//!
//! Three properties this module exists to guarantee:
//!
//! * **Only pinned URLs are requested.** There is no directory listing and no
//!   scraping. The set of bytes these tests can ever see is exactly the tables
//!   in `pins.rs` and `heif_pins.rs`.
//! * **The hash is checked on every run**, not only after a download. A cached
//!   file that no longer matches its pin is a hard error, so the tests can
//!   never quietly run against bytes we do not recognise.
//! * **Missing bytes are a skip, not a failure.** A fixture that cannot be
//!   fetched is skipped with a line naming it and its URL; a machine with no
//!   network passes once it says so with `MICROTHUMB_RAW_FIXTURES=offline`.
//!
//! `MICROTHUMB_RAW_FIXTURES` selects how much is fetched: unset pulls the
//! default subset ([`wanted`]), `all` pulls all of [`RAW_FIXTURES`] (~1 GiB),
//! `offline` pulls nothing and uses only what is already cached.
//!
//! Fetching shells out to `curl` rather than linking an HTTP client. microthumb
//! is deliberately dependency-light and has no network stack; the alternative
//! routes are worse. Going through `test-utils` would mean
//! `test-utils -> filen-sdk-rs -> microthumb`, a dev-dependency cycle, and
//! adding `ureq`/`reqwest` would drag a TLS stack (or a whole async runtime)
//! into the dev-tree of a leaf crate whose entire point is not having one.
//! `curl` ships on macOS, on Linux CI images and on Windows 10+; when it is
//! absent the fixture is simply unavailable, which is already a supported
//! state. The one dev-dependency added is `blake3`, which the workspace
//! already builds.

#![allow(dead_code)]

pub mod heif_pins;
pub mod pins;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub use pins::{RAW_FIXTURES, RawFixture};

/// What fetching and verifying needs of a pinned file, whichever table pins it.
pub trait Pinned {
	/// Exact URL the file is fetched from.
	fn url(&self) -> &'static str;
	/// BLAKE3 of the whole file, verified on every run.
	fn blake3(&self) -> &'static str;
	/// Exact byte length.
	fn byte_len(&self) -> u64;
	/// File name inside the fixture cache directory.
	fn cache_name(&self) -> &'static str;
}

impl Pinned for RawFixture {
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

/// Where the pinned samples are kept. Deliberately outside `target/` so
/// `cargo clean` does not force a ~1 GiB re-download, and gitignored so the
/// samples themselves are never committed.
pub fn cache_dir() -> PathBuf {
	match std::env::var_os("MICROTHUMB_RAW_FIXTURE_DIR") {
		Some(dir) => PathBuf::from(dir),
		None => Path::new(env!("CARGO_MANIFEST_DIR")).join("../.fixture-cache/raw"),
	}
}

/// Set `MICROTHUMB_RAW_FIXTURES=offline` to use only what is already cached.
pub fn offline() -> bool {
	mode().is_some_and(|v| v == "offline")
}

fn mode() -> Option<String> {
	std::env::var("MICROTHUMB_RAW_FIXTURES").ok()
}

/// Whether `fixture` is worth downloading on an ordinary run.
///
/// The whole table is ~1 GiB, too much to pull from a plain `cargo test`, so
/// the default is one sample per container format — the largest of each, since
/// the tests that fetch automatically are memory-ceiling tests and the biggest
/// file is the worst case for the decode path that container selects. About
/// 115 MiB, fetched once and kept outside `target/`.
/// `MICROTHUMB_RAW_FIXTURES=all` widens this to everything.
pub fn wanted(fixture: &RawFixture) -> bool {
	if offline() {
		return false;
	}
	if mode().is_some_and(|v| v == "all") {
		return true;
	}
	// ponytail: O(n^2) over a 100-row const table, run once per test.
	RAW_FIXTURES
		.iter()
		.filter(|other| other.container == fixture.container)
		.all(|other| other.len <= fixture.len)
}

pub enum Fixture {
	/// Present on disk and verified against the pin.
	Ready(PathBuf),
	/// Not cached and not fetchable. Carries a message naming the fixture and
	/// its URL, for the caller to print before skipping.
	Unavailable(String),
}

/// Verifies a file against its pin. Length first because it is free and
/// diagnoses the common case (a truncated file) more clearly than a hash
/// mismatch would.
///
/// Panics on any mismatch, by design: an unrecognised file is never something
/// to skip past.
fn verify(fixture: &impl Pinned, path: &Path) {
	let len = fs::metadata(path)
		.unwrap_or_else(|e| panic!("cannot stat pinned fixture {}: {e}", path.display()))
		.len();
	assert_eq!(
		len,
		fixture.byte_len(),
		"pinned fixture {} has the wrong length ({len} bytes, pinned {}). \
		 Delete it to re-download; if a fresh download still mismatches, the \
		 pin no longer describes what {} serves.",
		path.display(),
		fixture.byte_len(),
		fixture.url(),
	);

	let mut hasher = blake3::Hasher::new();
	let file = fs::File::open(path)
		.unwrap_or_else(|e| panic!("cannot open pinned fixture {}: {e}", path.display()));
	// Streamed, so verifying a 25 MB sample never costs 25 MB of residency —
	// these tests measure memory.
	hasher
		.update_reader(file)
		.unwrap_or_else(|e| panic!("cannot read pinned fixture {}: {e}", path.display()));
	let got = hasher.finalize().to_hex();
	assert_eq!(
		got.as_str(),
		fixture.blake3(),
		"pinned fixture {} does not match its BLAKE3 pin. Refusing to run \
		 against unrecognised bytes.",
		path.display(),
	);
}

/// Downloads to `dest`. `Err` means "could not fetch", which is a skip; it is
/// never used for a content mismatch.
fn fetch(fixture: &impl Pinned, dest: &Path) -> Result<(), String> {
	if offline() {
		return Err(format!(
			"{} is not cached and MICROTHUMB_RAW_FIXTURES=offline ({})",
			fixture.cache_name(),
			fixture.url()
		));
	}
	let out = Command::new("curl")
		.args([
			"-fsS",
			"--retry",
			"3",
			"--retry-delay",
			"2",
			"--max-time",
			"600",
			"-o",
		])
		.arg(dest)
		.arg(fixture.url())
		.output()
		.map_err(|e| {
			format!(
				"cannot run curl for {}: {e} ({})",
				fixture.cache_name(),
				fixture.url()
			)
		})?;
	if !out.status.success() {
		return Err(format!(
			"could not download {} from {}: curl {} {}",
			fixture.cache_name(),
			fixture.url(),
			out.status,
			String::from_utf8_lossy(&out.stderr).trim(),
		));
	}
	Ok(())
}

/// Returns a verified local path for `fixture`, downloading it if needed.
pub fn locate(fixture: &impl Pinned) -> Fixture {
	let path = cache_dir().join(fixture.cache_name());
	if path.exists() {
		// Verified on every run, not just after downloading: that is the
		// tamper-evident part.
		verify(fixture, &path);
		return Fixture::Ready(path);
	}

	if let Err(e) = fs::create_dir_all(cache_dir()) {
		return Fixture::Unavailable(format!(
			"cannot create fixture cache {}: {e}",
			cache_dir().display()
		));
	}

	// Download to a sibling and verify before it is allowed to become the
	// cached copy, so an interrupted download cannot poison a later run.
	let part = cache_dir().join(format!("{}.part", fixture.cache_name()));
	if let Err(why) = fetch(fixture, &part) {
		let _ = fs::remove_file(&part);
		return Fixture::Unavailable(why);
	}
	verify(fixture, &part);
	if let Err(e) = fs::rename(&part, &path) {
		let _ = fs::remove_file(&part);
		return Fixture::Unavailable(format!("cannot install {}: {e}", path.display()));
	}
	Fixture::Ready(path)
}

/// Calls `run` for every fixture that could be obtained, printing a line for
/// each one that could not. Returns the number skipped.
pub fn for_each_available<F: Pinned>(
	fixtures: &'static [F],
	mut run: impl FnMut(&'static F, &Path),
) -> usize {
	let mut skipped = 0;
	for fixture in fixtures {
		match locate(fixture) {
			Fixture::Ready(path) => run(fixture, &path),
			Fixture::Unavailable(why) => {
				eprintln!("SKIP {}: {why}", fixture.cache_name());
				skipped += 1;
			}
		}
	}
	skipped
}

/// A verified path for `fixture` if it is already cached, without ever going
/// to the network. For callers that want to use whatever is on disk but must
/// not turn a plain `cargo test` into a 1 GiB download.
///
/// Still verifies: a cached file that does not match its pin is a hard error
/// here too.
pub fn cached(fixture: &impl Pinned) -> Option<PathBuf> {
	let path = cache_dir().join(fixture.cache_name());
	path.exists().then(|| {
		verify(fixture, &path);
		path
	})
}

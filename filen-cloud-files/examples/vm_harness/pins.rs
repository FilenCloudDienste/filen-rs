//! The pin scenarios: a user's process pinning and unpinning items of the root (`attrib`, the
//! attributes Explorer's "Always keep on this device" and "Free up space" set) and asking the
//! platform to dehydrate files (`CfDehydratePlaceholder`), while `provide-local` serves it. Measured
//! (Windows 11, in the VM): the platform acts on neither attribute, so the provider
//! hydrates pinned items and frees unpinned ones itself. Every scenario works in the tree's `px`
//! directory or on `damaged.bin`-like files of its own. Runs after `remote` and before `check`.

use std::{
	fs::{self, File, OpenOptions},
	io::{self, Write},
	os::windows::fs::MetadataExt,
	process::Command,
	thread,
	time::{Duration, Instant},
};

use filen_cloud_files::test_support::user_dehydrate;
use serde_json::Value;

use crate::{
	local::{Scenarios, local_bytes, patched, served, wait},
	report::{Outcome, events, field, int},
	tree,
};

/// `FILE_ATTRIBUTE_OFFLINE`.
const OFFLINE: u32 = 0x1000;
/// `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`.
const RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
/// `FILE_ATTRIBUTE_PINNED`.
const PINNED: u32 = 0x8_0000;
/// `FILE_ATTRIBUTE_UNPINNED`.
const UNPINNED: u32 = 0x10_0000;
/// How long a scenario waits for the provider to bring an item to its pin: the watcher's
/// debounce, the job, a hydration.
const PINNED_WITHIN: Duration = Duration::from_secs(30);
/// How long a scenario waits for the provider to apply a change of the drive.
const APPLIED: Duration = Duration::from_secs(40);
/// How long after a read a file is left before a dehydration is asked for, so the provider's own
/// work on it (the hydration's events, the job's quiet window) is over.
const SETTLED: Duration = Duration::from_secs(12);
/// The control commands' first number, past any `local` and `remote` used.
const FIRST_COMMAND: u32 = 70_000;

pub(crate) fn run(root: &std::path::Path, out: &std::path::Path) -> Outcome {
	let mut scenarios = Scenarios::open(root, out, "pins.txt", FIRST_COMMAND)?;
	let result = run_all(&mut scenarios);
	if let Err(error) = &result {
		scenarios.report.fail("harness", error);
	}
	fs::write(out.join("pins.done"), "done")?;
	Ok(scenarios.report.passed())
}

fn run_all(s: &mut Scenarios) -> io::Result<()> {
	if !crate::report::wait_for(&s.out.join("provider.ready"), Duration::from_secs(60)) {
		s.check(
			"P0-ready",
			false,
			"the provider did not get ready within 60 s",
		);
		return Ok(());
	}
	list(s, "px")?;
	pin_file(s)?;
	pin_dir(s)?;
	pin_dir_recursive(s)?;
	new_under_pin(s);
	bump_pinned(s)?;
	free(s)?;
	keep_modified(s)?;
	user_dehydrations(s)?;
	pin_damaged(s)?;
	unpin_dir(s)?;
	dehydrate_held(s)?;
	mark(s);
	Ok(())
}

/// Lists the directory `path` as a user: populates it.
fn list(s: &Scenarios, path: &str) -> io::Result<Vec<String>> {
	let mut names: Vec<String> = fs::read_dir(s.path(path))?
		.map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
		.collect::<io::Result<_>>()?;
	names.sort();
	Ok(names)
}

/// The attributes of `path`, as a user sees them.
fn attributes(s: &Scenarios, path: &str) -> Option<u32> {
	fs::metadata(s.path(path))
		.ok()
		.map(|metadata| metadata.file_attributes())
}

/// Whether `bits` say the file's data is all on disk.
fn full(bits: u32) -> bool {
	bits & (OFFLINE | RECALL_ON_DATA_ACCESS) == 0
}

/// Runs `attrib` with `args` (each a flag or a full path) as the user; its output on failure.
fn attrib(args: &[&str]) -> Result<(), String> {
	let output = Command::new("attrib")
		.args(args)
		.output()
		.map_err(|error| error.to_string())?;
	let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
	if output.status.success() && text.is_empty() {
		Ok(())
	} else {
		Err(format!("{}: {text}", output.status))
	}
}

/// `attrib` of `flags` on the item `path` below the root.
fn attrib_on(s: &mut Scenarios, step: &str, flags: &[&str], path: &str) -> bool {
	let full = s.path(path);
	let mut args = flags.to_vec();
	let full = full.to_string_lossy().into_owned();
	args.push(&full);
	let result = attrib(&args);
	let ok = result.is_ok();
	if !ok {
		s.check(
			&format!("{step}-attrib"),
			false,
			format!("{flags:?} {path}: {result:?}"),
		);
	}
	ok
}

/// Waits until each of `paths` holds all its data on disk and, when `pinned`, still carries its
/// pin; checks it under `step` and that each reads back as the tree serves it.
fn expect_hydrated(s: &mut Scenarios, step: &str, paths: &[&str], pinned: bool) {
	let started = Instant::now();
	let done = wait(PINNED_WITHIN, || {
		paths
			.iter()
			.all(|path| {
				attributes(s, path)
					.is_some_and(|bits| full(bits) && (!pinned || bits & PINNED != 0))
			})
			.then_some(())
	});
	let states: Vec<String> = paths
		.iter()
		.map(|path| format!("{path}: {:x?}", attributes(s, path)))
		.collect();
	let bytes_ok = paths
		.iter()
		.all(|path| fs::read(s.path(path)).is_ok_and(|bytes| bytes == served(path)));
	s.check(
		step,
		done.is_some() && bytes_ok,
		format!(
			"hydrated {} after {:?}; {states:?}; bytes ok {bytes_ok}",
			done.is_some(),
			started.elapsed()
		),
	);
}

/// P1: `attrib +P` on a dehydrated file: the provider hydrates it whole.
fn pin_file(s: &mut Scenarios) -> io::Result<()> {
	let path = "px\\p1.bin";
	s.info("P1-before", format!("{:x?}", attributes(s, path)));
	if attrib_on(s, "P1", &["+P"], path) {
		expect_hydrated(s, "P1-pin-file-hydrated", &[path], true);
	}
	Ok(())
}

/// P2a: `attrib +P` on a directory alone, which marks no item in it: the provider hydrates every
/// file under it, in a populated subdirectory at once, and in one never listed once it is listed.
fn pin_dir(s: &mut Scenarios) -> io::Result<()> {
	list(s, "px\\pa")?;
	list(s, "px\\pa\\sub")?;
	if !attrib_on(s, "P2a", &["+P"], "px\\pa") {
		return Ok(());
	}
	expect_hydrated(
		s,
		"P2a-pin-dir-hydrated",
		&["px\\pa\\a.bin", "px\\pa\\sub\\b.bin"],
		false,
	);
	let lazy = attributes(s, "px\\pa\\lazy\\c.bin");
	s.info("P2a-lazy-before-listing", format!("{lazy:x?}"));
	let listed = list(s, "px\\pa\\lazy")?;
	s.info("P2a-lazy-listed", format!("{listed:?}"));
	expect_hydrated(
		s,
		"P2a-pin-dir-lazy-hydrated",
		&["px\\pa\\lazy\\c.bin"],
		false,
	);
	Ok(())
}

/// P2b: `attrib +P /S /D dir\*`, what a recursive pin does: marks every item below it, and lists
/// the unlisted directory itself; the provider hydrates every file.
fn pin_dir_recursive(s: &mut Scenarios) -> io::Result<()> {
	list(s, "px\\pr")?;
	list(s, "px\\pr\\sub")?;
	let pattern = format!("{}\\*", s.path("px\\pr").to_string_lossy());
	match attrib(&["+P", "/S", "/D", &pattern]) {
		Ok(()) => expect_hydrated(
			s,
			"P2b-pin-recursive-hydrated",
			&["px\\pr\\a.bin", "px\\pr\\sub\\b.bin", "px\\pr\\lazy\\c.bin"],
			true,
		),
		Err(error) => s.check("P2b-attrib", false, error),
	}
	Ok(())
}

/// P3: a file another device adds under the pinned `px\pa` arrives hydrated.
fn new_under_pin(s: &mut Scenarios) {
	let size = 9_100;
	if let Err(error) = s.control(&format!("add px\\pa new3.bin {size}")) {
		s.check("P3-add", false, error);
		return;
	}
	let path = "px\\pa\\new3.bin";
	let started = Instant::now();
	let done = wait(APPLIED, || attributes(s, path).filter(|bits| full(*bits)));
	let bytes = fs::read(s.path(path));
	s.check(
		"P3-new-under-pin-hydrated",
		done.is_some()
			&& bytes
				.as_ref()
				.is_ok_and(|bytes| *bytes == tree::added(size)),
		format!(
			"attributes {done:x?} after {:?}; read {:?}",
			started.elapsed(),
			bytes.map(|bytes| bytes.len())
		),
	);
}

/// P4: a pinned file another device changes ends pinned and hydrated with the new bytes.
fn bump_pinned(s: &mut Scenarios) -> io::Result<()> {
	let path = "px\\pin4.bin";
	if !attrib_on(s, "P4", &["+P"], path) {
		return Ok(());
	}
	expect_hydrated(s, "P4-pinned", &[path], true);
	let version = match s.control(&format!("bump {path}")) {
		Ok(version) => version,
		Err(error) => {
			s.check("P4-bump", false, error);
			return Ok(());
		}
	};
	let size = tree::size_of(path).expect("a file of the tree");
	let started = Instant::now();
	let done = wait(APPLIED, || {
		let seen = s.entry(path);
		let bits = attributes(s, path)?;
		(seen
			.identity("version")
			.is_some_and(|held| held.to_string() == version)
			&& full(bits)
			&& bits & PINNED != 0)
			.then_some(bits)
	});
	let bytes = fs::read(s.path(path));
	s.check(
		"P4-bumped-pinned-hydrated",
		done.is_some()
			&& bytes
				.as_ref()
				.is_ok_and(|bytes| *bytes == tree::bumped(size)),
		format!(
			"attributes {done:x?} after {:?}; entry {:?}; read ok {}",
			started.elapsed(),
			s.entry(path),
			bytes.is_ok_and(|bytes| bytes == tree::bumped(size))
		),
	);
	Ok(())
}

/// Whether the provider logged the local report `kind` naming `path`.
fn reported(s: &Scenarios, kind: &str, path: &str) -> bool {
	let prefix = format!("{kind} {{ path: RootPath({path:?})");
	s.reports().iter().any(|report| report.starts_with(&prefix))
}

/// Reads the tree's file `path` whole as a user, hydrating it, then leaves it to settle.
fn hydrate(s: &mut Scenarios, step: &str, path: &str) -> bool {
	let read = fs::read(s.path(path));
	let ok = read.as_ref().is_ok_and(|bytes| *bytes == served(path));
	s.check(
		&format!("{step}-hydrate"),
		ok,
		format!("{path}: {:?}", read.map(|bytes| bytes.len())),
	);
	thread::sleep(SETTLED);
	ok
}

/// P5: `attrib -P +U` on a clean hydrated file (Explorer's "Free up space"): the provider
/// dehydrates it and clears the unpin, so later reads behave as any other.
fn free(s: &mut Scenarios) -> io::Result<()> {
	let path = "px\\free.bin";
	if !hydrate(s, "P5", path) || !attrib_on(s, "P5", &["-P", "+U"], path) {
		return Ok(());
	}
	let started = Instant::now();
	let done = wait(PINNED_WITHIN, || {
		attributes(s, path).filter(|bits| !full(*bits) && bits & UNPINNED == 0)
	});
	s.check(
		"P5-freed",
		done.is_some() && reported(s, "Freed", path),
		format!(
			"attributes {:x?} after {:?}; freed reported {}",
			attributes(s, path),
			started.elapsed(),
			reported(s, "Freed", path)
		),
	);
	Ok(())
}

/// P6: a user's edit, then `attrib +U`: the provider keeps the bytes, uploads them as a new
/// version and reports the file kept.
fn keep_modified(s: &mut Scenarios) -> io::Result<()> {
	let path = "px\\keep.bin";
	if !hydrate(s, "P6", path) {
		return Ok(());
	}
	let patch = local_bytes(512, 6);
	{
		let mut file = OpenOptions::new().write(true).open(s.path(path))?;
		file.write_all(&patch)?;
	}
	let bytes = patched(served(path), 0, &patch);
	if !attrib_on(s, "P6", &["+U"], path) {
		return Ok(());
	}
	let carried = s.expect_carried("P6-uploaded", path, &bytes);
	thread::sleep(Duration::from_secs(8));
	let bits = attributes(s, path);
	let kept = reported(s, "KeptDownloaded", path);
	s.check(
		"P6-kept",
		carried.is_some() && bits.is_some_and(full) && kept,
		format!("attributes {bits:x?}; kept reported {kept}"),
	);
	Ok(())
}

/// The Win32 error in a failed HRESULT.
fn win32(hr: i32) -> u32 {
	hr.cast_unsigned() & 0xFFFF
}

/// P7: `CfDehydratePlaceholder` from this process. On a clean file the provider allows it and
/// dehydrates the file, which ends the call; on one appended to through an append-only handle,
/// which stays marked in sync with bytes written, it is refused and the bytes stay.
fn user_dehydrations(s: &mut Scenarios) -> io::Result<()> {
	let path = "px\\udh.bin";
	if hydrate(s, "P7a", path) {
		let started = Instant::now();
		let hr = user_dehydrate(&s.path(path)).get();
		let elapsed = started.elapsed();
		let bits = attributes(s, path);
		s.check(
			"P7a-user-dehydrate-clean",
			hr == 0 && bits.is_some_and(|bits| !full(bits)),
			format!(
				"hr {:#010x} in {elapsed:?}; attributes {bits:x?}",
				hr.cast_unsigned()
			),
		);
	}
	let path = "px\\udha.bin";
	if hydrate(s, "P7b", path) {
		let extra = local_bytes(100, 7);
		{
			let mut file = OpenOptions::new().append(true).open(s.path(path))?;
			file.write_all(&extra)?;
		}
		let seen = s.entry(path);
		let started = Instant::now();
		let hr = user_dehydrate(&s.path(path)).get();
		let elapsed = started.elapsed();
		let mut wanted = served(path);
		wanted.extend_from_slice(&extra);
		let bytes_kept = fs::read(s.path(path)).is_ok_and(|bytes| bytes == wanted);
		s.check(
			"P7b-user-dehydrate-appended-refused",
			hr != 0 && bytes_kept,
			format!(
				"hr {:#010x} (win32 {}) in {elapsed:?}; bytes kept {bytes_kept}; state before {seen:?}",
				hr.cast_unsigned(),
				win32(hr)
			),
		);
	}
	Ok(())
}

/// The `fetch_data` callbacks raised for files of `size` bytes, any process.
fn fetches_of_size(events: &[Value], size: usize) -> usize {
	let size = i64::try_from(size).expect("a tree file is a few MiB");
	events
		.iter()
		.filter(|event| {
			field(event, "message") == Some("cf callback")
				&& field(event, "callback") == Some("fetch_data")
				&& int(event, "file_size") == Some(size)
		})
		.count()
}

/// P9: `attrib +P` on a file whose content does not match its declared hash: the provider
/// hydrates it, finds the damage, dehydrates it, keeps it pinned, and never hydrates it again.
fn pin_damaged(s: &mut Scenarios) -> io::Result<()> {
	let path = "px\\pdam.bin";
	let size = tree::size_of(path).expect("a file of the tree");
	if !attrib_on(s, "P9", &["+P"], path) {
		return Ok(());
	}
	let started = Instant::now();
	let prefix = format!("Damaged {{ path: RootPath({path:?})");
	let report = wait(PINNED_WITHIN, || {
		s.reports()
			.into_iter()
			.find(|report| report.starts_with(&prefix))
	});
	thread::sleep(Duration::from_secs(1));
	let bits = attributes(s, path);
	s.check(
		"P9-pinned-damaged-dehydrated",
		report.is_some() && bits.is_some_and(|bits| !full(bits) && bits & PINNED != 0),
		format!(
			"report {report:?} after {:?}; attributes {bits:x?}",
			started.elapsed()
		),
	);
	let before = fetches_of_size(&events(&s.out), size);
	thread::sleep(Duration::from_secs(20));
	let after = fetches_of_size(&events(&s.out), size);
	s.check(
		"P9-not-hydrated-again",
		before == after && attributes(s, path).is_some_and(|bits| !full(bits)),
		format!(
			"fetches {before} then {after} 20 s later; attributes {:x?}",
			attributes(s, path)
		),
	);
	Ok(())
}

/// P10: `attrib +U` on a directory alone, with a file pinned of its own in it: the provider frees
/// every other file under it, a subdirectory's included, keeps the pinned one, and clears the
/// unpins it set, the directory's own included.
fn unpin_dir(s: &mut Scenarios) -> io::Result<()> {
	list(s, "px\\pu")?;
	list(s, "px\\pu\\sub")?;
	let freed = ["px\\pu\\a.bin", "px\\pu\\sub\\b.bin"];
	let kept = "px\\pu\\kept.bin";
	for path in freed.iter().chain([&kept]) {
		if !hydrate(s, "P10", path) {
			return Ok(());
		}
	}
	if !attrib_on(s, "P10", &["+P"], kept) || !attrib_on(s, "P10", &["+U"], "px\\pu") {
		return Ok(());
	}
	let started = Instant::now();
	let done = wait(PINNED_WITHIN, || {
		let freed = freed.iter().all(|path| {
			attributes(s, path).is_some_and(|bits| !full(bits) && bits & UNPINNED == 0)
		});
		let cleared = ["px\\pu", "px\\pu\\sub"]
			.iter()
			.all(|dir| attributes(s, dir).is_some_and(|bits| bits & UNPINNED == 0));
		(freed && cleared).then_some(())
	});
	let kept_bits = attributes(s, kept);
	let states: Vec<String> = ["px\\pu", "px\\pu\\sub"]
		.iter()
		.chain(freed.iter())
		.map(|path| format!("{path}: {:x?}", attributes(s, path)))
		.collect();
	s.check(
		"P10-unpin-dir-freed",
		done.is_some() && kept_bits.is_some_and(|bits| full(bits) && bits & PINNED != 0),
		format!(
			"freed and cleared {} after {:?}; {states:?}; kept {kept_bits:x?}",
			done.is_some(),
			started.elapsed()
		),
	);
	Ok(())
}

/// P11: `CfDehydratePlaceholder` while another handle of the user's holds the file open for
/// reading for 400 ms: the call returns soon either way, dehydrated with hr 0 or refused with the
/// bytes kept, never left waiting for the platform's time limit. Measured (2026-11-12, x64 and
/// ARM64): the platform refuses it itself with `ERROR_CLOUD_FILE_IN_USE` once the handle closes,
/// without asking the provider.
fn dehydrate_held(s: &mut Scenarios) -> io::Result<()> {
	let path = "px\\uheld.bin";
	if !hydrate(s, "P11", path) {
		return Ok(());
	}
	let reader = File::open(s.path(path))?;
	let target = s.path(path);
	let started = Instant::now();
	let call = thread::spawn(move || user_dehydrate(&target).get());
	thread::sleep(Duration::from_millis(400));
	drop(reader);
	let ended = wait(Duration::from_secs(10), || call.is_finished().then_some(()));
	let elapsed = started.elapsed();
	// A call still blocked is left to the platform's time limit; its thread ends with the process.
	let hr = ended.and_then(|()| call.join().ok());
	let bits = attributes(s, path);
	let bytes_kept = fs::read(s.path(path)).is_ok_and(|bytes| bytes == served(path));
	s.check(
		"P11-user-dehydrate-held-answered",
		match hr {
			Some(0) => bits.is_some_and(|bits| !full(bits)),
			Some(_) => bytes_kept,
			None => false,
		},
		format!(
			"hr {:x?} in {elapsed:?}; attributes {bits:x?}; bytes kept {bytes_kept}",
			hr.map(i32::cast_unsigned)
		),
	);
	Ok(())
}

/// Waits for the provider to go idle, then records the count of drive writes so far for `quiet`,
/// which checks the reader's scenarios added none.
fn mark(s: &mut Scenarios) {
	thread::sleep(Duration::from_secs(10));
	let writes = s.fresh_remote().writes().len();
	if let Err(error) = fs::write(s.out.join("ops-mark.txt"), writes.to_string()) {
		s.check("P-mark", false, error);
	}
	s.info("P-writes", writes);
}

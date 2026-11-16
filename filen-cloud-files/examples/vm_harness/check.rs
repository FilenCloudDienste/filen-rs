//! The reader: the scenarios a user's process sees through the sync root, each checked against the
//! tree and, where the answer is the provider's, against its events.

use std::{
	collections::BTreeSet,
	env, fs,
	fs::{File, OpenOptions},
	io::{self, Read, Seek, SeekFrom},
	os::windows::fs::{MetadataExt, OpenOptionsExt},
	path::Path,
	process::Command,
	thread,
	time::{Duration, Instant},
};

use filen_cloud_files::test_support::pattern;
use serde_json::Value;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

use crate::{
	report::{Outcome, Report, events, field, int, names_path, wait_for},
	stall::{Budget, Timer},
	tree::{self, EMPTYDIR},
};

/// `FILE_ATTRIBUTE_OFFLINE`.
const OFFLINE: u32 = 0x1000;
/// `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`.
const RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;

/// The names in `dir`, sorted.
fn names(dir: &Path) -> io::Result<Vec<String>> {
	let mut names = fs::read_dir(dir)?
		.map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
		.collect::<io::Result<Vec<_>>>()?;
	names.sort();
	Ok(names)
}

/// The tree's file at `path`, as its bytes.
fn expected(path: &str) -> Vec<u8> {
	pattern(tree::size_of(path).expect("a file of the tree"))
}

/// `len` bytes of the file at `path`, from `offset`.
fn read_range(path: &Path, offset: u64, len: usize) -> io::Result<Vec<u8>> {
	let mut file = File::open(path)?;
	file.seek(SeekFrom::Start(offset))?;
	let mut bytes = vec![0; len];
	file.read_exact(&mut bytes)?;
	Ok(bytes)
}

/// The provider's process id, which it logs once ready.
pub(crate) fn provider_pid(events: &[Value]) -> Option<i64> {
	events
		.iter()
		.find(|event| field(event, "message") == Some("provider pid"))
		.and_then(|event| int(event, "pid"))
}

/// Whether `event` is a `fetch_data` callback another process raised: the provider's own
/// requests, its completions and checks of what readers hydrated, are not the readers'.
fn user_fetch(event: &Value, provider: Option<i64>) -> bool {
	field(event, "message") == Some("cf callback")
		&& field(event, "callback") == Some("fetch_data")
		&& (provider.is_none() || int(event, "pid") != provider)
}

/// The `fetch_data` callbacks other processes raised for files of `size` bytes: their transfer
/// keys.
pub(crate) fn transfers_of_size(events: &[Value], size: usize) -> BTreeSet<i64> {
	let size = i64::try_from(size).expect("a tree file is a few MiB");
	let provider = provider_pid(events);
	events
		.iter()
		.filter(|event| user_fetch(event, provider) && int(event, "file_size") == Some(size))
		.filter_map(|event| int(event, "transfer"))
		.collect()
}

/// How many `fetch_data` callbacks other processes raised, for files of `size` bytes when given.
fn user_fetches(events: &[Value], size: Option<usize>) -> usize {
	let provider = provider_pid(events);
	let size = size.map(|size| i64::try_from(size).expect("a tree file is a few MiB"));
	events
		.iter()
		.filter(|event| {
			user_fetch(event, provider) && (size.is_none() || int(event, "file_size") == size)
		})
		.count()
}

/// The local reports the provider logged so far, as their `Debug` text.
fn local_reports(events: &[Value]) -> Vec<&str> {
	events
		.iter()
		.filter(|event| field(event, "message") == Some("on_local_report"))
		.filter_map(|event| field(event, "report"))
		.collect()
}

/// Whether the provider reported the file `name` damaged: `Some(true)` once its bytes were
/// dehydrated, `Some(false)` when it parked the file with its damaged bytes still on disk.
fn damage_reported(events: &[Value], name: &str) -> Option<bool> {
	let damaged = format!("Damaged {{ path: RootPath({name:?})");
	let parked = format!("Parked {{ path: RootPath({name:?})");
	let reports = local_reports(events);
	if reports.iter().any(|report| report.starts_with(&damaged)) {
		return Some(true);
	}
	reports
		.iter()
		.any(|report| report.starts_with(&parked) && report.contains("ContentDamaged"))
		.then_some(false)
}

/// The outcomes the hydration job logged for the file `name`, in order.
fn settled(events: &[Value], name: &str) -> Vec<String> {
	events
		.iter()
		.filter(|event| {
			field(event, "message") == Some("hydration settled") && names_path(events, event, name)
		})
		.filter_map(|event| field(event, "outcome").map(str::to_owned))
		.collect()
}

/// The events of `callback` with the message `message` under one of `transfers`.
fn under<'a>(
	events: &'a [Value],
	transfers: &'a BTreeSet<i64>,
	message: &'a str,
	callback: &'a str,
) -> impl Iterator<Item = &'a Value> + 'a {
	events.iter().filter(move |event| {
		field(event, "message") == Some(message)
			&& field(event, "callback") == Some(callback)
			&& int(event, "transfer").is_some_and(|key| transfers.contains(&key))
	})
}

/// The answers logged under `transfers`, in order, as `answer[/dehydrated]` strings.
pub(crate) fn answers(events: &[Value], transfers: &BTreeSet<i64>) -> Vec<String> {
	under(events, transfers, "cf answer", "fetch_data")
		.map(|event| {
			let answer = field(event, "answer").unwrap_or("?");
			match event.get("dehydrated").and_then(Value::as_bool) {
				Some(dehydrated) => format!("{answer}/dehydrated={dehydrated}"),
				None => answer.to_owned(),
			}
		})
		.collect()
}

/// Reads the whole of `path`, for the cancel scenario: started as a child and killed.
pub(crate) fn read_one(path: &Path) -> Outcome {
	let read = fs::read(path);
	println!(
		"read-one {}: {:?}",
		path.display(),
		read.map(|bytes| bytes.len())
	);
	Ok(true)
}

/// Reads `len` bytes of `path` at `offset` and writes the outcome to `file`: `ok <ms> <blake3>`
/// or `err <ms> <os error>`. For the overlap and abandon scenarios, started as a child.
pub(crate) fn read_to(path: &Path, offset: u64, len: usize, file: &Path) -> Outcome {
	let started = Timer::start();
	let read = read_range(path, offset, len);
	// The time this process ran: a stopped guest is no slow provider (see `stall`).
	let ms = started.running().as_millis();
	let line = match read {
		Ok(bytes) => format!("ok {ms} {}", blake3::hash(&bytes).to_hex()),
		Err(error) => format!("err {ms} {:?}", error.raw_os_error()),
	};
	fs::write(file, line)?;
	Ok(true)
}

pub(crate) fn run(root: &Path, out: &Path) -> Outcome {
	let mut report = Report::create(&out.join("check.txt"))?;
	if !wait_for(&out.join("provider.ready"), Duration::from_secs(60)) {
		report.fail("ready", "the provider did not get ready within 60 s");
		fs::write(out.join("check.done"), "done")?;
		return Ok(false);
	}
	let mut hydrated = Vec::new();
	listing(&mut report, root, out);
	whole_reads(&mut report, root, &mut hydrated);
	range_reads(&mut report, root, &mut hydrated);
	sequential(&mut report, root, out, &mut hydrated);
	concurrency(&mut report, root, &mut hydrated);
	cancel(&mut report, root, out)?;
	damaged(&mut report, root, out);
	damaged_locked(&mut report, root, out);
	truncated(&mut report, root);
	let partial = hydration_state(&mut report, root, out, &mut hydrated);
	damaged_seq(&mut report, root, out);
	held_open(&mut report, root, out);
	overlapping_cancel(&mut report, root, out)?;
	start_abandoned_read(&mut report, root, out)?;
	fs::write(out.join("hydrated.txt"), hydrated.join("\n"))?;
	fs::write(out.join("partial.txt"), partial.join("\n"))?;
	report.info("hydrated", hydrated.join(","));
	fs::write(out.join("check.done"), "done")?;
	Ok(report.passed())
}

/// 1 and 2: the listings, the skipped names, and how often `emptydir` is asked for.
fn listing(report: &mut Report, root: &Path, out: &Path) {
	let started = Instant::now();
	match names(root) {
		Ok(found) => report.check(
			"1-list-root",
			found == tree::expected_root(),
			format!("{found:?} in {:?}", started.elapsed()),
		),
		Err(error) => report.fail(
			"1-list-root",
			format!("{error} (os error {:?})", error.raw_os_error()),
		),
	}
	// The skip reports follow the population's answer.
	let mut budget = Budget::new(Duration::from_secs(5));
	let skipped = loop {
		let skipped: BTreeSet<String> = events(out)
			.iter()
			.filter(|event| field(event, "message") == Some("on_skipped_items"))
			.filter_map(|event| field(event, "name").map(str::to_owned))
			.collect();
		if skipped.len() >= tree::expected_skipped().len() || budget.spent() {
			break skipped;
		}
		thread::sleep(Duration::from_millis(250));
	};
	let wanted: BTreeSet<String> = tree::expected_skipped()
		.into_iter()
		.map(str::to_owned)
		.collect();
	report.check("1-skipped", skipped == wanted, format!("{skipped:?}"));

	for (dir, wanted) in [
		("sub", vec!["deeper", "inner.bin"]),
		("sub\\deeper", vec!["deep.bin"]),
		("emptydir", vec![]),
		("late", vec!["late.bin"]),
	] {
		match names(&root.join(dir)) {
			Ok(found) => report.check(
				&format!("2-list-{dir}"),
				found == wanted,
				format!("{found:?}"),
			),
			Err(error) => report.fail(&format!("2-list-{dir}"), error),
		}
	}
	let again = names(&root.join("emptydir"));
	let emptydir = EMPTYDIR.to_string();
	let asked = events(out)
		.iter()
		.filter(|event| {
			field(event, "message") == Some("populated a directory")
				&& field(event, "directory").is_some_and(|dir| dir.contains(&emptydir))
		})
		.count();
	report.info(
		"2-emptydir-populations",
		format!("{asked} over two listings (expected 1); second listing {again:?}"),
	);
}

/// 3: whole reads, byte for byte.
fn whole_reads(report: &mut Report, root: &Path, hydrated: &mut Vec<String>) {
	// `big.bin` is read whole after its ranges, in step 4.
	for name in ["small.txt", "hello.bin", "exact.bin", "empty.txt"] {
		let started = Instant::now();
		match fs::read(root.join(name)) {
			Ok(bytes) => {
				let ok = bytes == expected(name);
				report.check(
					&format!("3-read-{name}"),
					ok,
					format!("{} bytes in {:?}", bytes.len(), started.elapsed()),
				);
				if ok {
					hydrated.push(name.to_owned());
				}
			}
			Err(error) => report.fail(&format!("3-read-{name}"), format!("{error}")),
		}
	}
}

/// 4: range reads of `big.bin`, across a chunk boundary, unaligned, at EOF, then the whole file.
fn range_reads(report: &mut Report, root: &Path, hydrated: &mut Vec<String>) {
	let path = root.join("big.bin");
	let whole = expected("big.bin");
	for (offset, len) in [
		(1_048_476_u64, 200_usize),
		(12_293, 10),
		(5_242_953, 50),
		(3_000_000, 1000),
	] {
		let step = format!("4-range-{offset}+{len}");
		let start = usize::try_from(offset).expect("a tree file is a few MiB");
		match read_range(&path, offset, len) {
			Ok(bytes) => report.check(
				&step,
				whole.get(start..start + len) == Some(&bytes[..]),
				"compared",
			),
			Err(error) => report.fail(&step, error),
		}
	}
	// The whole file: the OS asks only for the ranges not yet on disk.
	match fs::read(&path) {
		Ok(bytes) => {
			let ok = bytes == whole;
			report.check("4-whole-big.bin", ok, format!("{} bytes", bytes.len()));
			if ok {
				hydrated.push("big.bin".to_owned());
			}
		}
		Err(error) => report.fail("4-whole-big.bin", error),
	}
}

/// Reads the whole of `path` in 4096-byte reads.
fn read_in_steps(path: &Path) -> io::Result<Vec<u8>> {
	let mut file = File::open(path)?;
	let mut bytes = Vec::new();
	let mut buffer = [0; 4096];
	loop {
		let read = file.read(&mut buffer)?;
		if read == 0 {
			return Ok(bytes);
		}
		bytes.extend_from_slice(buffer.get(..read).unwrap_or_default());
	}
}

/// 5: sequential 4096-byte reads: a small file, which the provider hydrates whole, and one of
/// several chunks, which the hydration job checks once it is all on disk.
fn sequential(report: &mut Report, root: &Path, out: &Path, hydrated: &mut Vec<String>) {
	for name in ["sub\\inner.bin", "seq.bin"] {
		match read_in_steps(&root.join(name)) {
			Ok(bytes) => {
				let ok = bytes == expected(name);
				report.check(
					&format!("5-sequential-{name}"),
					ok,
					format!("{} bytes", bytes.len()),
				);
				if ok {
					hydrated.push(name.to_owned());
				}
			}
			Err(error) => report.fail(&format!("5-sequential-{name}"), error),
		}
	}
	thread::sleep(Duration::from_millis(500));
	let events = events(out);
	let size = tree::size_of("seq.bin").expect("a file of the tree");
	let keys: BTreeSet<String> = transfers_of_size(&events, size)
		.into_iter()
		.map(|transfer| format!("TransferKey({transfer})"))
		.collect();
	let verifications: Vec<&str> = events
		.iter()
		.filter(|event| {
			field(event, "message") == Some("hydrated a range")
				&& field(event, "key").is_some_and(|key| keys.contains(key))
		})
		.filter_map(|event| field(event, "verification"))
		.collect();
	// Not a pass condition: a read across several requests is checked by the hydration job once
	// the file is all on disk, not as the windows arrive.
	report.info(
		"5-sequential-verified",
		format!(
			"seq.bin: {} transfer keys; verifications {verifications:?}",
			keys.len()
		),
	);
	match fs::read(root.join("sub\\deeper\\deep.bin")) {
		Ok(bytes) => {
			let ok = bytes == expected("sub\\deeper\\deep.bin");
			report.check("5-deep", ok, format!("{} bytes", bytes.len()));
			if ok {
				hydrated.push("sub\\deeper\\deep.bin".to_owned());
			}
		}
		Err(error) => report.fail("5-deep", error),
	}
}

/// 6: eight concurrent whole reads, and a listing and small read that must not starve behind
/// them.
fn concurrency(report: &mut Report, root: &Path, hydrated: &mut Vec<String>) {
	let started = Timer::start();
	let readers: Vec<_> = (0..8)
		.map(|n| {
			let name = format!("par{n}.bin");
			let path = root.join(&name);
			thread::spawn(move || (name, fs::read(path)))
		})
		.collect();
	thread::sleep(Duration::from_millis(100));
	let late = Timer::start();
	let listed = names(&root.join("late"));
	let read = fs::read(root.join("late\\late.bin"));
	let late = late.running();
	let ok = listed.as_ref().is_ok_and(|names| names == &["late.bin"])
		&& read
			.as_ref()
			.is_ok_and(|bytes| *bytes == expected("late\\late.bin"));
	report.check(
		"6-late",
		ok && late < Duration::from_secs(10),
		format!(
			"{late:?}; listed {listed:?}; read {:?}",
			read.map(|bytes| bytes.len())
		),
	);
	if ok {
		hydrated.push("late\\late.bin".to_owned());
	}
	for reader in readers {
		match reader.join() {
			Ok((name, Ok(bytes))) => {
				let ok = bytes == expected(&name);
				report.check(&format!("6-{name}"), ok, format!("{} bytes", bytes.len()));
				if ok {
					hydrated.push(name);
				}
			}
			Ok((name, Err(error))) => report.fail(&format!("6-{name}"), error),
			Err(_) => report.fail("6-reader", "a reader thread panicked"),
		}
	}
	let total = started.running();
	report.check(
		"6-total",
		total < Duration::from_secs(120),
		format!("{total:?}"),
	);
}

/// 7: a read cancelled by killing its process, and a read closed early.
fn cancel(report: &mut Report, root: &Path, out: &Path) -> Outcome {
	let size = tree::size_of("cancel.bin").expect("a file of the tree");
	let mut child = Command::new(env::current_exe()?)
		.args(["read-one".as_ref(), root.join("cancel.bin").as_os_str()])
		.spawn()?;
	thread::sleep(Duration::from_secs(2));
	child.kill()?;
	let status = child.wait()?;
	let killed = Timer::start();
	let mut budget = Budget::new(Duration::from_secs(85));
	report.info("7-child", format!("killed, {status}"));
	let (mut cancelled, mut answered) = (false, Vec::new());
	// The pass budget is 10 s; up to 75 s more shows what the OS does instead.
	while !budget.spent() {
		let events = events(out);
		let transfers = transfers_of_size(&events, size);
		cancelled = under(&events, &transfers, "cf callback", "cancel_fetch_data")
			.next()
			.is_some();
		answered = answers(&events, &transfers);
		if cancelled && !answered.is_empty() {
			break;
		}
		thread::sleep(Duration::from_millis(250));
	}
	let waited = killed.running();
	let events = events(out);
	let transfers = transfers_of_size(&events, size);
	let cancels: Vec<String> = under(&events, &transfers, "cf callback", "cancel_fetch_data")
		.map(|event| {
			format!(
				"[{}+{} flags {}]",
				int(event, "offset").unwrap_or(-1),
				int(event, "length").unwrap_or(-1),
				int(event, "flags").unwrap_or(-1)
			)
		})
		.collect();
	report.check(
		"7-cancel",
		cancelled
			&& waited < Duration::from_secs(10)
			&& answered.iter().all(|answer| answer == "cancelled")
			&& !answered.is_empty(),
		format!(
			"{} fetches; cancels {cancels:?}; answers {answered:?}; after {waited:?}",
			transfers.len()
		),
	);

	// Open, read 4096 bytes, close: does the OS cancel the rest of what it asked for?
	let file = tree::size_of("openclose.bin").expect("a file of the tree");
	let read = read_range(&root.join("openclose.bin"), 0, 4096);
	thread::sleep(Duration::from_secs(3));
	let events = crate::report::events(out);
	let after = transfers_of_size(&events, file);
	let cancels = under(&events, &after, "cf callback", "cancel_fetch_data").count();
	report.info(
		"7-open-read-close",
		format!(
			"openclose.bin: read {:?}; {} fetches; {cancels} cancels",
			read.map(|bytes| bytes.len()),
			after.len()
		),
	);
	Ok(true)
}

/// The attributes of `path` that say its data is not on disk.
fn offline(path: &Path) -> Option<u32> {
	fs::metadata(path)
		.ok()
		.map(|metadata| metadata.file_attributes() & (OFFLINE | RECALL_ON_DATA_ACCESS))
}

/// `ERROR_CLOUD_FILE_VALIDATION_FAILED`: what a read of damaged content fails with.
const VALIDATION_FAILED: i32 = 383;
/// How long the hydration job has to dehydrate damaged content once it was found.
const DAMAGE_DEHYDRATED: Duration = Duration::from_secs(15);

/// Waits up to `limit` until `path` has its data off the disk: the offline bits it then shows.
fn wait_offline(path: &Path, limit: Duration) -> Option<u32> {
	let mut budget = Budget::new(limit);
	loop {
		let bits = offline(path);
		if bits.is_some_and(|bits| bits != 0) || budget.spent() {
			return bits;
		}
		thread::sleep(Duration::from_millis(250));
	}
}

/// 8: a file whose content does not match its declared hash fails the read; the hydration job
/// then dehydrates what the read left on disk and reports it, and every later read fails with
/// `ERROR_CLOUD_FILE_VALIDATION_FAILED` without reading. A file of a single piece leaves nothing
/// on disk (the core withholds the piece it cannot check), so only a larger one is reported.
fn damaged(report: &mut Report, root: &Path, out: &Path) {
	for name in ["damaged.bin", "damaged-big.bin"] {
		let path = root.join(name);
		let size = tree::size_of(name).expect("a file of the tree");
		let first = fs::read(&path);
		report.check(
			&format!("8-{name}-read"),
			first.is_err(),
			format!(
				"{:?}",
				first.as_ref().map(Vec::len).map_err(|e| e.raw_os_error())
			),
		);
		let started = Instant::now();
		let attributes = wait_offline(&path, DAMAGE_DEHYDRATED);
		let expect_report = size > 1_048_576;
		let reported = if expect_report {
			crate::local::wait(DAMAGE_DEHYDRATED, || damage_reported(&events(out), name))
		} else {
			damage_reported(&events(out), name)
		};
		report.check(
			&format!("8-{name}-dehydrated"),
			attributes.is_some_and(|bits| bits != 0)
				&& reported != Some(false)
				&& (!expect_report || reported == Some(true)),
			format!(
				"offline bits {attributes:x?} after {:?}; damage reported {reported:?}; settled {:?}",
				started.elapsed(),
				settled(&events(out), name)
			),
		);
		let second = fs::read(&path);
		report.check(
			&format!("8-{name}-second-read"),
			second
				.as_ref()
				.is_err_and(|error| error.raw_os_error() == Some(VALIDATION_FAILED)),
			format!(
				"{:?}",
				second.as_ref().map(Vec::len).map_err(|e| e.raw_os_error())
			),
		);
		let transfers = transfers_of_size(&events(out), size);
		report.info(
			&format!("8-{name}-answers"),
			format!("{:?}", answers(&events(out), &transfers)),
		);
	}
}

/// 8b: as 8, with the reader's handle shared for reading only and held for 3 s after the failed
/// read: the job's dehydration is refused as in use until the handle closes, and retried.
fn damaged_locked(report: &mut Report, root: &Path, out: &Path) {
	let name = "damaged-locked.bin";
	let path = root.join(name);
	let read = OpenOptions::new()
		.read(true)
		.share_mode(FILE_SHARE_READ)
		.open(&path)
		.and_then(|mut file| {
			let mut bytes = Vec::new();
			let read = file.read_to_end(&mut bytes);
			thread::sleep(Duration::from_secs(3));
			read
		});
	report.check(
		&format!("8-{name}-read"),
		read.is_err(),
		format!("{:?}", read.map_err(|e| e.raw_os_error())),
	);
	let started = Instant::now();
	let reported = crate::local::wait(Duration::from_secs(20), || {
		damage_reported(&events(out), name)
	});
	let attributes = wait_offline(&path, Duration::from_secs(5));
	report.check(
		&format!("8-{name}-dehydrated"),
		reported == Some(true) && attributes.is_some_and(|bits| bits != 0),
		format!(
			"damage reported {reported:?} after {:?}; offline bits {attributes:x?}",
			started.elapsed()
		),
	);
}

/// 9: a file the server holds fewer bytes of than its size fails a whole read, while the bytes
/// it has still read.
fn truncated(report: &mut Report, root: &Path) {
	let path = root.join("truncated.bin");
	let whole = fs::read(&path);
	report.check(
		"9-truncated-whole",
		whole.is_err(),
		format!(
			"{:?}",
			whole.as_ref().map(Vec::len).map_err(|e| e.raw_os_error())
		),
	);
	let head = read_range(&path, 0, 49_152);
	let ok = head
		.as_ref()
		.is_ok_and(|bytes| expected("truncated.bin").get(..49_152) == Some(&bytes[..]));
	report.check(
		"9-truncated-head",
		ok,
		format!(
			"{:?}",
			head.as_ref().map(Vec::len).map_err(|e| e.raw_os_error())
		),
	);
}

/// Whether `path` has data off the disk: a placeholder not fully hydrated.
fn partly_hydrated(path: &Path) -> bool {
	fs::metadata(path)
		.is_ok_and(|metadata| metadata.file_attributes() & (OFFLINE | RECALL_ON_DATA_ACCESS) != 0)
}

/// How long the hydration job has to complete what readers left partly on disk: its quiet window
/// of 5 s, a completion, and a margin.
const SETTLE: Duration = Duration::from_secs(30);

/// 10: the state each fully read file is left in once the provider settled it, its attributes,
/// and whether reading it again asks the provider for anything. Moves every file still marked
/// `RECALL_ON_DATA_ACCESS` out of `hydrated` and returns them. Measured: a read whose last
/// transfer cfapi refused with `ERROR_NOT_A_CLOUD_FILE` still returned every byte to the reader;
/// the provider's own handle held over each request's transfers prevents that, and the hydration
/// job completes a file left so anyway.
fn hydration_state(
	report: &mut Report,
	root: &Path,
	out: &Path,
	hydrated: &mut Vec<String>,
) -> Vec<String> {
	let at_first: Vec<&String> = hydrated
		.iter()
		.filter(|name| partly_hydrated(&root.join(name)))
		.collect();
	report.info(
		"10-partial-before-settling",
		format!("{} of {}: {at_first:?}", at_first.len(), hydrated.len()),
	);
	let started = Instant::now();
	let settled_in = crate::local::wait(SETTLE, || {
		hydrated
			.iter()
			.all(|name| !partly_hydrated(&root.join(name)))
			.then(|| started.elapsed())
	});
	report.info("10-settled", format!("{settled_in:?} (limit {SETTLE:?})"));
	let mut partial = Vec::new();
	for name in hydrated.iter() {
		let path = root.join(name);
		let attributes = fs::metadata(&path).map(|metadata| metadata.file_attributes());
		if attributes
			.as_ref()
			.is_ok_and(|bits| bits & RECALL_ON_DATA_ACCESS != 0)
		{
			partial.push(name.clone());
		}
		let before = user_fetches(&events(out), None);
		let again = fs::read(&path).map(|bytes| bytes.len());
		thread::sleep(Duration::from_millis(200));
		let asked = user_fetches(&events(out), None) - before;
		report.info(
			&format!("10-state-{name}"),
			format!(
				"attributes {attributes:x?}; read again {again:?} with {asked} new fetches; settled \
				 {:?}",
				settled(&events(out), name)
			),
		);
	}
	hydrated.retain(|name| !partial.contains(name));
	report.check(
		"10-fully-hydrated",
		partial.is_empty(),
		format!("left partly hydrated after a whole read: {partial:?}"),
	);
	verified(report, out);
	partial
}

/// 10b: every file read across several requests, or read whole while others were, was checked
/// against its declared hash: by the hydration job once all on disk, or by the dispatcher when a
/// single request covered it.
fn verified(report: &mut Report, out: &Path) {
	let par: Vec<String> = (0..8).map(|n| format!("par{n}.bin")).collect();
	let hash_checked = |events: &[Value], name: &str| {
		let keys: BTreeSet<String> =
			transfers_of_size(events, tree::size_of(name).expect("a file of the tree"))
				.into_iter()
				.map(|transfer| format!("TransferKey({transfer})"))
				.collect();
		events
			.iter()
			.filter(|event| {
				field(event, "message") == Some("hydrated a range")
					&& field(event, "verification") == Some("HashChecked")
					&& field(event, "key").is_some_and(|key| keys.contains(key))
			})
			.count()
	};
	let unverified = |events: &[Value]| {
		let job = |name: &str| {
			settled(events, name)
				.iter()
				.any(|outcome| outcome == "verified")
		};
		let mut missing: Vec<String> = ["big.bin", "seq.bin"]
			.into_iter()
			.filter(|name| !job(name) && hash_checked(events, name) == 0)
			.map(str::to_owned)
			.collect();
		// The eight have one size: the dispatcher's checks count for any of them.
		let by_job: Vec<&String> = par.iter().filter(|name| !job(name)).collect();
		if hash_checked(events, "par0.bin") < by_job.len() {
			missing.extend(by_job.into_iter().cloned());
		}
		missing
	};
	let started = Instant::now();
	let done = crate::local::wait(Duration::from_secs(20), || {
		unverified(&events(out)).is_empty().then_some(())
	});
	let events = events(out);
	let outcomes: Vec<String> = ["big.bin", "seq.bin"]
		.iter()
		.copied()
		.chain(par.iter().map(String::as_str))
		.map(|name| format!("{name}: {:?}", settled(&events, name)))
		.collect();
	report.check(
		"10b-verified",
		done.is_some(),
		format!(
			"unverified {:?} after {:?}; job {outcomes:?}; dispatcher par checks {}",
			unverified(&events),
			started.elapsed(),
			hash_checked(&events, "par0.bin")
		),
	);
}

/// 14: a file whose content does not match its declared hash, read in 4096-byte steps, so no
/// request covers it whole: the reader gets the bytes, the hydration job then finds the damage,
/// dehydrates the file and reports it; a second read fails, and nothing reads it again.
fn damaged_seq(report: &mut Report, root: &Path, out: &Path) {
	let name = "damaged-seq.bin";
	let path = root.join(name);
	let size = tree::size_of(name).expect("a file of the tree");
	let read = read_in_steps(&path);
	report.check(
		"14-damaged-windowed-read",
		read.as_ref().is_ok_and(|bytes| bytes.len() == size),
		format!(
			"{:?}",
			read.as_ref().map(Vec::len).map_err(|e| e.raw_os_error())
		),
	);
	let started = Instant::now();
	let reported = crate::local::wait(Duration::from_secs(20), || {
		damage_reported(&events(out), name)
	});
	let attributes = wait_offline(&path, Duration::from_secs(5));
	report.check(
		"14-detected",
		reported == Some(true) && attributes.is_some_and(|bits| bits != 0),
		format!(
			"damage reported {reported:?} after {:?}; offline bits {attributes:x?}; settled {:?}",
			started.elapsed(),
			settled(&events(out), name)
		),
	);
	let second = fs::read(&path);
	report.check(
		"14-second-read",
		second
			.as_ref()
			.is_err_and(|error| error.raw_os_error() == Some(VALIDATION_FAILED)),
		format!(
			"{:?}",
			second.as_ref().map(Vec::len).map_err(|e| e.raw_os_error())
		),
	);
	let before = user_fetches(&events(out), Some(size));
	thread::sleep(Duration::from_secs(10));
	let after = user_fetches(&events(out), Some(size));
	let provider = provider_pid(&events(out));
	let own_after = events(out)
		.iter()
		.filter(|event| {
			field(event, "callback") == Some("fetch_data")
				&& field(event, "message") == Some("cf callback")
				&& int(event, "pid") == provider
				&& int(event, "file_size") == i64::try_from(size).ok()
		})
		.count();
	report.check(
		"14-no-refetch",
		before == after,
		format!(
			"user fetches {before} then {after} 10 s later; provider's own fetches {own_after}"
		),
	);
}

/// 11: the same whole read as step 3, with the handle held open for a second after the read.
fn held_open(report: &mut Report, root: &Path, out: &Path) {
	let path = root.join("heldopen.bin");
	let read = File::open(&path).and_then(|mut file| {
		let mut bytes = Vec::new();
		file.read_to_end(&mut bytes)?;
		thread::sleep(Duration::from_secs(1));
		Ok(bytes)
	});
	let ok = read
		.as_ref()
		.is_ok_and(|bytes| *bytes == expected("heldopen.bin"));
	let attributes = fs::metadata(&path).map(|metadata| metadata.file_attributes());
	let events = events(out);
	let size = tree::size_of("heldopen.bin").expect("a file of the tree");
	let transfers = transfers_of_size(&events, size);
	let refused: Vec<&str> = under(&events, &transfers, "cf answer refused", "fetch_data")
		.filter_map(|event| field(event, "hr"))
		.collect();
	report.info(
		"11-held-open",
		format!(
			"bytes ok {ok}; attributes {attributes:x?}; answers {:?}; refused {refused:?}",
			answers(&events, &transfers)
		),
	);
}

/// 12: two readers in other processes read overlapping, unequal ranges of a file the drive is slow
/// to serve; the first is killed. The second must still get its bytes, long before the OS's 60 s
/// timeout.
fn overlapping_cancel(report: &mut Report, root: &Path, out: &Path) -> Outcome {
	let name = "slow.bin";
	let path = root.join(name);
	let first = out.join("overlap-first.txt");
	let second = out.join("overlap-second.txt");
	let (offset, len) = (524_288_usize, 1_048_576_usize);
	let mut killed = Command::new(env::current_exe()?)
		.args([
			"read-to".as_ref(),
			path.as_os_str(),
			"0".as_ref(),
			"1048576".as_ref(),
		])
		.arg(&first)
		.spawn()?;
	thread::sleep(Duration::from_millis(300));
	let mut survivor = Command::new(env::current_exe()?)
		.args(["read-to".as_ref(), path.as_os_str()])
		.args([offset.to_string(), len.to_string()])
		.arg(&second)
		.spawn()?;
	thread::sleep(Duration::from_millis(1200));
	killed.kill()?;
	killed.wait()?;
	let mut budget = Budget::new(Duration::from_secs(90));
	let status = loop {
		if let Some(status) = survivor.try_wait()? {
			break Some(status);
		}
		if budget.spent() {
			survivor.kill()?;
			break None;
		}
		thread::sleep(Duration::from_millis(100));
	};
	let line = fs::read_to_string(&second).unwrap_or_default();
	let wanted = expected(name)
		.get(offset..offset + len)
		.map(|bytes| blake3::hash(bytes).to_hex().to_string())
		.unwrap_or_default();
	let fields: Vec<&str> = line.split(' ').collect();
	let ms: u128 = fields
		.get(1)
		.and_then(|ms| ms.parse().ok())
		.unwrap_or(u128::MAX);
	let events = events(out);
	let transfers = transfers_of_size(&events, tree::size_of(name).expect("a file of the tree"));
	let fetches: Vec<String> = under(&events, &transfers, "cf callback", "fetch_data")
		.map(|event| {
			format!(
				"[{}+{}]",
				int(event, "required_offset").unwrap_or(-1),
				int(event, "required_len").unwrap_or(-1)
			)
		})
		.collect();
	let cancels: Vec<String> = under(&events, &transfers, "cf callback", "cancel_fetch_data")
		.map(|event| {
			format!(
				"[{}+{} flags {}]",
				int(event, "offset").unwrap_or(-1),
				int(event, "length").unwrap_or(-1),
				int(event, "flags").unwrap_or(-1)
			)
		})
		.collect();
	report.check(
		"12-overlapping-cancel",
		fields.first() == Some(&"ok") && fields.get(2) == Some(&wanted.as_str()) && ms < 20_000,
		format!(
			"second reader {status:?}: {line:?}; fetches {fetches:?}; cancels {cancels:?}; answers \
			 {:?}",
			answers(&events, &transfers)
		),
	);
	Ok(true)
}

/// 13: starts a reader of a file the drive never serves and leaves it waiting, so the provider's
/// shutdown abandons its request; `after` checks what the reader got.
fn start_abandoned_read(report: &mut Report, root: &Path, out: &Path) -> Outcome {
	let child = Command::new(env::current_exe()?)
		.args(["read-to".as_ref(), root.join("abandon.bin").as_os_str()])
		.args(["0", "4096"])
		.arg(out.join("abandon.txt"))
		.spawn()?;
	thread::sleep(Duration::from_secs(1));
	report.info("13-abandon-started", format!("reader pid {}", child.id()));
	Ok(true)
}

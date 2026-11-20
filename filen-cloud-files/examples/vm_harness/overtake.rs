//! `overtake <root> <out>`: a user's process renaming a new file or directory in the instant
//! between the provider's checks of it and its convert, while `provide-local` serves the root.
//! That window is a few tens of milliseconds, which sixty saves in the VM never hit, so the
//! provider is held in it: its `hold` control command makes the job about to convert the next new
//! item whose name starts with `hold-` wait just before the convert, logging "paused before
//! convert", until `release`. Every scenario works in the tree's `ox` directory. Into
//! `<out>\overtake.txt`.
//!
//! - O1: a new file renamed over a placeholder with `cmd /c move /y`, an editor's save: the
//!   convert is taken back, the saved file keeps its identity and gains one version, the
//!   temporary upload goes to the trash, and nothing is moved back.
//! - O2: a new file renamed to a new name: one file on the drive, at the new name.
//! - O3: a new directory holding two files renamed: one directory on the drive at the new name
//!   with both files in it; the directory created for the old name, which the converted directory
//!   stood for a moment, goes to the drive's trash once nothing can be sending into it, and no
//!   folder is left at the old name.
//! - O4, the platform refusing the take-back: not reachable from here, reported as such.

use std::{
	fs, io,
	path::Path,
	process::Command,
	thread,
	time::{Duration, Instant},
};

use serde_json::Value;

use crate::{
	local::{Remote, Scenarios, hex_hash, local_bytes, same_uuid, served, stable, wait},
	report::{Outcome, events, field, names_path, wait_for},
};

/// The control commands' first number, past every other scenario set's.
const FIRST_COMMAND: u32 = 90_000;
/// The prefix of the names the provider is held on.
const HOLD_PREFIX: &str = "hold-";
/// The longest the provider is held before a convert: well past what each scenario needs, so a
/// scenario that fails still lets it go on.
const HOLD_MS: u64 = 60_000;
/// How long a scenario waits for the provider to reach the held convert.
const REACHED: Duration = Duration::from_secs(60);
/// How long a scenario waits for the provider to carry a change to the drive.
const CARRIED: Duration = Duration::from_secs(60);
/// How long a scenario watches, once carried, for the drive moving anything back.
const STILL: Duration = Duration::from_secs(10);
/// How long O3 waits for the directory created for the old name to go: past the first few looks
/// of the core's backoff (2 s, doubling), well within its last.
const LEFT_GOES: Duration = Duration::from_secs(150);
/// The tree's file the save goes over.
const SAVED: &str = "ox\\save.txt";
/// What the core logs as it takes back a convert a rename overtook.
const TAKEN_BACK_FILE: &str = "a rename overtook a file's convert; taken back";
const TAKEN_BACK_DIR: &str = "a rename overtook a directory's convert; taken back";
/// What it logs when the platform refused the take-back, or the detach after it.
const REVERT_FAILED: [&str; 4] = [
	"taking back a convert a rename overtook failed; detaching it",
	"detaching a file whose convert a rename overtook failed",
	"taking back a directory's convert a rename overtook failed; detaching it",
	"detaching a directory whose convert a rename overtook failed",
];

pub(crate) fn run(root: &Path, out: &Path) -> Outcome {
	let mut s = Scenarios::open(root, out, "overtake.txt", FIRST_COMMAND)?;
	let result = run_all(&mut s);
	if let Err(error) = &result {
		s.report.fail("harness", error);
	}
	// Never leaves the provider held.
	let _ = s.control("release");
	fs::write(out.join("overtake.done"), "done")?;
	Ok(s.report.passed())
}

fn run_all(s: &mut Scenarios) -> io::Result<()> {
	if !wait_for(&s.out.join("provider.ready"), Duration::from_secs(60)) {
		s.check(
			"O0-ready",
			false,
			"the provider did not get ready within 60 s",
		);
		return Ok(());
	}
	let mut names: Vec<String> = fs::read_dir(s.path("ox"))?
		.map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
		.collect::<io::Result<_>>()?;
	names.sort();
	s.check("O0-list-ox", names == ["save.txt"], format!("{names:?}"));
	thread::sleep(Duration::from_secs(3));
	save_over(s)?;
	rename_new(s)?;
	rename_dir(s)?;
	s.info(
		"O4-refused-revert",
		"not run: cfapi refuses CfRevertPlaceholder only for a file whose data is not all on disk, \
		 and a new file's always is; nothing a user's process does to the item makes the revert \
		 through the provider's own handle fail. Covered by the host tests on the fakes only",
	);
	Ok(())
}

/// How many events with `message` name `path`, a path below the root.
fn logged(s: &Scenarios, message: &str, path: &str) -> usize {
	let events = events(&s.out);
	events
		.iter()
		.filter(|event| {
			field(event, "message") == Some(message) && names_path(&events, event, path)
		})
		.count()
}

/// How many events say the platform refused a take-back, or the detach after it, for `path`.
fn revert_failures(s: &Scenarios, path: &str) -> usize {
	REVERT_FAILED
		.iter()
		.map(|message| logged(s, message, path))
		.sum()
}

/// Arms the hold, has `make` create the new item `held` below the root, and waits until the
/// provider is held before converting it: whether it got there.
fn hold_on(
	s: &mut Scenarios,
	step: &str,
	held: &str,
	make: impl FnOnce(&Scenarios) -> io::Result<()>,
) -> io::Result<bool> {
	if let Err(error) = s.control(&format!("hold {HOLD_PREFIX} {HOLD_MS}")) {
		s.check(&format!("{step}-armed"), false, error);
		return Ok(false);
	}
	make(s)?;
	let started = Instant::now();
	let reached = wait(REACHED, || {
		s.control("held").ok().filter(|path| path.as_str() != "-")
	});
	let ok = reached.as_deref() == Some(held);
	s.check(
		&format!("{step}-held"),
		ok,
		format!("held {reached:?} after {:?}", started.elapsed()),
	);
	if !ok {
		let _ = s.control("release");
	}
	Ok(ok)
}

/// Lets the held convert go on.
fn release(s: &mut Scenarios, step: &str) {
	let released = s.control("release");
	s.check(
		&format!("{step}-released"),
		released.is_ok(),
		format!("{released:?}"),
	);
}

/// `cmd /c move /y from to`: a rename over an existing file by another process, as an editor
/// saving with `move` does.
fn move_y(from: &Path, to: &Path) -> io::Result<()> {
	let output = Command::new("cmd")
		.arg("/c")
		.arg("move")
		.arg("/y")
		.arg(from)
		.arg(to)
		.output()?;
	if output.status.success() {
		Ok(())
	} else {
		Err(io::Error::other(
			String::from_utf8_lossy(&output.stdout).into_owned(),
		))
	}
}

/// The live files of the drive in the directory `dir` (a cache id) whose content hashes `hash`.
fn live_with_hash<'a>(remote: &'a Remote, dir: &str, hash: &str) -> Vec<&'a Value> {
	remote
		.items()
		.iter()
		.filter(|item| {
			field(item, "kind") == Some("file")
				&& field(item, "dir") == Some(dir)
				&& field(item, "hash") == Some(hash)
		})
		.collect()
}

/// The trashed files whose content hashes `hash`.
fn trashed_with_hash<'a>(remote: &'a Remote, hash: &str) -> Vec<&'a Value> {
	remote
		.trash()
		.iter()
		.filter(|item| field(item, "kind") == Some("file") && field(item, "hash") == Some(hash))
		.collect()
}

/// The cache id `ox` is listed under, from the drive's dump.
fn ox_id(remote: &Remote) -> Option<String> {
	remote
		.find("ox")
		.and_then(|item| field(item, "uuid"))
		.map(|uuid| format!("stable/{uuid}"))
}

/// O1: a new file renamed over the placeholder `ox\save.txt` with `move /y` in the window before
/// its convert.
fn save_over(s: &mut Scenarios) -> io::Result<()> {
	let step = "O1-save";
	let temp = "ox\\hold-1.tmp";
	let read = fs::read(s.path(SAVED))?;
	s.check(
		&format!("{step}-hydrate"),
		read == served(SAVED),
		format!("{} bytes", read.len()),
	);
	thread::sleep(Duration::from_secs(3));
	let before = s.synced(SAVED);
	let id = format!("stable/{}", stable(SAVED));
	let versions_before = s.fresh_remote().count("UploadVersion", "id", &id);
	let bytes = local_bytes(read.len() + 77, 41);
	let hash = hex_hash(&bytes);
	if !hold_on(s, step, temp, |s| fs::write(s.path(temp), &bytes))? {
		return Ok(());
	}
	let moved = move_y(&s.path(temp), &s.path(SAVED));
	s.check(
		&format!("{step}-moved"),
		moved.is_ok(),
		format!("{moved:?}"),
	);
	release(s, step);
	let Some(after) = s.expect_carried(step, SAVED, &bytes) else {
		return Ok(());
	};
	s.check(
		&format!("{step}-taken-back"),
		logged(s, TAKEN_BACK_FILE, temp) == 1,
		format!("{} lines", logged(s, TAKEN_BACK_FILE, temp)),
	);
	s.check(
		&format!("{step}-no-revert-error"),
		revert_failures(s, temp) == 0,
		format!("{} lines", revert_failures(s, temp)),
	);
	let remote = s.fresh_remote();
	let versions = remote.count("UploadVersion", "id", &id) - versions_before;
	s.check(
		&format!("{step}-same-file-one-version"),
		after.stable == stable(SAVED)
			&& before
				.as_ref()
				.is_some_and(|before| before.version != after.version)
			&& versions == 1,
		format!("before {before:?}, after {after:?}, {versions} versions"),
	);
	let trashed = trashed_with_hash(&remote, &hash);
	s.check(
		&format!("{step}-temporary-upload-trashed"),
		trashed.len() == 1
			&& trashed.iter().all(|item| {
				field(item, "uuid").is_some_and(|uuid| !same_uuid(uuid, &after.stable.to_string()))
			}),
		format!("{trashed:?}"),
	);
	// The drive's remote side would move a placeholder of the temporary upload back to its name.
	thread::sleep(STILL);
	let still = s.synced(SAVED);
	let remote = s.fresh_remote();
	let local_temp = s.path(temp).exists();
	let content = fs::read(s.path(SAVED));
	s.check(
		&format!("{step}-nothing-moved-back"),
		still.as_ref() == Some(&after)
			&& remote.find(temp).is_none()
			&& !local_temp
			&& content.as_ref().is_ok_and(|content| *content == bytes)
			&& remote
				.find(SAVED)
				.and_then(|item| field(item, "uuid"))
				.is_some_and(|uuid| same_uuid(uuid, &after.stable.to_string())),
		format!(
			"now {still:?}; remote {:?}; {temp} on disk {local_temp}; content {:?}",
			remote.find(SAVED),
			content.map(|content| content.len())
		),
	);
	Ok(())
}

/// O2: a new file renamed to a name nothing holds in the window before its convert.
fn rename_new(s: &mut Scenarios) -> io::Result<()> {
	let step = "O2-rename";
	let (temp, to) = ("ox\\hold-2.tmp", "ox\\renamed-2.txt");
	let bytes = local_bytes(5_432, 42);
	let hash = hex_hash(&bytes);
	if !hold_on(s, step, temp, |s| fs::write(s.path(temp), &bytes))? {
		return Ok(());
	}
	let renamed = fs::rename(s.path(temp), s.path(to));
	s.check(
		&format!("{step}-renamed"),
		renamed.is_ok(),
		format!("{renamed:?}"),
	);
	release(s, step);
	let Some(after) = s.expect_carried(step, to, &bytes) else {
		return Ok(());
	};
	s.check(
		&format!("{step}-taken-back"),
		logged(s, TAKEN_BACK_FILE, temp) == 1,
		format!("{} lines", logged(s, TAKEN_BACK_FILE, temp)),
	);
	s.check(
		&format!("{step}-no-revert-error"),
		revert_failures(s, temp) == 0,
		format!("{} lines", revert_failures(s, temp)),
	);
	thread::sleep(STILL);
	let remote = s.fresh_remote();
	let live = ox_id(&remote)
		.map(|ox| live_with_hash(&remote, &ox, &hash).len())
		.unwrap_or(0);
	let trashed = trashed_with_hash(&remote, &hash).len();
	s.check(
		&format!("{step}-one-file"),
		live == 1
			&& remote.find(temp).is_none()
			&& !s.path(temp).exists()
			&& s.synced(to).as_ref() == Some(&after),
		format!(
			"{live} live with its bytes, {trashed} trashed; now {:?}",
			s.synced(to)
		),
	);
	s.info(
		&format!("{step}-trash"),
		format!("{trashed} trashed with its bytes (the upload made for the old name)"),
	);
	Ok(())
}

/// O3: a new directory holding two files renamed in the window before its convert.
fn rename_dir(s: &mut Scenarios) -> io::Result<()> {
	let step = "O3-dir";
	let (dir, to) = ("ox\\hold-dir-3", "ox\\renamed-dir-3");
	let (a, b) = (local_bytes(3_001, 43), local_bytes(3_002, 44));
	let made = hold_on(s, step, dir, |s| {
		fs::create_dir(s.path(dir))?;
		fs::write(s.path(&format!("{dir}\\a.txt")), &a)?;
		fs::write(s.path(&format!("{dir}\\b.txt")), &b)
	})?;
	if !made {
		return Ok(());
	}
	let renamed = fs::rename(s.path(dir), s.path(to));
	s.check(
		&format!("{step}-renamed"),
		renamed.is_ok(),
		format!("{renamed:?}"),
	);
	release(s, step);
	let uuid = wait(CARRIED, || s.synced_dir(to));
	s.check(
		&format!("{step}-taken-in"),
		uuid.is_some(),
		format!("{:?}", s.entry(to)),
	);
	s.check(
		&format!("{step}-taken-back"),
		logged(s, TAKEN_BACK_DIR, dir) == 1,
		format!("{} lines", logged(s, TAKEN_BACK_DIR, dir)),
	);
	s.check(
		&format!("{step}-no-revert-error"),
		revert_failures(s, dir) == 0,
		format!("{} lines", revert_failures(s, dir)),
	);
	s.expect_carried(&format!("{step}-a"), &format!("{to}\\a.txt"), &a);
	s.expect_carried(&format!("{step}-b"), &format!("{to}\\b.txt"), &b);
	let Some(uuid) = uuid else {
		return Ok(());
	};
	let remote = s.fresh_remote();
	let mut under: Vec<&str> = remote
		.items()
		.iter()
		.filter(|item| field(item, "dir") == Some(format!("stable/{uuid}").as_str()))
		.filter_map(|item| field(item, "name"))
		.collect();
	under.sort_unstable();
	let at_new = remote
		.find(to)
		.and_then(|item| field(item, "uuid"))
		.is_some_and(|held| same_uuid(held, &uuid.to_string()));
	s.check(
		&format!("{step}-one-dir-both-files"),
		at_new && under == ["a.txt", "b.txt"],
		format!("at the new name {at_new}, holding {under:?}"),
	);
	// The directory created for the old name stood, once converted, for the directory renamed, so
	// it waits until nothing can be sending into it, then goes to the trash while still empty, and
	// any empty folder the reconcile of `ox` placed for it goes with it.
	let Some(ox) = ox_id(&remote) else {
		s.check(&format!("{step}-old-dir-gone"), false, "no ox on the drive");
		return Ok(());
	};
	let name = dir.rsplit('\\').next().unwrap_or(dir);
	let gone = wait(LEFT_GOES, || {
		let remote = s.fresh_remote();
		let trashed: Vec<String> = remote
			.trash()
			.iter()
			.filter(|item| {
				field(item, "kind") == Some("dir")
					&& field(item, "dir") == Some(ox.as_str())
					&& field(item, "name") == Some(name)
			})
			.filter_map(|item| field(item, "uuid").map(str::to_owned))
			.collect();
		(remote.find(dir).is_none() && trashed.len() == 1 && !s.path(dir).exists())
			.then_some(trashed)
	});
	let remote = s.fresh_remote();
	let old_holds = gone.as_ref().map_or(0, |trashed| {
		remote
			.trash()
			.iter()
			.chain(remote.items())
			.filter(|item| {
				trashed
					.iter()
					.any(|old| field(item, "dir") == Some(format!("stable/{old}").as_str()))
			})
			.count()
	});
	s.check(
		&format!("{step}-old-dir-gone"),
		gone.is_some() && old_holds == 0,
		format!(
			"trashed {gone:?} holding {old_holds}; live {:?}; local {:?}",
			remote.find(dir),
			s.entry(dir)
		),
	);
	Ok(())
}

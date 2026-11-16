//! The remote-change scenarios: another device changing the fake drive, through the provider's
//! control commands, while `provide-local` serves the root; each change checked from this process,
//! a user's, by what it reads, and through the provider's reading of a placeholder's state. Every
//! scenario works in the tree's `rx` directory, so the other scenarios find the rest of the tree as
//! served. Runs after `local` and before `check`.

use std::{
	fs::{self, File, OpenOptions},
	io::{self, Read, Seek, SeekFrom, Write},
	os::windows::fs::OpenOptionsExt,
	path::Path,
	thread,
	time::{Duration, Instant},
};

use filen_types::fs::Uuid;
use serde_json::Value;

use crate::{
	local::{Scenarios, Synced, hex_hash, local_bytes, patched, same_uuid, served, stable, wait},
	report::{Outcome, field},
	tree,
};

/// How long a scenario waits for the provider to apply a change of the drive.
const APPLIED: Duration = Duration::from_secs(40);
/// How long a scenario waits for a change the provider deferred while a file was held, after the
/// holder let go: the busy retry backs off up to 30 s.
const AFTER_RELEASE: Duration = Duration::from_secs(75);
/// How long a holder keeps a file open while the provider is to apply a change to it.
const HELD: Duration = Duration::from_secs(8);
/// How long the provider must stay idle at the end: no drive write while nothing changes.
const IDLE: Duration = Duration::from_secs(20);
/// How long the feed's tick is at most, with a margin: a change that raised no signal is applied
/// by then.
const TICK: Duration = Duration::from_secs(45);
/// `FILE_SHARE_READ`: a holder that refuses writers and deletes.
const SHARE_READ: u32 = 0x1;
/// The control commands' first number, past any `local` used.
const FIRST_COMMAND: u32 = 50_000;

/// The tree's uuid of the directory `path`.
fn dir(path: &str) -> Uuid {
	tree::dir_uuid(path).expect("a directory of the tree")
}

impl Scenarios {
	/// Sends `command`, failing `step` when it is refused.
	fn ctl(&mut self, step: &str, command: &str) -> Option<String> {
		match self.control(command) {
			Ok(answer) => Some(answer),
			Err(error) => {
				self.check(step, false, format!("{command:?}: {error}"));
				None
			}
		}
	}

	/// Reads `path` whole as a user: whether it holds `bytes`, or why not.
	fn reads(&self, path: &str, bytes: &[u8]) -> Result<(), String> {
		match fs::read(self.path(path)) {
			Ok(read) if read == bytes => Ok(()),
			Ok(read) => Err(format!(
				"{} bytes, not the {} expected",
				read.len(),
				bytes.len()
			)),
			Err(error) => Err(error.to_string()),
		}
	}

	/// Reads the tree's file `path` whole, hydrating it, and checks it holds what the tree serves;
	/// then waits for the provider to settle what the hydration raised.
	fn hydrate(&mut self, step: &str, path: &str) -> bool {
		let read = self.reads(path, &served(path));
		let ok = read.is_ok();
		self.check(&format!("{step}-hydrate"), ok, format!("{path}: {read:?}"));
		thread::sleep(Duration::from_secs(3));
		ok
	}

	/// Waits until the file placeholder at `path` is clean, of `stable` at `version`.
	fn wait_version(
		&self,
		path: &str,
		stable: Uuid,
		version: &str,
		limit: Duration,
	) -> Option<Synced> {
		wait(limit, || {
			self.synced(path).filter(|synced| {
				synced.stable == stable && same_uuid(&synced.version.to_string(), version)
			})
		})
	}

	/// Waits until nothing is at `path`.
	fn wait_missing(&self, path: &str, limit: Duration) -> bool {
		wait(limit, || {
			(self.entry(path).kind() == Some("missing")).then_some(())
		})
		.is_some()
	}

	/// The names of the directory `path` as this process lists it, sorted.
	fn names(&self, path: &str) -> io::Result<Vec<String>> {
		let mut names: Vec<String> = fs::read_dir(self.path(path))?
			.map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
			.collect::<io::Result<_>>()?;
		names.sort();
		Ok(names)
	}

	/// The names the provider's own listing of `path` returns, sorted: a listing that neither
	/// populates nor hydrates.
	fn provider_names(&self, path: &str) -> Result<Vec<String>, String> {
		let answer = self.control(&format!("list {path}"))?;
		let mut names: Vec<String> = serde_json::from_str(&answer).map_err(|e| e.to_string())?;
		names.sort();
		Ok(names)
	}

	/// Whether a local report naming `what` and `path` was logged.
	fn reported(&self, what: &str, path: &str) -> bool {
		self.reports()
			.iter()
			.any(|report| report.contains(what) && report.contains(path))
	}

	/// The drive's writes so far.
	fn write_count(&self) -> usize {
		self.fresh_remote().writes().len()
	}

	/// Checks the placeholder at `path` reached the drive's `version` of `stable`, dehydrated,
	/// and reads as `bytes`.
	fn expect_updated(&mut self, step: &str, path: &str, version: &str, bytes: &[u8]) {
		let started = Instant::now();
		let synced = self.wait_version(path, stable(path), version, APPLIED);
		let elapsed = started.elapsed();
		self.check(
			step,
			synced.as_ref().is_some_and(|synced| {
				synced.on_disk == 0 && u64::try_from(bytes.len()).ok() == Some(synced.size)
			}),
			format!("{synced:?} after {elapsed:?}; now {:?}", self.entry(path)),
		);
		let read = self.reads(path, bytes);
		self.check(&format!("{step}-reads"), read.is_ok(), format!("{read:?}"));
	}
}

/// Writes `patch` at `at` into `path` through a handle of its own, as an editor saving in place.
fn write_at(path: &Path, at: u64, patch: &[u8]) -> io::Result<()> {
	let mut file = OpenOptions::new().write(true).open(path)?;
	file.seek(SeekFrom::Start(at))?;
	file.write_all(patch)
}

pub(crate) fn run(root: &Path, out: &Path) -> Outcome {
	let mut s = Scenarios::open(root, out, "remote.txt", FIRST_COMMAND)?;
	let result = run_all(&mut s);
	if let Err(error) = &result {
		s.report.fail("harness", error);
	}
	fs::write(out.join("remote.done"), "done")?;
	Ok(s.report.passed())
}

fn run_all(s: &mut Scenarios) -> io::Result<()> {
	if !crate::report::wait_for(&s.out.join("provider.ready"), Duration::from_secs(60)) {
		s.check(
			"R0-ready",
			false,
			"the provider did not get ready within 60 s",
		);
		return Ok(());
	}
	let start = s.write_count();
	populate(s)?;
	create(s)?;
	unpopulated(s)?;
	content(s)?;
	held(s)?;
	renames(s)?;
	trashes(s)?;
	dir_trash(s)?;
	attributes(s)?;
	restore(s)?;
	offline_delete(s)?;
	restart(s)?;
	quiet(s)?;
	wipe(s)?;
	no_own_writes(s, start);
	Ok(())
}

/// R0: listing `rx` and the directories the scenarios change populates them; the provider then
/// knows them as populated.
fn populate(s: &mut Scenarios) -> io::Result<()> {
	for path in [
		"rx",
		"rx\\sub",
		"rx\\mvdir",
		"rx\\trdir",
		"rx\\cleandir",
		"rx\\attrdir",
	] {
		let names = s.names(path)?;
		s.info(&format!("R0-list-{path}"), format!("{names:?}"));
	}
	let rx = s.names("rx")?;
	let wanted: Vec<String> = {
		let mut wanted: Vec<String> = tree::FILES
			.iter()
			.map(|file| file.path)
			.chain(tree::DIRS.iter().map(|(path, _)| *path))
			.filter_map(|path| path.strip_prefix("rx\\"))
			.filter(|name| !name.contains('\\'))
			.map(str::to_owned)
			.collect();
		wanted.sort();
		wanted
	};
	s.check("R0-list-rx", rx == wanted, format!("{rx:?}"));
	// The population reaches the engine through the dispatcher; give its reconcile a moment.
	thread::sleep(Duration::from_secs(3));
	Ok(())
}

/// R1, R3: a new remote file in a populated directory becomes a dehydrated placeholder in sync,
/// reading as the drive's bytes; a new remote directory becomes a directory placeholder the
/// platform has not populated, which lists empty.
fn create(s: &mut Scenarios) -> io::Result<()> {
	if let Some(uuid) = s.ctl("R1-add", "add rx new1.txt 3333") {
		let started = Instant::now();
		let seen = wait(APPLIED, || {
			let seen = s.entry("rx\\new1.txt");
			seen.file()
				.is_some_and(|(stable, _)| same_uuid(&stable.to_string(), &uuid))
				.then_some(seen)
		});
		s.check(
			"R1-create",
			seen.as_ref().is_some_and(|seen| {
				seen.flag("in_sync") && seen.number("on_disk") == 0 && seen.number("size") == 3333
			}),
			format!(
				"{:?} after {:?}",
				seen.map(|seen| seen.0),
				started.elapsed()
			),
		);
		let read = s.reads("rx\\new1.txt", &tree::added(3333));
		s.check("R1-create-reads", read.is_ok(), format!("{read:?}"));
		s.check(
			"R1-create-reported",
			s.reported("Created", "new1.txt"),
			"a Created report",
		);
	}
	if let Some(uuid) = s.ctl("R3-adddir", "adddir rx newdir") {
		let seen = wait(APPLIED, || {
			let seen = s.entry("rx\\newdir");
			(seen.kind() == Some("placeholder")
				&& seen
					.identity("uuid")
					.is_some_and(|held| same_uuid(&held.to_string(), &uuid)))
			.then_some(seen)
		});
		s.check(
			"R3-create-dir",
			seen.as_ref().is_some_and(|seen| {
				seen.flag("dir") && seen.flag("in_sync") && seen.flag("partial")
			}),
			format!("{:?}", seen.map(|seen| seen.0)),
		);
		let listed = s.names("rx\\newdir");
		s.check(
			"R3-create-dir-lists",
			listed.as_ref().is_ok_and(Vec::is_empty),
			format!("{listed:?}"),
		);
	}
	Ok(())
}

/// R2, R11, R12: a remote file added to a directory the platform never populated, and a clean
/// placeholder the drive moved into it, leave its population to its first listing, which shows each
/// once; a placeholder moved into a directory with no placeholder at all is removed, the drive
/// keeping it.
fn unpopulated(s: &mut Scenarios) -> io::Result<()> {
	let lazy_new = s.ctl("R2-add", "add rx\\lazy lazynew.txt 3334");
	s.hydrate("R11", "rx\\mvlazy.txt");
	s.ctl(
		"R11-move",
		&format!("move {} rx\\lazy", stable("rx\\mvlazy.txt")),
	);
	s.hydrate("R12", "rx\\out.txt");
	s.ctl(
		"R12-move",
		&format!("move {} rx\\lazy\\deep", stable("rx\\out.txt")),
	);
	let gone = s.wait_missing("rx\\out.txt", APPLIED);
	s.check(
		"R12-move-out-removed",
		gone,
		format!("{:?}", s.entry("rx\\out.txt")),
	);
	let moved = wait(APPLIED, || s.synced("rx\\lazy\\mvlazy.txt"));
	s.check(
		"R11-move-unpopulated",
		moved.as_ref().is_some_and(|moved| {
			moved.stable == stable("rx\\mvlazy.txt") && moved.on_disk == moved.size
		}) && s.entry("rx\\mvlazy.txt").kind() == Some("missing"),
		format!("{moved:?}"),
	);
	thread::sleep(Duration::from_secs(2));
	let before = s.provider_names("rx\\lazy");
	s.check(
		"R2-not-in-unpopulated",
		before.as_ref().is_ok_and(|names| names == &["mvlazy.txt"]),
		format!("the provider lists {before:?} before the user does"),
	);
	let listed = s.names("rx\\lazy")?;
	s.check(
		"R2-listed-once",
		listed == ["deep", "lazynew.txt", "mvlazy.txt"],
		format!("{listed:?}"),
	);
	if let Some(uuid) = lazy_new {
		let file = s.entry("rx\\lazy\\lazynew.txt").file();
		let read = s.reads("rx\\lazy\\lazynew.txt", &tree::added(3334));
		s.check(
			"R2-listed-reads",
			file.is_some_and(|(stable, _)| same_uuid(&stable.to_string(), &uuid)) && read.is_ok(),
			format!("{file:?}, {read:?}"),
		);
	}
	let read = s.reads("rx\\lazy\\mvlazy.txt", &served("rx\\mvlazy.txt"));
	s.check("R11-move-reads", read.is_ok(), format!("{read:?}"));
	let remote = s.fresh_remote();
	s.check(
		"R12-no-trash",
		!remote.trashed(&stable("rx\\out.txt").to_string()),
		"the drive keeps the moved-out file",
	);
	Ok(())
}

/// R4-R6: a newer remote version of a dehydrated and of a hydrated file updates each to it,
/// dehydrated; a file edited locally just before keeps both, the edit uploaded aside.
fn content(s: &mut Scenarios) -> io::Result<()> {
	let path = "rx\\dehyd.txt";
	if let Some(head) = s.ctl("R4-bump", &format!("bump {path}")) {
		s.expect_updated("R4-update-dehydrated", path, &head, &tree::bumped(8001));
	}

	let path = "rx\\hyd.txt";
	if s.hydrate("R5", path)
		&& let Some(head) = s.ctl("R5-bump", &format!("bump {path}"))
	{
		s.expect_updated("R5-update-hydrated", path, &head, &tree::bumped(8002));
	}

	let path = "rx\\mod.txt";
	if s.hydrate("R6", path) {
		let patch = local_bytes(4096, 21);
		write_at(&s.path(path), 0, &patch)?;
		if let Some(head) = s.ctl("R6-bump", &format!("bump {path}")) {
			let edited = patched(served(path), 0, &patch);
			let aside = s.expect_carried("R6-edit-kept-aside", "rx\\mod.old.txt", &edited);
			let placed = s.wait_version(path, stable(path), &head, APPLIED);
			let read = s.reads(path, &tree::bumped(8003));
			s.check(
				"R6-remote-at-name",
				placed.is_some() && read.is_ok() && aside.is_some_and(|a| a.stable != stable(path)),
				format!("{placed:?}, {read:?}"),
			);
			s.check(
				"R6-reported",
				s.reported("EditVsEdit", "mod.old.txt"),
				"an EditVsEdit report",
			);
		}
	}

	// A local edit and a remote edit at once: whichever lands first, both sets of bytes survive.
	let path = "rx\\both.txt";
	if s.hydrate("R6b", path) {
		let patch = local_bytes(2048, 22);
		let bumping = s.ctl("R6b-bump", &format!("bump {path}"));
		write_at(&s.path(path), 0, &patch)?;
		if let Some(head) = bumping {
			thread::sleep(Duration::from_secs(12));
			let local = fs::read(s.path(path)).unwrap_or_default();
			let aside = fs::read(s.path("rx\\both.old.txt")).ok();
			let remote = s.fresh_remote();
			let on_drive = |bytes: &[u8]| {
				let hash = hex_hash(bytes);
				remote
					.items()
					.iter()
					.any(|item| field(item, "hash") == Some(hash.as_str()))
			};
			let bumped = tree::bumped(8017);
			let edit_kept = [Some(local.as_slice()), aside.as_deref()]
				.into_iter()
				.flatten()
				.any(|bytes| bytes.starts_with(&patch) && on_drive(bytes));
			let head_kept =
				on_drive(&bumped) || local.get(patch.len()..) == bumped.get(patch.len()..);
			s.check(
				"R6b-both-kept",
				edit_kept && head_kept,
				format!(
					"head {head}; local starts with the edit {}, aside {}, edit on the drive \
					 {edit_kept}, head's bytes kept {head_kept}",
					local.starts_with(&patch),
					aside.is_some()
				),
			);
		}
	}
	Ok(())
}

/// R7, R8: a remote change while a user holds the file. A reader, even one that lets writers in
/// or only reads, defers the update until it lets go (`CfDehydratePlaceholder` refuses a file in
/// use, measured), reading the old bytes through its handle meanwhile; a writer defers it too, so
/// it never writes into the new version unseen; a writer that wrote meanwhile keeps both; a holder
/// refusing deletes defers a trash.
fn held(s: &mut Scenarios) -> io::Result<()> {
	let path = "rx\\holdr.txt";
	if s.hydrate("R7", path) {
		let before = s.synced(path);
		let mut reader = OpenOptions::new()
			.read(true)
			.share_mode(SHARE_READ)
			.open(s.path(path))?;
		let mut first = [0; 100];
		reader.read_exact(&mut first)?;
		if let Some(head) = s.ctl("R7-bump", &format!("bump {path}")) {
			thread::sleep(HELD);
			let during = s.synced(path);
			s.check(
				"R7-deferred-while-read",
				during.is_some() && during == before,
				format!("before {before:?}, during {during:?}"),
			);
			let mut next = vec![0; 100];
			reader.seek(SeekFrom::Start(0))?;
			let read = reader.read(&mut next);
			s.check(
				"R7-holder-reads-old",
				read.is_ok() && next.get(..100) == served(path).get(..100),
				format!("{read:?}"),
			);
			drop(reader);
			let released = Instant::now();
			let applied = s.wait_version(path, stable(path), &head, AFTER_RELEASE);
			s.check(
				"R7-applied-after-release",
				applied.as_ref().is_some_and(|applied| applied.on_disk == 0),
				format!("{applied:?} {:?} after the release", released.elapsed()),
			);
			let read = s.reads(path, &tree::bumped(8004));
			s.check("R7-update-reads", read.is_ok(), format!("{read:?}"));
		}
	}

	let path = "rx\\holdw.txt";
	if s.hydrate("R8", path) {
		let writer = OpenOptions::new().write(true).open(s.path(path))?;
		if let Some(head) = s.ctl("R8-bump", &format!("bump {path}")) {
			thread::sleep(HELD);
			let during = s.synced(path);
			s.check(
				"R8-deferred-while-written",
				during
					.as_ref()
					.is_some_and(|during| !same_uuid(&during.version.to_string(), &head)),
				format!("{during:?} while held"),
			);
			drop(writer);
			let released = Instant::now();
			let applied = s.wait_version(path, stable(path), &head, AFTER_RELEASE);
			s.check(
				"R8-applied-after-release",
				applied.as_ref().is_some_and(|applied| applied.on_disk == 0),
				format!("{applied:?} {:?} after the release", released.elapsed()),
			);
			let read = s.reads(path, &tree::bumped(8005));
			s.check("R8-reads", read.is_ok(), format!("{read:?}"));
		}
	}

	let path = "rx\\holdww.txt";
	if s.hydrate("R8b", path) {
		let mut writer = OpenOptions::new().write(true).open(s.path(path))?;
		if let Some(head) = s.ctl("R8b-bump", &format!("bump {path}")) {
			thread::sleep(HELD);
			let patch = local_bytes(8, 23);
			writer.write_all(&patch)?;
			drop(writer);
			let edited = patched(served(path), 0, &patch);
			let aside = s.expect_carried("R8b-write-kept-aside", "rx\\holdww.old.txt", &edited);
			let placed = s.wait_version(path, stable(path), &head, AFTER_RELEASE);
			let read = s.reads(path, &tree::bumped(8006));
			s.check(
				"R8b-remote-at-name",
				placed.is_some() && read.is_ok() && aside.is_some(),
				format!("{placed:?}, {read:?}"),
			);
		}
	}

	let path = "rx\\holdd.txt";
	if s.hydrate("R21", path) {
		let reader = OpenOptions::new()
			.read(true)
			.share_mode(SHARE_READ)
			.open(s.path(path))?;
		if s.ctl("R21-trash", &format!("trash {}", stable(path)))
			.is_some()
		{
			thread::sleep(HELD);
			let during = s.synced(path);
			s.check(
				"R21-trash-deferred-while-held",
				during.is_some(),
				format!("{during:?} while held"),
			);
			drop(reader);
			let released = Instant::now();
			let gone = s.wait_missing(path, AFTER_RELEASE);
			s.check(
				"R21-trash-after-release",
				gone,
				format!("{:?} after the release", released.elapsed()),
			);
		}
	}
	Ok(())
}

/// R9, R10, R14: remote renames and moves follow locally, keeping each placeholder's identity and
/// hydration, in sync, with no drive write: a rename, a case-only rename, a move between populated
/// directories, and a directory rename with hydrated children.
fn renames(s: &mut Scenarios) -> io::Result<()> {
	let path = "rx\\ren.txt";
	if s.hydrate("R9", path)
		&& s.ctl("R9-rename", &format!("rename {} ren2.txt", stable(path)))
			.is_some()
	{
		let renamed = wait(APPLIED, || s.synced("rx\\ren2.txt"));
		let names = s.names("rx")?;
		s.check(
			"R9-rename",
			renamed.as_ref().is_some_and(|synced| {
				synced.stable == stable(path) && synced.on_disk == synced.size
			}) && names.iter().any(|name| name == "ren2.txt")
				&& !names.iter().any(|name| name == "ren.txt"),
			format!("{renamed:?}"),
		);
	}

	let path = "rx\\case.txt";
	if s.ctl("R9b-rename", &format!("rename {} CASE.txt", stable(path)))
		.is_some()
	{
		let names = wait(APPLIED, || {
			let names = s.names("rx").ok()?;
			names.iter().any(|name| name == "CASE.txt").then_some(names)
		});
		let synced = s.synced("rx\\CASE.txt");
		s.check(
			"R9b-rename-case",
			names
				.as_ref()
				.is_some_and(|names| !names.iter().any(|name| name == "case.txt"))
				&& synced.is_some_and(|synced| synced.stable == stable(path)),
			format!("{names:?}"),
		);
	}

	let path = "rx\\mv.txt";
	if s.hydrate("R10", path)
		&& s.ctl("R10-move", &format!("move {} rx\\sub", stable(path)))
			.is_some()
	{
		let moved = wait(APPLIED, || s.synced("rx\\sub\\mv.txt"));
		s.check(
			"R10-move",
			moved.as_ref().is_some_and(|synced| {
				synced.stable == stable(path) && synced.on_disk == synced.size
			}) && s.entry(path).kind() == Some("missing"),
			format!("{moved:?}"),
		);
		let read = s.reads("rx\\sub\\mv.txt", &served(path));
		s.check("R10-move-reads", read.is_ok(), format!("{read:?}"));
	}

	let children = ["h1.bin", "h2.bin"];
	let mut hydrated = true;
	for child in children {
		hydrated &= s.hydrate("R10b", &format!("rx\\mvdir\\{child}"));
	}
	if hydrated
		&& s.ctl(
			"R10b-rename",
			&format!("rename {} mvdir2", dir("rx\\mvdir")),
		)
		.is_some()
	{
		let renamed = wait(APPLIED, || s.synced_dir("rx\\mvdir2"));
		s.check(
			"R10b-dir-rename",
			renamed == Some(dir("rx\\mvdir")) && s.entry("rx\\mvdir").kind() == Some("missing"),
			format!("{:?}", s.entry("rx\\mvdir2")),
		);
		for child in children {
			let synced = s.synced(&format!("rx\\mvdir2\\{child}"));
			let tree_path = format!("rx\\mvdir\\{child}");
			let read = s.reads(&format!("rx\\mvdir2\\{child}"), &served(&tree_path));
			s.check(
				&format!("R10b-dir-rename-{child}"),
				synced.as_ref().is_some_and(|synced| {
					synced.stable == stable(&tree_path) && synced.on_disk == synced.size
				}) && read.is_ok(),
				format!("{synced:?}, {read:?}"),
			);
		}
	}
	Ok(())
}

/// R15-R17, R19: a trashed clean file goes, dehydrated or hydrated; one edited locally just before
/// is kept, uploaded as a new file and reported; a file deleted for good goes.
fn trashes(s: &mut Scenarios) -> io::Result<()> {
	let path = "rx\\trash.txt";
	if s.ctl("R15-trash", &format!("trash {}", stable(path)))
		.is_some()
	{
		let gone = s.wait_missing(path, APPLIED);
		s.check("R15-trash-dehydrated", gone, format!("{:?}", s.entry(path)));
	}

	let path = "rx\\trashh.txt";
	if s.hydrate("R16", path)
		&& s.ctl("R16-trash", &format!("trash {}", stable(path)))
			.is_some()
	{
		let gone = s.wait_missing(path, APPLIED);
		s.check("R16-trash-hydrated", gone, format!("{:?}", s.entry(path)));
	}

	let path = "rx\\trashm.txt";
	if s.hydrate("R17", path) {
		let patch = local_bytes(1024, 24);
		write_at(&s.path(path), 10, &patch)?;
		if s.ctl("R17-trash", &format!("trash {}", stable(path)))
			.is_some()
		{
			let edited = patched(served(path), 10, &patch);
			let kept = s.expect_carried("R17-trash-modified-kept", path, &edited);
			s.check(
				"R17-uploaded-as-new",
				kept.as_ref()
					.is_some_and(|kept| kept.stable != stable(path)),
				format!("{kept:?}"),
			);
			s.check(
				"R17-reported",
				s.reported("KeptLocal", "trashm.txt"),
				"a KeptLocal report",
			);
		}
	}

	let path = "rx\\perm.txt";
	if s.ctl("R19-delete", &format!("delete {}", stable(path)))
		.is_some()
	{
		let gone = s.wait_missing(path, APPLIED);
		s.check("R19-delete-for-good", gone, format!("{:?}", s.entry(path)));
	}
	Ok(())
}

/// R18: a trashed directory with clean dehydrated and hydrated files, an edited file, an ordinary
/// file still being written and a never-populated subdirectory: the clean items go, the directory is
/// kept for the others and created on the drive anew, and they are uploaded into it. A trashed
/// directory with only clean items goes whole.
fn dir_trash(s: &mut Scenarios) -> io::Result<()> {
	let clean_ok = s.hydrate("R18b", "rx\\cleandir\\c1.bin");
	if clean_ok
		&& s.ctl("R18b-trash", &format!("trash {}", dir("rx\\cleandir")))
			.is_some()
	{
		let gone = s.wait_missing("rx\\cleandir", APPLIED);
		s.check(
			"R18b-clean-dir-removed",
			gone,
			format!("{:?}", s.entry("rx\\cleandir")),
		);
	}

	if !(s.hydrate("R18", "rx\\trdir\\b.bin") && s.hydrate("R18", "rx\\trdir\\m.bin")) {
		return Ok(());
	}
	let patch = local_bytes(512, 25);
	write_at(&s.path("rx\\trdir\\m.bin"), 0, &patch)?;
	let edited = patched(served("rx\\trdir\\m.bin"), 0, &patch);
	let plain = local_bytes(700, 26);
	let mut writing = File::create(s.path("rx\\trdir\\p.txt"))?;
	writing.write_all(&plain)?;
	writing.flush()?;
	if s.ctl("R18-trash", &format!("trash {}", dir("rx\\trdir")))
		.is_none()
	{
		return Ok(());
	}
	let cleared = wait(APPLIED, || {
		["rx\\trdir\\a.bin", "rx\\trdir\\b.bin", "rx\\trdir\\inner"]
			.iter()
			.all(|path| s.entry(path).kind() == Some("missing"))
			.then_some(())
	});
	let names = s.names("rx\\trdir");
	s.check(
		"R18-clean-children-removed",
		cleared.is_some()
			&& names
				.as_ref()
				.is_ok_and(|names| names == &["m.bin", "p.txt"]),
		format!("{names:?}"),
	);
	drop(writing);
	let m = s.expect_carried("R18-edited-kept", "rx\\trdir\\m.bin", &edited);
	let p = s.expect_carried("R18-plain-kept", "rx\\trdir\\p.txt", &plain);
	let remote = s.fresh_remote();
	let new_dir = remote
		.find("rx\\trdir")
		.and_then(|item| field(item, "uuid").map(str::to_owned));
	s.check(
		"R18-dir-created-anew",
		new_dir
			.as_deref()
			.is_some_and(|uuid| !same_uuid(uuid, &dir("rx\\trdir").to_string()))
			&& s.synced_dir("rx\\trdir").is_some_and(|local| {
				new_dir
					.as_deref()
					.is_some_and(|uuid| same_uuid(uuid, &local.to_string()))
			}) && m.is_some_and(|m| m.stable != stable("rx\\trdir\\m.bin"))
			&& p.is_some(),
		format!("remote {new_dir:?}, local {:?}", s.entry("rx\\trdir")),
	);
	s.check(
		"R18-reported",
		s.reported("DirKept", "trdir"),
		"a DirKept report",
	);
	Ok(())
}

/// Makes `path` read-only, as a user's attribute change.
fn set_readonly(path: &Path) -> io::Result<()> {
	let mut permissions = fs::metadata(path)?.permissions();
	permissions.set_readonly(true);
	fs::set_permissions(path, permissions)
}

/// R28, R29: a clean file the user made read-only goes with its remote file's trash; a directory
/// the user made read-only, which may clear its in-sync state, goes with its remote directory's
/// trash, with its clean file.
fn attributes(s: &mut Scenarios) -> io::Result<()> {
	let path = "rx\\ro.txt";
	set_readonly(&s.path(path))?;
	let resynced = wait(APPLIED, || s.synced(path));
	s.info(
		"R28-readonly-in-sync",
		format!("{resynced:?}; {:?}", s.entry(path)),
	);
	if s.ctl("R28-trash", &format!("trash {}", stable(path)))
		.is_some()
	{
		let gone = s.wait_missing(path, APPLIED);
		s.check("R28-trash-readonly", gone, format!("{:?}", s.entry(path)));
	}

	let path = "rx\\attrdir";
	if !s.hydrate("R29", "rx\\attrdir\\f.bin") {
		return Ok(());
	}
	set_readonly(&s.path(path))?;
	s.info("R29-readonly-dir", format!("{:?}", s.entry(path)));
	if s.ctl("R29-trash", &format!("trash {}", dir(path)))
		.is_some()
	{
		let gone = s.wait_missing(path, APPLIED);
		s.check(
			"R29-trash-readonly-dir",
			gone,
			format!("{:?}", s.entry(path)),
		);
	}
	Ok(())
}

/// R20: a trashed file that was removed comes back with its restore.
fn restore(s: &mut Scenarios) -> io::Result<()> {
	let path = "rx\\restore.txt";
	let uuid = stable(path);
	if s.ctl("R20-trash", &format!("trash {uuid}")).is_none() {
		return Ok(());
	}
	let gone = s.wait_missing(path, APPLIED);
	s.check("R20-trashed-removed", gone, format!("{:?}", s.entry(path)));
	if s.ctl("R20-restore", &format!("restore {uuid}")).is_some() {
		let back = wait(APPLIED, || s.synced(path));
		let read = s.reads(path, &served(path));
		s.check(
			"R20-restored",
			back.as_ref().is_some_and(|back| back.stable == uuid) && read.is_ok(),
			format!("{back:?}, {read:?}"),
		);
	}
	Ok(())
}

/// R22: a file deleted for good while the drive is unreachable stays (the cache's miss is
/// confirmed with the server first); once the drive answers again, it goes.
fn offline_delete(s: &mut Scenarios) -> io::Result<()> {
	let path = "rx\\offdel.txt";
	s.offline(true);
	let sent = s.ctl("R22-delete", &format!("delete {}", stable(path)));
	thread::sleep(Duration::from_secs(6));
	let kept = s.synced(path);
	s.offline(false);
	if sent.is_none() {
		return Ok(());
	}
	s.check(
		"R22-kept-while-offline",
		kept.is_some(),
		format!("{kept:?}"),
	);
	let gone = s.wait_missing(path, AFTER_RELEASE);
	s.check("R22-applied-online", gone, format!("{:?}", s.entry(path)));
	Ok(())
}

/// R23: changes made on the drive while the provider was not running (disconnected, still
/// registered) are applied when it starts again: a new version, a rename, a trash and a new file.
fn restart(s: &mut Scenarios) -> io::Result<()> {
	s.hydrate("R23", "rx\\stop1.txt");
	if s.ctl("R23-pause", "pause").is_none() {
		return Ok(());
	}
	let head = s.ctl("R23-bump", "bump rx\\stop1.txt");
	let renamed = s.ctl(
		"R23-rename",
		&format!("rename {} stop2b.txt", stable("rx\\stop2.txt")),
	);
	let trashed = s.ctl("R23-trash", &format!("trash {}", stable("rx\\stop3.txt")));
	let added = s.ctl("R23-add", "add rx stopnew.txt 3335");
	thread::sleep(Duration::from_secs(2));
	// Listed by this process: the provider's reading of a placeholder's state needs the root
	// connected.
	let names = s.names("rx")?;
	s.check(
		"R23-nothing-while-stopped",
		["stop1.txt", "stop2.txt", "stop3.txt"]
			.iter()
			.all(|name| names.iter().any(|held| held == name))
			&& !names
				.iter()
				.any(|name| name == "stop2b.txt" || name == "stopnew.txt"),
		format!("{names:?}"),
	);
	s.info(
		"R23-inspect-while-stopped",
		format!("{:?}", s.entry("rx\\stop2.txt").0),
	);
	if s.ctl("R23-resume", "resume").is_none() {
		return Ok(());
	}
	if let Some(head) = head {
		s.expect_updated(
			"R23-bump-applied",
			"rx\\stop1.txt",
			&head,
			&tree::bumped(8026),
		);
	}
	if renamed.is_some() {
		let moved = wait(APPLIED, || s.synced("rx\\stop2b.txt"));
		s.check(
			"R23-rename-applied",
			moved.is_some_and(|moved| moved.stable == stable("rx\\stop2.txt"))
				&& s.entry("rx\\stop2.txt").kind() == Some("missing"),
			format!("{:?}", s.entry("rx\\stop2b.txt")),
		);
	}
	if trashed.is_some() {
		let gone = s.wait_missing("rx\\stop3.txt", APPLIED);
		s.check(
			"R23-trash-applied",
			gone,
			format!("{:?}", s.entry("rx\\stop3.txt")),
		);
	}
	if let Some(uuid) = added {
		let placed = wait(APPLIED, || s.entry("rx\\stopnew.txt").file());
		s.check(
			"R23-add-applied",
			placed.is_some_and(|(stable, _)| same_uuid(&stable.to_string(), &uuid)),
			format!("{placed:?}"),
		);
	}
	Ok(())
}

/// R24: a change that tells the provider nothing (no working-set signal) is applied on the feed's
/// tick.
fn quiet(s: &mut Scenarios) -> io::Result<()> {
	let path = "rx\\quiet.txt";
	if s.ctl("R24-quiet", "quiet on").is_none() {
		return Ok(());
	}
	let head = s.ctl("R24-bump", &format!("bump {path}"));
	if let Some(head) = head {
		let started = Instant::now();
		let applied = s.wait_version(path, stable(path), &head, TICK);
		s.check(
			"R24-applied-on-tick",
			applied.is_some(),
			format!("{applied:?} after {:?}", started.elapsed()),
		);
	}
	s.ctl("R24-loud", "quiet off");
	Ok(())
}

/// R25: an expired anchor (the cache wiped) reconciles every populated directory, creating no
/// second placeholder and writing nothing to the drive.
fn wipe(s: &mut Scenarios) -> io::Result<()> {
	let before = s.names("rx")?;
	let writes = s.write_count();
	if s.ctl("R25-wipe", "wipe").is_none() {
		return Ok(());
	}
	thread::sleep(Duration::from_secs(10));
	let after = s.names("rx")?;
	let added = s.write_count() - writes;
	s.check(
		"R25-wipe-reconciled",
		before == after && added == 0,
		format!("{added} writes; names before {before:?}, after {after:?}"),
	);
	Ok(())
}

/// R27: the provider's own changes to the root wrote nothing to the drive but the uploads the
/// scenarios keep local data with: no rename, move or trash, and none at all while idle. Records
/// the count of writes for `quiet`, which checks the reader's scenarios added none.
fn no_own_writes(s: &mut Scenarios, start: usize) {
	thread::sleep(Duration::from_secs(5));
	let before = s.write_count();
	thread::sleep(IDLE);
	let remote = s.fresh_remote();
	let writes = remote.writes();
	let idle: Vec<String> = writes
		.get(before..)
		.unwrap_or_default()
		.iter()
		.map(ToString::to_string)
		.collect();
	s.check(
		"R27-idle-no-writes",
		idle.is_empty(),
		format!("{} added over {IDLE:?}: {idle:?}", idle.len()),
	);
	let both = format!("stable/{}", stable("rx\\both.txt"));
	let expected = |write: &Value| match field(write, "op") {
		// A new file goes up under a temporary name, then is renamed to its own.
		Some("Upload")
			if field(write, "name").is_some_and(|name| name.starts_with(".filen-upload-")) =>
		{
			true
		}
		Some("Upload" | "Rename") => matches!(
			field(write, "name"),
			Some(
				"mod.old.txt"
					| "holdww.old.txt"
					| "both.old.txt"
					| "trashm.txt" | "m.bin"
					| "p.txt"
			)
		),
		Some("UploadVersion") => field(write, "id") == Some(both.as_str()),
		Some("CreateDir") => field(write, "name") == Some("trdir"),
		_ => false,
	};
	let stage: Vec<&Value> = writes.get(start..).unwrap_or_default().to_vec();
	let unexpected: Vec<String> = stage
		.iter()
		.filter(|write| !expected(write))
		.map(ToString::to_string)
		.collect();
	s.check(
		"R27-only-keep-local-uploads",
		unexpected.is_empty(),
		format!(
			"{} writes in the stage, unexpected: {unexpected:?}",
			stage.len()
		),
	);
	s.info(
		"R27-writes",
		format!(
			"{:?}",
			stage.iter().map(ToString::to_string).collect::<Vec<_>>()
		),
	);
	if let Err(error) = fs::write(s.out.join("ops-mark.txt"), writes.len().to_string()) {
		s.check("R27-mark", false, error);
	}
}

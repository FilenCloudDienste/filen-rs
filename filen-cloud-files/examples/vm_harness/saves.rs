//! The save and restart scenarios: a user copying a file out while a newer version arrives, saving
//! files while the provider is not running, holding an edited file open across a restart, and the
//! engine's count of what the drive does not have yet, while `provide-local` serves the root. Every
//! scenario works in the tree's `sx` directory. Runs after `local` and before `remote`, whose
//! last step marks the drive's writes for `quiet`.

use std::{
	fs::{self, File, OpenOptions},
	io::{self, Read, Seek, SeekFrom, Write},
	path::{Path, PathBuf},
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	},
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
/// How long a scenario waits for the provider to carry a local change to the drive.
const CARRIED: Duration = Duration::from_secs(60);
/// How long a held file stays held while the provider runs, before the scenario looks.
const HELD: Duration = Duration::from_secs(15);
/// The pause between a copier's reads of 64 KiB: a 32 MiB copy takes over 10 s.
const COPY_PACE: Duration = Duration::from_millis(20);
/// How long the drive must see no write for the provider to count as idle: no upload, convert or
/// version in flight.
const QUIET: Duration = Duration::from_secs(5);
/// How long a scenario waits for the provider to go idle, and then for its count of what the drive
/// does not have to settle.
const COUNT_SETTLE: Duration = Duration::from_secs(60);
/// The control commands' first number, past any `local` used and below `remote`'s.
const FIRST_COMMAND: u32 = 30_000;

impl Scenarios {
	/// Sends `command`, failing `step` when it is refused.
	fn send(&mut self, step: &str, command: &str) -> Option<String> {
		match self.control(command) {
			Ok(answer) => Some(answer),
			Err(error) => {
				self.check(step, false, format!("{command:?}: {error}"));
				None
			}
		}
	}

	/// The engine's count of items the drive does not have yet: `None` when it does not know.
	fn pending(&self) -> Option<u64> {
		let answer = self.control("pending").ok()?;
		serde_json::from_str::<Value>(&answer).ok()?.as_u64()
	}

	/// Whether the drive saw no write for [`QUIET`], within `limit`: nothing the provider carries is
	/// in flight.
	fn idle(&self, limit: Duration) -> bool {
		let mut last = None;
		let mut since = Instant::now();
		wait(limit, || {
			let writes = self.fresh_remote().writes().len();
			if last != Some(writes) {
				last = Some(writes);
				since = Instant::now();
			}
			let quiet = since.elapsed() >= QUIET;
			if !quiet {
				thread::sleep(Duration::from_secs(1));
			}
			quiet.then_some(())
		})
		.is_some()
	}

	/// The count once the provider is idle, known and the same twice 3 s apart, within
	/// [`COUNT_SETTLE`] each; with whether the provider went idle.
	fn idle_pending(&self) -> (bool, Option<u64>) {
		let idle = self.idle(COUNT_SETTLE);
		(idle, self.settled_pending(COUNT_SETTLE))
	}

	/// Checks under `step` that the count settled back on `base` once the provider is idle: a count
	/// read while something is in flight is never taken, and one that stays off `base`, above or
	/// below, fails.
	fn count_back(&mut self, step: &str, base: Option<u64>) {
		let (idle, after) = self.idle_pending();
		self.check(
			step,
			idle && base.is_some() && after == base,
			format!("base {base:?}, after {after:?}, idle {idle}"),
		);
	}

	/// The count once it is known and the same twice 3 s apart, within `limit`.
	fn settled_pending(&self, limit: Duration) -> Option<u64> {
		let mut last = None;
		wait(limit, || {
			let now = self.pending();
			let settled = now.is_some() && now == last;
			last = now;
			if !settled {
				thread::sleep(Duration::from_secs(3));
			}
			now.filter(|_| settled)
		})
	}

	/// Reads the tree's file `path` whole and checks it holds what the tree serves; then lets the
	/// provider settle what the hydration raised.
	fn read_served(&mut self, step: &str, path: &str) -> bool {
		let read = fs::read(self.path(path));
		let ok = read.as_ref().is_ok_and(|read| *read == served(path));
		self.check(
			step,
			ok,
			format!("{path}: {:?}", read.map(|read| read.len())),
		);
		thread::sleep(Duration::from_secs(3));
		ok
	}

	/// Waits until the file placeholder at `path` is clean, of `stable` at `version`.
	fn at_version(
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

	/// The drive's item `stable`: its version and its content's hash.
	fn remote_head(&self, stable: Uuid) -> Option<(String, String)> {
		let remote = self.fresh_remote();
		let item = remote.by_uuid(&stable.to_string())?;
		Some((
			field(item, "version")?.to_owned(),
			field(item, "hash")?.to_owned(),
		))
	}

	/// How many new versions of the file `stable` the provider uploaded so far.
	fn versions_uploaded(&self, stable: Uuid) -> usize {
		self.fresh_remote()
			.count("UploadVersion", "id", &format!("stable/{stable}"))
	}

	/// Whether a local report naming `what` and `name` was logged.
	fn reported_as(&self, what: &str, name: &str) -> bool {
		self.reports()
			.iter()
			.any(|report| report.contains(what) && report.contains(name))
	}
}

pub(crate) fn run(root: &Path, out: &Path) -> Outcome {
	let mut s = Scenarios::open(root, out, "saves.txt", FIRST_COMMAND)?;
	let result = run_all(&mut s);
	if let Err(error) = &result {
		s.report.fail("harness", error);
	}
	fs::write(out.join("saves.done"), "done")?;
	Ok(s.report.passed())
}

fn run_all(s: &mut Scenarios) -> io::Result<()> {
	if !crate::report::wait_for(&s.out.join("provider.ready"), Duration::from_secs(60)) {
		s.check(
			"S0-ready",
			false,
			"the provider did not get ready within 60 s",
		);
		return Ok(());
	}
	let mut names: Vec<String> = fs::read_dir(s.path("sx"))?
		.map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
		.collect::<io::Result<_>>()?;
	names.sort();
	s.info("S0-list-sx", format!("{names:?}"));
	thread::sleep(Duration::from_secs(3));
	copy_during_update(s)?;
	copy_file_during_update(s)?;
	stopped_saves(s)?;
	held_across_restart(s)?;
	pending_held(s)?;
	pending_offline(s)?;
	Ok(())
}

/// A copy of `from` into `to`, read in 64 KiB steps `pace` apart from a handle shared with every
/// other, as a slow copier reads: what it read, and `done` set once it ends.
fn slow_copy(
	from: PathBuf,
	to: PathBuf,
	pace: Duration,
	done: Arc<AtomicBool>,
) -> thread::JoinHandle<io::Result<Vec<u8>>> {
	thread::spawn(move || {
		let copied = (|| {
			let mut file = File::open(&from)?;
			let mut copy = Vec::new();
			let mut buffer = vec![0; 65_536];
			loop {
				let read = file.read(&mut buffer)?;
				if read == 0 {
					break;
				}
				copy.extend_from_slice(buffer.get(..read).unwrap_or_default());
				thread::sleep(pace);
			}
			fs::write(&to, &copy)?;
			Ok(copy)
		})();
		done.store(true, Ordering::SeqCst);
		copied
	})
}

/// Which version `bytes` are: `old`, `new`, or neither (a mixed or damaged copy).
fn which(bytes: &[u8], old: &[u8], new: &[u8]) -> &'static str {
	if bytes == old {
		"old"
	} else if bytes == new {
		"new"
	} else {
		"neither"
	}
}

/// S1: a newer version of a downloaded file arrives while another process copies it out. The copy
/// is one version whole, never the old bytes then the new; the update waits for the copier to let
/// go (`CfDehydratePlaceholder` refuses a file in use), then the file reads as the new version.
fn copy_during_update(s: &mut Scenarios) -> io::Result<()> {
	let path = "sx\\copy.bin";
	if !s.read_served("S1-hydrate", path) {
		return Ok(());
	}
	let before = s.synced(path);
	let done = Arc::new(AtomicBool::new(false));
	let copier = slow_copy(
		s.path(path),
		s.out.join("copy-s1.bin"),
		COPY_PACE,
		Arc::clone(&done),
	);
	thread::sleep(Duration::from_secs(1));
	let head = s.send("S1-bump", &format!("bump {path}"));
	thread::sleep(Duration::from_secs(6));
	let during = s.synced(path);
	let copying = !done.load(Ordering::SeqCst);
	if copying {
		s.check(
			"S1-deferred-while-copied",
			during.is_some() && during == before,
			format!("before {before:?}, during {during:?}"),
		);
	} else {
		s.info(
			"S1-deferred-while-copied",
			format!("the copy ended within 7 s; during {during:?}"),
		);
	}
	let copied = copier
		.join()
		.unwrap_or_else(|_| Err(io::Error::other("the copier panicked")));
	let size = tree::size_of(path).unwrap_or(0);
	let (old, new) = (served(path), tree::bumped(size));
	match &copied {
		Ok(bytes) => s.check(
			"S1-copy-one-version",
			which(bytes, &old, &new) != "neither",
			format!(
				"{} bytes, the {} version",
				bytes.len(),
				which(bytes, &old, &new)
			),
		),
		Err(error) => s.check("S1-copy-one-version", false, error),
	}
	let Some(head) = head else {
		return Ok(());
	};
	let released = Instant::now();
	let applied = s.at_version(path, stable(path), &head, AFTER_RELEASE);
	s.check(
		"S1-applied-after-copy",
		applied.as_ref().is_some_and(|applied| applied.on_disk == 0),
		format!("{applied:?} {:?} after the copy", released.elapsed()),
	);
	let read = fs::read(s.path(path));
	s.check(
		"S1-reads-new",
		read.as_ref().is_ok_and(|read| *read == new),
		format!("{:?}", read.map(|read| which(&read, &old, &new))),
	);
	Ok(())
}

/// S2: the same with `CopyFileW` (`std::fs::copy`), the newer version sent as the copy starts:
/// the copy is one version whole, and the file reaches the newer version.
fn copy_file_during_update(s: &mut Scenarios) -> io::Result<()> {
	let path = "sx\\copy.bin";
	let size = tree::size_of(path).unwrap_or(0);
	let (old, new) = (tree::bumped(size), tree::added(size));
	let read = fs::read(s.path(path));
	if !read.as_ref().is_ok_and(|read| *read == old) {
		s.check(
			"S2-hydrate",
			false,
			format!("{:?}", read.map(|read| read.len())),
		);
		return Ok(());
	}
	thread::sleep(Duration::from_secs(3));
	let (from, to) = (s.path(path), s.out.join("copy-s2.bin"));
	let copier = thread::spawn(move || fs::copy(&from, &to).and_then(|_| fs::read(&to)));
	let head = s.send("S2-bump", &format!("bump2 {path}"));
	let copied = copier
		.join()
		.unwrap_or_else(|_| Err(io::Error::other("the copier panicked")));
	match &copied {
		Ok(bytes) => s.check(
			"S2-copy-one-version",
			which(bytes, &old, &new) != "neither",
			format!(
				"{} bytes, the {} version",
				bytes.len(),
				which(bytes, &old, &new)
			),
		),
		Err(error) => s.check("S2-copy-one-version", false, error),
	}
	if let Some(head) = head {
		let applied = s.at_version(path, stable(path), &head, AFTER_RELEASE);
		let read = fs::read(s.path(path));
		s.check(
			"S2-reads-newest",
			applied.is_some() && read.as_ref().is_ok_and(|read| *read == new),
			format!(
				"{applied:?}, {:?}",
				read.map(|read| which(&read, &old, &new))
			),
		);
	}
	Ok(())
}

/// An editor's save while the provider is not running: the file renamed to a backup, the new
/// content written at the name, the backup deleted.
fn save_aside(s: &Scenarios, path: &str, bytes: &[u8]) -> io::Result<()> {
	let backup = s.path(&format!("{path}.bak"));
	fs::rename(s.path(path), &backup)?;
	fs::write(s.path(path), bytes)?;
	fs::remove_file(&backup)
}

/// S3: saves made while the provider was not running (disconnected, still registered), each
/// carried as a new version of the same file once it runs again, under its name, with nothing
/// set aside: a save through a backup (S3a), an overwrite in place (S3b), a temporary file
/// renamed over the file (S3c). With a newer version on the drive meanwhile, the save is kept
/// beside it under a `.old` name and the newer version keeps the name (S3d).
fn stopped_saves(s: &mut Scenarios) -> io::Result<()> {
	let files = [
		"sx\\save.txt",
		"sx\\over.txt",
		"sx\\repl.txt",
		"sx\\conf.txt",
	];
	let mut before = Vec::new();
	for path in files {
		if !s.read_served(&format!("S3-hydrate-{path}"), path) {
			return Ok(());
		}
		before.push(s.synced(path));
	}
	// The engine keeps which file each path holds about 2 s after it changes.
	thread::sleep(Duration::from_secs(4));
	let uploads_before: Vec<usize> = files
		.iter()
		.map(|path| s.versions_uploaded(stable(path)))
		.collect();
	if s.send("S3-pause", "pause").is_none() {
		return Ok(());
	}
	let saved = local_bytes(10_001 + 300, 31);
	let over = local_bytes(10_002 - 2_000, 32);
	let replaced = local_bytes(10_003 + 100, 33);
	let conflicted = local_bytes(10_004 + 50, 34);
	let done = save_aside(s, "sx\\save.txt", &saved)
		.and_then(|()| fs::write(s.path("sx\\over.txt"), &over))
		.and_then(|()| fs::write(s.path("sx\\repl.txt.tmp"), &replaced))
		.and_then(|()| fs::rename(s.path("sx\\repl.txt.tmp"), s.path("sx\\repl.txt")))
		.and_then(|()| save_aside(s, "sx\\conf.txt", &conflicted));
	s.check("S3-saved-while-stopped", done.is_ok(), format!("{done:?}"));
	let head = s.send("S3d-bump", "bump sx\\conf.txt");
	if s.send("S3-resume", "resume").is_none() {
		return Ok(());
	}
	for (n, (step, path, bytes)) in [
		("S3a-save-aside", "sx\\save.txt", &saved),
		("S3b-overwrite", "sx\\over.txt", &over),
		("S3c-rename-over", "sx\\repl.txt", &replaced),
	]
	.into_iter()
	.enumerate()
	{
		let started = Instant::now();
		let found = wait(CARRIED, || {
			let synced = s.synced(path)?;
			let (version, hash) = s.remote_head(stable(path))?;
			(synced.stable == stable(path)
				&& same_uuid(&version, &synced.version.to_string())
				&& hash == hex_hash(bytes))
			.then_some(synced)
		});
		let local = fs::read(s.path(path));
		let name = path.rsplit('\\').next().unwrap_or(path);
		let old_name = format!("{}.old.txt", name.trim_end_matches(".txt"));
		let remote = s.fresh_remote();
		s.check(
			step,
			found.as_ref().is_some_and(|found| {
				before
					.get(n)
					.and_then(Option::as_ref)
					.is_some_and(|before| before.version != found.version)
			}) && local.as_ref().is_ok_and(|local| local == bytes)
				&& remote.uploads(name) == 0
				&& !remote.trashed(&stable(path).to_string())
				&& s.versions_uploaded(stable(path))
					== uploads_before.get(n).copied().unwrap_or(0) + 1
				&& s.entry(&format!("sx\\{old_name}")).kind() == Some("missing"),
			format!(
				"{found:?} after {:?}; local {:?}; before {:?}",
				started.elapsed(),
				local.map(|local| local.len()),
				before.get(n)
			),
		);
	}
	let path = "sx\\conf.txt";
	let aside = s.expect_carried("S3d-save-kept-aside", "sx\\conf.old.txt", &conflicted);
	if let Some(head) = head {
		let placed = s.at_version(path, stable(path), &head, APPLIED);
		let read = fs::read(s.path(path));
		s.check(
			"S3d-newer-at-name",
			placed.is_some()
				&& read
					.as_ref()
					.is_ok_and(|read| *read == tree::bumped(10_004))
				&& aside.is_some_and(|aside| aside.stable != stable(path)),
			format!("{placed:?}, {:?}", read.map(|read| read.len())),
		);
		s.check(
			"S3d-reported",
			s.reported_as("EditVsEdit", "conf.old.txt"),
			"an EditVsEdit report",
		);
	}
	Ok(())
}

/// Writes `patch` at `at` into `path` through a handle of its own, as an editor saving in place.
fn write_at(path: &Path, at: u64, patch: &[u8]) -> io::Result<()> {
	let mut file = OpenOptions::new().write(true).open(path)?;
	file.seek(SeekFrom::Start(at))?;
	file.write_all(patch)
}

/// S4: an edit whose upload was killed on its way (the cache holds it marked), a newer version
/// on the drive meanwhile, and the file held open by its editor when the provider starts again.
/// The marked edit never replaces the newer version; once the editor lets go, the edit is kept
/// beside it under a `.old` name and the newer version keeps the name.
fn held_across_restart(s: &mut Scenarios) -> io::Result<()> {
	let path = "sx\\f1.txt";
	if !s.read_served("S4-hydrate", path) {
		return Ok(());
	}
	let uploads = s.versions_uploaded(stable(path));
	if s.send("S4-pause", "pause").is_none() {
		return Ok(());
	}
	let patch = local_bytes(4096, 35);
	write_at(&s.path(path), 0, &patch)?;
	let edited = patched(served(path), 0, &patch);
	let marked = s.send("S4-mark", &format!("mark {path}"));
	s.check(
		"S4-marked",
		marked.as_deref() == Some(edited.len().to_string().as_str()),
		format!("{marked:?}"),
	);
	let head = s.send("S4-bump", &format!("bump {path}"));
	let writer = OpenOptions::new().write(true).open(s.path(path))?;
	if s.send("S4-resume", "resume").is_none() {
		return Ok(());
	}
	thread::sleep(HELD);
	let bumped = tree::bumped(10_005);
	let during = s.remote_head(stable(path));
	s.check(
		"S4-head-kept-while-held",
		head.as_ref()
			.zip(during.as_ref())
			.is_some_and(|(head, (version, hash))| {
				same_uuid(head, version) && *hash == hex_hash(&bumped)
			}) && s.versions_uploaded(stable(path)) == uploads,
		format!("head {head:?}, drive {during:?}"),
	);
	let pending = s.pending();
	s.check(
		"S4-pending-while-held",
		pending.is_some_and(|pending| pending >= 1),
		format!("{pending:?}"),
	);
	drop(writer);
	let aside = s.expect_carried("S4-edit-kept-aside", "sx\\f1.old.txt", &edited);
	if let Some(head) = head {
		let placed = s.at_version(path, stable(path), &head, AFTER_RELEASE);
		let read = fs::read(s.path(path));
		let after = s.remote_head(stable(path));
		s.check(
			"S4-newer-at-name",
			placed.is_some()
				&& read.as_ref().is_ok_and(|read| *read == bumped)
				&& aside.is_some_and(|aside| aside.stable != stable(path))
				&& after.is_some_and(|(version, hash)| {
					same_uuid(&head, &version) && hash == hex_hash(&bumped)
				}) && s.versions_uploaded(stable(path)) == uploads,
			format!("{placed:?}, {:?}", read.map(|read| read.len())),
		);
	}
	let copies: Vec<String> = fs::read_dir(s.path("sx"))?
		.filter_map(Result::ok)
		.map(|entry| entry.file_name().to_string_lossy().into_owned())
		.filter(|name| name.starts_with("f1"))
		.collect();
	s.check("S4-one-copy", copies.len() == 2, format!("{copies:?}"));
	Ok(())
}

/// S5: a file an editor holds open with an edit written counts as not on the drive until the
/// editor lets go and the edit is carried.
fn pending_held(s: &mut Scenarios) -> io::Result<()> {
	let path = "sx\\pend.txt";
	if !s.read_served("S5-hydrate", path) {
		return Ok(());
	}
	let (idle, base) = s.idle_pending();
	s.info("S5-base", format!("{base:?}, idle {idle}"));
	let patch = local_bytes(2048, 36);
	let mut writer = OpenOptions::new().write(true).open(s.path(path))?;
	writer.write_all(&patch)?;
	writer.flush()?;
	thread::sleep(Duration::from_secs(8));
	let held = s.pending();
	s.check(
		"S5-counted-while-held",
		base.zip(held).is_some_and(|(base, held)| held > base),
		format!("base {base:?}, held {held:?}"),
	);
	drop(writer);
	s.expect_carried("S5-carried", path, &patched(served(path), 0, &patch));
	s.count_back("S5-count-back", base);
	Ok(())
}

/// S6: a new file written while the drive is unreachable counts as not on the drive until the
/// drive answers again and it is carried.
fn pending_offline(s: &mut Scenarios) -> io::Result<()> {
	let path = "sx\\offnew.txt";
	let (idle, base) = s.idle_pending();
	s.info("S6-base", format!("{base:?}, idle {idle}"));
	s.offline(true);
	let bytes = local_bytes(5_555, 37);
	fs::write(s.path(path), &bytes)?;
	thread::sleep(Duration::from_secs(10));
	let offline = s.pending();
	let listed = s.fresh_remote().find(path).is_some();
	s.offline(false);
	s.check(
		"S6-counted-while-offline",
		base.zip(offline)
			.is_some_and(|(base, offline)| offline > base)
			&& !listed,
		format!("base {base:?}, offline {offline:?}, on the drive {listed}"),
	);
	s.expect_carried("S6-carried", path, &bytes);
	s.count_back("S6-count-back", base);
	Ok(())
}

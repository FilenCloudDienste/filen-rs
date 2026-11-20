//! The local-change scenarios: a user's process changing the sync root while `provide-local` serves
//! it, each change checked on the fake drive, through the dump the provider writes to
//! `<out>\remote.json`, and locally, through the core's own reading of a placeholder's state.
//! Every scenario works in the tree's `lx` directory, or moves an item between it and
//! `<out>\outside`, so the reader's scenarios (`check`) still find the rest of the tree as served.

use std::{
	cell::Cell,
	fmt::Display,
	fs::{self, File, OpenOptions},
	io::{self, Seek, SeekFrom, Write},
	os::windows::fs::MetadataExt,
	path::{Path, PathBuf},
	process::Command,
	thread,
	time::{Duration, Instant},
};

use filen_cloud_files::{
	LocalEntry, LocalFs, RootPath, SyncRootPath, WinLocalFs, test_support::pattern,
};
use filen_types::fs::Uuid;
use serde_json::Value;

use crate::{
	report::{Outcome, Report, events, field},
	saves_loop::{self, FailedReplace},
	stall::Budget,
	tree,
};

/// How long a scenario waits for the provider to carry a change to the drive.
const CARRIED: Duration = Duration::from_secs(40);
/// How long the provider must stay idle at the end: no drive write while nothing changes locally.
const IDLE: Duration = Duration::from_secs(20);
/// `ERROR_CLOUD_FILE_NETWORK_UNAVAILABLE`: what a veto for an unreachable server shows the user.
const NETWORK_UNAVAILABLE: i32 = 388;

/// The bytes a scenario writes locally where the pattern would be, `salt` telling scenarios apart.
pub(crate) fn local_bytes(size: usize, salt: u8) -> Vec<u8> {
	pattern(size)
		.into_iter()
		.map(|byte| byte ^ 0x33 ^ salt)
		.collect()
}

/// `bytes` with `patch` written over them at `at`, as a write in place leaves a file.
pub(crate) fn patched(mut bytes: Vec<u8>, at: usize, patch: &[u8]) -> Vec<u8> {
	if let Some(target) = bytes.get_mut(at..at + patch.len()) {
		target.copy_from_slice(patch);
	}
	bytes
}

/// The hex BLAKE3 hash of `bytes`, as the dump writes a file's.
pub(crate) fn hex_hash(bytes: &[u8]) -> String {
	blake3::hash(bytes).to_hex().to_string()
}

/// Polls `probe` every 250 ms until it returns something or `limit` passes.
pub(crate) fn wait<T>(limit: Duration, mut probe: impl FnMut() -> Option<T>) -> Option<T> {
	let mut budget = Budget::new(limit);
	loop {
		if let Some(found) = probe() {
			return Some(found);
		}
		if budget.spent() {
			return None;
		}
		thread::sleep(Duration::from_millis(250));
	}
}

/// Whether two uuid texts name the same uuid.
pub(crate) fn same_uuid(a: &str, b: &str) -> bool {
	matches!((Uuid::parse_str(a), Uuid::parse_str(b)), (Ok(a), Ok(b)) if a == b)
}

/// The drive as the provider last dumped it.
pub(crate) struct Remote(Value);

impl Remote {
	pub(crate) fn items(&self) -> &[Value] {
		self.0
			.get("items")
			.and_then(Value::as_array)
			.map_or(&[], Vec::as_slice)
	}

	/// The listed item at `path`, by its exact names from the root.
	pub(crate) fn find(&self, path: &str) -> Option<&Value> {
		let mut dir = self.0.get("root")?.as_str()?.to_owned();
		let mut found = None;
		for name in path.split('\\') {
			let item = self.items().iter().find(|item| {
				field(item, "dir") == Some(dir.as_str()) && field(item, "name") == Some(name)
			})?;
			dir = format!("stable/{}", field(item, "uuid")?);
			found = Some(item);
		}
		found
	}

	/// The listed item `uuid`, wherever it is.
	pub(crate) fn by_uuid(&self, uuid: &str) -> Option<&Value> {
		self.items()
			.iter()
			.find(|item| field(item, "uuid").is_some_and(|held| same_uuid(held, uuid)))
	}

	/// The items in the trash.
	pub(crate) fn trash(&self) -> &[Value] {
		self.0
			.get("trash")
			.and_then(Value::as_array)
			.map_or(&[], Vec::as_slice)
	}

	/// Whether the item `uuid` is in the trash.
	pub(crate) fn trashed(&self, uuid: &str) -> bool {
		self.0
			.get("trash")
			.and_then(Value::as_array)
			.is_some_and(|trash| {
				trash
					.iter()
					.any(|item| field(item, "uuid").is_some_and(|held| same_uuid(held, uuid)))
			})
	}

	/// The calls that changed the drive, in order.
	pub(crate) fn writes(&self) -> Vec<&Value> {
		self.0
			.get("ops")
			.and_then(Value::as_array)
			.map(|ops| {
				ops.iter()
					.filter(|op| {
						!matches!(
							field(op, "op"),
							Some("List" | "Refresh" | "CopyPending") | None
						)
					})
					.collect()
			})
			.unwrap_or_default()
	}

	/// How many writes `op` with `key` equal to `value` the drive answered.
	pub(crate) fn count(&self, op: &str, key: &str, value: &str) -> usize {
		self.writes()
			.iter()
			.filter(|write| field(write, "op") == Some(op) && field(write, key) == Some(value))
			.count()
	}

	/// How many files named `name` the provider uploaded: an `Upload` under that name, or a file
	/// uploaded under a temporary name and then renamed to it, as every new file now goes up (so a
	/// same-name file another device makes meanwhile is never versioned over). Such a rename is the
	/// first write naming its item, which is not one of the tree's.
	pub(crate) fn uploads(&self, name: &str) -> usize {
		let mut seen: Vec<&str> = Vec::new();
		let mut count = 0;
		for write in self.writes() {
			let id = field(write, "id");
			let named = field(write, "name") == Some(name);
			match field(write, "op") {
				Some("Upload") if named => count += 1,
				Some("Rename") if named => {
					let first = id.is_some_and(|id| !seen.contains(&id));
					let from_tree = id
						.and_then(|id| Uuid::parse_str(id.trim_start_matches("stable/")).ok())
						.is_some_and(|uuid| uuid.as_u128() < 1 << 32);
					if first && !from_tree {
						count += 1;
					}
				}
				_ => {}
			}
			if let Some(id) = id {
				seen.push(id);
			}
		}
		count
	}

	/// How many reads the drive was asked for.
	pub(crate) fn reads(&self) -> u64 {
		self.0.get("reads").and_then(Value::as_u64).unwrap_or(0)
	}
}

/// What a placeholder in sync holds, as the core reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Synced {
	pub(crate) stable: Uuid,
	pub(crate) version: Uuid,
	pub(crate) size: u64,
	pub(crate) on_disk: u64,
}

/// What the core reads at a path, as the provider answered an `inspect`.
#[derive(Debug)]
pub(crate) struct Seen(pub(crate) Value);

impl Seen {
	pub(crate) fn kind(&self) -> Option<&str> {
		field(&self.0, "kind")
	}

	pub(crate) fn flag(&self, name: &str) -> bool {
		self.0.get(name).and_then(Value::as_bool).unwrap_or(false)
	}

	pub(crate) fn number(&self, name: &str) -> u64 {
		self.0.get(name).and_then(Value::as_u64).unwrap_or(0)
	}

	/// The identity's field `name`, as a uuid.
	pub(crate) fn identity(&self, name: &str) -> Option<Uuid> {
		Uuid::parse_str(field(self.0.get("identity")?, name)?).ok()
	}

	/// The file placeholder's stable id and version, whatever its sync state.
	pub(crate) fn file(&self) -> Option<(Uuid, Uuid)> {
		(self.kind() == Some("placeholder") && !self.flag("dir"))
			.then(|| Some((self.identity("stable")?, self.identity("version")?)))
			.flatten()
	}
}

/// The scenarios' state: where they run and what they found.
pub(crate) struct Scenarios {
	root: PathBuf,
	pub(crate) out: PathBuf,
	outside: PathBuf,
	/// The core's reading of the root from this process, which is not the provider.
	fs: WinLocalFs,
	pub(crate) report: Report,
	commands: Cell<u32>,
}

impl Scenarios {
	/// The full path of `path`, a path below the root.
	pub(crate) fn path(&self, path: &str) -> PathBuf {
		self.root.join(path)
	}

	/// The drive as last dumped, waiting up to 10 s for a dump that parses.
	pub(crate) fn remote(&self) -> Remote {
		let file = self.out.join("remote.json");
		let dump = wait(Duration::from_secs(10), || {
			serde_json::from_str(&fs::read_to_string(&file).ok()?).ok()
		});
		Remote(dump.unwrap_or(Value::Null))
	}

	/// The drive's dump once it was written after now: one that saw what just happened.
	pub(crate) fn fresh_remote(&self) -> Remote {
		thread::sleep(Duration::from_millis(1300));
		self.remote()
	}

	/// What the core reads at `path`, in the provider's process.
	pub(crate) fn entry(&self, path: &str) -> Seen {
		let answer = self.control(&format!("inspect {path}"));
		Seen(match answer {
			Ok(json) => serde_json::from_str(&json).unwrap_or(Value::Null),
			Err(error) => serde_json::json!({ "kind": "error", "error": error }),
		})
	}

	/// What the core reads at `path` in this process, which is not the provider's.
	pub(crate) fn own_entry(&self, path: &str) -> Result<LocalEntry, String> {
		let at = RootPath::from_relative(Path::new(path)).map_err(|error| error.to_string())?;
		self.fs.inspect(&at).map_err(|error| error.to_string())
	}

	/// The file placeholder at `path` when it is in sync with nothing written since.
	pub(crate) fn synced(&self, path: &str) -> Option<Synced> {
		let seen = self.entry(path);
		let (stable, version) = seen.file()?;
		// Missing, `modified` would read as nothing written.
		let unmodified = seen.0.get("modified").and_then(Value::as_u64) == Some(0);
		(seen.flag("in_sync") && unmodified).then(|| Synced {
			stable,
			version,
			size: seen.number("size"),
			on_disk: seen.number("on_disk"),
		})
	}

	/// The directory placeholder at `path` when it is in sync: its uuid.
	pub(crate) fn synced_dir(&self, path: &str) -> Option<Uuid> {
		let seen = self.entry(path);
		(seen.kind() == Some("placeholder") && seen.flag("dir") && seen.flag("in_sync"))
			.then(|| seen.identity("uuid"))
			.flatten()
	}

	/// Whether the item at `path` is an ordinary one, not a placeholder.
	pub(crate) fn plain(&self, path: &str) -> bool {
		self.entry(path).kind() == Some("plain")
	}

	/// Waits until the file at `path` is a placeholder in sync whose identity is the drive's file at
	/// `path`, holding `bytes` both locally and on the drive; what it holds, or why not.
	pub(crate) fn carried(&self, path: &str, bytes: &[u8]) -> Result<Synced, String> {
		let hash = hex_hash(bytes);
		let found = wait(CARRIED, || {
			let synced = self.synced(path)?;
			let remote = self.remote();
			let item = remote.find(path)?;
			(field(item, "uuid").is_some_and(|uuid| same_uuid(uuid, &synced.stable.to_string()))
				&& field(item, "version")
					.is_some_and(|version| same_uuid(version, &synced.version.to_string()))
				&& field(item, "hash") == Some(hash.as_str()))
			.then_some(synced)
		});
		let Some(synced) = found else {
			let remote = self.remote();
			return Err(format!(
				"not carried within {CARRIED:?}: local {:?}; remote {:?}",
				self.entry(path),
				remote.find(path)
			));
		};
		match fs::read(self.path(path)) {
			Ok(local) if local == bytes => Ok(synced),
			Ok(local) => Err(format!("local bytes differ ({} bytes)", local.len())),
			Err(error) => Err(format!("reading it back failed: {error}")),
		}
	}

	/// Waits until the drive holds the item `uuid` in its trash.
	pub(crate) fn trashed(&self, uuid: Uuid) -> bool {
		wait(CARRIED, || {
			self.remote().trashed(&uuid.to_string()).then_some(())
		})
		.is_some()
	}

	/// Sends the provider a control command and waits for its answer, cleaning up after it.
	pub(crate) fn control(&self, command: &str) -> Result<String, String> {
		let n = self.commands.get() + 1;
		self.commands.set(n);
		let name = format!("ctl-{n:05}");
		let temp = self.out.join(format!("{name}.tmp"));
		let sent = self.out.join(format!("{name}.txt"));
		fs::write(&temp, command)
			.and_then(|()| fs::rename(&temp, &sent))
			.map_err(|error| error.to_string())?;
		let answer = self.out.join(format!("{name}.done"));
		// No claim on how fast the provider answers, which a loaded VM's disk holds up for tens of
		// seconds (measured, see `stall`): the budget only tells a provider that stopped.
		let answered = wait(Duration::from_secs(60), || fs::read_to_string(&answer).ok());
		// The command first, so the provider never sees it unanswered again.
		let _ = fs::remove_file(&sent);
		let _ = fs::remove_file(&answer);
		let answered = answered.ok_or_else(|| format!("no answer to {command:?}"))?;
		match answered.strip_prefix("ok ") {
			Some(value) => Ok(value.to_owned()),
			None => Err(answered),
		}
	}

	/// Takes the drive offline or back online, and waits for the provider to say it has: a step
	/// that follows acts on the drive as switched.
	pub(crate) fn offline(&self, offline: bool) {
		let flag = self.out.join("offline");
		let set = if offline {
			fs::write(&flag, "offline")
		} else {
			fs::remove_file(&flag)
		};
		if let Err(error) = set {
			eprintln!("switching the drive offline={offline} failed: {error}");
		}
		let wanted = if offline { "offline" } else { "online" };
		// As for a control command: the budget only tells a provider that stopped.
		let seen = wait(Duration::from_secs(60), || {
			let seen = fs::read_to_string(self.out.join("offline.seen")).ok()?;
			(seen == wanted).then_some(())
		});
		if seen.is_none() {
			eprintln!("the provider did not see the drive {wanted}");
		}
	}

	/// The local reports the provider logged so far.
	pub(crate) fn reports(&self) -> Vec<String> {
		events(&self.out)
			.iter()
			.filter(|event| field(event, "message") == Some("on_local_report"))
			.filter_map(|event| field(event, "report").map(str::to_owned))
			.collect()
	}

	pub(crate) fn check(&mut self, step: &str, ok: bool, detail: impl Display) {
		self.report.check(step, ok, detail);
	}

	pub(crate) fn info(&mut self, step: &str, detail: impl Display) {
		self.report.info(step, detail);
	}

	/// Checks `path` was carried with `bytes` under `step`, returning what it holds.
	pub(crate) fn expect_carried(
		&mut self,
		step: &str,
		path: &str,
		bytes: &[u8],
	) -> Option<Synced> {
		let started = Instant::now();
		match self.carried(path, bytes) {
			Ok(synced) => {
				self.check(
					step,
					true,
					format!("{synced:?} after {:?}", started.elapsed()),
				);
				Some(synced)
			}
			Err(error) => {
				self.check(step, false, error);
				None
			}
		}
	}
}

/// The tree's stable id of `path`.
pub(crate) fn stable(path: &str) -> Uuid {
	tree::stable_of(path).expect("a file of the tree")
}

/// The tree's pattern for `path`.
pub(crate) fn served(path: &str) -> Vec<u8> {
	pattern(tree::size_of(path).expect("a file of the tree"))
}

impl Scenarios {
	/// Scenarios on `root` that `provide-local` serves into `<out>`, reporting into `<out>\<report>`;
	/// control commands are numbered from `first_command`, so two runs never reuse a name.
	pub(crate) fn open(
		root: &Path,
		out: &Path,
		report: &str,
		first_command: u32,
	) -> Result<Self, Box<dyn std::error::Error>> {
		let outside = out.join("outside");
		fs::create_dir_all(&outside)?;
		Ok(Self {
			root: root.to_path_buf(),
			out: out.to_path_buf(),
			outside,
			fs: WinLocalFs::new(&SyncRootPath::from_path(root)?),
			report: Report::create(&out.join(report))?,
			commands: Cell::new(first_command),
		})
	}
}

pub(crate) fn run(root: &Path, out: &Path) -> Outcome {
	let mut scenarios = Scenarios::open(root, out, "local.txt", 0)?;
	let result = run_all(&mut scenarios);
	if let Err(error) = &result {
		scenarios.report.fail("harness", error);
	}
	fs::write(out.join("local.done"), "done")?;
	Ok(scenarios.report.passed())
}

fn run_all(s: &mut Scenarios) -> io::Result<()> {
	if !crate::report::wait_for(&s.out.join("provider.ready"), Duration::from_secs(60)) {
		s.check(
			"L0-ready",
			false,
			"the provider did not get ready within 60 s",
		);
		return Ok(());
	}
	populate(s)?;
	new_file(s)?;
	chunked(s)?;
	held(s)?;
	edit(s)?;
	save_by_rename(s, "lx\\save.txt", "L5a-save-std-rename", |from, to| {
		fs::rename(from, to)
	})?;
	save_by_rename(s, "lx\\save2.txt", "L5b-save-move-y", |from, to| {
		let status = Command::new("cmd")
			.arg("/c")
			.arg("move")
			.arg("/y")
			.arg(from)
			.arg(to)
			.output()?;
		if status.status.success() {
			Ok(())
		} else {
			Err(io::Error::other(
				String::from_utf8_lossy(&status.stdout).into_owned(),
			))
		}
	})?;
	office_save(s)?;
	replace_file(s)?;
	tree_drop(s)?;
	delete(s)?;
	delete_offline(s)?;
	rename(s)?;
	rename_offline(s)?;
	moves(s)?;
	out_of_root(s)?;
	into_root(s)?;
	dir_rename(s)?;
	dir_delete(s)?;
	edit_conflict(s)?;
	create_conflict(s)?;
	skipped_names(s)?;
	idle(s);
	Ok(())
}

/// L0: listing `lx` populates it with the tree's children.
fn populate(s: &mut Scenarios) -> io::Result<()> {
	let mut found: Vec<String> = fs::read_dir(s.path("lx"))?
		.map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
		.collect::<io::Result<_>>()?;
	found.sort();
	let mut wanted: Vec<String> = tree::FILES
		.iter()
		.filter_map(|file| file.path.strip_prefix("lx\\"))
		.chain(
			tree::DIRS
				.iter()
				.filter_map(|(path, _)| path.strip_prefix("lx\\")),
		)
		.filter(|name| !name.contains('\\'))
		.map(str::to_owned)
		.collect();
	wanted.sort();
	s.check("L0-list-lx", found == wanted, format!("{found:?}"));
	disguise(s, "M-own-view-dehydrated", "lx\\edit.txt");
	disguise(s, "M-own-view-dir", "lx");
	Ok(())
}

/// L1: a new file is uploaded and becomes a placeholder in sync with the same bytes.
fn new_file(s: &mut Scenarios) -> io::Result<()> {
	let bytes = local_bytes(50_000, 1);
	fs::write(s.path("lx\\new.txt"), &bytes)?;
	s.expect_carried("L1-new-file", "lx\\new.txt", &bytes);
	disguise(s, "M-own-view-hydrated", "lx\\new.txt");
	Ok(())
}

/// What a process other than the provider reads at `path` next to what the provider reads: the
/// platform hides a placeholder's reparse attribute from other processes.
fn disguise(s: &mut Scenarios, step: &str, path: &str) {
	let own = s.own_entry(path);
	let provider = s.entry(path);
	s.info(
		step,
		format!("this process: {own:?}; the provider: {}", provider.0),
	);
}

/// L2: a file written in chunks over a few seconds through one handle is uploaded once, after it
/// was closed and settled.
fn chunked(s: &mut Scenarios) -> io::Result<()> {
	let chunk = 256 * 1024;
	let bytes = local_bytes(12 * chunk, 2);
	let mut file = File::create(s.path("lx\\big.new"))?;
	for piece in bytes.chunks(chunk) {
		file.write_all(piece)?;
		file.flush()?;
		thread::sleep(Duration::from_millis(300));
	}
	let before = s.fresh_remote().uploads("big.new");
	drop(file);
	s.check(
		"L2-chunked-not-before-close",
		before == 0,
		format!("{before} uploads while it was open"),
	);
	if s.expect_carried("L2-chunked", "lx\\big.new", &bytes)
		.is_some()
	{
		let remote = s.fresh_remote();
		let uploads = remote.uploads("big.new");
		let versions = remote
			.find("lx\\big.new")
			.and_then(|item| field(item, "uuid"))
			.map_or(0, |uuid| {
				remote.count("UploadVersion", "id", &format!("stable/{uuid}"))
			});
		s.check(
			"L2-chunked-once",
			uploads == 1 && versions == 0,
			format!("{uploads} uploads, {versions} versions"),
		);
	}
	Ok(())
}

/// L20: a file held open for writing is uploaded only once it is closed.
fn held(s: &mut Scenarios) -> io::Result<()> {
	let bytes = local_bytes(8192, 3);
	let mut file = File::create(s.path("lx\\held.txt"))?;
	file.write_all(&bytes)?;
	file.flush()?;
	thread::sleep(Duration::from_secs(18));
	let before = s.remote().uploads("held.txt");
	s.check(
		"L20-held-not-while-open",
		before == 0 && s.plain("lx\\held.txt"),
		format!(
			"{before} uploads after 18 s open; {:?}",
			s.entry("lx\\held.txt")
		),
	);
	drop(file);
	if s.expect_carried("L20-held", "lx\\held.txt", &bytes)
		.is_some()
	{
		let uploads = s.fresh_remote().uploads("held.txt");
		s.check("L20-held-once", uploads == 1, format!("{uploads} uploads"));
	}
	Ok(())
}

/// L3: an edit of a hydrated placeholder becomes a new version of its file, uploaded from the
/// local bytes, which stay on disk: nothing is downloaded again.
fn edit(s: &mut Scenarios) -> io::Result<()> {
	let path = "lx\\edit.txt";
	let read = fs::read(s.path(path))?;
	s.check(
		"L3-hydrate",
		read == served(path),
		format!("{} bytes", read.len()),
	);
	thread::sleep(Duration::from_secs(3));
	let before = s.synced(path);
	let reads = s.fresh_remote().reads();
	let patch = local_bytes(4096, 4);
	let mut file = OpenOptions::new().write(true).open(s.path(path))?;
	file.seek(SeekFrom::Start(100))?;
	file.write_all(&patch)?;
	drop(file);
	let bytes = patched(read, 100, &patch);
	if let Some(after) = s.expect_carried("L3-edit", path, &bytes) {
		let remote = s.fresh_remote();
		s.check(
			"L3-edit-new-version",
			after.stable == stable(path)
				&& before
					.as_ref()
					.is_some_and(|before| before.version != after.version)
				&& remote.count("UploadVersion", "id", &format!("stable/{}", after.stable)) == 1
				&& remote.uploads("edit.txt") == 0,
			format!("before {before:?}, after {after:?}"),
		);
		s.check(
			"L3-edit-stays-hydrated",
			after.on_disk == after.size && remote.reads() == reads,
			format!(
				"{} of {} bytes on disk; drive reads {reads} -> {}",
				after.on_disk,
				after.size,
				remote.reads()
			),
		);
	}
	Ok(())
}

/// L5: an editor's save by renaming a new file over the old one (no callback is raised) makes a
/// new version of the same file, not a new file and not a delete.
fn save_by_rename(
	s: &mut Scenarios,
	path: &str,
	step: &str,
	rename: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
	let read = fs::read(s.path(path))?;
	s.check(
		&format!("{step}-hydrate"),
		read == served(path),
		format!("{} bytes", read.len()),
	);
	thread::sleep(Duration::from_secs(3));
	let before = s.synced(path);
	let bytes = local_bytes(read.len() + 500, 5);
	let temp = s.path(&format!("{path}.tmp"));
	fs::write(&temp, &bytes)?;
	if let Err(error) = rename(&temp, &s.path(path)) {
		s.check(step, false, format!("the rename failed: {error}"));
		return Ok(());
	}
	if let Some(after) = s.expect_carried(step, path, &bytes) {
		let remote = s.fresh_remote();
		let name = path.rsplit('\\').next().unwrap_or(path);
		s.check(
			&format!("{step}-same-file"),
			after.stable == stable(path)
				&& before
					.as_ref()
					.is_some_and(|before| before.version != after.version)
				&& remote.uploads(name) == 0
				&& !remote.trashed(&after.stable.to_string())
				&& remote.find(&format!("{path}.tmp")).is_none(),
			format!("before {before:?}, after {after:?}"),
		);
	}
	Ok(())
}

/// L4: an Office-style save: the old file renamed to a backup, the new one renamed into its name,
/// the backup deleted. The file keeps its id and gets a new version; nothing is trashed.
fn office_save(s: &mut Scenarios) -> io::Result<()> {
	let path = "lx\\office.txt";
	let read = fs::read(s.path(path))?;
	thread::sleep(Duration::from_secs(3));
	let before = s.synced(path);
	let bytes = local_bytes(read.len() + 300, 6);
	let temp = s.path("lx\\~WRL0001.tmp");
	let backup = s.path("lx\\office.txt.bak");
	fs::write(&temp, &bytes)?;
	let steps = fs::rename(s.path(path), &backup)
		.and_then(|()| fs::rename(&temp, s.path(path)))
		.and_then(|()| fs::remove_file(&backup));
	if let Err(error) = steps {
		s.check("L4-office-save", false, format!("the save failed: {error}"));
		return Ok(());
	}
	if let Some(after) = s.expect_carried("L4-office-save", path, &bytes) {
		thread::sleep(Duration::from_secs(5));
		let remote = s.fresh_remote();
		s.check(
			"L4-office-same-file",
			after.stable == stable(path)
				&& before
					.as_ref()
					.is_some_and(|before| before.version != after.version)
				&& !remote.trashed(&after.stable.to_string())
				&& remote.find("lx\\office.txt.bak").is_none()
				&& remote.uploads("office.txt") == 0,
			format!(
				"before {before:?}, after {after:?}, trashed {}, backup listed {}",
				remote.trashed(&after.stable.to_string()),
				remote.find("lx\\office.txt.bak").is_some()
			),
		);
	}
	Ok(())
}

/// L4: `ReplaceFileW` (.NET's `File.Replace`, from PowerShell): the old file renamed to a backup
/// and deleted, the new one renamed into its name. The new file is copied into place by the same
/// PowerShell just before, as an editor writes its temporary file: one left longer than the
/// settle time would be uploaded as a file of its own first. The platform fails about one such
/// replace in sixty over a placeholder, which is checked as the saves loop checks it.
fn replace_file(s: &mut Scenarios) -> io::Result<()> {
	let path = "lx\\replace.txt";
	let temp = "lx\\replace.new";
	let read = fs::read(s.path(path))?;
	thread::sleep(Duration::from_secs(3));
	let before = s.synced(path);
	let versions = saves_loop::versions(s, path);
	let undone = saves_loop::undone(s, path);
	let bytes = local_bytes(read.len() + 200, 7);
	let source = s.outside.join("replace.src");
	fs::write(&source, &bytes)?;
	let script = saves_loop::replace_script(&source, &s.path(temp), &s.path(path), None);
	let (replaced, text) = saves_loop::run_replace(&script)?;
	if !replaced {
		return saves_loop::replace_failed(
			s,
			"L4-replacefile-failed",
			&FailedReplace {
				replaced: path,
				replacement: temp,
				before: &read,
				bytes: &bytes,
				versions,
				undone,
				text: &text,
			},
		);
	}
	if let Some(after) = s.expect_carried("L4-replacefile", path, &bytes) {
		thread::sleep(Duration::from_secs(5));
		let remote = s.fresh_remote();
		s.check(
			"L4-replacefile-same-file",
			after.stable == stable(path)
				&& before
					.as_ref()
					.is_some_and(|before| before.version != after.version)
				&& !remote.trashed(&after.stable.to_string())
				&& remote.uploads("replace.txt") == 0,
			format!(
				"before {before:?}, after {after:?}, trashed {}",
				remote.trashed(&after.stable.to_string())
			),
		);
	}
	Ok(())
}

/// L12: a tree created at once, and a tree built outside the root and moved in, are taken in
/// whole: each directory created once, each file uploaded once, each under its own parent.
fn tree_drop(s: &mut Scenarios) -> io::Result<()> {
	fs::create_dir_all(s.path("lx\\tree\\a\\b\\c"))?;
	let created = [
		("lx\\tree\\t1.txt", local_bytes(3001, 8)),
		("lx\\tree\\a\\t2.txt", local_bytes(3002, 8)),
		("lx\\tree\\a\\b\\c\\t3.txt", local_bytes(3003, 8)),
	];
	for (path, bytes) in &created {
		fs::write(s.path(path), bytes)?;
	}
	let pre = s.outside.join("pre");
	fs::create_dir_all(pre.join("x"))?;
	let moved = [
		("lx\\pre\\z.txt", local_bytes(3004, 9)),
		("lx\\pre\\x\\y.txt", local_bytes(3005, 9)),
	];
	for (path, bytes) in &moved {
		let below = path.strip_prefix("lx\\pre\\").expect("under lx\\pre");
		fs::write(pre.join(below), bytes)?;
	}
	fs::rename(&pre, s.path("lx\\pre"))?;
	let mut ok = true;
	for (path, bytes) in created.iter().chain(&moved) {
		ok &= s
			.expect_carried(&format!("L12-tree-{path}"), path, bytes)
			.is_some();
	}
	let remote = s.fresh_remote();
	for dir in [
		"lx\\tree",
		"lx\\tree\\a",
		"lx\\tree\\a\\b",
		"lx\\tree\\a\\b\\c",
		"lx\\pre",
		"lx\\pre\\x",
	] {
		let local = s.synced_dir(dir);
		let listed = remote.find(dir).and_then(|item| field(item, "uuid"));
		let name = dir.rsplit('\\').next().unwrap_or(dir);
		let creates = remote.count("CreateDir", "name", name);
		let matches = matches!((local, listed), (Some(local), Some(listed)) if same_uuid(&local.to_string(), listed));
		s.check(
			&format!("L12-dir-{dir}"),
			matches && creates == 1,
			format!("local {local:?}, remote {listed:?}, {creates} creates"),
		);
	}
	for (path, _) in created.iter().chain(&moved) {
		let name = path.rsplit('\\').next().unwrap_or(path);
		let uploads = remote.uploads(name);
		ok &= uploads == 1;
		s.check(
			&format!("L12-once-{path}"),
			uploads == 1,
			format!("{uploads} uploads"),
		);
	}
	s.info("L12-tree", format!("all carried: {ok}"));
	Ok(())
}

/// L5: a local delete moves the remote file to the trash.
fn delete(s: &mut Scenarios) -> io::Result<()> {
	let path = "lx\\del.txt";
	let removed = fs::remove_file(s.path(path));
	s.check("L5-delete", removed.is_ok(), format!("{removed:?}"));
	let trashed = s.trashed(stable(path));
	s.check("L5-delete-trashed", trashed, format!("trashed: {trashed}"));
	Ok(())
}

/// What a vetoed operation left: its error code, checked against what an unreachable server shows.
fn vetoed(s: &mut Scenarios, step: &str, result: io::Result<()>) {
	match result {
		Ok(()) => s.check(step, false, "the operation went ahead"),
		Err(error) => {
			s.check(step, true, format!("refused: {error}"));
			s.check(
				&format!("{step}-code"),
				error.raw_os_error() == Some(NETWORK_UNAVAILABLE),
				format!("os error {:?}", error.raw_os_error()),
			);
		}
	}
}

/// L6: a delete while the drive is offline is vetoed and the file keeps its place; once the drive
/// is back, the delete goes through.
fn delete_offline(s: &mut Scenarios) -> io::Result<()> {
	let path = "lx\\deloff.txt";
	s.offline(true);
	vetoed(s, "L6-delete-offline", fs::remove_file(s.path(path)));
	let kept = s.synced(path);
	s.offline(false);
	let remote = s.fresh_remote();
	s.check(
		"L6-delete-offline-kept",
		kept.as_ref()
			.is_some_and(|kept| kept.stable == stable(path))
			&& remote.find(path).is_some()
			&& !remote.trashed(&stable(path).to_string()),
		format!("local {kept:?}"),
	);
	let removed = fs::remove_file(s.path(path));
	s.check(
		"L6-delete-online",
		removed.is_ok() && s.trashed(stable(path)),
		format!("{removed:?}"),
	);
	Ok(())
}

/// L7: a rename renames the remote file, which keeps its id; the placeholder is in sync again.
fn rename(s: &mut Scenarios) -> io::Result<()> {
	let (from, to) = ("lx\\ren.txt", "lx\\ren2.txt");
	let renamed = fs::rename(s.path(from), s.path(to));
	s.check("L7-rename", renamed.is_ok(), format!("{renamed:?}"));
	let carried = wait(CARRIED, || {
		let remote = s.remote();
		let item = remote.find(to)?;
		(same_uuid(field(item, "uuid")?, &stable(from).to_string())
			&& remote.find(from).is_none()
			&& s.synced(to).is_some())
		.then_some(())
	});
	s.check(
		"L7-rename-carried",
		carried.is_some(),
		format!("local {:?}", s.entry(to)),
	);
	Ok(())
}

/// L9: a rename while the drive is offline is vetoed; the name stays.
fn rename_offline(s: &mut Scenarios) -> io::Result<()> {
	let (from, to) = ("lx\\renoff.txt", "lx\\renoff2.txt");
	s.offline(true);
	vetoed(s, "L9-rename-offline", fs::rename(s.path(from), s.path(to)));
	s.offline(false);
	let remote = s.fresh_remote();
	s.check(
		"L9-rename-offline-kept",
		s.path(from).exists()
			&& !s.path(to).exists()
			&& remote.find(from).is_some()
			&& remote.find(to).is_none(),
		format!("local {:?}", s.entry(from)),
	);
	Ok(())
}

/// L8: moves between directories: into a remote directory, vetoed offline, and into a directory
/// just created locally, which is created on the drive first.
fn moves(s: &mut Scenarios) -> io::Result<()> {
	let (from, to) = ("lx\\mv.txt", "lx\\dst\\mv.txt");
	let moved = fs::rename(s.path(from), s.path(to));
	s.check("L8-move", moved.is_ok(), format!("{moved:?}"));
	let dst = Uuid::from_u128(0x3006).to_string();
	let carried = wait(CARRIED, || {
		let remote = s.remote();
		let item = remote.by_uuid(&stable(from).to_string())?;
		(field(item, "dir") == Some(format!("stable/{dst}").as_str())
			&& field(item, "name") == Some("mv.txt")
			&& s.synced(to).is_some())
		.then_some(())
	});
	s.check(
		"L8-move-carried",
		carried.is_some(),
		format!("local {:?}", s.entry(to)),
	);

	let (from, to) = ("lx\\mvoff.txt", "lx\\dst\\mvoff.txt");
	s.offline(true);
	vetoed(s, "L8-move-offline", fs::rename(s.path(from), s.path(to)));
	s.offline(false);
	let remote = s.fresh_remote();
	s.check(
		"L8-move-offline-kept",
		s.path(from).exists() && !s.path(to).exists() && remote.find(from).is_some(),
		format!("local {:?}", s.entry(from)),
	);

	let (from, to) = ("lx\\mv2.txt", "lx\\fresh\\mv2.txt");
	fs::create_dir(s.path("lx\\fresh"))?;
	let moved = fs::rename(s.path(from), s.path(to));
	s.check("L8-move-new-dir", moved.is_ok(), format!("{moved:?}"));
	let carried = wait(CARRIED, || {
		let remote = s.remote();
		let dir = remote.find("lx\\fresh")?;
		let item = remote.find(to)?;
		(same_uuid(field(item, "uuid")?, &stable(from).to_string())
			&& s.synced(to).is_some()
			&& s.synced_dir("lx\\fresh").is_some_and(|uuid| {
				field(dir, "uuid").is_some_and(|held| same_uuid(held, &uuid.to_string()))
			}))
		.then_some(())
	});
	s.check(
		"L8-move-new-dir-carried",
		carried.is_some(),
		format!("local {:?} / {:?}", s.entry("lx\\fresh"), s.entry(to)),
	);
	Ok(())
}

/// L10: a move out of the root and a delete to the recycle bin trash the remote file.
fn out_of_root(s: &mut Scenarios) -> io::Result<()> {
	let path = "lx\\out.txt";
	let target = s.outside.join("out.txt");
	let moved = fs::rename(s.path(path), &target);
	s.check("L10-move-out", moved.is_ok(), format!("{moved:?}"));
	let trashed = s.trashed(stable(path));
	let detail = match fs::metadata(&target) {
		Ok(metadata) => format!(
			"outside: {} bytes, attributes {:#x}, same bytes {}",
			metadata.len(),
			metadata.file_attributes(),
			fs::read(&target).is_ok_and(|bytes| bytes == served(path))
		),
		Err(error) => format!("outside: {error}"),
	};
	s.check("L10-move-out-trashed", trashed, detail);

	let path = "lx\\recycle.txt";
	let script = format!(
		"Add-Type -AssemblyName Microsoft.VisualBasic; \
		 [Microsoft.VisualBasic.FileIO.FileSystem]::DeleteFile('{}', 'OnlyErrorDialogs', \
		 'SendToRecycleBin')",
		s.path(path).display()
	);
	let output = Command::new("powershell")
		.args(["-NoProfile", "-NonInteractive", "-Command", &script])
		.output()?;
	s.check(
		"L10-recycle",
		output.status.success() && !s.path(path).exists(),
		String::from_utf8_lossy(&output.stderr).trim().to_owned(),
	);
	let trashed = s.trashed(stable(path));
	s.check(
		"L10-recycle-trashed",
		trashed,
		format!("trashed: {trashed}"),
	);
	Ok(())
}

/// L11: a file moved in from outside the root is uploaded.
fn into_root(s: &mut Scenarios) -> io::Result<()> {
	let bytes = local_bytes(9000, 10);
	let source = s.outside.join("movein.txt");
	fs::write(&source, &bytes)?;
	fs::rename(&source, s.path("lx\\movedin.txt"))?;
	s.expect_carried("L11-move-in", "lx\\movedin.txt", &bytes);
	Ok(())
}

/// L13: a directory rename renames the remote directory; its children stay placeholders in sync.
fn dir_rename(s: &mut Scenarios) -> io::Result<()> {
	// Listed first, so the child is a placeholder before the rename.
	let listed = fs::read_dir(s.path("lx\\dirren"))?.count();
	s.info("L13-dir-listed", format!("{listed} children"));
	let renamed = fs::rename(s.path("lx\\dirren"), s.path("lx\\dirren2"));
	s.check("L13-dir-rename", renamed.is_ok(), format!("{renamed:?}"));
	let dir = Uuid::from_u128(0x3007);
	let carried = wait(CARRIED, || {
		let remote = s.remote();
		let item = remote.find("lx\\dirren2")?;
		(same_uuid(field(item, "uuid")?, &dir.to_string())
			&& s.synced_dir("lx\\dirren2") == Some(dir))
		.then_some(())
	});
	s.check(
		"L13-dir-rename-carried",
		carried.is_some(),
		format!("local {:?}", s.entry("lx\\dirren2")),
	);
	let child = s.synced("lx\\dirren2\\x.bin");
	s.check(
		"L13-dir-rename-child",
		child
			.as_ref()
			.is_some_and(|child| child.stable == stable("lx\\dirren\\x.bin")),
		format!("{child:?}"),
	);
	Ok(())
}

/// L13: deleting a directory tree trashes the remote directory.
fn dir_delete(s: &mut Scenarios) -> io::Result<()> {
	let removed = fs::remove_dir_all(s.path("lx\\deldir"));
	s.check("L13-dir-delete", removed.is_ok(), format!("{removed:?}"));
	let dir = Uuid::from_u128(0x3008);
	let trashed = s.trashed(dir);
	let remote = s.fresh_remote();
	let trashes: Vec<String> = remote
		.writes()
		.iter()
		.filter(|write| field(write, "op") == Some("Trash"))
		.filter_map(|write| field(write, "id").map(str::to_owned))
		.collect();
	s.check(
		"L13-dir-delete-trashed",
		trashed,
		format!("trash calls so far {trashes:?}"),
	);
	Ok(())
}

/// L14: a local edit while the drive got a newer version keeps both: the edit is uploaded as
/// `conflict.old.txt`, and `conflict.txt` becomes a placeholder of the drive's version.
fn edit_conflict(s: &mut Scenarios) -> io::Result<()> {
	let path = "lx\\conflict.txt";
	let read = fs::read(s.path(path))?;
	s.check(
		"L14-hydrate",
		read == served(path),
		format!("{} bytes", read.len()),
	);
	thread::sleep(Duration::from_secs(3));
	// The edit first: a newer version applied before it would make it an edit of that version
	// (the write lands on the version the provider just dehydrated the file to).
	let patch = local_bytes(4096, 11);
	let mut file = OpenOptions::new().write(true).open(s.path(path))?;
	file.write_all(&patch)?;
	drop(file);
	let head = match s.control(&format!("bump {path}")) {
		Ok(version) => version,
		Err(error) => {
			s.check("L14-bump", false, error);
			return Ok(());
		}
	};
	let bumped = tree::bumped(read.len());
	let bytes = patched(read, 0, &patch);
	let aside = s.expect_carried("L14-conflict-aside", "lx\\conflict.old.txt", &bytes);
	let placed = wait(CARRIED, || {
		s.entry(path)
			.file()
			.filter(|(_, version)| same_uuid(&version.to_string(), &head))
	});
	let content = fs::read(s.path(path));
	s.check(
		"L14-conflict-head",
		placed.is_some_and(|(held, version)| {
			held == stable(path) && same_uuid(&version.to_string(), &head)
		}) && content.as_ref().is_ok_and(|content| *content == bumped),
		format!(
			"placed {placed:?}, head {head}, reads the head: {}",
			content.as_ref().is_ok_and(|content| *content == bumped)
		),
	);
	let remote = s.fresh_remote();
	let item = remote.by_uuid(&stable(path).to_string());
	s.check(
		"L14-conflict-remote-kept",
		item.is_some_and(|item| {
			field(item, "version").is_some_and(|version| same_uuid(version, &head))
				&& field(item, "hash") == Some(hex_hash(&bumped).as_str())
		}) && aside.is_some_and(|aside| aside.stable != stable(path)),
		format!("remote {item:?}"),
	);
	let reported = s
		.reports()
		.iter()
		.any(|report| report.contains("EditVsEdit") && report.contains("conflict.old.txt"));
	s.check("L14-conflict-reported", reported, "an EditVsEdit report");
	Ok(())
}

/// L15: a new local file whose name a different remote file holds, in another case, keeps both: the
/// local file is uploaded under a `.old` name, and the remote file gets a placeholder.
fn create_conflict(s: &mut Scenarios) -> io::Result<()> {
	// The local file first: a remote file placed before it would make the write an edit of that
	// file, its placeholder taking the write under any case of the name.
	let bytes = local_bytes(6100, 12);
	fs::write(s.path("lx\\TAKEN.TXT"), &bytes)?;
	let uuid = match s.control("add lx Taken.txt 6000") {
		Ok(uuid) => uuid,
		Err(error) => {
			s.check("L15-add", false, error);
			return Ok(());
		}
	};
	let aside = s.expect_carried("L15-create-conflict-aside", "lx\\TAKEN.old.TXT", &bytes);
	let placed = wait(CARRIED, || {
		s.entry("lx\\Taken.txt")
			.file()
			.map(|(stable, _)| stable)
			.filter(|stable| same_uuid(&stable.to_string(), &uuid))
	});
	let added = tree::added(6000);
	let content = fs::read(s.path("lx\\Taken.txt"));
	let names: Vec<String> = fs::read_dir(s.path("lx"))?
		.filter_map(Result::ok)
		.map(|entry| entry.file_name().to_string_lossy().into_owned())
		.filter(|name| name.to_lowercase().starts_with("taken"))
		.collect();
	s.check(
		"L15-create-conflict-remote",
		placed.is_some_and(|placed| same_uuid(&placed.to_string(), &uuid))
			&& content.as_ref().is_ok_and(|content| *content == added)
			&& aside.is_some_and(|aside| !same_uuid(&aside.stable.to_string(), &uuid)),
		format!("placed {placed:?}, names {names:?}"),
	);
	let remote = s.fresh_remote();
	let item = remote.by_uuid(&uuid);
	s.check(
		"L15-create-conflict-remote-kept",
		item.is_some_and(|item| {
			field(item, "name") == Some("Taken.txt")
				&& field(item, "hash") == Some(hex_hash(&added).as_str())
		}) && remote.count("UploadVersion", "id", &format!("stable/{uuid}")) == 0,
		format!("remote {item:?}"),
	);
	Ok(())
}

/// L16: names the server refuses are skipped and reported; names every machine writes and nobody
/// syncs are skipped silently. Each stays an ordinary local file, never uploaded.
fn skipped_names(s: &mut Scenarios) -> io::Result<()> {
	let long = format!("{}.txt", "\u{e9}".repeat(126));
	let names = [
		(" lead.txt".to_owned(), true),
		(long, true),
		("~$x.docx".to_owned(), false),
		("desktop.ini".to_owned(), false),
	];
	for (name, _) in &names {
		fs::write(s.path(&format!("lx\\{name}")), local_bytes(500, 13))?;
	}
	thread::sleep(Duration::from_secs(10));
	let remote = s.fresh_remote();
	let reports = s.reports();
	for (n, (name, reported)) in names.iter().enumerate() {
		let path = format!("lx\\{name}");
		let plain = s.plain(&path);
		let uploads = remote.uploads(name);
		let was_reported = reports
			.iter()
			.any(|report| report.contains("Skipped") && report.contains(name.as_str()));
		s.check(
			&format!("L16-skip-{n}"),
			plain && uploads == 0 && was_reported == *reported,
			format!(
				"{} bytes of name: plain {plain}, {uploads} uploads, reported {was_reported}",
				name.len()
			),
		);
	}
	Ok(())
}

/// L18: with nothing changing locally, the provider writes nothing to the drive: its own
/// hydrations, populations and converts are not uploads. Records the count of writes so far for
/// `quiet`, which checks the reader's scenarios added none.
fn idle(s: &mut Scenarios) {
	thread::sleep(Duration::from_secs(5));
	let before = s.fresh_remote().writes().len();
	thread::sleep(IDLE);
	let remote = s.fresh_remote();
	let writes = remote.writes();
	let added: Vec<String> = writes
		.get(before..)
		.unwrap_or_default()
		.iter()
		.map(ToString::to_string)
		.collect();
	s.check(
		"L18-idle-no-writes",
		added.is_empty(),
		format!(
			"{} writes, {} added over {IDLE:?}: {added:?}",
			writes.len(),
			added.len()
		),
	);
	if let Err(error) = fs::write(s.out.join("ops-mark.txt"), writes.len().to_string()) {
		s.check("L18-mark", false, error);
	}
	s.info("L18-writes", format!("{writes:?}"));
}

/// After the provider stopped: the reader's scenarios, which run after these, added no drive
/// write. Into `<out>\quiet.txt`.
pub(crate) fn quiet(out: &Path) -> Outcome {
	let mut report = Report::create(&out.join("quiet.txt"))?;
	let mark: Option<usize> = fs::read_to_string(out.join("ops-mark.txt"))
		.ok()
		.and_then(|text| text.trim().parse().ok());
	let summary: Option<Value> = fs::read_to_string(out.join("provider-summary.json"))
		.ok()
		.and_then(|text| serde_json::from_str(&text).ok());
	let ops: Vec<String> = summary
		.as_ref()
		.and_then(|summary| summary.get("ops"))
		.and_then(Value::as_array)
		.map(|ops| {
			ops.iter()
				.filter_map(Value::as_str)
				.map(str::to_owned)
				.collect()
		})
		.unwrap_or_default();
	match mark {
		Some(mark) => {
			let added = ops.get(mark..).unwrap_or_default();
			report.check(
				"L18-check-no-writes",
				summary.is_some() && added.is_empty(),
				format!(
					"{} writes at the mark, {} at the end: {added:?}",
					mark,
					ops.len()
				),
			);
		}
		None => report.fail("L18-check-no-writes", "no ops-mark.txt"),
	}
	fs::write(out.join("quiet.done"), "done")?;
	Ok(report.passed())
}

//! `saves-loop <root> <out> <iterations>`: atomic saves in a loop, a user's process saving the files
//! of `lx` while `provide-local` serves the root. Each iteration saves `lx\replace.txt` with
//! `ReplaceFileW` (.NET's `File.Replace`, from PowerShell), then `lx\office.txt` the way Office
//! does or `lx\save.txt` by a rename over it, in turn, then `lx\save2.txt` by `cmd /c move /y`
//! of a temporary file written a random 0 to 3 s before ([`move_once`]): long enough, and the
//! provider uploads the temporary file as a file of its own first. Each save must reach the drive
//! as one new version of the same file; every fourth iteration also overwrites `lx\save2.txt` in
//! place. A
//! `File.Replace` the platform fails with `ERROR_UNABLE_TO_REMOVE_REPLACED` is counted and checked
//! as [`replace_failed`] checks it; the processes Restart Manager says hold the files at that
//! moment are listed beside it. Any other failure fails the loop. After every iteration
//! [`invariants`] checks what no save may leave behind. The loop ends idle: no drive write while
//! nothing changes. Into `<out>\loop.txt`.

use std::{
	collections::BTreeSet,
	fs::{self, OpenOptions},
	io::{self, Write},
	path::Path,
	process::Command,
	thread,
	time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::Value;

use crate::{
	local::{Remote, Scenarios, local_bytes, same_uuid, stable, wait},
	report::{Outcome, field, wait_for},
};

const REPLACED: &str = "lx\\replace.txt";
const REPLACEMENT: &str = "lx\\replace.new";
const OFFICE: &str = "lx\\office.txt";
const OFFICE_TEMP: &str = "lx\\~WRL0001.tmp";
const OFFICE_BACKUP: &str = "lx\\office.txt.bak";
const SAVED: &str = "lx\\save.txt";
const SAVED_TEMP: &str = "lx\\save.txt.tmp";
const OVERWRITTEN: &str = "lx\\save2.txt";
/// The file [`move_once`] saves, and its temporary file.
const MOVED: &str = "lx\\save2.txt";
const MOVED_TEMP: &str = "lx\\save2.txt.tmp";
/// The longest [`move_once`] waits between writing its temporary file and moving it, in ms.
const MOVE_GAP_MAX_MS: u64 = 3_000;
/// The files the loop saves, which must keep their identity whatever the save.
const SAVED_FILES: [&str; 4] = [REPLACED, OFFICE, SAVED, MOVED];
/// How long a failed `File.Replace`'s files have to settle.
const SETTLE: Duration = Duration::from_secs(40);
/// How long the drive must stay unwritten once a failed `File.Replace`'s files settled.
const SETTLED_QUIET: Duration = Duration::from_secs(6);
/// `ERROR_UNABLE_TO_REMOVE_REPLACED` as the `HResult` .NET reports it: what `ReplaceFileW` fails
/// with over a placeholder about once in sixty, after the provider allowed its rename of the old
/// file (measured in the VM, x64 and ARM64; never in a directory outside a sync root).
const UNABLE_TO_REMOVE_REPLACED: &str = "hr=0x80070497";
/// How long the loop must end idle.
const IDLE: Duration = Duration::from_secs(20);

/// Lists the processes Restart Manager says hold a file open: compiled once into `<out>`, loaded by
/// each `File.Replace` that fails.
const RESTART_MANAGER: &str = r#"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;

public static class LoopRm {
	[StructLayout(LayoutKind.Sequential)]
	struct Unique { public int Pid; public System.Runtime.InteropServices.ComTypes.FILETIME Start; }

	[StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
	struct Info {
		public Unique Process;
		[MarshalAs(UnmanagedType.ByValTStr, SizeConst = 256)] public string App;
		[MarshalAs(UnmanagedType.ByValTStr, SizeConst = 64)] public string Service;
		public int Type;
		public uint Status;
		public uint Session;
		[MarshalAs(UnmanagedType.Bool)] public bool Restartable;
	}

	[DllImport("rstrtmgr.dll", CharSet = CharSet.Unicode)]
	static extern int RmStartSession(out uint session, int flags, string key);
	[DllImport("rstrtmgr.dll")]
	static extern int RmEndSession(uint session);
	[DllImport("rstrtmgr.dll", CharSet = CharSet.Unicode)]
	static extern int RmRegisterResources(uint session, uint files, string[] names, uint apps, Unique[] procs, uint services, string[] svcs);
	[DllImport("rstrtmgr.dll")]
	static extern int RmGetList(uint session, out uint needed, ref uint count, [In, Out] Info[] info, ref uint reasons);

	public static string Holders(string file) {
		uint session;
		int r = RmStartSession(out session, 0, Guid.NewGuid().ToString());
		if (r != 0) return "start failed " + r;
		try {
			r = RmRegisterResources(session, 1, new[] { file }, 0, null, 0, null);
			if (r != 0) return "register failed " + r;
			uint needed = 0, count = 0, reasons = 0;
			r = RmGetList(session, out needed, ref count, null, ref reasons);
			if (r == 0) return "none";
			if (r != 234) return "list failed " + r;
			Info[] info = new Info[needed];
			count = needed;
			r = RmGetList(session, out needed, ref count, info, ref reasons);
			if (r != 0) return "list failed " + r;
			List<string> held = new List<string>();
			for (int i = 0; i < count; i++) held.Add(info[i].Process.Pid + ":" + info[i].App);
			return string.Join(",", held);
		} finally {
			RmEndSession(session);
		}
	}
}
"#;

pub(crate) fn run(root: &std::path::Path, out: &std::path::Path, iterations: u32) -> Outcome {
	let mut s = Scenarios::open(root, out, "loop.txt", 0)?;
	let result = run_loop(&mut s, iterations);
	if let Err(error) = &result {
		s.report.fail("harness", error);
	}
	fs::write(out.join("loop.done"), "done")?;
	Ok(s.report.passed())
}

fn run_loop(s: &mut Scenarios, iterations: u32) -> io::Result<()> {
	if !wait_for(&s.out.join("provider.ready"), Duration::from_secs(60)) {
		s.check(
			"loop-ready",
			false,
			"the provider did not get ready within 60 s",
		);
		return Ok(());
	}
	let rm = compile_restart_manager(s)?;
	for path in [REPLACED, OFFICE, SAVED] {
		fs::read(s.path(path))?;
	}
	thread::sleep(Duration::from_secs(3));
	let baseline = Baseline::now(s);
	let seed = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map_or(1, |since| {
			(since.as_secs() << 30) ^ u64::from(since.subsec_nanos()) | 1
		});
	let mut dice = Dice(seed);
	s.info(
		"loop-baseline",
		format!(
			"{} items on the drive, {} vetoes, gap seed {seed:#x}",
			baseline.items.len(),
			baseline.vetoes
		),
	);
	let mut failures = 0_u32;
	let started = Instant::now();
	for i in 0..iterations {
		if !replace_once(s, i, &rm)? {
			failures += 1;
		}
		if i % 2 == 0 {
			office_once(s, i)?;
		} else {
			rename_once(s, i)?;
		}
		if i % 4 == 3 {
			overwrite_once(s, i)?;
		}
		move_once(s, i, Duration::from_millis(dice.below(MOVE_GAP_MAX_MS + 1)))?;
		invariants(s, i, &baseline)?;
		if i % 10 == 9 {
			s.info(
				"loop-progress",
				format!(
					"{} iterations, {failures} File.Replace failures, {:?}",
					i + 1,
					started.elapsed()
				),
			);
		}
	}
	s.info(
		"loop-replace-failures",
		format!("{failures} of {iterations}"),
	);
	let remote = s.fresh_remote();
	let temporaries = remote
		.writes()
		.iter()
		.filter(|write| field(write, "op") == Some("Trash"))
		.count();
	s.info(
		"loop-temporary-uploads",
		format!("{temporaries} temporary files uploaded before their save went on, then trashed"),
	);
	idle(s);
	Ok(())
}

/// Compiles [`RESTART_MANAGER`] into `<out>\looprm.dll`, and returns its path.
fn compile_restart_manager(s: &Scenarios) -> io::Result<String> {
	let source = s.out.join("looprm.cs");
	let dll = s.out.join("looprm.dll");
	fs::write(&source, RESTART_MANAGER)?;
	let script = format!(
		"Add-Type -Path '{}' -OutputAssembly '{}' -OutputType Library",
		source.display(),
		dll.display()
	);
	let output = Command::new("powershell")
		.args(["-NoProfile", "-NonInteractive", "-Command", &script])
		.output()?;
	if !output.status.success() || !dll.exists() {
		return Err(io::Error::other(format!(
			"compiling the Restart Manager probe failed: {}",
			String::from_utf8_lossy(&output.stderr)
		)));
	}
	Ok(dll.display().to_string())
}

/// How many versions the provider uploaded of the tree's file at `path`.
pub(crate) fn versions(s: &Scenarios, path: &str) -> usize {
	s.fresh_remote()
		.count("UploadVersion", "id", &format!("stable/{}", stable(path)))
}

/// How many versions the provider uploaded of the file at `path` since it had uploaded `before`;
/// `None`, a failed check, should the drive list fewer than that.
fn added_since(s: &Scenarios, path: &str, before: usize) -> Option<usize> {
	versions(s, path).checked_sub(before)
}

/// The bytes the `i`-th save of a file `size` long before writes, `salt` telling files apart.
fn save_bytes(base: usize, i: u32, salt: u8) -> Vec<u8> {
	let step = usize::try_from(i % 17).unwrap_or(0);
	let salt = salt.wrapping_add(u8::try_from(i % 200).unwrap_or(0));
	local_bytes(base + step, salt)
}

/// How many times the provider reported the rename of `path` undone.
pub(crate) fn undone(s: &Scenarios, path: &str) -> usize {
	let report = format!("Undone {{ path: RootPath({path:?}), op: Rename }}");
	s.reports().iter().filter(|seen| **seen == report).count()
}

/// The PowerShell that copies `source` to `replacement` and replaces `replaced` with it through
/// `File.Replace` with no backup name (`ReplaceFileW`), as an editor writes its temporary file
/// just before: prints `replaced`, or `failed hr=0x<HResult> <message>` and exits 3, after the
/// processes the Restart Manager probe `rm` says hold the files and `dir` when one is given.
pub(crate) fn replace_script(
	source: &Path,
	replacement: &Path,
	replaced: &Path,
	rm: Option<(&str, &Path)>,
) -> String {
	let holders = rm.map_or_else(String::new, |(rm, dir)| {
		format!(
			"Add-Type -Path '{rm}'; \
			 'holders replaced=' + [LoopRm]::Holders('{old}'); \
			 'holders replacement=' + [LoopRm]::Holders('{new}'); \
			 'holders dir=' + [LoopRm]::Holders('{dir}'); ",
			old = replaced.display(),
			new = replacement.display(),
			dir = dir.display(),
		)
	});
	format!(
		"try {{ [System.IO.File]::Copy('{src}', '{new}', $true); \
		 [System.IO.File]::Replace('{new}', '{old}', [NullString]::Value); 'replaced' }} \
		 catch {{ $e = $_.Exception; if ($e.InnerException) {{ $e = $e.InnerException }}; \
		 'failed hr=0x' + $e.HResult.ToString('X8') + ' ' + $e.Message; {holders}exit 3 }}",
		src = source.display(),
		new = replacement.display(),
		old = replaced.display(),
	)
}

/// Runs [`replace_script`]'s PowerShell: whether it replaced the file, and what it printed.
pub(crate) fn run_replace(script: &str) -> io::Result<(bool, String)> {
	let output = Command::new("powershell")
		.args(["-NoProfile", "-NonInteractive", "-Command", script])
		.output()?;
	let text = String::from_utf8_lossy(&output.stdout).replace(['\r', '\n'], " ");
	let replaced = output.status.success() && text.contains("replaced");
	let text = format!(
		"{text} {}",
		String::from_utf8_lossy(&output.stderr).replace(['\r', '\n'], " ")
	);
	Ok((replaced, text))
}

/// A `File.Replace` of `replaced` by `replacement` that failed, as [`run_replace`] printed it in
/// `text`: `before` the old file's bytes and `bytes` the new one's, `versions` and `undone` the
/// old file's uploaded versions and undone renames before it.
pub(crate) struct FailedReplace<'a> {
	pub(crate) replaced: &'a str,
	pub(crate) replacement: &'a str,
	pub(crate) before: &'a [u8],
	pub(crate) bytes: &'a [u8],
	pub(crate) versions: usize,
	pub(crate) undone: usize,
	pub(crate) text: &'a str,
}

/// Checks a failed `File.Replace` under `step`. The platform's failure over a placeholder,
/// `ERROR_UNABLE_TO_REMOVE_REPLACED`, is recorded as INFO, and the provider's settled state for it
/// is checked: both files as `File.Replace` left them, the old one in sync again with at most one
/// upload of it, its rename to the backup name reported undone, the new one taken in as a file of
/// its own, and no drive write once that settled. Any other failure is a failed check. The new
/// file is then deleted, as the next save copies one anew, and checked gone from the drive.
pub(crate) fn replace_failed(
	s: &mut Scenarios,
	step: &str,
	failed: &FailedReplace<'_>,
) -> io::Result<()> {
	let (replaced, replacement) = (s.path(failed.replaced), s.path(failed.replacement));
	if failed.text.contains(UNABLE_TO_REMOVE_REPLACED) {
		s.info(step, failed.text);
	} else {
		s.check(step, false, format!("File.Replace failed: {}", failed.text));
		if replacement.exists() {
			fs::remove_file(&replacement)?;
		}
		return Ok(());
	}
	let old_kept = fs::read(&replaced).is_ok_and(|now| now == failed.before);
	let new_kept = fs::read(&replacement).is_ok_and(|now| now == failed.bytes);
	s.check(
		&format!("{step}-files-kept"),
		old_kept && new_kept,
		format!("old kept {old_kept}, new kept {new_kept}"),
	);
	let synced = wait(SETTLE, || {
		s.synced(failed.replaced)
			.filter(|synced| synced.stable == stable(failed.replaced))
	});
	let added = added_since(s, failed.replaced, failed.versions);
	let own = s.carried(failed.replacement, failed.bytes);
	s.check(
		&format!("{step}-replacement-own-file"),
		own.as_ref()
			.is_ok_and(|own| own.stable != stable(failed.replaced)),
		format!("{own:?}"),
	);
	let reported = wait(SETTLE, || {
		(undone(s, failed.replaced) > failed.undone).then_some(())
	});
	s.check(
		&format!("{step}-undone"),
		reported.is_some(),
		"the rename to the backup name reported undone",
	);
	let writes = s.fresh_remote().writes().len();
	thread::sleep(SETTLED_QUIET);
	let later = added_since(s, failed.replaced, failed.versions);
	let writes_later = s.fresh_remote().writes().len();
	s.check(
		&format!("{step}-settles"),
		synced.is_some() && added.is_some_and(|added| added <= 1) && later == added,
		format!(
			"in sync {synced:?}, {added:?} versions uploaded, {later:?} {SETTLED_QUIET:?} later"
		),
	);
	s.check(
		&format!("{step}-quiet"),
		writes_later == writes,
		format!("{writes} drive writes, {writes_later} {SETTLED_QUIET:?} later"),
	);
	// The new file stays a file of its own, as the platform left it: the next save copies one anew.
	fs::remove_file(&replacement)?;
	let gone = wait(SETTLE, || {
		s.remote().find(failed.replacement).is_none().then_some(())
	});
	s.check(
		&format!("{step}-replacement-removed"),
		gone.is_some(),
		"the replacement left on the drive",
	);
	Ok(())
}

/// One `File.Replace` of [`REPLACED`]; whether it succeeded.
fn replace_once(s: &mut Scenarios, i: u32, rm: &str) -> io::Result<bool> {
	let before = fs::read(s.path(REPLACED))?;
	let versions_before = versions(s, REPLACED);
	let undone_before = undone(s, REPLACED);
	let bytes = save_bytes(7_205, i, 7);
	let source = s.out.join("outside").join("replace.src");
	fs::write(&source, &bytes)?;
	let script = replace_script(
		&source,
		&s.path(REPLACEMENT),
		&s.path(REPLACED),
		Some((rm, &s.path("lx"))),
	);
	let (replaced, text) = run_replace(&script)?;
	if replaced {
		let step = format!("R{i:03}-replace");
		if let Some(after) = s.expect_carried(&step, REPLACED, &bytes) {
			let added = added_since(s, REPLACED, versions_before);
			let gone = s.remote().find(REPLACEMENT).is_none();
			s.check(
				&format!("{step}-once"),
				after.stable == stable(REPLACED) && added == Some(1) && gone,
				format!("{added:?} versions uploaded, replacement listed {}", !gone),
			);
		}
		return Ok(true);
	}
	replace_failed(
		s,
		&format!("R{i:03}-replace-failed"),
		&FailedReplace {
			replaced: REPLACED,
			replacement: REPLACEMENT,
			before: &before,
			bytes: &bytes,
			versions: versions_before,
			undone: undone_before,
			text: &text,
		},
	)?;
	Ok(false)
}

/// An Office-style save of [`OFFICE`]: the old file renamed to a backup, the new one renamed into
/// its name, the backup deleted.
fn office_once(s: &mut Scenarios, i: u32) -> io::Result<()> {
	let versions_before = versions(s, OFFICE);
	let bytes = save_bytes(7_304, i, 6);
	let temp = s.path(OFFICE_TEMP);
	let backup = s.path(OFFICE_BACKUP);
	write_new(&temp, &bytes)?;
	let steps = fs::rename(s.path(OFFICE), &backup)
		.and_then(|()| fs::rename(&temp, s.path(OFFICE)))
		.and_then(|()| fs::remove_file(&backup));
	let step = format!("O{i:03}-office");
	if let Err(error) = steps {
		s.check(&step, false, format!("the save failed: {error}"));
		return Ok(());
	}
	if let Some(after) = s.expect_carried(&step, OFFICE, &bytes) {
		let remote = s.fresh_remote();
		let added = added_since(s, OFFICE, versions_before);
		s.check(
			&format!("{step}-once"),
			after.stable == stable(OFFICE)
				&& added == Some(1)
				&& !remote.trashed(&after.stable.to_string())
				&& remote.find(OFFICE_BACKUP).is_none(),
			format!("{added:?} versions uploaded"),
		);
	}
	Ok(())
}

/// A save of [`SAVED`] by renaming a new file over it.
fn rename_once(s: &mut Scenarios, i: u32) -> io::Result<()> {
	let versions_before = versions(s, SAVED);
	let bytes = save_bytes(7_502, i, 5);
	let temp = s.path(SAVED_TEMP);
	write_new(&temp, &bytes)?;
	let step = format!("S{i:03}-rename");
	if let Err(error) = fs::rename(&temp, s.path(SAVED)) {
		s.check(&step, false, format!("the rename failed: {error}"));
		return Ok(());
	}
	if let Some(after) = s.expect_carried(&step, SAVED, &bytes) {
		let remote = s.fresh_remote();
		let added = added_since(s, SAVED, versions_before);
		s.check(
			&format!("{step}-once"),
			after.stable == stable(SAVED) && added == Some(1) && remote.find(SAVED_TEMP).is_none(),
			format!("{added:?} versions uploaded"),
		);
	}
	Ok(())
}

/// A save of [`MOVED`] the way `cmd /c move /y` makes it: the new bytes written to [`MOVED_TEMP`],
/// `gap` later the temporary file moved over the file. Past the settle time the provider has
/// uploaded the temporary file as a file of its own (`L5b-save-move-y`, which failed so about once
/// in fifty runs): the file must still keep its identity with exactly one more version, and the
/// temporary file be gone from the drive, into its trash when it was uploaded.
fn move_once(s: &mut Scenarios, i: u32, gap: Duration) -> io::Result<()> {
	let versions_before = versions(s, MOVED);
	let bytes = save_bytes(7_503, i, 3);
	let temp = s.path(MOVED_TEMP);
	write_new(&temp, &bytes)?;
	thread::sleep(gap);
	let step = format!("Y{i:03}-move-y");
	let output = Command::new("cmd")
		.arg("/c")
		.arg("move")
		.arg("/y")
		.arg(&temp)
		.arg(s.path(MOVED))
		.output()?;
	if !output.status.success() {
		s.check(
			&step,
			false,
			format!(
				"gap {gap:?}, the move failed: {}",
				String::from_utf8_lossy(&output.stdout).trim()
			),
		);
		return Ok(());
	}
	if let Some(after) = s.expect_carried(&step, MOVED, &bytes) {
		let remote = s.fresh_remote();
		let added = added_since(s, MOVED, versions_before);
		s.check(
			&format!("{step}-once"),
			after.stable == stable(MOVED)
				&& added == Some(1)
				&& !remote.trashed(&after.stable.to_string())
				&& remote.find(MOVED_TEMP).is_none(),
			format!("gap {gap:?}, {added:?} versions uploaded"),
		);
	}
	Ok(())
}

/// A small generator for the gaps, seeded per run and logged, so a failing gap can be told.
struct Dice(u64);

impl Dice {
	fn below(&mut self, n: u64) -> u64 {
		self.0 ^= self.0 << 13;
		self.0 ^= self.0 >> 7;
		self.0 ^= self.0 << 17;
		self.0 % n
	}
}

/// An overwrite of [`OVERWRITTEN`] in place, through a handle opened for writing.
fn overwrite_once(s: &mut Scenarios, i: u32) -> io::Result<()> {
	let versions_before = versions(s, OVERWRITTEN);
	let bytes = save_bytes(7_003, i, 4);
	let mut file = OpenOptions::new()
		.write(true)
		.truncate(true)
		.open(s.path(OVERWRITTEN))?;
	file.write_all(&bytes)?;
	file.sync_all()?;
	drop(file);
	let step = format!("W{i:03}-overwrite");
	if let Some(after) = s.expect_carried(&step, OVERWRITTEN, &bytes) {
		let added = added_since(s, OVERWRITTEN, versions_before);
		s.check(
			&format!("{step}-once"),
			after.stable == stable(OVERWRITTEN) && added == Some(1),
			format!("{added:?} versions uploaded"),
		);
	}
	Ok(())
}

/// What the loop starts from, which no save may change: the items the drive lists, and the
/// requests the provider vetoed.
struct Baseline {
	items: BTreeSet<String>,
	vetoes: usize,
}

impl Baseline {
	fn now(s: &Scenarios) -> Self {
		Self {
			items: live_items(&s.fresh_remote()),
			vetoes: vetoes(s),
		}
	}
}

/// The ids of the items the drive lists, the provider's own upload names left out: an upload on
/// its way to its name. A file under its transit name, on its way to another directory or out of
/// a rename's way, counts, and is left behind once a save is done ([`transient_name`]).
fn live_items(remote: &Remote) -> BTreeSet<String> {
	remote
		.items()
		.iter()
		.filter(|item| {
			field(item, "name")
				.is_none_or(|name| !name.starts_with(".filen-upload") || is_transit(name))
		})
		.filter_map(|item| field(item, "uuid"))
		.map(str::to_owned)
		.collect()
}

/// Whether `name` is a file's transit name ([`filen_cloud_files`]'s `transit_name`).
fn is_transit(name: &str) -> bool {
	name.starts_with(".filen-upload-move-")
}

/// How many requests the provider vetoed so far.
fn vetoes(s: &Scenarios) -> usize {
	s.reports()
		.iter()
		.filter(|report| report.starts_with("Vetoed"))
		.count()
}

/// Whether `name` is one a save uses only for a moment: a backup, a temporary file, a reserved
/// `ReplaceFileW` name, or the `.old` name a kept conflict gets.
fn transient_name(name: &str) -> bool {
	let name = name.to_ascii_lowercase();
	name.ends_with(".bak")
		|| name.ends_with(".tmp")
		|| name.starts_with("~wrl")
		|| name.contains("~rf")
		|| name.contains(".old")
		|| name == "replace.new"
		|| is_transit(&name)
}

/// What no save may leave, checked after every iteration `i` against `baseline`: two placeholders
/// of one remote item anywhere in the root (read in the provider's process), a save's backup,
/// temporary or conflict name in `lx` on either side, more items on the drive than the loop started
/// with, a saved file under another identity than the tree's or in the drive's trash, and a vetoed
/// request (each save's own check catches the save that failed; this one catches a veto of any
/// other request too).
fn invariants(s: &mut Scenarios, i: u32, baseline: &Baseline) -> io::Result<()> {
	let step = format!("I{i:03}");
	let answer = s.control("placeholders");
	let parsed: Option<Value> = answer
		.as_ref()
		.ok()
		.and_then(|json| serde_json::from_str(json).ok());
	let doubled: Vec<String> = parsed
		.as_ref()
		.and_then(|value| value.get("items"))
		.and_then(Value::as_object)
		.map(|items| {
			items
				.iter()
				.filter(|(_, paths)| paths.as_array().is_some_and(|paths| paths.len() > 1))
				.map(|(uuid, paths)| format!("{uuid}: {paths}"))
				.collect()
		})
		.unwrap_or_default();
	// A walk that read nothing, or could not read a path, proves nothing.
	let listed = parsed
		.as_ref()
		.and_then(|value| value.get("items"))
		.and_then(Value::as_object)
		.is_some_and(|items| !items.is_empty());
	let all_read = parsed
		.as_ref()
		.and_then(|value| value.get("unreadable"))
		.and_then(Value::as_array)
		.is_some_and(Vec::is_empty);
	s.check(
		&format!("{step}-one-placeholder-per-item"),
		listed && all_read && doubled.is_empty(),
		match &parsed {
			Some(value) => format!(
				"doubled {doubled:?}, unreadable {}",
				value.get("unreadable").unwrap_or(&Value::Null)
			),
			None => format!("no answer: {answer:?}"),
		},
	);
	let mut local: Vec<String> = Vec::new();
	for entry in fs::read_dir(s.path("lx"))? {
		let name = entry?.file_name().to_string_lossy().into_owned();
		if transient_name(&name) {
			local.push(name);
		}
	}
	let remote = s.fresh_remote();
	let lx_dir = remote
		.find("lx")
		.and_then(|lx| field(lx, "uuid"))
		.map(|uuid| format!("stable/{uuid}"));
	let remote_left: Vec<String> = remote
		.items()
		.iter()
		.filter(|item| field(item, "dir") == lx_dir.as_deref())
		.filter_map(|item| field(item, "name"))
		.filter(|name| transient_name(name))
		.map(str::to_owned)
		.collect();
	s.check(
		&format!("{step}-nothing-left-behind"),
		lx_dir.is_some() && local.is_empty() && remote_left.is_empty(),
		format!("lx on the drive {lx_dir:?}, local {local:?}, drive {remote_left:?}"),
	);
	let items = live_items(&remote);
	let added: Vec<&String> = items.difference(&baseline.items).collect();
	let lost: Vec<&String> = baseline.items.difference(&items).collect();
	s.check(
		&format!("{step}-no-new-items"),
		added.is_empty() && lost.is_empty(),
		format!(
			"{} items on the drive, {} at the start; added {added:?}, gone {lost:?}",
			items.len(),
			baseline.items.len()
		),
	);
	let moved: Vec<String> = SAVED_FILES
		.iter()
		.filter(|path| {
			let uuid = stable(path).to_string();
			remote.trashed(&uuid)
				|| remote
					.find(path)
					.and_then(|item| field(item, "uuid"))
					.is_none_or(|held| !same_uuid(held, &uuid))
		})
		.map(|path| (*path).to_owned())
		.collect();
	s.check(
		&format!("{step}-files-keep-identity"),
		moved.is_empty(),
		format!("not the tree's file, or trashed: {moved:?}"),
	);
	let vetoed = vetoes(s);
	s.check(
		&format!("{step}-no-veto"),
		vetoed == baseline.vetoes,
		format!("{vetoed} vetoes, {} at the start", baseline.vetoes),
	);
	Ok(())
}

/// Writes `bytes` to the new file `path`, closed before it returns.
fn write_new(path: &std::path::Path, bytes: &[u8]) -> io::Result<()> {
	let mut file = fs::File::create(path)?;
	file.write_all(bytes)?;
	file.sync_all()
}

/// No drive write for [`IDLE`] once the loop is done.
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
		"loop-idle-no-writes",
		added.is_empty(),
		format!("{} added over {IDLE:?}: {added:?}", added.len()),
	);
}

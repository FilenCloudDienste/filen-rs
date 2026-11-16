//! `helper-drive`: the desktop app's side of the helper. Spawns a helper executable with piped
//! stdin and stdout as the app's supervisor does, speaks the protocol to it, logs every line it
//! writes to `helper-events.jsonl` (stderr to `helper-<n>.stderr`), and checks what a user sees in
//! the root through `user`, a third process, as the OS does not call a provider back for its own
//! process. Into `<out>\helper.txt`, then `<out>\helper.done`.
//!
//! Two modes:
//! - `fake`: `<exe>` is this harness (`vm_harness helper`), serving the tree from the fake drive:
//!   H1 to H12 and H16 of the design, and H17.
//! - `prod`: `<exe>` is the shipping `filen-cloud-files.exe`, with no session: a pushed session
//!   whose every field is made up, which nothing may echo.
//! - `leave`: as `fake`, only registers, serves, reads one file and shuts down, leaving the root
//!   registered for an `--unregister-all` to find.
//!
//! `user <op> <path> [text]` is the user: `read`, `write`, `append`, `delete`, `exists`, `list`,
//! `readat` (`<offset>:<len>`) and `writeat` (`<offset>:<text>`), each answered by one JSON line
//! on stdout.

use std::{
	ffi::OsString,
	fs::{self, File, OpenOptions},
	io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
	os::windows::fs::MetadataExt,
	path::{Path, PathBuf},
	process::{Child, ChildStdin, Command, Stdio},
	sync::{
		Arc, Mutex, PoisonError,
		mpsc::{self, Receiver},
	},
	thread,
	time::{Duration, Instant},
};

use filen_cloud_files::test_support::pattern;
use serde_json::{Value, json};

use crate::{
	explorer,
	report::{Outcome, Report},
	tree,
};

/// The user id the scenarios register as.
const USER: &str = "1";
/// The user id of the `prod` mode's made-up session.
const PROD_USER: &str = "987654321";
/// Markers the `prod` mode's made-up session carries in its secret fields: none may appear in any
/// line or log the helper writes.
const MARKERS: [&str; 5] = [
	"mk-email-7c1f",
	"mk-pass-9a2e",
	"mk-master-3b8d",
	"mk-api-5e6f",
	"mk-priv-1d4c",
];
/// `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`: a placeholder with data not on disk has it.
const RECALL_ON_DATA_ACCESS: u64 = 0x40_0000;
/// How long a request's reply may take: a loaded guest stops for tens of seconds (see `stall`).
const REPLY: Duration = Duration::from_secs(120);
/// The name the `fake` mode's root is shown under in File Explorer.
const DISPLAY_NAME: &str = "Filen (H18)";
/// An icon file every Windows has, for the `fake` mode's root.
const ICON: &str = r"C:\Windows\explorer.exe";

/// One line the helper wrote, as the driver logs it.
fn log_line(log: &Mutex<File>, n: u32, started: Instant, line: &str) {
	let entry = json!({ "n": n, "ms": started.elapsed().as_millis(), "line": line });
	let mut file = log.lock().unwrap_or_else(PoisonError::into_inner);
	let _ = writeln!(file, "{entry}");
	let _ = file.flush();
}

/// A running helper.
struct Helper {
	child: Child,
	stdin: Option<ChildStdin>,
	lines: Receiver<Value>,
	/// Lines read but not yet looked at.
	seen: Vec<Value>,
	next_id: u64,
}

impl Helper {
	/// Spawns `exe` with `args` as helper number `n`.
	fn spawn(
		exe: &Path,
		args: &[OsString],
		out: &Path,
		n: u32,
		log: &Arc<Mutex<File>>,
		started: Instant,
	) -> std::io::Result<Self> {
		let stderr = File::create(out.join(format!("helper-{n}.stderr")))?;
		let mut child = Command::new(exe)
			.args(args)
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(stderr)
			.spawn()?;
		let stdin = child.stdin.take();
		let stdout = child
			.stdout
			.take()
			.ok_or_else(|| std::io::Error::other("no stdout pipe"))?;
		let (tx, lines) = mpsc::channel();
		let log = Arc::clone(log);
		thread::spawn(move || {
			for line in BufReader::new(stdout).lines() {
				let Ok(line) = line else { break };
				log_line(&log, n, started, &line);
				let value = serde_json::from_str(&line).unwrap_or(Value::String(line));
				if tx.send(value).is_err() {
					break;
				}
			}
		});
		Ok(Self {
			child,
			stdin,
			lines,
			seen: Vec::new(),
			next_id: 1,
		})
	}

	/// Writes `line` as it is.
	fn send_raw(&mut self, line: &str) -> bool {
		match &mut self.stdin {
			Some(stdin) => writeln!(stdin, "{line}")
				.and_then(|()| stdin.flush())
				.is_ok(),
			None => false,
		}
	}

	/// The next line, waiting at most `wait`.
	fn recv(&mut self, wait: Duration) -> Option<Value> {
		// A timeout and a helper that closed stdout both end the wait.
		self.lines.recv_timeout(wait).ok()
	}

	/// Sends `message` with the next id and waits for its reply; the lines before it are kept.
	fn request(&mut self, mut message: Value) -> Option<Value> {
		let id = self.next_id;
		self.next_id += 1;
		message["id"] = json!(id);
		if !self.send_raw(&message.to_string()) {
			return None;
		}
		let deadline = Instant::now() + REPLY;
		while let Some(left) = deadline.checked_duration_since(Instant::now()) {
			let line = self.recv(left)?;
			if line["t"] == "reply" && line["id"] == json!(id) {
				return Some(line);
			}
			self.seen.push(line);
		}
		None
	}

	/// The first line, read already or within `wait`, that `matches`; removed from what is kept.
	fn event(&mut self, wait: Duration, matches: impl Fn(&Value) -> bool) -> Option<Value> {
		if let Some(at) = self.seen.iter().position(&matches) {
			return Some(self.seen.remove(at));
		}
		let deadline = Instant::now() + wait;
		while let Some(left) = deadline.checked_duration_since(Instant::now()) {
			let line = self.recv(left)?;
			if matches(&line) {
				return Some(line);
			}
			self.seen.push(line);
		}
		None
	}

	/// Closes stdin, as a parent that went away.
	fn close_stdin(&mut self) {
		self.stdin = None;
	}

	/// The exit code once the helper ended, waiting at most `wait`.
	fn exit_code(&mut self, wait: Duration) -> Option<i32> {
		let deadline = Instant::now() + wait;
		loop {
			match self.child.try_wait() {
				Ok(Some(status)) => return status.code(),
				Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(200)),
				Ok(None) | Err(_) => return None,
			}
		}
	}

	/// Kills the helper (`TerminateProcess`), as `taskkill /f` does.
	fn kill(&mut self) {
		let _ = self.child.kill();
		let _ = self.child.wait();
	}
}

impl Drop for Helper {
	fn drop(&mut self) {
		if let Ok(None) = self.child.try_wait() {
			let _ = self.child.kill();
		}
	}
}

/// Whether `reply` succeeded.
fn ok(reply: Option<&Value>) -> bool {
	reply.is_some_and(|reply| reply["ok"] == true)
}

/// The value of a successful `reply`, `null` otherwise.
fn value(reply: Option<&Value>) -> Value {
	reply.map_or(Value::Null, |reply| reply["value"].clone())
}

/// The error code of a failed `reply`.
fn code(reply: Option<&Value>) -> Option<String> {
	reply.and_then(|reply| reply["error"]["code"].as_str().map(str::to_owned))
}

/// Whether `line` is an `activity` line with an entry of `kind` whose path ends in `name`.
fn activity(line: &Value, kind: &str, name: &str) -> bool {
	line["t"] == "activity"
		&& line["entries"].as_array().is_some_and(|entries| {
			entries.iter().any(|entry| {
				entry["kind"] == kind
					&& entry["path"]
						.as_str()
						.is_some_and(|path| path.ends_with(name))
			})
		})
}

/// The `user` process: runs `op` on `path` in a process of its own.
fn user(op: &str, path: &Path, text: Option<&str>) -> Value {
	let mut command = Command::new(std::env::current_exe().unwrap_or_else(|_| "vm_harness".into()));
	command.arg("user").arg(op).arg(path);
	if let Some(text) = text {
		command.arg(text);
	}
	match command.output() {
		Ok(output) => serde_json::from_slice(&output.stdout).unwrap_or_else(
			|_| json!({ "ok": false, "harness": String::from_utf8_lossy(&output.stdout) }),
		),
		Err(error) => json!({ "ok": false, "harness": error.to_string() }),
	}
}

/// What reading a tree file whole should give: its length and BLAKE3.
fn expected(path: &str) -> Value {
	let size = tree::size_of(path).unwrap_or(0);
	json!({ "ok": true, "len": size, "hash": blake3::hash(&pattern(size)).to_hex().as_str() })
}

/// The `user` subcommand: one JSON line on stdout.
pub(crate) fn user_op(op: &str, path: &Path, text: Option<&str>) -> Outcome {
	let os_error = |error: &std::io::Error| json!({ "ok": false, "code": error.raw_os_error() });
	let answer = match op {
		"read" => match fs::read(path) {
			Ok(bytes) => {
				json!({ "ok": true, "len": bytes.len(), "hash": blake3::hash(&bytes).to_hex().as_str() })
			}
			Err(error) => os_error(&error),
		},
		"write" => match fs::write(path, text.unwrap_or_default()) {
			Ok(()) => json!({ "ok": true }),
			Err(error) => os_error(&error),
		},
		"append" => match OpenOptions::new()
			.append(true)
			.open(path)
			.and_then(|mut file| file.write_all(text.unwrap_or_default().as_bytes()))
		{
			Ok(()) => json!({ "ok": true }),
			Err(error) => os_error(&error),
		},
		// `readat <path> <offset>:<len>` and `writeat <path> <offset>:<text>`.
		"readat" => match at_offset(text).and_then(|(offset, len)| {
			let len = len.parse::<usize>().map_err(std::io::Error::other)?;
			let mut file = File::open(path)?;
			file.seek(SeekFrom::Start(offset))?;
			let mut bytes = vec![0; len];
			file.read_exact(&mut bytes)?;
			Ok(bytes)
		}) {
			Ok(bytes) => {
				json!({ "ok": true, "len": bytes.len(), "hash": blake3::hash(&bytes).to_hex().as_str() })
			}
			Err(error) => os_error(&error),
		},
		"writeat" => match at_offset(text).and_then(|(offset, patch)| {
			let mut file = OpenOptions::new().write(true).open(path)?;
			file.seek(SeekFrom::Start(offset))?;
			file.write_all(patch.as_bytes())
		}) {
			Ok(()) => json!({ "ok": true }),
			Err(error) => os_error(&error),
		},
		"delete" => match fs::remove_file(path) {
			Ok(()) => json!({ "ok": true }),
			Err(error) => os_error(&error),
		},
		"exists" => match fs::symlink_metadata(path) {
			Ok(meta) => json!({ "ok": true, "exists": true, "attributes": meta.file_attributes() }),
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
				json!({ "ok": true, "exists": false })
			}
			Err(error) => os_error(&error),
		},
		"list" => match fs::read_dir(path) {
			Ok(entries) => {
				let mut names: Vec<String> = entries
					.filter_map(Result::ok)
					.map(|entry| entry.file_name().to_string_lossy().into_owned())
					.collect();
				names.sort();
				json!({ "ok": true, "names": names })
			}
			Err(error) => os_error(&error),
		},
		_ => json!({ "ok": false, "harness": format!("unknown op {op}") }),
	};
	println!("{answer}");
	Ok(true)
}

/// `<offset>:<rest>` from a user op's text.
fn at_offset(text: Option<&str>) -> std::io::Result<(u64, &str)> {
	let (offset, rest) = text
		.and_then(|text| text.split_once(':'))
		.ok_or_else(|| std::io::Error::other("expected <offset>:<rest>"))?;
	let offset = offset.parse().map_err(std::io::Error::other)?;
	Ok((offset, rest))
}

/// The session the scenarios push: account `user`, its root `base_folder`, and `secret` in every
/// field that holds one.
fn session(user: &str, base_folder: &str, secret: impl Fn(usize) -> String) -> String {
	json!({
		"providerEnabled": true,
		"sdkConfig": {
			"email": format!("{}@example.invalid", secret(0)),
			"password": secret(1),
			"twoFactorCode": "",
			"masterKeys": [secret(2)],
			"apiKey": secret(3),
			"publicKey": "",
			"privateKey": secret(4),
			"authVersion": 2,
			"baseFolderUUID": base_folder,
			"userId": user,
			"metadataCache": false,
			"tmpPath": "",
			"connectToSocket": false,
		}
	})
	.to_string()
}

fn register(root: &Path, data: &Path, user: &str) -> Value {
	json!({
		"t": "register",
		"root": root.to_string_lossy(),
		"userId": user,
		"dataDir": data.to_string_lossy(),
	})
}

/// Where a run keeps what it makes, from its output directory `out` (`C:\work\<run>\<mode>`):
/// the root under `C:\cfroot\<run>-<mode>\Filen`, the state and data directories in `out`.
struct Layout {
	root: PathBuf,
	state: PathBuf,
	data: PathBuf,
}

impl Layout {
	fn of(out: &Path) -> Self {
		// `<run>-<mode>` from `C:\work\<run>\<mode>`, so no two runs share a root.
		let name = |path: Option<&Path>| {
			path.and_then(Path::file_name)
				.map_or_else(|| "hx".into(), |name| name.to_string_lossy().into_owned())
		};
		let run = format!("{}-{}", name(out.parent()), name(Some(out)));
		Self {
			root: PathBuf::from(format!("C:\\cfroot\\{run}\\Filen")),
			state: out.join("state"),
			data: out.join("data"),
		}
	}
}

pub(crate) fn run(exe: &Path, out: &Path, mode: &str) -> Outcome {
	fs::create_dir_all(out)?;
	let mut report = Report::create(&out.join("helper.txt"))?;
	let log = Arc::new(Mutex::new(File::create(out.join("helper-events.jsonl"))?));
	let started = Instant::now();
	let layout = Layout::of(out);
	if let Some(parent) = layout.root.parent() {
		fs::create_dir_all(parent)?;
	}
	let passed = match mode {
		"fake" => fake(exe, out, &layout, &mut report, &log, started),
		"prod" => prod(exe, out, &layout, &mut report, &log, started),
		"leave" => leave(exe, out, &layout, &mut report, &log, started),
		_ => {
			report.fail("mode", format!("unknown mode {mode}"));
			Ok(false)
		}
	};
	fs::write(out.join("helper.done"), "done")?;
	passed.map(|passed| passed && report.passed())
}

/// The helper's arguments for the state directory `state`, and the rearm the fake mode asks for.
fn serve_args(state: &Path, fake: bool) -> Vec<OsString> {
	let mut args: Vec<OsString> = Vec::new();
	if fake {
		args.push("helper".into());
	}
	args.push("--state-dir".into());
	args.push(state.into());
	if fake {
		args.push("--rearm-secs".into());
		args.push("5".into());
	}
	args
}

/// Waits up to `wait` for `user`'s `exists` of `path` to answer `exists`.
fn wait_exists(path: &Path, exists: bool, wait: Duration) -> bool {
	let deadline = Instant::now() + wait;
	loop {
		if user("exists", path, None)["exists"] == exists {
			return true;
		}
		if Instant::now() >= deadline {
			return false;
		}
		thread::sleep(Duration::from_millis(500));
	}
}

#[allow(clippy::too_many_lines)] // One scenario after another, in the order they depend on.
fn fake(
	exe: &Path,
	out: &Path,
	layout: &Layout,
	report: &mut Report,
	log: &Arc<Mutex<File>>,
	started: Instant,
) -> Outcome {
	let root = &layout.root;
	let base = tree::ROOT.to_string();
	let auth = session(USER, &base, |n| format!("{}-{n}", MARKERS[n.min(4)]));
	let args = serve_args(&layout.state, true);
	// H18-sweep: what a run killed between the OS's unregister and its clean-up left, planted.
	let gone = root.with_file_name("Orphan");
	let orphan = explorer::plant_orphan(report, &gone);
	let mut helper = Helper::spawn(exe, &args, out, 1, log, started)?;

	// H1: hello first.
	let hello = helper.recv(REPLY);
	report.check(
		"H1-hello",
		hello
			.as_ref()
			.is_some_and(|line| line["t"] == "hello" && line["protocol"] == 1),
		format!("{hello:?}"),
	);

	// H2: register, naming what File Explorer shows; the root exists afterwards, not yet
	// registered with the OS.
	let mut branded = register(root, &layout.data, USER);
	branded["displayName"] = json!(DISPLAY_NAME);
	branded["iconPath"] = json!(ICON);
	branded["iconIndex"] = json!(0);
	let reply = helper.request(branded);
	report.check(
		"H2-register",
		value(reply.as_ref()) == json!({ "state": "registered", "resumed": false }),
		format!("{reply:?}"),
	);
	// The helper swept the planted entries when it started.
	explorer::check_unbranded(report, "H18-sweep", &gone, orphan.as_deref(), out)?;
	let listed = helper.request(json!({ "t": "listRoots" }));
	report.info("H2-listRoots", format!("{listed:?}"));

	// H3: setAuth serves.
	let reply = helper.request(json!({ "t": "setAuth", "userId": USER, "json": auth }));
	report.check(
		"H3-setAuth",
		value(reply.as_ref()) == json!({ "state": "serving", "adopted": false, "branded": true }),
		format!("{reply:?}"),
	);
	let watching = helper.event(Duration::from_secs(60), |line| {
		line["t"] == "status" && line["state"] == "serving" && line["watching"] == true
	});
	report.check("H3-watching", watching.is_some(), format!("{watching:?}"));
	let listed = helper.request(json!({ "t": "listRoots" }));
	report.check(
		"H3-listRoots",
		value(listed.as_ref())["roots"][0]["registered"] == true
			&& value(listed.as_ref())["roots"][0]["serving"] == true,
		format!("{listed:?}"),
	);

	// H18: what File Explorer is told about the served root, and what its commands do.
	let clsid = explorer::check_branded(report, "H18", root, USER, DISPLAY_NAME, out)?;
	explorer::check_commands(report, "H18", &root.join("big.bin"));

	// H13: the registry keys the registration wrote.
	for (hive, file) in [("HKCU", "reg-hkcu.txt"), ("HKLM", "reg-hklm.txt")] {
		let key = format!(
			"{hive}\\Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\SyncRootManager"
		);
		let output = Command::new("reg").args(["query", &key, "/s"]).output();
		let text = output.map_or_else(
			|error| error.to_string(),
			|output| {
				format!(
					"{}{}",
					String::from_utf8_lossy(&output.stdout),
					String::from_utf8_lossy(&output.stderr)
				)
			},
		);
		fs::write(out.join(file), &text)?;
		report.info(
			"H13-registry",
			format!("{hive}: {} lines in {file}", text.lines().count()),
		);
	}

	// H4: a listing and a whole read from another process.
	let listing = user("list", root, None);
	report.check(
		"H4-list",
		listing["names"]
			.as_array()
			.is_some_and(|names| names.iter().any(|name| name == "small.txt")),
		format!("{} names", listing["names"].as_array().map_or(0, Vec::len)),
	);
	let read = user("read", &root.join("small.txt"), None);
	report.check("H4-read", read == expected("small.txt"), format!("{read}"));

	// H5: a new file and an edit reach the drive.
	let created = user(
		"write",
		&root.join("h5-new.txt"),
		Some("created by the user"),
	);
	let uploaded = helper.event(Duration::from_secs(120), |line| {
		activity(line, "uploaded", "h5-new.txt")
	});
	report.check(
		"H5-create",
		created["ok"] == true && uploaded.is_some(),
		format!("{uploaded:?}"),
	);
	let edited = user("append", &root.join("small.txt"), Some("edited"));
	let uploaded = helper.event(Duration::from_secs(120), |line| {
		activity(line, "uploaded", "small.txt")
	});
	report.check(
		"H5-edit",
		edited["ok"] == true && uploaded.is_some(),
		format!("{uploaded:?}"),
	);
	let pending = helper.event(Duration::from_secs(30), |line| {
		line["t"] == "pendingUploads"
	});
	report.check(
		"H5-pendingUploads",
		pending.is_some(),
		format!("{pending:?}"),
	);
	let state = helper.request(json!({ "t": "getState" }));
	report.info("H5-getState", format!("{state:?}"));

	// H12: after the watcher was replaced (every 5 s here), a create still arrives.
	thread::sleep(Duration::from_secs(12));
	let created = user(
		"write",
		&root.join("h12-after-rearm.txt"),
		Some("after the rearm"),
	);
	let uploaded = helper.event(Duration::from_secs(120), |line| {
		activity(line, "uploaded", "h12-after-rearm.txt")
	});
	report.check(
		"H12-rearm",
		created["ok"] == true && uploaded.is_some(),
		format!("{uploaded:?}"),
	);

	// H6: clearAuth; a file never read fails with 362, a hydrated one still reads.
	let reply = helper.request(json!({ "t": "clearAuth", "userId": USER }));
	report.check(
		"H6-clearAuth",
		value(reply.as_ref()) == json!({ "cleared": true }),
		format!("{reply:?}"),
	);
	let never = user("read", &root.join("never.bin"), None);
	report.check(
		"H6-dehydrated-362",
		never["code"] == 362,
		format!("{never}"),
	);
	let hydrated = user("read", &root.join("hello.bin"), None);
	report.info("H6-not-read-before", format!("{hydrated}"));
	let created = user("read", &root.join("h5-new.txt"), None);
	report.check(
		"H6-local-file-reads",
		created["ok"] == true,
		format!("{created}"),
	);
	let auth_json = layout.data.join("auth.json");
	report.check(
		"H6-auth-json-gone",
		!auth_json.exists(),
		auth_json.display(),
	);

	// H11: a delete while disconnected; then H7 setAuth again.
	let deleted = user("delete", &root.join("exact.bin"), None);
	report.info("H11-delete-while-disconnected", format!("{deleted}"));
	let reply = helper.request(json!({ "t": "setAuth", "userId": USER, "json": auth }));
	report.check(
		"H7-setAuth-again",
		value(reply.as_ref()) == json!({ "state": "serving", "adopted": false, "branded": true }),
		format!("{reply:?}"),
	);
	let never = user("read", &root.join("never.bin"), None);
	report.check(
		"H7-read",
		never == expected("never.bin"),
		format!("{never}"),
	);
	if deleted["ok"] == true {
		let back = wait_exists(&root.join("exact.bin"), true, Duration::from_secs(400));
		report.check("H11-restored", back, "exact.bin back as a placeholder");
	}

	// H8: an edit, then the helper killed before it can carry it; 362 while it is gone; a new
	// helper resumes on register and carries the edit.
	let edit = user(
		"write",
		&root.join("h8-dirty.txt"),
		Some("written just before the kill"),
	);
	helper.kill();
	let early = helper
		.seen
		.iter()
		.any(|line| activity(line, "uploaded", "h8-dirty.txt"));
	report.info(
		"H8-killed",
		format!("write {edit}, carried before the kill: {early}"),
	);
	let gone = user("read", &root.join("par0.bin"), None);
	report.check(
		"H8-362-while-killed",
		gone["code"] == 362 || gone["code"] == 404,
		format!("{gone}"),
	);
	let mut helper = Helper::spawn(exe, &args, out, 2, log, started)?;
	let _ = helper.recv(REPLY);
	let reply = helper.request(register(root, &layout.data, USER));
	report.check(
		"H8-resume",
		value(reply.as_ref()) == json!({ "state": "serving", "resumed": true }),
		format!("{reply:?}"),
	);
	let carried = helper.event(Duration::from_secs(400), |line| {
		activity(line, "uploaded", "h8-dirty.txt")
	});
	report.check("H8-carried", carried.is_some(), format!("{carried:?}"));

	// H16: a second helper on the same state directory is refused.
	let mut second = Helper::spawn(exe, &args, out, 3, log, started)?;
	let fatal = second.recv(REPLY);
	let exit = second.exit_code(Duration::from_secs(60));
	report.check(
		"H16-second-instance",
		exit == Some(12)
			&& fatal
				.as_ref()
				.is_some_and(|line| line["code"] == "alreadyRunning"),
		format!("exit {exit:?} line {fatal:?}"),
	);

	// H9: the parent closes stdin: exit 0, the root still registered.
	helper.close_stdin();
	let exit = helper.exit_code(Duration::from_secs(120));
	report.check("H9-eof-exit-0", exit == Some(0), format!("exit {exit:?}"));
	let mut helper = Helper::spawn(exe, &args, out, 4, log, started)?;
	let _ = helper.recv(REPLY);
	let listed = helper.request(json!({ "t": "listRoots" }));
	report.check(
		"H9-still-registered",
		value(listed.as_ref())["roots"][0]["registered"] == true,
		format!("{listed:?}"),
	);

	// H10a: unregister keeping downloaded files, with a file changed while signed out and a new
	// ordinary file, both kept.
	let reply = helper.request(register(root, &layout.data, USER));
	report.info("H10-register", format!("{reply:?}"));
	// Downloaded while served, so it can be changed once signed out.
	let read = user("read", &root.join("hello.bin"), None);
	report.check("H10-read", read == expected("hello.bin"), format!("{read}"));
	let reply = helper.request(json!({ "t": "clearAuth", "userId": USER }));
	report.info("H10-clearAuth", format!("{reply:?}"));
	let changed = user(
		"append",
		&root.join("hello.bin"),
		Some("changed while signed out"),
	);
	let plain = user("write", &root.join("h10-plain.txt"), Some("never uploaded"));
	let lazy = user(
		"write",
		&root.join("rx\\lazy\\h10-lazy.txt"),
		Some("in a directory never listed"),
	);
	report.info(
		"H10-setup",
		format!("changed {changed} plain {plain} lazy {lazy}"),
	);
	let reply =
		helper.request(json!({ "t": "unregister", "userId": USER, "deleteHydrated": false }));
	let done = value(reply.as_ref());
	report.check(
		"H10a-unregister",
		done["unregistered"] == true,
		format!("{reply:?}"),
	);
	// H18-unbranded: nothing of what File Explorer was told is left.
	explorer::check_unbranded(report, "H18-unbranded", root, clsid.as_deref(), out)?;
	let kept_in = done["keptIn"].as_str().map(PathBuf::from);
	let kept = |name: &str| {
		done["kept"].as_array().is_some_and(|kept| {
			kept.iter().any(|item| {
				item["path"]
					.as_str()
					.is_some_and(|path| path.ends_with(name))
			})
		})
	};
	report.check(
		"H10a-kept-changed",
		kept("hello.bin"),
		"hello.bin reported kept",
	);
	report.check(
		"H10a-kept-plain",
		kept("h10-plain.txt"),
		"h10-plain.txt reported kept",
	);
	if let Some(kept_in) = &kept_in {
		let changed = user("read", &kept_in.join("hello.bin"), None);
		report.check(
			"H10a-changed-bytes",
			changed["len"] == json!(300_000 + "changed while signed out".len()),
			format!("{changed}"),
		);
		let plain = user("read", &kept_in.join("h10-plain.txt"), None);
		report.check(
			"H10a-plain-bytes",
			plain["len"] == json!("never uploaded".len()),
			format!("{plain}"),
		);
		// Read whole in H7 and never changed: a clean downloaded file, kept as it is.
		let never = user("read", &kept_in.join("never.bin"), None);
		report.check(
			"H10a-hydrated-kept",
			never == expected("never.bin"),
			format!("{never}"),
		);
		let never = user("exists", &kept_in.join("par1.bin"), None);
		report.check(
			"H10a-dehydrated-gone",
			never["exists"] == false,
			format!("{never}"),
		);
		let lazy = user("read", &kept_in.join("rx\\lazy\\h10-lazy.txt"), None);
		report.info("H10a-lazy", format!("{lazy}"));
	} else {
		report.fail("H10a-kept-in", "no keptIn");
	}
	report.check("H10a-root-freed", !root.exists(), root.display());

	// H10b: the same root again, deleting downloaded files, with a dirty file kept.
	let data_b = layout.data.with_file_name("data-b");
	let reply = helper.request(register(root, &data_b, USER));
	report.check("H10b-register", ok(reply.as_ref()), format!("{reply:?}"));
	let reply = helper.request(json!({ "t": "setAuth", "userId": USER, "json": auth }));
	report.check("H10b-setAuth", ok(reply.as_ref()), format!("{reply:?}"));
	let read = user("read", &root.join("hello.bin"), None);
	report.check(
		"H10b-read",
		read == expected("hello.bin"),
		format!("{read}"),
	);
	let read = user("read", &root.join("small.txt"), None);
	report.check(
		"H10b-read-small",
		read == expected("small.txt"),
		format!("{read}"),
	);
	// A changed file below a directory placeholder: the directory is made ordinary too.
	let inner = user("read", &root.join("sub\\inner.bin"), None);
	report.check(
		"H10b-read-inner",
		inner == expected("sub\\inner.bin"),
		format!("{inner}"),
	);
	let reply = helper.request(json!({ "t": "clearAuth", "userId": USER }));
	report.info("H10b-clearAuth", format!("{reply:?}"));
	let dirty = user("append", &root.join("small.txt"), Some("dirty"));
	let dirty_inner = user("append", &root.join("sub\\inner.bin"), Some("dirty"));
	report.info("H10b-dirty", format!("{dirty} {dirty_inner}"));
	let reply =
		helper.request(json!({ "t": "unregister", "userId": USER, "deleteHydrated": true }));
	let done = value(reply.as_ref());
	report.check(
		"H10b-unregister",
		done["unregistered"] == true && done["deletedHydrated"].as_u64().is_some_and(|n| n >= 1),
		format!("{reply:?}"),
	);
	match done["keptIn"].as_str().map(PathBuf::from) {
		Some(kept_in) => {
			let small = user("read", &kept_in.join("small.txt"), None);
			report.check(
				"H10b-dirty-kept",
				small["len"] == json!(10_000 + "dirty".len()),
				format!("{small}"),
			);
			let hello = user("exists", &kept_in.join("hello.bin"), None);
			report.check(
				"H10b-hydrated-deleted",
				hello["exists"] == false,
				format!("{hello}"),
			);
			let inner = user("read", &kept_in.join("sub\\inner.bin"), None);
			report.check(
				"H10b-dirty-in-dir-kept",
				inner["len"] == json!(10_000 + "dirty".len()),
				format!("{inner}"),
			);
		}
		None => report.fail("H10b-kept-in", format!("{done}")),
	}

	unregister_offline(&mut helper, layout, report, &auth);

	// For H14: a root left registered and served once, for the shipping binary to unregister.
	let reply = helper.request(register(root, &layout.data.with_file_name("data-c"), USER));
	report.info("H14-setup-register", format!("{reply:?}"));
	let reply = helper.request(json!({ "t": "setAuth", "userId": USER, "json": auth }));
	report.info("H14-setup-setAuth", format!("{reply:?}"));
	let read = user("read", &root.join("hello.bin"), None);
	report.info("H14-setup-read", format!("{read}"));
	let reply = helper.request(json!({ "t": "shutdown" }));
	let exit = helper.exit_code(Duration::from_secs(120));
	report.check(
		"H-shutdown-exit-0",
		ok(reply.as_ref()) && exit == Some(0),
		format!("exit {exit:?}"),
	);
	Ok(true)
}

/// H17: an unregister while the drive is unreachable, with a file changed while the root was not
/// served and so only partly on disk (measured: a write while the provider is disconnected leaves
/// a placeholder partly on disk and not in sync, where a connected provider is asked for the whole
/// file first): refused as `unsyncedChanges`, the root still registered and served, nothing lost;
/// once the drive answers, the unregister keeps the file whole, the change in it.
fn unregister_offline(helper: &mut Helper, layout: &Layout, report: &mut Report, auth: &str) {
	let root = &layout.root;
	// Each of its reads is delivered 4 s late: the first piece is on disk while the rest of the
	// window is still on its way when the root stops being served, leaving it partly on disk.
	let file = root.join("slow.bin");
	let reply = helper.request(register(root, &layout.data.with_file_name("data-d"), USER));
	report.check("H17-register", ok(reply.as_ref()), format!("{reply:?}"));
	let reply = helper.request(json!({ "t": "setAuth", "userId": USER, "json": auth }));
	report.check("H17-setAuth", ok(reply.as_ref()), format!("{reply:?}"));
	let part = user("readat", &file, Some("0:4096"));
	report.check("H17-read-part", part["ok"] == true, format!("{part}"));
	// Not served from here on, the root still registered: as while the helper is not running.
	let reply = helper.request(json!({ "t": "clearAuth", "userId": USER }));
	report.info("H17-clearAuth", format!("{reply:?}"));
	let patch = "changed while not served";
	let wrote = user("writeat", &file, Some(&format!("0:{patch}")));
	report.check("H17-write-partial", wrote["ok"] == true, format!("{wrote}"));
	let left = user("exists", &file, None);
	report.check(
		"H17-partly-on-disk",
		left["attributes"]
			.as_u64()
			.is_some_and(|attributes| attributes & RECALL_ON_DATA_ACCESS != 0),
		format!("{left}"),
	);
	let offline = layout.state.with_file_name("offline");
	if let Err(error) = fs::write(&offline, "offline") {
		report.fail("H17-offline", error);
		return;
	}
	// The helper looks for the switch every 250 ms: offline before it serves again, or the start's
	// walk fetches the rest of the file first.
	thread::sleep(Duration::from_secs(2));
	let reply = helper.request(json!({ "t": "setAuth", "userId": USER, "json": auth }));
	report.check(
		"H17-setAuth-offline",
		ok(reply.as_ref()),
		format!("{reply:?}"),
	);
	thread::sleep(Duration::from_secs(3));
	let reply =
		helper.request(json!({ "t": "unregister", "userId": USER, "deleteHydrated": false }));
	report.check(
		"H17-refused-offline",
		code(reply.as_ref()).as_deref() == Some("unsyncedChanges"),
		format!("{reply:?}"),
	);
	let listed = helper.request(json!({ "t": "listRoots" }));
	report.check(
		"H17-still-registered",
		value(listed.as_ref())["roots"][0]["registered"] == true,
		format!("{listed:?}"),
	);
	let head = user("readat", &file, Some(&format!("0:{}", patch.len())));
	report.check(
		"H17-change-in-place",
		head["hash"] == blake3::hash(patch.as_bytes()).to_hex().as_str(),
		format!("{head}"),
	);
	if let Err(error) = fs::remove_file(&offline) {
		report.fail("H17-online", error);
		return;
	}
	thread::sleep(Duration::from_secs(3));
	let reply =
		helper.request(json!({ "t": "unregister", "userId": USER, "deleteHydrated": false }));
	let done = value(reply.as_ref());
	report.check(
		"H17-unregister-online",
		done["unregistered"] == true,
		format!("{reply:?}"),
	);
	let size = tree::size_of("slow.bin").unwrap_or(0);
	let mut whole = pattern(size);
	if let Some(target) = whole.get_mut(..patch.len()) {
		target.copy_from_slice(patch.as_bytes());
	}
	match done["keptIn"].as_str().map(PathBuf::from) {
		Some(kept_in) => {
			let kept = user("read", &kept_in.join("slow.bin"), None);
			report.check(
				"H17-kept-whole",
				kept["len"] == json!(size)
					&& kept["hash"] == blake3::hash(&whole).to_hex().as_str(),
				format!("{kept}"),
			);
		}
		None => report.fail("H17-kept-in", format!("{done}")),
	}
	report.check("H17-root-freed", !root.exists(), root.display());
}

#[allow(clippy::too_many_lines)] // One scenario after another, in the order they depend on.
fn prod(
	exe: &Path,
	out: &Path,
	layout: &Layout,
	report: &mut Report,
	log: &Arc<Mutex<File>>,
	started: Instant,
) -> Outcome {
	let root = &layout.root;
	let base = "6d7c1a2b-3c4d-4e5f-8a9b-0c1d2e3f4a5b";
	let auth = session(PROD_USER, base, |n| format!("{}-{n}", MARKERS[n.min(4)]));
	let args = serve_args(&layout.state, false);
	let mut helper = Helper::spawn(exe, &args, out, 1, log, started)?;
	let hello = helper.recv(REPLY);
	report.check(
		"P1-hello",
		hello
			.as_ref()
			.is_some_and(|line| line["t"] == "hello" && line["protocol"] == 1),
		format!("{hello:?}"),
	);
	let reply = helper.request(register(root, &layout.data, PROD_USER));
	report.check(
		"P2-register",
		value(reply.as_ref()) == json!({ "state": "registered", "resumed": false }),
		format!("{reply:?}"),
	);
	let listed = helper.request(json!({ "t": "listRoots" }));
	report.check("P3-listRoots", ok(listed.as_ref()), format!("{listed:?}"));
	let bad = helper.request(json!({ "t": "setAuth", "userId": PROD_USER, "json": "{}" }));
	report.check(
		"P4-setAuth-malformed",
		code(bad.as_ref()).as_deref() == Some("payloadInvalid")
			|| code(bad.as_ref()).as_deref() == Some("invalidMessage"),
		format!("{bad:?}"),
	);
	let reply = helper.request(json!({ "t": "setAuth", "userId": PROD_USER, "json": auth }));
	report.info("P5-setAuth-made-up", format!("{reply:?}"));
	let state = helper.request(json!({ "t": "getState" }));
	report.info("P5-getState", format!("{state:?}"));
	let listing = user("list", root, None);
	report.info("P5-user-list", format!("{listing}"));
	// Long enough for the revocation probe (30 s) and the pending poll to run.
	let revoked = helper.event(Duration::from_secs(45), |line| line["t"] == "authRevoked");
	report.info("P5-authRevoked", format!("{revoked:?}"));
	let reply = helper.request(json!({ "t": "clearAuth", "userId": PROD_USER }));
	report.check("P6-clearAuth", ok(reply.as_ref()), format!("{reply:?}"));
	let reply =
		helper.request(json!({ "t": "unregister", "userId": PROD_USER, "deleteHydrated": true }));
	report.check(
		"P7-unregister",
		value(reply.as_ref())["unregistered"].is_boolean(),
		format!("{reply:?}"),
	);
	report.check("P7-root-gone", !root.exists(), root.display());
	let reply = helper.request(json!({ "t": "shutdown" }));
	let exit = helper.exit_code(Duration::from_secs(60));
	report.check(
		"P8-shutdown",
		ok(reply.as_ref()) && exit == Some(0),
		format!("exit {exit:?}"),
	);

	// EOF, and a second instance beside it.
	let mut helper = Helper::spawn(exe, &args, out, 2, log, started)?;
	let _ = helper.recv(REPLY);
	let mut second = Helper::spawn(exe, &args, out, 3, log, started)?;
	let fatal = second.recv(REPLY);
	let exit = second.exit_code(Duration::from_secs(60));
	report.check(
		"P10-second-instance",
		exit == Some(12)
			&& fatal
				.as_ref()
				.is_some_and(|line| line["code"] == "alreadyRunning"),
		format!("exit {exit:?} line {fatal:?}"),
	);
	helper.close_stdin();
	let exit = helper.exit_code(Duration::from_secs(60));
	report.check("P9-eof-exit-0", exit == Some(0), format!("exit {exit:?}"));
	let usage = Command::new(exe).output();
	report.check(
		"P11-usage",
		usage
			.as_ref()
			.is_ok_and(|output| output.status.code() == Some(2)),
		format!(
			"{:?}",
			usage.map(|output| (
				output.status.code(),
				String::from_utf8_lossy(&output.stdout).into_owned()
			))
		),
	);

	// Nothing the made-up session holds appears in any line or log.
	let mut leaked = Vec::new();
	for name in [
		"helper-events.jsonl",
		"helper-1.stderr",
		"helper-2.stderr",
		"helper-3.stderr",
	] {
		let text = fs::read_to_string(out.join(name)).unwrap_or_default();
		for marker in MARKERS {
			if text.contains(marker) {
				leaked.push(format!("{marker} in {name}"));
			}
		}
	}
	report.check("P12-no-secret", leaked.is_empty(), format!("{leaked:?}"));
	Ok(true)
}

/// Leaves a served root registered, its session sealed, for `--unregister-all`.
fn leave(
	exe: &Path,
	out: &Path,
	layout: &Layout,
	report: &mut Report,
	log: &Arc<Mutex<File>>,
	started: Instant,
) -> Outcome {
	let root = &layout.root;
	let auth = session(USER, &tree::ROOT.to_string(), |n| {
		format!("{}-{n}", MARKERS[n.min(4)])
	});
	let mut helper = Helper::spawn(exe, &serve_args(&layout.state, true), out, 1, log, started)?;
	let _ = helper.recv(REPLY);
	let reply = helper.request(register(root, &layout.data, USER));
	report.check("L-register", ok(reply.as_ref()), format!("{reply:?}"));
	let reply = helper.request(json!({ "t": "setAuth", "userId": USER, "json": auth }));
	report.check("L-setAuth", ok(reply.as_ref()), format!("{reply:?}"));
	let read = user("read", &root.join("small.txt"), None);
	report.check("L-read", read == expected("small.txt"), format!("{read}"));
	let reply = helper.request(json!({ "t": "shutdown" }));
	let exit = helper.exit_code(Duration::from_secs(120));
	report.check(
		"L-shutdown",
		ok(reply.as_ref()) && exit == Some(0),
		format!("exit {exit:?}"),
	);
	Ok(true)
}

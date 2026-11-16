//! The provider: registers a fresh sync root, serves the tree through the real Windows layer until
//! told to stop, then shuts down in the documented order and unregisters, logging each lifecycle
//! result. With local changes on, it also runs the local engine and the root's watcher, so what a
//! user does in the root reaches the fake drive, whose operations the summary lists, and what
//! changes on the fake drive reaches the root.

use std::{
	cell::Cell,
	fs, mem,
	panic::{self, AssertUnwindSafe},
	path::{Path, PathBuf},
	process,
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	},
	thread,
	time::{Duration, Instant},
};

use filen_cloud_files::{
	CloudFilesCallback, ConnectedRoot, DispatchConfig, Dispatcher, LocalConfig, LocalEngine,
	LocalEntry, LocalError, LocalFs, LocalKind, LocalReport, LocalWatcher, PlaceholderIdentity,
	ProviderInfo, RegisteredRoot, RootPath, SkippedItem, SyncRootPath, WinLocalFs, platform_info,
	register, unregister_path,
};
use filen_types::fs::Uuid;
use serde_json::{Map, Value, json};
use tracing::{debug, info, warn};

use filen_cloud_files::test_support::FakeDrive;

use crate::{events, report::Outcome, stall, tree};

/// The GUID of the harness's provider, so its registrations never collide with Filen's.
const HARNESS_ID: u128 = 0xDFDD_B98D_C333_4FDC_BE62_8657_AD1F_363A;

/// Logs every skip report as an event the readers look for.
struct Skips;

impl CloudFilesCallback for Skips {
	fn on_skipped_items(&self, directory: PlaceholderIdentity, items: &[SkippedItem]) {
		for item in items {
			info!(
				target: "vm_harness",
				?directory,
				id = item.id(),
				name = item.name(),
				reason = ?item.reason(),
				"on_skipped_items"
			);
		}
	}

	fn on_local_report(&self, report: &LocalReport) {
		info!(target: "vm_harness", ?report, "on_local_report");
	}
}

/// The lifecycle results, for the summary.
#[derive(Default)]
struct Lifecycle {
	results: Map<String, Value>,
	ok: bool,
}

impl Lifecycle {
	/// Records `step`'s result; a failure fails the run when `required`.
	fn record<E: std::fmt::Display>(&mut self, step: &str, result: Result<(), E>, required: bool) {
		let value = match &result {
			Ok(()) => "ok".to_owned(),
			Err(error) => error.to_string(),
		};
		match &result {
			Ok(()) => info!(target: "vm_harness", lifecycle = step, result = %value, "lifecycle"),
			Err(_) => warn!(target: "vm_harness", lifecycle = step, result = %value, "lifecycle"),
		}
		if required && result.is_err() {
			self.ok = false;
		}
		self.results.insert(step.to_owned(), Value::String(value));
	}
}

type Engine = LocalEngine<FakeDrive, WinLocalFs>;

/// What the run serves the root with.
struct Context {
	root: PathBuf,
	drive: Arc<FakeDrive>,
	local_fs: Arc<WinLocalFs>,
	runtime: tokio::runtime::Runtime,
	local: bool,
	/// Where the engine keeps what it needs across a restart, as the helper keeps it in the root's
	/// data directory.
	records: PathBuf,
	/// Whether the drive is offline, as `offline.seen` last said.
	offline_seen: Cell<Option<bool>>,
}

/// The root while it is connected, and what serves it.
struct Serving {
	dispatcher: Arc<Dispatcher<FakeDrive>>,
	engine: Arc<Engine>,
	watcher: Option<LocalWatcher>,
	connected: ConnectedRoot,
}

/// The root as the run holds it.
enum Root {
	/// Connected and served.
	Serving(Serving),
	/// Disconnected but still registered, as while the provider is not running: `pause`.
	Paused(RegisteredRoot),
	/// Lost: a reconnect or a disconnect failed.
	Lost,
}

impl Context {
	/// Connects `registered` with a new dispatcher and a new local engine and, with local changes
	/// on, a watcher, as a provider starting up does; the root back when cfapi refused it. Without
	/// the watcher the engine still answers the platform's notifications and completes and checks
	/// what readers hydrate, and no local change reaches the drive.
	fn connect(
		&self,
		registered: RegisteredRoot,
		lifecycle: &mut Lifecycle,
		step: &str,
	) -> Result<Serving, RegisteredRoot> {
		let engine = Arc::new(LocalEngine::new_keeping_records(
			Arc::clone(&self.drive),
			Arc::clone(&self.local_fs),
			self.runtime.handle().clone(),
			LocalConfig::default(),
			Arc::new(Skips),
			self.records.clone(),
		));
		let dispatcher = Arc::new(
			Dispatcher::new(
				Arc::clone(&self.drive),
				self.runtime.handle().clone(),
				DispatchConfig::default(),
				Arc::new(Skips),
			)
			.with_hints(engine.hints()),
		);
		let connecting =
			registered.connect_with_local(Arc::clone(&dispatcher), Arc::clone(&engine));
		let connected = match connecting {
			Ok(connected) => {
				lifecycle.record::<String>(step, Ok(()), true);
				connected
			}
			Err(failed) => {
				lifecycle.record(step, Err(&failed.error), true);
				return Err(failed.root);
			}
		};
		engine.start();
		let watcher = if self.local {
			let watching = LocalWatcher::start(&self.root, Arc::clone(&engine));
			lifecycle.record(
				"watch",
				watching.as_ref().map(|_| ()).map_err(ToString::to_string),
				true,
			);
			watching.ok()
		} else {
			None
		};
		Ok(Serving {
			dispatcher,
			engine,
			watcher,
			connected,
		})
	}

	/// Stops serving in the documented order and disconnects: the root back, still registered.
	fn disconnect(
		&self,
		serving: Serving,
		lifecycle: &mut Lifecycle,
		step: &str,
	) -> Option<RegisteredRoot> {
		drop(serving.watcher);
		self.runtime.block_on(serving.engine.shutdown());
		self.runtime.block_on(serving.dispatcher.shutdown());
		match serving.connected.disconnect() {
			Ok(registered) => {
				lifecycle.record::<String>(step, Ok(()), true);
				Some(registered)
			}
			Err(failed) => {
				lifecycle.record(step, Err(&failed.error), true);
				None
			}
		}
	}
}

/// Serves `root` for `secs` or until `<out>\\stop` exists; with `local`, carries local changes to
/// the fake drive too, and applies the drive's changes to the root.
pub(crate) fn run(root: &Path, out: &Path, secs: u64, local: bool) -> Outcome {
	fs::create_dir_all(out)?;
	events::install(&out.join("events.jsonl"))?;
	let mut lifecycle = Lifecycle {
		ok: true,
		..Lifecycle::default()
	};
	let runtime = tokio::runtime::Builder::new_multi_thread()
		.worker_threads(4)
		.enable_all()
		.build()?;
	info!(target: "vm_harness", unix_ms = %stall::unix_ms(), "clock");
	stall::watch_thread(true);
	stall::watch_runtime(runtime.handle());
	let path = SyncRootPath::from_path(root)?;
	let local_fs = Arc::new(WinLocalFs::new(&path));
	// A root a crashed run left behind; failing is the normal case.
	lifecycle.record("unregister_stale", unregister_path(&path), false);
	if root.exists() {
		fs::remove_dir_all(root)?;
	}
	fs::create_dir_all(root)?;
	let records = out.join("records");
	fs::create_dir_all(&records)?;
	match platform_info() {
		Ok(platform) => info!(target: "vm_harness", ?platform, "platform"),
		Err(error) => lifecycle.record("platform_info", Err(error), true),
	}
	let provider = ProviderInfo::new("FilenHarness", "0.1", HARNESS_ID)?;
	let registered = match register(path, &provider, tree::ROOT) {
		Ok(registered) => {
			lifecycle.record::<String>("register", Ok(()), true);
			registered
		}
		Err(failed) => {
			lifecycle.record("register", Err(&failed.error), true);
			return Err(failed.into());
		}
	};
	let context = Context {
		root: root.to_path_buf(),
		drive: Arc::new(tree::build()),
		local_fs,
		runtime,
		local,
		records,
		offline_seen: Cell::new(None),
	};
	let mut state = match context.connect(registered, &mut lifecycle, "connect") {
		Ok(serving) => Root::Serving(serving),
		Err(registered) => {
			let unregistered = registered.unregister().map_err(|failed| failed.error);
			lifecycle.record("unregister", unregistered, true);
			return Ok(false);
		}
	};
	// The checks tell the provider's own requests (its completions and checks) from a user's by it.
	info!(target: "vm_harness", pid = process::id(), "provider pid");
	events::flush();
	fs::write(out.join("provider.ready"), "ready")?;

	let stop = out.join("stop");
	let deadline = Instant::now() + Duration::from_secs(secs);
	// The dump runs beside the control commands: a loaded VM's disk held one dump up for 47 s
	// (measured), and no command waits for that.
	let dumping = Arc::new(AtomicBool::new(local));
	let dumper = local.then(|| {
		let (drive, out, dumping) = (
			Arc::clone(&context.drive),
			out.to_path_buf(),
			Arc::clone(&dumping),
		);
		thread::spawn(move || {
			while dumping.load(Ordering::Relaxed) {
				let started = Instant::now();
				dump(&drive, &out);
				let took = started.elapsed();
				if took >= Duration::from_secs(1) {
					info!(target: "vm_harness", dump_ms = %took.as_millis(), "slow dump");
				}
				thread::sleep(Duration::from_secs(1).saturating_sub(took));
			}
		})
	});
	while !stop.exists() && Instant::now() < deadline {
		thread::sleep(Duration::from_millis(250));
		if local {
			let started = Instant::now();
			control(&context, &mut state, &mut lifecycle, out);
			let took = started.elapsed();
			if took >= Duration::from_secs(1) {
				info!(target: "vm_harness", control_ms = %took.as_millis(), "slow poll");
			}
		}
	}
	info!(target: "vm_harness", stopped = stop.exists(), "stopping");
	dumping.store(false, Ordering::Relaxed);
	if let Some(dumper) = dumper
		&& dumper.join().is_err()
	{
		warn!(target: "vm_harness", "the dump thread panicked");
	}

	let drive = &context.drive;
	let registered = match state {
		Root::Serving(serving) => context.disconnect(serving, &mut lifecycle, "disconnect"),
		Root::Paused(registered) => Some(registered),
		Root::Lost => None,
	};
	let summary = json!({
		"reads": drive.reads().len(),
		"max_concurrent_reads": drive.max_concurrent_reads(),
		"ops": drive.writes().iter().map(|op| format!("{op:?}")).collect::<Vec<_>>(),
		"lifecycle": lifecycle.results,
	});
	fs::write(out.join("provider-summary.json"), summary.to_string())?;
	if local {
		dump(drive, out);
	}
	events::flush();
	fs::write(out.join("provider.disconnected"), "disconnected")?;

	let unregistered = match registered {
		Some(registered) => registered
			.unregister()
			.map_err(|failed| failed.error.to_string()),
		None => Err("not disconnected".to_owned()),
	};
	let marker = match &unregistered {
		Ok(()) => "ok".to_owned(),
		Err(error) => error.clone(),
	};
	lifecycle.record("unregister", unregistered, true);
	let summary = json!({
		"reads": drive.reads().len(),
		"max_concurrent_reads": drive.max_concurrent_reads(),
		"ops": drive.writes().iter().map(|op| format!("{op:?}")).collect::<Vec<_>>(),
		"lifecycle": lifecycle.results,
	});
	fs::write(out.join("provider-summary.json"), summary.to_string())?;
	events::flush();
	fs::write(out.join("provider.unregistered"), marker)?;
	Ok(lifecycle.ok)
}

/// Applies what the scenarios ask of the drive and the provider through files in `out`:
/// `offline`, while it exists, makes every drive call fail as the server unreachable, and once the
/// drive is switched `offline.seen` says `offline` or `online`; each
/// `ctl-<n>.txt` is a command, answered by `ctl-<n>.done` holding `ok ...` or `err ...`:
/// - `bump <path>`: gives the tree's file `path` a new version of the same size, `bumped` bytes;
/// - `add <dir> <name> <size>`: lists a new file `name` of `size` `added` bytes in the tree's
///   directory `dir` (`-` for the root), as an upload from elsewhere would; answers its uuid;
/// - `adddir <dir> <name>`: lists a new directory there; answers its uuid;
/// - `quiet on|off`: another device's changes stop or start telling the provider; `wipe`: the
///   cache becomes a new incarnation, refusing the provider's anchor as expired;
/// - `rename <uuid> <name>`, `move <uuid> <dir>`, `trash <uuid>`, `restore <uuid>` and
///   `delete <uuid>` (for good): another device's change of the item `uuid`;
/// - `inspect <path>`: what the core reads at `path` below the root, as JSON. Asked of the
///   provider because another process sees a hydrated placeholder as an ordinary file (measured:
///   its reparse attribute is hidden from it);
/// - `list <path>`: the names the provider's own listing of the directory `path` returns, which
///   neither populates nor hydrates (measured);
/// - `bump2 <path>`: as `bump`, with bytes neither the tree's nor `bump`'s, `added` bytes;
/// - `mark <path>`: marks the bytes of the file at `path` below the root as an edit of the tree's
///   file there the cache holds unsent, as an upload killed on its way leaves it;
/// - `pending`: the engine's count of items the drive does not have yet after a walk of the root,
///   as the helper's `getState` answers it, `null` when unknown;
/// - `pause` and `resume`: stop serving and disconnect the root, keeping it registered, as a
///   provider that is not running leaves it; then connect it again with a new engine, keeping the
///   records of the one before, as a provider starting up, which reconciles the root with the
///   drive.
fn control(context: &Context, state: &mut Root, lifecycle: &mut Lifecycle, out: &Path) {
	let offline = out.join("offline").exists();
	context.drive.set_offline(offline);
	if context.offline_seen.replace(Some(offline)) != Some(offline) {
		let seen = if offline { "offline" } else { "online" };
		let temp = out.join("offline.tmp");
		let said =
			fs::write(&temp, seen).and_then(|()| fs::rename(&temp, out.join("offline.seen")));
		if let Err(error) = said {
			warn!(target: "vm_harness", %error, "saying the drive is {seen} failed");
			context.offline_seen.set(None);
		}
	}
	let Ok(entries) = fs::read_dir(out) else {
		return;
	};
	let mut commands: Vec<PathBuf> = entries
		.filter_map(Result::ok)
		.map(|entry| entry.path())
		.filter(|path| {
			path.file_name()
				.and_then(|name| name.to_str())
				.is_some_and(|name| name.starts_with("ctl-") && name.ends_with(".txt"))
				&& !path.with_extension("done").exists()
		})
		.collect();
	commands.sort();
	for command in commands {
		let text = fs::read_to_string(&command).unwrap_or_default();
		let answer = match text.trim() {
			"pause" => pause(context, state, lifecycle),
			"resume" => resume(context, state, lifecycle),
			"pending" => match state {
				// As the helper's `getState` counts: after a walk of the root.
				Root::Serving(serving) => {
					let count = context.runtime.block_on(
						serving
							.engine
							.unsynced_count_after_walk(Duration::from_secs(4)),
					);
					format!("ok {}", json!(count))
				}
				_ => "err not serving".to_owned(),
			},
			text => {
				// A drive helper asserts what it is given; a bad command must not end the run.
				panic::catch_unwind(AssertUnwindSafe(|| run_command(context, text)))
					.unwrap_or_else(|_| format!("err the command {text:?} panicked"))
			}
		};
		debug!(target: "vm_harness", command = text.trim(), %answer, "control");
		let temp = command.with_extension("ans");
		let answered = fs::write(&temp, answer)
			.and_then(|()| fs::rename(&temp, command.with_extension("done")));
		if let Err(error) = answered {
			warn!(target: "vm_harness", %error, "answering a control command failed");
		}
	}
}

fn pause(context: &Context, state: &mut Root, lifecycle: &mut Lifecycle) -> String {
	match mem::replace(state, Root::Lost) {
		Root::Serving(serving) => match context.disconnect(serving, lifecycle, "pause") {
			Some(registered) => {
				*state = Root::Paused(registered);
				"ok paused".to_owned()
			}
			None => "err the disconnect failed".to_owned(),
		},
		other => {
			*state = other;
			"err not serving".to_owned()
		}
	}
}

fn resume(context: &Context, state: &mut Root, lifecycle: &mut Lifecycle) -> String {
	match mem::replace(state, Root::Lost) {
		Root::Paused(registered) => match context.connect(registered, lifecycle, "resume") {
			Ok(serving) => {
				*state = Root::Serving(serving);
				"ok resumed".to_owned()
			}
			Err(registered) => {
				*state = Root::Paused(registered);
				"err the connect failed".to_owned()
			}
		},
		other => {
			*state = other;
			"err not paused".to_owned()
		}
	}
}

fn run_command(context: &Context, command: &str) -> String {
	let (drive, local_fs) = (&context.drive, &context.local_fs);
	if let Some(path) = command.strip_prefix("inspect ") {
		return match RootPath::from_relative(Path::new(path)) {
			Ok(at) => format!("ok {}", entry_json(local_fs.inspect(&at))),
			Err(error) => format!("err {error}"),
		};
	}
	if command == "placeholders" {
		return format!("ok {}", placeholders(local_fs));
	}
	if let Some(path) = command.strip_prefix("list ") {
		let at = if path == "-" {
			Ok(RootPath::root())
		} else {
			RootPath::from_relative(Path::new(path))
		};
		return match at.map(|at| local_fs.list_dir(&at)) {
			Ok(Ok(children)) => {
				let names: Vec<String> = children
					.iter()
					.map(|child| match child {
						Ok(path) => path
							.name()
							.map_or_else(String::new, |name| name.as_str().to_owned()),
						Err(error) => format!("<{error}>"),
					})
					.collect();
				format!("ok {}", json!(names))
			}
			Ok(Err(error)) => format!("err {error}"),
			Err(error) => format!("err {error}"),
		};
	}
	let words: Vec<&str> = command.split(' ').collect();
	let dir_of = |dir: &str| tree::dir_id(if dir == "-" { "" } else { dir });
	match words.as_slice() {
		["bump", path] => match (tree::stable_of(path), tree::size_of(path)) {
			(Some(stable), Some(size)) => {
				let version = drive.bump_head(stable, &tree::bumped(size));
				format!("ok {version}")
			}
			_ => format!("err no file {path}"),
		},
		["bump2", path] => match (tree::stable_of(path), tree::size_of(path)) {
			(Some(stable), Some(size)) => {
				let version = drive.bump_head(stable, &tree::added(size));
				format!("ok {version}")
			}
			_ => format!("err no file {path}"),
		},
		["mark", path] => match (tree::stable_of(path), fs::read(context.root.join(path))) {
			(Some(stable), Ok(bytes)) => {
				drive.mark_pending(stable, &bytes);
				format!("ok {}", bytes.len())
			}
			(None, _) => format!("err no file {path}"),
			(_, Err(error)) => format!("err reading {path}: {error}"),
		},
		["add", dir, name, size] => match (dir_of(dir), size.parse::<usize>()) {
			(Some(id), Ok(size)) => {
				let file = drive.add_remote_file(&id, name, &tree::added(size));
				format!("ok {}", file.stable_uuid)
			}
			_ => format!("err no directory {dir} or size {size}"),
		},
		["quiet", switch @ ("on" | "off")] => {
			drive.quiet_signals(*switch == "on");
			"ok quiet".to_owned()
		}
		["wipe"] => {
			drive.wipe_feed();
			"ok wiped".to_owned()
		}
		["adddir", dir, name] => match dir_of(dir) {
			Some(id) => format!("ok {}", drive.add_remote_dir(&id, name).uuid),
			None => format!("err no directory {dir}"),
		},
		[
			verb @ ("rename" | "move" | "trash" | "restore" | "delete"),
			uuid,
			rest @ ..,
		] => {
			let Ok(uuid) = Uuid::parse_str(uuid) else {
				return format!("err not a uuid {uuid}");
			};
			match (*verb, rest) {
				("rename", [name]) => drive.remote_rename(uuid, name),
				("move", [dir]) => match dir_of(dir) {
					Some(id) => drive.remote_move(uuid, &id),
					None => return format!("err no directory {dir}"),
				},
				("trash", []) => drive.remote_trash(uuid),
				("restore", []) => drive.remote_restore(uuid),
				("delete", []) => drive.remote_delete(uuid),
				_ => return format!("err bad arguments {command:?}"),
			}
			"ok done".to_owned()
		}
		_ => format!("err unknown command {command:?}"),
	}
}

/// Every placeholder in the root, read in the provider's process, as JSON: each remote item's
/// stable id or uuid with the paths of its placeholders, and `unreadable`, the paths that could not
/// be listed or read. Listing a directory never populated raises no callback in the provider's
/// own process (fifth round), so the walk changes nothing.
fn placeholders(local_fs: &WinLocalFs) -> Value {
	let mut found: Map<String, Value> = Map::new();
	let mut unreadable = Vec::new();
	let mut dirs = vec![RootPath::root()];
	while let Some(dir) = dirs.pop() {
		let children = match local_fs.list_dir(&dir) {
			Ok(children) => children,
			Err(error) => {
				unreadable.push(format!("{dir}: {error}"));
				continue;
			}
		};
		for child in children {
			let Ok(path) = child else {
				unreadable.push(format!("{dir}: an unrepresentable name"));
				continue;
			};
			let uuid = match local_fs.inspect(&path) {
				Ok(LocalEntry::Placeholder {
					kind,
					identity: Ok(identity),
					..
				}) => {
					if matches!(kind, LocalKind::Dir { .. }) {
						dirs.push(path.clone());
					}
					match identity {
						PlaceholderIdentity::File { stable, .. } => Some(stable),
						PlaceholderIdentity::Dir { uuid } => Some(uuid),
						PlaceholderIdentity::Root { .. } | PlaceholderIdentity::Detached { .. } => {
							None
						}
					}
				}
				Ok(LocalEntry::Plain {
					kind: LocalKind::Dir { .. },
					..
				}) => {
					dirs.push(path.clone());
					None
				}
				Ok(_) => None,
				Err(error) => {
					unreadable.push(format!("{path}: {error}"));
					None
				}
			};
			if let Some(uuid) = uuid {
				let paths = found
					.entry(uuid.to_string())
					.or_insert_with(|| Value::Array(Vec::new()));
				if let Value::Array(paths) = paths {
					paths.push(json!(path.to_string()));
				}
			}
		}
	}
	json!({ "items": found, "unreadable": unreadable })
}

/// What the core read at a path, as JSON: `kind` is `missing`, `plain`, `placeholder`,
/// `unsupported` or `error`.
fn entry_json(entry: Result<LocalEntry, LocalError>) -> Value {
	let kind_json = |kind: &LocalKind| match kind {
		LocalKind::File(stamp) => json!({ "dir": false, "size": stamp.size }),
		LocalKind::Dir { .. } => json!({ "dir": true }),
	};
	match entry {
		Ok(LocalEntry::Missing) => json!({ "kind": "missing" }),
		Ok(LocalEntry::Plain { kind, .. }) => {
			let mut value = kind_json(&kind);
			value["kind"] = json!("plain");
			value
		}
		Ok(LocalEntry::Placeholder {
			kind,
			identity,
			state,
			..
		}) => {
			let mut value = kind_json(&kind);
			value["kind"] = json!("placeholder");
			value["in_sync"] = json!(state.in_sync());
			value["partial"] = json!(state.partial());
			value["on_disk"] = json!(state.on_disk());
			value["modified"] = json!(state.modified_bytes());
			value["pin"] = json!(format!("{:?}", state.pin()));
			value["identity"] = match identity {
				Ok(PlaceholderIdentity::File { stable, version }) => json!({
					"kind": "file",
					"stable": stable.to_string(),
					"version": version.to_string(),
				}),
				Ok(PlaceholderIdentity::Dir { uuid }) => {
					json!({ "kind": "dir", "uuid": uuid.to_string() })
				}
				Ok(other) => json!({ "kind": format!("{other:?}") }),
				Err(error) => json!({ "kind": "error", "error": error.to_string() }),
			};
			value
		}
		Ok(LocalEntry::Unsupported(kind)) => {
			json!({ "kind": "unsupported", "what": format!("{kind:?}") })
		}
		Err(error) => json!({ "kind": "error", "error": error.to_string() }),
	}
}

/// Writes the drive's [`FakeDrive::dump`] to `<out>\\remote.json`, through a rename so a reader
/// never sees half a file.
fn dump(drive: &FakeDrive, out: &Path) {
	let temp = out.join("remote.json.tmp");
	let written = fs::write(&temp, drive.dump().to_string())
		.and_then(|()| fs::rename(&temp, out.join("remote.json")));
	if let Err(error) = written {
		warn!(target: "vm_harness", %error, "writing the drive's dump failed");
	}
}

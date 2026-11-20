//! No line the helper writes and no log it makes shows a pushed session's secrets, and no log at
//! INFO, WARN or ERROR shows a file or folder name. Its own test binary, so its logger is the
//! process's global one and catches every thread's logs, the blocking pool's included.

use std::{
	io::Write,
	sync::{Arc, Mutex, OnceLock},
	time::Duration,
};

use filen_cloud_files::{
	CloudFilesCallback, HelperConfig, LocalConfig, LocalEngine, LocalError, LocalReport, OsError,
	PinAttr, PlaceholderIdentity, Platform, RootPath, SkippedItem, StateDir, run_helper,
	test_support::{DriveCall, FakeDrive, FakeLocal, FakeOpener, FakePlatform, pattern, root_path},
};
use filen_mobile_native_cache::{CacheError, ffi::FfiFile};
use filen_types::fs::Uuid;
use serde_json::{Value, json};
use tokio::{
	io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream},
	runtime::Handle,
	time::{sleep, timeout},
};

const ACCOUNT_ROOT: Uuid = Uuid::from_u128(0x100);

/// What every item the name test makes is called: no log at INFO or WARN may show it.
const NAME_MARK: &str = "Quill";

/// A session for `user`, every secret a marker.
fn session(user: &str) -> String {
	json!({
		"providerEnabled": true,
		"sdkConfig": {
			"email": "marker-email@example.invalid",
			"password": "marker-password",
			"twoFactorCode": "marker-2fa",
			"masterKeys": ["marker-master-key"],
			"apiKey": "marker-api-key",
			"publicKey": "marker-public-key",
			"privateKey": "marker-private-key",
			"authVersion": 2,
			"baseFolderUUID": ACCOUNT_ROOT.to_string(),
			"userId": user,
			"metadataCache": false,
			"tmpPath": "marker-tmp-path",
			"connectToSocket": false
		}
	})
	.to_string()
}

/// A writer into a shared buffer.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
	fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
		self.0.lock().unwrap().extend_from_slice(bytes);
		Ok(bytes.len())
	}

	fn flush(&mut self) -> std::io::Result<()> {
		Ok(())
	}
}

impl Capture {
	/// Everything logged so far, by every test of the binary.
	fn text(&self) -> String {
		String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
	}

	/// The lines logged so far at a level of `levels`. A line that does not start with a level
	/// continues the event before it.
	fn at(&self, levels: &[&str]) -> Vec<String> {
		let mut level = "";
		let mut lines = Vec::new();
		for line in self.text().lines() {
			if let Some(first) = line.split_whitespace().next()
				&& ["TRACE", "DEBUG", "INFO", "WARN", "ERROR"].contains(&first)
			{
				level = first;
			}
			if levels.contains(&level) {
				lines.push(line.to_owned());
			}
		}
		lines
	}
}

/// The process's logger, installed once for every test of the binary: every level, each line
/// starting with its level.
fn capture() -> &'static Capture {
	static CAPTURE: OnceLock<Capture> = OnceLock::new();
	CAPTURE.get_or_init(|| {
		let capture = Capture::default();
		let writer = capture.clone();
		tracing_subscriber::fmt()
			.with_max_level(tracing::Level::TRACE)
			.with_ansi(false)
			.without_time()
			.with_writer(move || writer.clone())
			.init();
		capture
	})
}

/// Reads every line the helper writes until it closes its output.
async fn read_all(output: DuplexStream) -> Vec<Value> {
	let mut output = BufReader::new(output);
	let mut lines = Vec::new();
	let mut line = String::new();
	while timeout(Duration::from_secs(3600), output.read_line(&mut line))
		.await
		.unwrap()
		.unwrap()
		> 0
	{
		lines.push(serde_json::from_str::<Value>(&line).unwrap());
		line.clear();
	}
	lines
}

#[tokio::test(start_paused = true)]
async fn no_line_and_no_log_shows_a_secret() {
	let capture = capture();
	let temp = tempfile::tempdir().unwrap();
	let local = FakeLocal::new(PlaceholderIdentity::Root { root: ACCOUNT_ROOT });
	let drive = Arc::new(FakeDrive::new(ACCOUNT_ROOT));
	drive.list(&ACCOUNT_ROOT.to_string(), Vec::new());
	let platform = FakePlatform::new(local);
	let (mut input, helper_input) = tokio::io::duplex(1 << 22);
	let (helper_output, output) = tokio::io::duplex(1 << 22);
	let security = platform.secure_thumbnails();
	let helper = tokio::spawn(run_helper(
		platform,
		security,
		FakeOpener::new(Arc::clone(&drive)),
		HelperConfig::new(StateDir::new(temp.path().join("state")).unwrap()),
		helper_input,
		helper_output,
	));
	let requests = [
		json!({
			"t": "register", "id": 1, "userId": "42",
			"root": temp.path().join("Filen").to_str().unwrap(),
			"dataDir": temp.path().join("data").to_str().unwrap(),
		})
		.to_string(),
		json!({ "t": "setAuth", "id": 2, "userId": "42", "json": session("42") }).to_string(),
		// Another user's session, refused.
		json!({ "t": "setAuth", "id": 3, "userId": "42", "json": session("43") }).to_string(),
		// The session as an object where a string belongs, refused by the schema.
		format!(
			r#"{{"t":"setAuth","id":4,"userId":"42","json":{}}}"#,
			session("42")
		),
		// A secret in an unknown field.
		json!({ "t": "clearAuth", "id": 5, "userId": "42", "password": "marker-field" })
			.to_string(),
		json!({ "t": "clearAuth", "id": 6, "userId": "42" }).to_string(),
	];
	for request in requests {
		input.write_all(request.as_bytes()).await.unwrap();
		input.write_all(b"\n").await.unwrap();
	}
	drop(input);
	let lines = read_all(output).await;
	helper.await.unwrap();
	let replies: Vec<(u64, bool)> = lines
		.iter()
		.filter(|line| line["t"] == "reply")
		.map(|line| (line["id"].as_u64().unwrap(), line["ok"].as_bool().unwrap()))
		.collect();
	assert_eq!(
		replies,
		[
			(1, true),
			(2, true),
			(3, false),
			(4, false),
			(5, false),
			(6, true)
		]
	);
	let logs = capture.text();
	assert!(logs.contains("request refused"), "{logs}");
	for text in lines.iter().map(Value::to_string).chain([logs]) {
		assert!(!text.contains("marker-"), "{text}");
	}
}

/// Records every local report.
#[derive(Default)]
struct Reports(Mutex<Vec<LocalReport>>);

impl CloudFilesCallback for Reports {
	fn on_skipped_items(&self, _directory: PlaceholderIdentity, _items: &[SkippedItem]) {}

	fn on_local_report(&self, report: &LocalReport) {
		self.0.lock().unwrap().push(report.clone());
	}
}

/// The placeholder identity of the remote file `file`.
fn identity_of(file: &FfiFile) -> PlaceholderIdentity {
	PlaceholderIdentity::File {
		stable: Uuid::parse_str(&file.stable_uuid).unwrap(),
		version: Uuid::parse_str(&file.uuid).unwrap(),
	}
}

/// Lets `span` pass on the paused clock, handing `engine` the watcher's events every second.
async fn run(engine: &LocalEngine<FakeDrive, FakeLocal>, local: &FakeLocal, span: Duration) {
	for _ in 0..span.as_secs() {
		for event in local.take_watch_events() {
			engine.on_local_event(event);
		}
		sleep(Duration::from_secs(1)).await;
	}
}

/// Runs the local engine over an upload, a hydration, a rename, a delete, a placement the OS
/// refuses, an item the walk cannot read and a parked upload, each of an item whose name holds
/// [`NAME_MARK`].
async fn run_the_engine() {
	let root = ACCOUNT_ROOT.to_string();
	let drive = Arc::new(FakeDrive::new(ACCOUNT_ROOT));
	drive.list(&root, Vec::new());
	let local = FakeLocal::new(PlaceholderIdentity::Root { root: ACCOUNT_ROOT });
	local.set_populated(&RootPath::root(), true);
	drive.link_local(&local);
	let source = Arc::clone(&drive);
	local.hydrate_from(move |stable| source.content(stable));
	let place = |name: &str, hydrated: bool| {
		let file = drive.add_remote_file(&root, name, &pattern(5000));
		local.place_file_with(
			&root_path(name),
			identity_of(&file),
			&pattern(5000),
			hydrated,
		);
	};
	place("QuillHydrated.bin", false);
	place("QuillRenamedFrom.txt", true);
	place("QuillDeleted.txt", true);
	place("QuillUnreadable.txt", true);
	local.fail_inspect(&root_path("QuillUnreadable.txt"), Some(LocalError::Busy));
	let folder = drive.add_remote_dir(&root, "QuillFolder");
	local.place_dir_unpopulated(
		&root_path("QuillFolder"),
		PlaceholderIdentity::Dir {
			uuid: Uuid::parse_str(&folder.uuid).unwrap(),
		},
	);
	drive.add_remote_file(
		&format!("stable/{}", folder.uuid),
		"QuillRefused.bin",
		&pattern(10),
	);
	local.fail_placements_named(
		"QuillRefused.bin",
		LocalError::Os(OsError::Rejected { code: -1 }),
	);
	let reports = Arc::new(Reports::default());
	let engine = LocalEngine::new(
		Arc::clone(&drive),
		Arc::new(local.clone()),
		Handle::current(),
		LocalConfig::default(),
		Arc::clone(&reports) as Arc<dyn CloudFilesCallback>,
	);
	engine.start();
	run(&engine, &local, Duration::from_secs(10)).await;

	local.user_create(&root_path("QuillUploaded.txt"), b"uploaded");
	local.user_pin(&root_path("QuillHydrated.bin"), PinAttr::Pinned);
	local.user_pin(&root_path("QuillFolder"), PinAttr::Pinned);
	let (request, responder) = local
		.user_rename(
			&root_path("QuillRenamedFrom.txt"),
			&root_path("QuillRenamedTo.txt"),
		)
		.unwrap()
		.asked();
	engine.on_notify_rename(request, responder);
	let (request, responder) = local.user_delete(&root_path("QuillDeleted.txt")).asked();
	engine.on_notify_delete(request, responder);
	run(&engine, &local, Duration::from_secs(60)).await;

	drive.fail_always(DriveCall::Upload, || {
		CacheError::FailedToDecrypt("QuillParked.txt".into())
	});
	local.user_create(&root_path("QuillParked.txt"), b"parked");
	run(&engine, &local, Duration::from_secs(60)).await;
	engine.shutdown().await;

	let reports = reports.0.lock().unwrap().clone();
	let reported = |wanted: &dyn Fn(&LocalReport) -> bool| reports.iter().any(wanted);
	assert!(
		reported(
			&|report| matches!(report, LocalReport::Uploaded { path, .. } if *path == root_path("QuillUploaded.txt"))
		),
		"{reports:?}"
	);
	assert!(
		reported(
			&|report| matches!(report, LocalReport::Moved { to, .. } if *to == root_path("QuillRenamedTo.txt"))
		),
		"{reports:?}"
	);
	assert!(
		reported(
			&|report| matches!(report, LocalReport::Trashed { path } if *path == root_path("QuillDeleted.txt"))
		),
		"{reports:?}"
	);
	assert!(
		reported(
			&|report| matches!(report, LocalReport::Parked { path, .. } if *path == root_path("QuillParked.txt"))
		),
		"{reports:?}"
	);
	assert_eq!(
		local
			.entry(&root_path("QuillHydrated.bin"))
			.map(|entry| entry.on_disk),
		Some(5000)
	);
}

/// Unregisters a served root through the helper while the edit it would keep cannot be fetched:
/// the unregister stops, naming the file in its reply.
async fn unregister_through_the_helper() {
	let temp = tempfile::tempdir().unwrap();
	let local = FakeLocal::new(PlaceholderIdentity::Root { root: ACCOUNT_ROOT });
	let drive = Arc::new(FakeDrive::new(ACCOUNT_ROOT));
	drive.list(&ACCOUNT_ROOT.to_string(), Vec::new());
	drive.link_local(&local);
	let platform = FakePlatform::new(local);
	let (mut input, helper_input) = tokio::io::duplex(1 << 22);
	let (helper_output, output) = tokio::io::duplex(1 << 22);
	let security = platform.secure_thumbnails();
	let helper = tokio::spawn(run_helper(
		platform.clone(),
		security,
		FakeOpener::new(Arc::clone(&drive)),
		HelperConfig::new(StateDir::new(temp.path().join("state")).unwrap()),
		helper_input,
		helper_output,
	));
	let edited = root_path("QuillKept.txt");
	let identity = PlaceholderIdentity::File {
		stable: Uuid::from_u128(3),
		version: Uuid::from_u128(1003),
	};
	platform.local().place_file(&edited, identity, b"old data");
	platform.local().user_write(&edited, b"new data").unwrap();
	platform.local().set_on_disk(&edited, 3);
	platform.local().fail_hydrations(Some(LocalError::Busy));
	let requests = [
		json!({
			"t": "register", "id": 1, "userId": "42",
			"root": temp.path().join("Filen").to_str().unwrap(),
			"dataDir": temp.path().join("data").to_str().unwrap(),
		}),
		json!({ "t": "setAuth", "id": 2, "userId": "42", "json": session("42") }),
		json!({ "t": "unregister", "id": 3, "userId": "42", "deleteHydrated": true }),
	];
	for request in requests {
		input
			.write_all(request.to_string().as_bytes())
			.await
			.unwrap();
		input.write_all(b"\n").await.unwrap();
	}
	drop(input);
	let lines = read_all(output).await;
	helper.await.unwrap();
	let unregistered = lines
		.iter()
		.find(|line| line["t"] == "reply" && line["id"] == 3)
		.unwrap();
	assert_eq!(
		unregistered["error"]["code"], "unsyncedChanges",
		"{unregistered}"
	);
	assert!(
		unregistered["error"]["message"]
			.as_str()
			.unwrap()
			.contains("QuillKept.txt"),
		"the reply names the file: {unregistered}"
	);
}

#[tokio::test(start_paused = true)]
async fn no_log_at_info_or_warn_shows_a_file_or_folder_name() {
	let capture = capture();
	run_the_engine().await;
	unregister_through_the_helper().await;
	let shown = capture.at(&["INFO", "WARN", "ERROR"]);
	assert!(
		shown.iter().any(|line| line.contains("hydration settled")),
		"the lines at INFO and WARN are read: {shown:#?}"
	);
	let named: Vec<&String> = shown
		.iter()
		.filter(|line| line.contains(NAME_MARK))
		.collect();
	assert!(named.is_empty(), "{named:#?}");
	let debug = capture.at(&["DEBUG"]).join("\n");
	for name in [
		"QuillUploaded.txt",
		"QuillHydrated.bin",
		"QuillRenamedFrom.txt",
		"QuillRenamedTo.txt",
		"QuillDeleted.txt",
		"QuillUnreadable.txt",
		"QuillFolder",
		"QuillParked.txt",
		"QuillKept.txt",
	] {
		assert!(debug.contains(name), "{name} is logged at DEBUG");
	}
}

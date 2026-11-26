//! The helper's command loop end to end, over in-memory pipes, on the fake platform and drive.

use std::{path::PathBuf, sync::Arc, time::Duration};

use filen_mobile_native_cache::{CacheError, ffi::FfiNonRootObject};
use filen_types::fs::Uuid;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::{
	io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream},
	task::{JoinHandle, yield_now},
	time::{sleep, timeout},
};

use crate::{
	auth::DATA_DIR_MARKER_NAME,
	branding::{Branding, ExplorerCommands},
	failure::Failure,
	helper::{
		BrandingScope, ErrorCode, ExitReason, HelperConfig, Platform, RootRefusal, StateDir,
		UserId, blocking, run_helper, unregister_all,
	},
	identity::PlaceholderIdentity,
	os::LocalError,
	test_support::{
		BrandingCall, DriveOp, FakeDrive, FakeEntry, FakeEvent, FakeLocal, FakeOpener,
		FakePlatform, FakeStep, Gated, KillPoint, LocalOp, PlatformCall, ffi_dir, ffi_file,
		root_path,
	},
	thumbnails::{ThumbnailCancel, ThumbnailRefusal},
};

const ACCOUNT_ROOT: Uuid = Uuid::from_u128(0x100);
const OTHER_ROOT: Uuid = Uuid::from_u128(0x200);

/// A session for `user` whose account root is `root`, every secret a marker no line may show.
fn session(user: &str, root: Uuid) -> String {
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
			"baseFolderUUID": root.to_string(),
			"userId": user,
			"metadataCache": false,
			"tmpPath": "marker-tmp-path",
			"connectToSocket": false
		}
	})
	.to_string()
}

/// The directories one helper's runs share.
struct Disk(TempDir);

impl Disk {
	fn new() -> Self {
		Self(tempfile::tempdir().unwrap())
	}

	fn root(&self) -> PathBuf {
		self.0.path().join("Filen")
	}

	fn data(&self) -> PathBuf {
		self.0.path().join("data")
	}

	fn state(&self) -> PathBuf {
		self.0.path().join("state")
	}

	fn has_session(&self) -> bool {
		self.data().join("auth.json").is_file()
	}
}

/// A fake platform and drive for the account whose root is [`ACCOUNT_ROOT`].
fn world() -> (FakePlatform, Arc<FakeDrive>) {
	let local = FakeLocal::new(PlaceholderIdentity::Root { root: ACCOUNT_ROOT });
	let drive = Arc::new(FakeDrive::new(ACCOUNT_ROOT));
	drive.list(&ACCOUNT_ROOT.to_string(), Vec::new());
	drive.link_local(&local);
	(FakePlatform::new(local), drive)
}

/// A running helper and the parent's ends of its pipes.
struct Rig {
	input: Option<DuplexStream>,
	output: BufReader<DuplexStream>,
	task: JoinHandle<ExitReason>,
	/// Every line read, in order.
	lines: Vec<Value>,
	next_id: u64,
}

impl Rig {
	fn start(disk: &Disk, platform: &FakePlatform, drive: &Arc<FakeDrive>) -> Self {
		let (input, helper_input) = tokio::io::duplex(1 << 22);
		let (helper_output, output) = tokio::io::duplex(1 << 22);
		let config = HelperConfig::new(StateDir::new(disk.state()).unwrap());
		// Before the helper runs, as the process's start sets it.
		let security = platform.secure_thumbnails();
		let task = tokio::spawn(run_helper(
			platform.clone(),
			security,
			FakeOpener::new(Arc::clone(drive)),
			config,
			helper_input,
			helper_output,
		));
		Self {
			input: Some(input),
			output: BufReader::new(output),
			task,
			lines: Vec::new(),
			next_id: 1,
		}
	}

	/// The next line the helper writes; fails after an hour of quiet.
	async fn line(&mut self) -> Value {
		let mut text = String::new();
		let read = timeout(Duration::from_secs(3600), self.output.read_line(&mut text))
			.await
			.expect("the helper wrote no line for an hour")
			.unwrap();
		assert!(read > 0, "the helper closed its output");
		let line: Value = serde_json::from_str(&text).unwrap();
		self.lines.push(line.clone());
		line
	}

	async fn send_raw(&mut self, text: &str) {
		let input = self.input.as_mut().unwrap();
		input.write_all(text.as_bytes()).await.unwrap();
		input.write_all(b"\n").await.unwrap();
	}

	/// Sends `message` with the next id, without waiting; the id.
	async fn send(&mut self, mut message: Value) -> u64 {
		let id = self.next_id;
		self.next_id += 1;
		message["id"] = json!(id);
		self.send_raw(&message.to_string()).await;
		id
	}

	/// Reads lines until the reply to `id`.
	async fn reply_to(&mut self, id: u64) -> Value {
		loop {
			let line = self.line().await;
			if line["t"] == "reply" && line["id"] == id {
				return line;
			}
		}
	}

	/// Sends `message` and waits for its reply.
	async fn request(&mut self, message: Value) -> Value {
		let id = self.send(message).await;
		self.reply_to(id).await
	}

	/// Reads lines until one matches `wanted`.
	async fn until(&mut self, wanted: impl Fn(&Value) -> bool) -> Value {
		if let Some(line) = self.lines.iter().find(|line| wanted(line)) {
			return line.clone();
		}
		loop {
			let line = self.line().await;
			if wanted(&line) {
				return line;
			}
		}
	}

	async fn register(&mut self, disk: &Disk) -> Value {
		self.request(register(disk, "42")).await
	}

	async fn set_auth(&mut self, user: &str, root: Uuid) -> Value {
		self.request(json!({ "t": "setAuth", "userId": user, "json": session(user, root) }))
			.await
	}

	/// Closes the parent's end of stdin and waits for the helper to exit.
	async fn close(mut self) -> (ExitReason, Vec<Value>) {
		drop(self.input.take());
		let reason = self.task.await.unwrap();
		let mut rest = String::new();
		while self.output.read_line(&mut rest).await.unwrap() > 0 {
			self.lines.push(serde_json::from_str(&rest).unwrap());
			rest.clear();
		}
		(reason, self.lines)
	}
}

fn register(disk: &Disk, user: &str) -> Value {
	json!({
		"t": "register",
		"root": disk.root().to_str().unwrap(),
		"userId": user,
		"dataDir": disk.data().to_str().unwrap(),
	})
}

fn ok(reply: &Value) -> &Value {
	assert_eq!(reply["ok"], true, "{reply}");
	&reply["value"]
}

fn code(reply: &Value) -> &str {
	assert_eq!(reply["ok"], false, "{reply}");
	reply["error"]["code"].as_str().unwrap()
}

fn unauthenticated() -> CacheError {
	CacheError::Unauthenticated("signed out".into())
}

#[tokio::test(start_paused = true)]
async fn hello_comes_first_and_shutdown_exits_cleanly() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	let hello = rig.line().await;
	assert_eq!(hello["t"], "hello");
	assert_eq!(hello["protocol"], 1);
	assert_eq!(hello["build"], 26300);
	assert_eq!(rig.line().await["state"], "idle");
	assert_eq!(
		ok(&rig.request(json!({ "t": "shutdown" })).await),
		&json!({})
	);
	assert_eq!(rig.task.await.unwrap(), ExitReason::Shutdown);
}

#[tokio::test(start_paused = true)]
async fn set_auth_before_register_is_refused() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	let reply = rig.set_auth("42", ACCOUNT_ROOT).await;
	assert_eq!(code(&reply), "notRegistered");
	assert!(!disk.has_session());
}

#[tokio::test(start_paused = true)]
async fn register_then_set_auth_serves_the_root() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	assert_eq!(
		ok(&rig.register(&disk).await),
		&json!({ "state": "registered", "resumed": false })
	);
	assert!(disk.root().is_dir());
	assert_eq!(
		ok(&rig.set_auth("42", ACCOUNT_ROOT).await),
		&json!({ "state": "serving", "adopted": false })
	);
	rig.until(|line| line["t"] == "status" && line["watching"] == true)
		.await;
	assert_eq!(
		platform.calls(),
		[
			PlatformCall::Register {
				root: disk.root(),
				account_root: ACCOUNT_ROOT,
			},
			PlatformCall::Connect(disk.root()),
			PlatformCall::Watch,
		]
	);
	assert!(disk.has_session());
	assert_eq!(platform.keys("42").raw().map(|key| key.len()), Some(32));
	let state = ok(&rig.request(json!({ "t": "getState" })).await).clone();
	assert_eq!(state["state"], "serving");
	assert_eq!(state["userId"], "42");
	assert_eq!(state["watching"], true);
	// Null until the engine has looked at the whole root (local::tests covers that), which may or
	// may not have happened by now.
	assert!(
		state["pendingUploads"].is_null() || state["pendingUploads"] == 0,
		"{state}"
	);
	rig.until(|line| line["t"] == "pendingUploads" && line["count"] == 0)
		.await;
	let state = ok(&rig.request(json!({ "t": "getState" })).await).clone();
	assert_eq!(state["pendingUploads"], 0);
	rig.until(|line| line["t"] == "status" && line["state"] == "serving")
		.await;
}

#[tokio::test(start_paused = true)]
async fn registering_the_same_root_twice_is_idempotent_and_another_is_refused() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	assert_eq!(
		ok(&rig.register(&disk).await),
		&json!({ "state": "registered", "resumed": false })
	);
	assert_eq!(
		code(&rig.request(register(&disk, "43")).await),
		"alreadyRegistered"
	);
}

#[tokio::test(start_paused = true)]
async fn a_session_of_another_user_is_refused() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	let reply = rig
		.request(json!({ "t": "setAuth", "userId": "42", "json": session("43", ACCOUNT_ROOT) }))
		.await;
	assert_eq!(code(&reply), "ownerMismatch");
	assert_eq!(
		code(&rig.set_auth("43", ACCOUNT_ROOT).await),
		"ownerMismatch"
	);
	assert!(!disk.has_session());
}

#[tokio::test(start_paused = true)]
async fn a_session_of_another_account_than_the_roots_is_refused() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	ok(&rig
		.request(json!({ "t": "clearAuth", "userId": "42" }))
		.await);
	assert_eq!(code(&rig.set_auth("42", OTHER_ROOT).await), "ownerMismatch");
	assert!(!disk.has_session());
}

#[tokio::test(start_paused = true)]
async fn a_cache_signed_in_as_another_account_is_refused_and_its_session_cleared() {
	let disk = Disk::new();
	let (platform, _drive) = world();
	let other = Arc::new(FakeDrive::new(OTHER_ROOT));
	let mut rig = Rig::start(&disk, &platform, &other);
	ok(&rig.register(&disk).await);
	assert_eq!(
		code(&rig.set_auth("42", ACCOUNT_ROOT).await),
		"ownerMismatch"
	);
	assert!(!disk.has_session());
	assert!(other.stopped());
	assert_eq!(
		platform.registration(&disk.root()),
		None,
		"nothing is registered for an account the session is not of"
	);
	let state = ok(&rig.request(json!({ "t": "getState" })).await).clone();
	assert_eq!(state["state"], "registered");
}

#[tokio::test(start_paused = true)]
async fn a_session_the_cache_never_signs_in_with_is_rejected_after_the_wait() {
	let disk = Disk::new();
	let (platform, drive) = world();
	drive.fail_root_always(Some(unauthenticated));
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	let started = tokio::time::Instant::now();
	assert_eq!(
		code(&rig.set_auth("42", ACCOUNT_ROOT).await),
		"sessionRejected"
	);
	assert!(started.elapsed() >= Duration::from_secs(30));
	assert!(drive.stopped());
	assert!(
		disk.has_session(),
		"the session stays for the cache's next try"
	);
	let state = ok(&rig.request(json!({ "t": "getState" })).await).clone();
	assert_eq!(state["state"], "registered");
}

#[tokio::test(start_paused = true)]
async fn a_new_session_for_a_served_root_is_adopted_without_a_restart() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	assert_eq!(
		ok(&rig.set_auth("42", ACCOUNT_ROOT).await),
		&json!({ "state": "serving", "adopted": true })
	);
	let connects = platform
		.calls()
		.iter()
		.filter(|call| matches!(call, PlatformCall::Connect(_)))
		.count();
	assert_eq!(connects, 1);
	assert!(!drive.stopped());
}

#[tokio::test(start_paused = true)]
async fn clear_auth_stops_the_drive_then_deletes_the_session_and_the_key() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	assert_eq!(
		ok(&rig
			.request(json!({ "t": "clearAuth", "userId": "42" }))
			.await),
		&json!({ "cleared": true })
	);
	assert!(drive.stopped());
	assert!(!drive.has_listener());
	assert!(!disk.has_session());
	assert_eq!(platform.keys("42").raw(), None);
	assert_eq!(platform.keys("42").delete_count(), 1);
	assert!(!platform.is_connected(&disk.root()));
	assert_eq!(platform.live_watchers(), 0);
	let calls = platform.calls();
	let unwatched = calls.iter().position(|call| *call == PlatformCall::Unwatch);
	let disconnected = calls
		.iter()
		.position(|call| matches!(call, PlatformCall::Disconnect(_)));
	assert!(unwatched < disconnected, "{calls:?}");
	assert_eq!(
		platform.registration(&disk.root()),
		Some(Some(ACCOUNT_ROOT))
	);
	let state = ok(&rig.request(json!({ "t": "getState" })).await).clone();
	assert_eq!(state["state"], "registered");
}

#[tokio::test(start_paused = true)]
async fn clear_auth_of_an_unknown_user_clears_nothing() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	assert_eq!(
		ok(&rig
			.request(json!({ "t": "clearAuth", "userId": "7" }))
			.await),
		&json!({ "cleared": false })
	);
}

#[tokio::test(start_paused = true)]
async fn closing_stdin_stops_serving_but_unregisters_nothing() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	let (reason, lines) = rig.close().await;
	assert_eq!(reason, ExitReason::InputClosed);
	assert!(lines.iter().any(|line| line["state"] == "stopping"));
	assert!(drive.stopped());
	assert!(disk.has_session());
	assert_eq!(
		platform.registration(&disk.root()),
		Some(Some(ACCOUNT_ROOT))
	);
	assert!(
		!platform
			.calls()
			.iter()
			.any(|call| matches!(call, PlatformCall::Unregister(_)))
	);
}

#[tokio::test(start_paused = true)]
async fn a_closed_output_stops_the_helper() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	rig.line().await;
	let Rig {
		input,
		output,
		task,
		..
	} = rig;
	drop(output);
	let mut input = input.unwrap();
	// Each request's reply is a write that fails once the parent's end is gone.
	for id in 1..4 {
		let line = json!({ "t": "getState", "id": id }).to_string() + "\n";
		if input.write_all(line.as_bytes()).await.is_err() {
			break;
		}
		sleep(Duration::from_millis(10)).await;
	}
	assert_eq!(task.await.unwrap(), ExitReason::OutputClosed);
}

#[tokio::test(start_paused = true)]
async fn a_restarted_helper_resumes_the_sealed_session_on_register() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	rig.close().await;
	let mut rig = Rig::start(&disk, &platform, &drive);
	assert_eq!(
		ok(&rig.register(&disk).await),
		&json!({ "state": "serving", "resumed": true })
	);
	assert!(platform.is_connected(&disk.root()));
}

#[tokio::test(start_paused = true)]
async fn a_root_holding_files_another_providers_root_and_a_bad_data_dir_are_refused() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	std::fs::create_dir_all(disk.root()).unwrap();
	std::fs::write(disk.root().join("stray.txt"), b"x").unwrap();
	assert_eq!(code(&rig.register(&disk).await), "rootNotEmpty");
	std::fs::remove_file(disk.root().join("stray.txt")).unwrap();
	platform.register_foreign(&disk.root());
	assert_eq!(code(&rig.register(&disk).await), "rootInUse");
	let mut inside = register(&disk, "42");
	inside["root"] = json!(disk.data().join("Filen").to_str().unwrap());
	assert_eq!(code(&rig.request(inside).await), "rootOverlap");
	let mut relative = register(&disk, "42");
	relative["dataDir"] = json!("relative\\data");
	assert_eq!(code(&rig.request(relative).await), "dataDirInvalid");
	platform.refuse_roots(RootRefusal::NotNtfs("FAT32".to_owned()));
	assert_eq!(code(&rig.register(&disk).await), "notNtfs");
}

#[tokio::test(start_paused = true)]
async fn bad_lines_are_answered_and_the_helper_goes_on() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	rig.send_raw("not json").await;
	assert_eq!(
		rig.until(|line| line["t"] == "protocolError").await["code"],
		"invalidJson"
	);
	assert_eq!(
		code(&rig.request(json!({ "t": "frobnicate" })).await),
		"unknownMessage"
	);
	assert_eq!(
		ok(&rig.request(json!({ "t": "getState" })).await)["state"],
		"idle"
	);
}

#[tokio::test(start_paused = true)]
async fn a_queued_shutdown_waits_for_the_request_before_it() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	let set_auth = rig
		.send(json!({ "t": "setAuth", "userId": "42", "json": session("42", ACCOUNT_ROOT) }))
		.await;
	let shutdown = rig.send(json!({ "t": "shutdown" })).await;
	// The first reply after the register's.
	let first = rig
		.until(|line| line["t"] == "reply" && line["id"].as_u64() >= Some(set_auth))
		.await;
	assert_eq!(first["id"], set_auth);
	ok(&first);
	ok(&rig.reply_to(shutdown).await);
	assert_eq!(rig.task.await.unwrap(), ExitReason::Shutdown);
	assert!(drive.stopped());
}

#[tokio::test(start_paused = true)]
async fn a_file_created_in_the_root_reaches_the_drive_and_is_reported() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	rig.until(|line| line["t"] == "pendingUploads" && line["count"] == 0)
		.await;
	let pump = {
		let platform = platform.clone();
		tokio::spawn(async move {
			loop {
				platform.deliver();
				sleep(Duration::from_secs(1)).await;
			}
		})
	};
	platform
		.local()
		.user_create(&root_path("new.txt"), b"hello");
	let activity = rig
		.until(|line| {
			line["t"] == "activity"
				&& line["entries"]
					.as_array()
					.unwrap()
					.iter()
					.any(|entry| entry["kind"] == "uploaded" && entry["path"] == "new.txt")
		})
		.await;
	assert_eq!(activity["t"], "activity");
	pump.abort();
	assert!(drive.child(&ACCOUNT_ROOT.to_string(), "new.txt").is_some());
	ok(&rig
		.request(json!({ "t": "clearAuth", "userId": "42" }))
		.await);
	ok(&rig.request(json!({ "t": "shutdown" })).await);
	assert_eq!(rig.task.await.unwrap(), ExitReason::Shutdown);
}

#[tokio::test(start_paused = true)]
async fn a_revoked_session_is_reported_once() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	drive.fail_root_always(Some(unauthenticated));
	assert_eq!(
		rig.until(|line| line["t"] == "authRevoked").await["userId"],
		"42"
	);
	sleep(Duration::from_secs(120)).await;
	ok(&rig.request(json!({ "t": "getState" })).await);
	let revoked = rig
		.lines
		.iter()
		.filter(|line| line["t"] == "authRevoked")
		.count();
	assert_eq!(revoked, 1);
}

#[tokio::test(start_paused = true)]
async fn unregistering_a_served_root_forgets_it_and_deletes_the_users_data_and_key() {
	let disk = Disk::new();
	let (platform, drive) = world();
	platform
		.local()
		.place_file(&root_path("clean.txt"), file_identity(1), b"clean");
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	platform.local().user_create(&root_path("new.txt"), b"mine");
	let reply = rig
		.request(json!({ "t": "unregister", "userId": "42", "deleteHydrated": true }))
		.await;
	let report = ok(&reply).clone();
	assert_eq!(report["unregistered"], true);
	assert_eq!(report["deletedHydrated"], 1);
	assert_eq!(
		report["kept"],
		json!([{ "path": "new.txt", "reason": "notUploaded" }])
	);
	assert_eq!(report["keyDeleted"], true);
	assert!(report["keptIn"].as_str().unwrap().contains("(not synced "));
	assert_eq!(platform.registration(&disk.root()), None);
	assert!(!disk.data().exists());
	assert_eq!(platform.keys("42").raw(), None);
	assert!(drive.stopped());
	let state = ok(&rig.request(json!({ "t": "getState" })).await).clone();
	assert_eq!(state["state"], "idle");
	assert_eq!(
		ok(&rig.request(json!({ "t": "listRoots" })).await),
		&json!({ "roots": [] })
	);
}

#[tokio::test(start_paused = true)]
async fn unregistering_an_unknown_user_does_nothing() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	let reply = rig
		.request(json!({ "t": "unregister", "userId": "7", "deleteHydrated": true }))
		.await;
	assert_eq!(ok(&reply)["unregistered"], false);
}

#[tokio::test(start_paused = true)]
async fn an_unregister_stopped_by_unsynced_changes_leaves_the_root_registered() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	ok(&rig
		.request(json!({ "t": "clearAuth", "userId": "42" }))
		.await);
	let local = platform.local();
	local.place_file(&root_path("edited.txt"), file_identity(3), b"old data");
	local
		.user_write(&root_path("edited.txt"), b"new data")
		.unwrap();
	local.set_on_disk(&root_path("edited.txt"), 3);
	let reply = rig
		.request(json!({ "t": "unregister", "userId": "42", "deleteHydrated": true }))
		.await;
	assert_eq!(code(&reply), "unsyncedChanges");
	assert_eq!(
		platform.registration(&disk.root()),
		Some(Some(ACCOUNT_ROOT))
	);
	assert!(local.entry(&root_path("edited.txt")).unwrap().placeholder);
	let state = ok(&rig.request(json!({ "t": "getState" })).await).clone();
	assert_eq!(state["state"], "registered");
}

#[tokio::test(start_paused = true)]
async fn unregister_all_elevated_unregisters_every_root_and_leaves_the_keys() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	rig.close().await;
	platform.set_elevated(true);
	let done = unregister_all(
		platform.clone(),
		&StateDir::new(disk.state()).unwrap(),
		true,
	)
	.await;
	assert_eq!(done.roots.len(), 1);
	let report = done.roots[0].outcome.as_ref().unwrap();
	assert!(report.unregistered);
	assert!(report.root_removed);
	assert!(!report.key_deleted);
	assert!(!report.data_deleted);
	assert!(done.roots[0].key_left);
	assert_eq!(done.exit_code(), 0);
	assert_eq!(platform.keys("42").raw().map(|key| key.len()), Some(32));
	assert_eq!(platform.registration(&disk.root()), None);
	assert!(disk.has_session(), "left for a run as the user");
	let line = done.line().to_line();
	assert!(line.contains("\"keyLeft\":true"), "{line}");
}

fn file_identity(n: u128) -> PlaceholderIdentity {
	PlaceholderIdentity::File {
		stable: Uuid::from_u128(n),
		version: Uuid::from_u128(n + 1000),
	}
}

fn index_text(disk: &Disk) -> String {
	std::fs::read_to_string(disk.state().join("roots.json")).unwrap()
}

#[tokio::test(start_paused = true)]
async fn a_register_elsewhere_after_a_restart_is_refused_and_the_first_root_kept() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	platform.local().user_create(&root_path("new.txt"), b"mine");
	rig.close().await;
	let before = index_text(&disk);
	let mut rig = Rig::start(&disk, &platform, &drive);
	let mut elsewhere = register(&disk, "42");
	elsewhere["root"] = json!(disk.0.path().join("Other").to_str().unwrap());
	assert_eq!(code(&rig.request(elsewhere).await), "alreadyRegistered");
	let mut other_data = register(&disk, "42");
	other_data["dataDir"] = json!(disk.0.path().join("data2").to_str().unwrap());
	assert_eq!(code(&rig.request(other_data).await), "alreadyRegistered");
	assert_eq!(index_text(&disk), before);
	let roots = ok(&rig.request(json!({ "t": "listRoots" })).await).clone();
	assert_eq!(roots["roots"][0]["root"], disk.root().to_str().unwrap());
	assert_eq!(roots["roots"][0]["registered"], true);
	assert_eq!(
		platform.registration(&disk.root()),
		Some(Some(ACCOUNT_ROOT))
	);
	assert!(disk.has_session());
}

#[tokio::test(start_paused = true)]
async fn clear_auth_wipes_what_the_cache_kept() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	std::fs::write(disk.data().join("db").join("cache.db"), b"metadata").unwrap();
	std::fs::write(disk.data().join("files").join("copy"), b"decrypted").unwrap();
	ok(&rig
		.request(json!({ "t": "clearAuth", "userId": "42" }))
		.await);
	assert!(!disk.data().join("db").exists());
	assert!(!disk.data().join("files").exists());
	assert!(!disk.has_session());
	// Signed out but still registered, and served again on the next session.
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	assert!(disk.data().join("db").is_dir());
}

#[tokio::test(start_paused = true)]
async fn clear_auth_after_a_restart_wipes_what_the_cache_kept() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	rig.close().await;
	std::fs::write(disk.data().join("files").join("copy"), b"decrypted").unwrap();
	let mut rig = Rig::start(&disk, &platform, &drive);
	assert_eq!(
		ok(&rig
			.request(json!({ "t": "clearAuth", "userId": "42" }))
			.await),
		&json!({ "cleared": true })
	);
	assert!(!disk.data().join("files").exists());
	assert!(!disk.has_session());
	assert_eq!(platform.keys("42").raw(), None);
}

#[tokio::test(start_paused = true)]
async fn a_shutdown_ends_a_sign_in_wait() {
	let disk = Disk::new();
	let (platform, drive) = world();
	drive.fail_root_always(Some(unauthenticated));
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	let started = tokio::time::Instant::now();
	let set_auth = rig
		.send(json!({ "t": "setAuth", "userId": "42", "json": session("42", ACCOUNT_ROOT) }))
		.await;
	sleep(Duration::from_secs(2)).await;
	let shutdown = rig.send(json!({ "t": "shutdown" })).await;
	assert_eq!(code(&rig.reply_to(set_auth).await), "shuttingDown");
	ok(&rig.reply_to(shutdown).await);
	assert!(started.elapsed() < Duration::from_secs(5));
	assert_eq!(rig.task.await.unwrap(), ExitReason::Shutdown);
	assert!(disk.has_session(), "the session stays for the next start");
}

#[tokio::test(start_paused = true)]
async fn a_state_asked_just_before_a_shutdown_counts_nothing() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	rig.until(|line| line["t"] == "pendingUploads" && line["count"] == 0)
		.await;
	// One write, so the reader has the shutdown before the loop answers the state: the state
	// does not wait for a walk of the root then.
	rig.send_raw(
		&[
			json!({ "t": "getState", "id": 901 }).to_string(),
			json!({ "t": "shutdown", "id": 902 }).to_string(),
		]
		.join("\n"),
	)
	.await;
	let state = ok(&rig.reply_to(901).await).clone();
	assert_eq!(state["state"], "serving");
	assert!(state["pendingUploads"].is_null(), "{state}");
	ok(&rig.reply_to(902).await);
	assert_eq!(rig.task.await.unwrap(), ExitReason::Shutdown);
}

#[tokio::test(start_paused = true)]
async fn requests_queued_behind_a_shutdown_are_answered_shutting_down() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	rig.line().await;
	// One write, so the reader queues both before the loop answers the first.
	rig.send_raw(
		&[
			json!({ "t": "shutdown", "id": 1 }).to_string(),
			json!({ "t": "getState", "id": 2 }).to_string(),
		]
		.join("\n"),
	)
	.await;
	ok(&rig.reply_to(1).await);
	assert_eq!(code(&rig.reply_to(2).await), "shuttingDown");
	assert_eq!(rig.task.await.unwrap(), ExitReason::Shutdown);
}

#[tokio::test(start_paused = true)]
async fn a_request_the_reader_holds_on_a_full_queue_is_answered_shutting_down() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	rig.line().await;
	// One write: the reader queues the shutdown and 15 more, the queue's 16, and holds the 16th
	// get while the loop, which runs only once the reader waits, takes the shutdown and stops.
	let lines: Vec<String> = std::iter::once(json!({ "t": "shutdown", "id": 1 }).to_string())
		.chain((2..18).map(|id| json!({ "t": "getState", "id": id }).to_string()))
		.collect();
	rig.send_raw(&lines.join("\n")).await;
	let (reason, lines) = rig.close().await;
	assert_eq!(reason, ExitReason::Shutdown);
	let replies: Vec<&Value> = lines.iter().filter(|line| line["t"] == "reply").collect();
	assert_eq!(
		replies
			.iter()
			.map(|line| line["id"].as_u64().unwrap())
			.collect::<Vec<_>>(),
		(1..18).collect::<Vec<_>>(),
		"every request read is answered once, in order"
	);
	for reply in &replies[1..] {
		assert_eq!(code(reply), "shuttingDown");
	}
}

#[tokio::test(start_paused = true)]
async fn a_request_that_panics_is_answered_and_the_helper_stops_in_order() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	platform.panic_on_check_root();
	let reply = rig.register(&disk).await;
	assert_eq!(code(&reply), "internal");
	assert_eq!(rig.task.await.unwrap(), ExitReason::Internal);
	assert_eq!(ExitReason::Internal.exit_code(), 1);
}

#[tokio::test(start_paused = true)]
async fn an_unreadable_roots_index_is_one_fatal_line_and_no_hello() {
	let disk = Disk::new();
	let (platform, drive) = world();
	std::fs::create_dir_all(disk.state()).unwrap();
	std::fs::write(disk.state().join("roots.json"), b"nonsense").unwrap();
	let rig = Rig::start(&disk, &platform, &drive);
	let (reason, lines) = rig.close().await;
	assert_eq!(reason, ExitReason::StateDirInvalid);
	assert_eq!(reason.exit_code(), 13);
	assert_eq!(lines.len(), 1, "{lines:?}");
	assert_eq!(lines[0]["t"], "fatal");
	assert_eq!(lines[0]["code"], "stateDirInvalid");
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn an_index_that_cannot_record_the_account_serves_nothing() {
	use std::os::unix::fs::PermissionsExt;

	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	let state = disk.state();
	std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o500)).unwrap();
	let reply = rig.set_auth("42", ACCOUNT_ROOT).await;
	std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
	assert_eq!(code(&reply), "io");
	assert!(!platform.is_connected(&disk.root()));
	assert!(drive.stopped());
	let state = ok(&rig.request(json!({ "t": "getState" })).await).clone();
	assert_eq!(state["state"], "registered");
}

#[tokio::test(start_paused = true)]
async fn an_unregister_stopped_while_serving_serves_the_root_again() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	let local = platform.local();
	local.place_file(&root_path("edited.txt"), file_identity(3), b"old data");
	local
		.user_write(&root_path("edited.txt"), b"new data")
		.unwrap();
	local.set_on_disk(&root_path("edited.txt"), 3);
	// Every hydrate, as the engine, still running while the unregister fetches what it keeps,
	// hydrates the edit too: one that succeeded would leave nothing to stop the unregister.
	local.fail_hydrations(Some(LocalError::Busy));
	let reply = rig
		.request(json!({ "t": "unregister", "userId": "42", "deleteHydrated": true }))
		.await;
	assert_eq!(code(&reply), "unsyncedChanges");
	let state = ok(&rig.request(json!({ "t": "getState" })).await).clone();
	assert_eq!(state["state"], "serving");
	assert!(platform.is_connected(&disk.root()));
	assert!(disk.has_session());
}

#[tokio::test(start_paused = true)]
async fn a_root_left_registered_with_its_session_is_served_again_to_fetch_what_it_keeps_when_unregistered()
 {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	let local = platform.local();
	local.place_file(&root_path("edited.txt"), file_identity(3), b"old data");
	local
		.user_write(&root_path("edited.txt"), b"new data")
		.unwrap();
	local.set_on_disk(&root_path("edited.txt"), 3);
	local.fail_hydrations(Some(LocalError::Busy));
	let reply = rig
		.request(json!({ "t": "unregister", "userId": "42", "deleteHydrated": true }))
		.await;
	assert_eq!(code(&reply), "unsyncedChanges");
	// The parent keeps the session for a later try, here by a helper started anew, which serves
	// nothing until asked: the edit can be fetched now.
	rig.close().await;
	local.fail_hydrations(None);
	let mut rig = Rig::start(&disk, &platform, &drive);
	let report = ok(&rig
		.request(json!({ "t": "unregister", "userId": "42", "deleteHydrated": true }))
		.await)
	.clone();
	assert_eq!(report["unregistered"], true);
	assert_eq!(report["keptCount"], 1);
	assert_eq!(report["kept"][0]["path"], "edited.txt");
	assert_eq!(platform.registration(&disk.root()), None);
	assert!(!disk.has_session());
}

#[tokio::test(start_paused = true)]
async fn an_unregister_of_a_served_root_fetches_no_renamed_file_with_no_byte_on_disk() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	let local = platform.local();
	local.place_file_with(
		&root_path("cloud.txt"),
		file_identity(3),
		b"cloud data",
		false,
	);
	// Renamed while nothing served the root, and still not in sync when the unregister walks it.
	local.swallow_events(&root_path("renamed.txt"));
	local.user_rename_unserved(&root_path("cloud.txt"), &root_path("renamed.txt"));
	let report = ok(&rig
		.request(json!({ "t": "unregister", "userId": "42", "deleteHydrated": true }))
		.await)
	.clone();
	assert_eq!(report["unregistered"], true);
	assert_eq!(report["keptCount"], 0);
	assert_eq!(report["rootRemoved"], true);
	assert!(
		!local
			.ops()
			.iter()
			.any(|op| matches!(op, LocalOp::Hydrated(_))),
		"{:?}",
		local.ops()
	);
	// The rename never reached the drive, and the unregister sends none: the drive keeps its name.
	assert!(
		!drive
			.ops()
			.iter()
			.any(|op| matches!(op, DriveOp::Rename { .. })),
		"{:?}",
		drive.ops()
	);
	assert!(local.entries().is_empty());
}

#[tokio::test(start_paused = true)]
async fn signing_out_unregisters_while_served_and_leaves_nothing_to_clear() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	let report = ok(&rig
		.request(json!({ "t": "unregister", "userId": "42", "deleteHydrated": true }))
		.await)
	.clone();
	assert_eq!(report["dataDeleted"], true);
	assert_eq!(report["keyDeleted"], true);
	assert_eq!(
		ok(&rig
			.request(json!({ "t": "clearAuth", "userId": "42" }))
			.await),
		&json!({ "cleared": false })
	);
	assert!(!disk.data().exists());
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn a_data_directory_that_cannot_be_wiped_keeps_the_root_in_the_index() {
	use std::os::unix::fs::PermissionsExt;

	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	std::fs::write(disk.data().join("db").join("cache.db"), b"metadata").unwrap();
	let db = disk.data().join("db");
	std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o500)).unwrap();
	let report = ok(&rig
		.request(json!({ "t": "unregister", "userId": "42", "deleteHydrated": true }))
		.await)
	.clone();
	std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o700)).unwrap();
	assert_eq!(report["unregistered"], true);
	assert_eq!(report["dataDeleted"], false);
	assert_eq!(report["keyDeleted"], true);
	assert_eq!(platform.registration(&disk.root()), None);
	let roots = ok(&rig.request(json!({ "t": "listRoots" })).await).clone();
	assert_eq!(roots["roots"][0]["registered"], false);
	// A second unregister finds the root again and finishes the wipe.
	let report = ok(&rig
		.request(json!({ "t": "unregister", "userId": "42", "deleteHydrated": true }))
		.await)
	.clone();
	assert_eq!(report["dataDeleted"], true);
	assert!(!disk.data().exists());
	assert_eq!(
		ok(&rig.request(json!({ "t": "listRoots" })).await),
		&json!({ "roots": [] })
	);
}

/// Writes a roots index naming `user`'s root at `root` with the data directory `data`.
fn write_index(disk: &Disk, user: &str, root: &std::path::Path, data: &std::path::Path) {
	std::fs::create_dir_all(disk.state()).unwrap();
	let text = json!({
		"version": 1,
		"roots": [{
			"userId": user,
			"root": root.to_str().unwrap(),
			"dataDir": data.to_str().unwrap(),
			"accountRoot": null,
		}],
	})
	.to_string();
	std::fs::write(disk.state().join("roots.json"), text).unwrap();
}

#[tokio::test(start_paused = true)]
async fn an_elevated_unregister_all_touches_no_path_the_os_does_not_confirm() {
	let disk = Disk::new();
	let (platform, _drive) = world();
	// Paths a process of the user wrote into the index: nothing the OS registered.
	let outside = disk.0.path().join("Program Files").join("X");
	let other_data = disk.0.path().join("ProgramData").join("Y");
	std::fs::create_dir_all(outside.join("empty")).unwrap();
	std::fs::write(outside.join("app.exe"), b"exe").unwrap();
	std::fs::create_dir_all(other_data.join("db")).unwrap();
	std::fs::write(other_data.join("db").join("x"), b"x").unwrap();
	write_index(&disk, "42", &outside, &other_data);
	platform.set_elevated(true);
	let state = StateDir::new(disk.state()).unwrap();
	let done = unregister_all(platform.clone(), &state, true).await;
	let report = done.roots[0].outcome.as_ref().unwrap();
	assert!(!report.data_deleted);
	assert!(!report.root_removed);
	assert!(outside.join("empty").is_dir());
	assert!(outside.join("app.exe").is_file());
	assert!(other_data.join("db").join("x").is_file());
	assert!(!platform.calls().iter().any(|call| matches!(
		call,
		PlatformCall::SetAside(_) | PlatformCall::RootRemoved(_)
	)));
	// Still indexed, for a run as the user.
	assert!(index_text(&disk).contains("\"42\""));
}

#[tokio::test(start_paused = true)]
async fn unregister_all_refuses_an_indexed_root_the_os_cannot_take() {
	let disk = Disk::new();
	let (platform, _drive) = world();
	write_index(&disk, "42", &disk.root(), &disk.data());
	std::fs::create_dir_all(disk.data().join("db")).unwrap();
	platform.refuse_roots(RootRefusal::Invalid("relative".to_owned()));
	let state = StateDir::new(disk.state()).unwrap();
	let done = unregister_all(platform.clone(), &state, true).await;
	assert_eq!(
		done.roots[0].outcome.as_ref().unwrap_err().code,
		ErrorCode::RootInvalid
	);
	assert!(disk.data().join("db").is_dir());
	assert_eq!(done.exit_code(), 21);
}

#[tokio::test(start_paused = true)]
async fn unregister_all_exits_20_when_an_item_was_kept() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	platform.local().user_create(&root_path("new.txt"), b"mine");
	rig.close().await;
	let state = StateDir::new(disk.state()).unwrap();
	let done = unregister_all(platform.clone(), &state, true).await;
	let report = done.roots[0].outcome.as_ref().unwrap();
	assert_eq!(report.kept_count, 1);
	assert!(report.data_deleted);
	assert_eq!(done.exit_code(), 20);
}

#[tokio::test(start_paused = true)]
async fn an_unregister_finding_the_root_already_let_go_counts_every_file_it_sets_aside() {
	let disk = Disk::new();
	let (platform, _drive) = world();
	// What an unregister killed after the OS let the root go leaves: no registration, and the
	// items it kept made ordinary, in the root still indexed.
	platform.local().user_create(&root_path("new.txt"), b"mine");
	platform.local().user_mkdir(&root_path("d"));
	platform
		.local()
		.user_create(&root_path("d\\edited.txt"), b"changed");
	write_index(&disk, "42", &disk.root(), &disk.data());
	let state = StateDir::new(disk.state()).unwrap();
	let done = unregister_all(platform.clone(), &state, true).await;
	let report = done.roots[0].outcome.as_ref().unwrap();
	assert_eq!(report.kept_count, 2);
	assert!(
		report
			.kept_in
			.as_ref()
			.unwrap()
			.to_string_lossy()
			.contains("(not synced ")
	);
	assert_eq!(done.exit_code(), 20);
}

#[tokio::test(start_paused = true)]
async fn a_register_for_the_held_root_with_another_data_directory_is_refused() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	let before = index_text(&disk);
	let mut other_data = register(&disk, "42");
	other_data["dataDir"] = json!(disk.0.path().join("data2").to_str().unwrap());
	assert_eq!(code(&rig.request(other_data).await), "alreadyRegistered");
	assert!(!disk.0.path().join("data2").exists());
	assert_eq!(index_text(&disk), before);
	let state = ok(&rig.request(json!({ "t": "getState" })).await).clone();
	assert_eq!(state["state"], "serving");
}

/// Puts a file in the folder `dir`'s `files/`, as a user's own folder named by mistake holds.
fn users_folder(dir: &std::path::Path) -> PathBuf {
	let file = dir.join("files").join("thesis.docx");
	std::fs::create_dir_all(file.parent().unwrap()).unwrap();
	std::fs::write(&file, b"mine").unwrap();
	file
}

#[tokio::test(start_paused = true)]
async fn a_register_naming_a_folder_the_helper_did_not_create_as_its_data_directory_is_refused() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let file = users_folder(&disk.data());
	let mut rig = Rig::start(&disk, &platform, &drive);
	assert_eq!(code(&rig.register(&disk).await), "dataDirNotEmpty");
	assert_eq!(std::fs::read(&file).unwrap(), b"mine");
	assert!(!disk.state().join("roots.json").exists());
	assert!(!disk.root().exists());
	assert_eq!(
		std::fs::read_dir(disk.data()).unwrap().count(),
		1,
		"nothing was added to the folder"
	);
}

#[tokio::test(start_paused = true)]
async fn an_indexed_data_directory_the_helper_did_not_create_is_never_wiped() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let folder = disk.0.path().join("Documents");
	let file = users_folder(&folder);
	write_index(&disk, "42", &disk.root(), &folder);
	let mut rig = Rig::start(&disk, &platform, &drive);
	assert_eq!(
		code(
			&rig.request(json!({ "t": "clearAuth", "userId": "42" }))
				.await
		),
		"dataDirNotEmpty"
	);
	assert_eq!(std::fs::read(&file).unwrap(), b"mine");
	rig.close().await;
	let state = StateDir::new(disk.state()).unwrap();
	let done = unregister_all(platform.clone(), &state, true).await;
	let report = done.roots[0].outcome.as_ref().unwrap();
	assert!(!report.data_deleted);
	assert_eq!(std::fs::read(&file).unwrap(), b"mine");
	assert!(
		index_text(&disk).contains("\"42\""),
		"kept, to be looked at"
	);
}

// ---- a helper killed part way through an unregister ----
//
// Each test kills the helper (a panic at one of the fake platform's steps, as a kill stops it
// there) during an unregister of a served root holding three changes, then starts a fresh helper
// on the same state: what was kept is reported exactly once, where it is, and nothing is lost.

/// The kept files the served root holds, with their bytes.
const KEPT_FILES: [(&str, &[u8]); 3] = [
	("new.txt", b"mine"),
	("edited.txt", b"new data"),
	("d\\inside.txt", b"inside"),
];

fn unregister_request() -> Value {
	json!({ "t": "unregister", "userId": "42", "deleteHydrated": true })
}

/// A served root holding a clean downloaded file, an edited one, one never uploaded and one in a
/// directory placeholder.
async fn served_with_changes(disk: &Disk, platform: &FakePlatform, drive: &Arc<FakeDrive>) -> Rig {
	let local = platform.local();
	local.place_file(&root_path("clean.txt"), file_identity(1), b"clean");
	local.place_file(&root_path("edited.txt"), file_identity(3), b"old data");
	local.place_dir(
		&root_path("d"),
		PlaceholderIdentity::Dir {
			uuid: Uuid::from_u128(5),
		},
	);
	let mut rig = Rig::start(disk, platform, drive);
	ok(&rig.register(disk).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	local
		.user_write(&root_path("edited.txt"), b"new data")
		.unwrap();
	local.user_create(&root_path("new.txt"), b"mine");
	local.user_create(&root_path("d\\inside.txt"), b"inside");
	rig
}

/// Sends the unregister the platform kills the helper in, and waits for the helper to exit.
async fn killed_unregister(mut rig: Rig, platform: &FakePlatform) {
	let reply = rig.request(unregister_request()).await;
	assert_eq!(code(&reply), "internal");
	assert!(!platform.kill_pending(), "the kill point was never reached");
	let (reason, _) = rig.close().await;
	assert_eq!(reason, ExitReason::Internal);
}

/// Whether `entries` hold every kept file with its bytes, as ordinary files.
fn holds_kept(entries: &[FakeEntry]) -> bool {
	KEPT_FILES.iter().all(|(path, bytes)| {
		entries.iter().any(|entry| {
			entry.path.to_string() == *path && entry.bytes == *bytes && !entry.placeholder
		})
	})
}

/// Where the kept files are: the aside the root was renamed to, or the root.
fn kept_place(disk: &Disk, platform: &FakePlatform) -> Option<String> {
	let place = platform
		.asides()
		.into_iter()
		.find(|aside| {
			platform
				.aside(aside)
				.is_some_and(|entries| holds_kept(&entries))
		})
		.or_else(|| holds_kept(&platform.local().entries()).then(|| disk.root()))?;
	Some(place.to_str().unwrap().to_owned())
}

/// Every kept report the replies among `lines` carry, in order: its id, count and place.
fn kept_reports(lines: &[Value]) -> Vec<(String, u64, Value)> {
	let mut reports = Vec::new();
	for line in lines
		.iter()
		.filter(|line| line["t"] == "reply" && line["ok"] == true)
	{
		let value = &line["value"];
		if let Some(id) = value["keptId"].as_str() {
			reports.push((
				id.to_owned(),
				value["keptCount"].as_u64().unwrap(),
				value["keptIn"].clone(),
			));
		}
		for earlier in value["keptEarlier"].as_array().into_iter().flatten() {
			reports.push((
				earlier["id"].as_str().unwrap().to_owned(),
				earlier["keptCount"].as_u64().unwrap(),
				earlier["keptIn"].clone(),
			));
		}
	}
	reports
}

/// Checks that `lines`, a fresh helper's after the kill, report the three kept files once, at the
/// place they are, and that nothing of the user's is left to report or wipe: the next unregister
/// finds nothing, as it would after a reply the parent read.
async fn reported_once(disk: &Disk, platform: &FakePlatform, mut rig: Rig) {
	ok(&rig.request(json!({ "t": "getState" })).await);
	let again = ok(&rig.request(unregister_request()).await).clone();
	let (_, lines) = rig.close().await;
	let reports = kept_reports(&lines);
	assert_eq!(reports.len(), 1, "{reports:?}");
	let place = kept_place(disk, platform).expect("the kept files were lost");
	assert_eq!(reports[0].1, 3, "{reports:?}");
	assert_eq!(reports[0].2, json!(place), "{reports:?}");
	assert_eq!(
		again["unregistered"], false,
		"forgotten once reported: {again}"
	);
	assert!(!index_text(disk).contains("\"42\""), "{}", index_text(disk));
	assert!(!disk.data().exists());
	assert_eq!(platform.keys("42").raw(), None);
}

/// Kills the helper at `kill` in an unregister of the served root, then unregisters again in a
/// fresh helper, as the launch's sweep does.
async fn killed_then_unregistered(kill: impl FnOnce(&FakePlatform)) {
	let disk = Disk::new();
	let (platform, drive) = world();
	let rig = served_with_changes(&disk, &platform, &drive).await;
	kill(&platform);
	killed_unregister(rig, &platform).await;
	let mut rig = Rig::start(&disk, &platform, &drive);
	let report = ok(&rig.request(unregister_request()).await).clone();
	assert_eq!(report["unregistered"], true, "{report}");
	reported_once(&disk, &platform, rig).await;
}

/// Kills the helper at `kill` in an unregister of the served root, then registers the root again
/// in a fresh helper, as the desktop's restart replays it, and unregisters it.
async fn killed_then_registered(kill: impl FnOnce(&FakePlatform)) {
	let disk = Disk::new();
	let (platform, drive) = world();
	let rig = served_with_changes(&disk, &platform, &drive).await;
	kill(&platform);
	killed_unregister(rig, &platform).await;
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	ok(&rig.request(unregister_request()).await);
	reported_once(&disk, &platform, rig).await;
}

fn at(point: KillPoint) -> impl FnOnce(&FakePlatform) {
	move |platform| platform.kill_at(point)
}

#[tokio::test(start_paused = true)]
async fn a_kill_before_the_root_refuses_requests_is_reported_by_the_next_unregister() {
	killed_then_unregistered(at(KillPoint::Before(FakeStep::ConnectRefusing))).await;
}

#[tokio::test(start_paused = true)]
async fn a_kill_once_the_root_refuses_requests_is_reported_by_the_next_unregister() {
	killed_then_unregistered(at(KillPoint::After(FakeStep::ConnectRefusing))).await;
}

#[tokio::test(start_paused = true)]
async fn a_kill_while_kept_items_are_made_ordinary_is_reported_by_the_next_unregister() {
	killed_then_unregistered(|platform| platform.local().kill_after_reverts(1)).await;
}

#[tokio::test(start_paused = true)]
async fn a_kill_before_the_root_is_disconnected_is_reported_by_the_next_unregister() {
	killed_then_unregistered(at(KillPoint::Before(FakeStep::Disconnect))).await;
}

#[tokio::test(start_paused = true)]
async fn a_kill_once_the_root_is_disconnected_is_reported_by_the_next_unregister() {
	killed_then_unregistered(at(KillPoint::After(FakeStep::Disconnect))).await;
}

#[tokio::test(start_paused = true)]
async fn a_kill_once_the_kept_items_are_recorded_is_reported_by_the_next_unregister() {
	killed_then_unregistered(at(KillPoint::Before(FakeStep::Unregister))).await;
}

#[tokio::test(start_paused = true)]
async fn a_kill_once_the_os_let_the_root_go_is_reported_by_the_next_unregister() {
	killed_then_unregistered(at(KillPoint::After(FakeStep::Unregister))).await;
}

#[tokio::test(start_paused = true)]
async fn a_kill_once_the_os_let_the_root_go_is_reported_by_the_register_replayed() {
	killed_then_registered(at(KillPoint::After(FakeStep::Unregister))).await;
}

#[tokio::test(start_paused = true)]
async fn a_kill_while_the_empty_directories_go_is_reported_by_the_next_unregister() {
	killed_then_unregistered(at(KillPoint::After(FakeStep::Clear))).await;
}

#[tokio::test(start_paused = true)]
async fn a_kill_just_before_the_rename_aside_is_reported_by_the_next_unregister() {
	killed_then_unregistered(at(KillPoint::Before(FakeStep::SetAside))).await;
}

#[tokio::test(start_paused = true)]
async fn a_kill_just_after_the_rename_aside_is_reported_by_the_next_unregister() {
	killed_then_unregistered(at(KillPoint::After(FakeStep::SetAside))).await;
}

#[tokio::test(start_paused = true)]
async fn a_kill_just_after_the_rename_aside_is_reported_by_the_register_replayed() {
	killed_then_registered(at(KillPoint::After(FakeStep::SetAside))).await;
}

#[tokio::test(start_paused = true)]
async fn a_kill_once_the_data_is_wiped_is_reported_by_the_next_unregister() {
	killed_then_unregistered(at(KillPoint::Before(FakeStep::KeyDelete))).await;
}

#[tokio::test(start_paused = true)]
async fn a_kill_once_the_key_is_deleted_is_reported_by_the_next_unregister() {
	killed_then_unregistered(at(KillPoint::After(FakeStep::KeyDelete))).await;
}

#[tokio::test(start_paused = true)]
async fn a_kill_once_the_key_is_deleted_is_reported_by_the_register_replayed() {
	killed_then_registered(at(KillPoint::After(FakeStep::KeyDelete))).await;
}

/// The unregister of the served root answered, and the helper killed before the parent's next
/// request: whether the parent read the reply is not known. The fresh helper's reply to `next`
/// reports it again, under the same id, then never again.
async fn a_reply_not_known_read(next: fn(&Disk) -> Value) {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = served_with_changes(&disk, &platform, &drive).await;
	let first = ok(&rig.request(unregister_request()).await).clone();
	assert_eq!(first["keptCount"], 3);
	rig.task.abort();
	let _ = (&mut rig.task).await;
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.request(next(&disk)).await);
	let lines = rig.lines.clone();
	let reports = kept_reports(&lines);
	assert_eq!(reports.len(), 1, "{reports:?}");
	assert_eq!(json!(reports[0].0), first["keptId"]);
	assert_eq!(reports[0].2, first["keptIn"]);
	if next(&disk)["t"] == "register" {
		ok(&rig.request(unregister_request()).await);
	}
	reported_once(&disk, &platform, rig).await;
}

#[tokio::test(start_paused = true)]
async fn a_reply_the_parent_may_not_have_read_is_reported_again_by_the_next_unregister() {
	a_reply_not_known_read(|_| unregister_request()).await;
}

#[tokio::test(start_paused = true)]
async fn a_reply_the_parent_may_not_have_read_is_reported_again_by_the_next_register() {
	a_reply_not_known_read(|disk| register(disk, "42")).await;
}

#[tokio::test(start_paused = true)]
async fn a_reply_the_parent_read_is_never_reported_again() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = served_with_changes(&disk, &platform, &drive).await;
	ok(&rig.request(unregister_request()).await);
	ok(&rig.request(json!({ "t": "listRoots" })).await);
	let (_, lines) = rig.close().await;
	assert_eq!(kept_reports(&lines).len(), 1);
	let mut rig = Rig::start(&disk, &platform, &drive);
	let again = ok(&rig.register(&disk).await).clone();
	assert!(again.get("keptEarlier").is_none(), "{again}");
	assert!(!kept_place(&disk, &platform).unwrap().ends_with("Filen"));
}

/// The unregister killed once the OS let the root go, and the fresh helper's unregister killed
/// at `second`: a third helper reports it once.
async fn killed_twice(second: KillPoint) {
	let disk = Disk::new();
	let (platform, drive) = world();
	let rig = served_with_changes(&disk, &platform, &drive).await;
	platform.kill_at(KillPoint::After(FakeStep::Unregister));
	killed_unregister(rig, &platform).await;
	platform.kill_at(second);
	killed_unregister(Rig::start(&disk, &platform, &drive), &platform).await;
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.request(unregister_request()).await);
	reported_once(&disk, &platform, rig).await;
}

#[tokio::test(start_paused = true)]
async fn a_second_kill_before_the_recovery_renames_the_root_is_reported_once() {
	killed_twice(KillPoint::Before(FakeStep::SetAside)).await;
}

#[tokio::test(start_paused = true)]
async fn a_second_kill_after_the_recovery_renamed_the_root_is_reported_once() {
	killed_twice(KillPoint::After(FakeStep::SetAside)).await;
}

#[tokio::test(start_paused = true)]
async fn a_second_kill_as_the_recovery_deletes_the_key_is_reported_once() {
	killed_twice(KillPoint::After(FakeStep::KeyDelete)).await;
}

/// The planned aside of the record in the index.
fn planned_aside(disk: &Disk) -> PathBuf {
	let index: Value = serde_json::from_str(&index_text(disk)).unwrap();
	let kept = &index["roots"][0]["kept"][0];
	assert_eq!(kept["settled"], false, "{index}");
	PathBuf::from(kept["path"].as_str().unwrap())
}

#[tokio::test(start_paused = true)]
async fn a_planned_aside_taken_since_is_never_renamed_onto() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let rig = served_with_changes(&disk, &platform, &drive).await;
	platform.kill_at(KillPoint::After(FakeStep::Unregister));
	killed_unregister(rig, &platform).await;
	let planned = planned_aside(&disk);
	platform.occupy(&planned);
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.request(unregister_request()).await);
	assert_eq!(platform.aside(&planned), Some(Vec::new()), "left as it was");
	let place = kept_place(&disk, &platform).unwrap();
	assert_eq!(place, format!("{} 2", planned.to_str().unwrap()));
	reported_once(&disk, &platform, rig).await;
}

#[tokio::test(start_paused = true)]
async fn an_unregister_picks_an_aside_nothing_is_at() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = served_with_changes(&disk, &platform, &drive).await;
	// The fake platform's clock, 2026-11-12 12:00 UTC.
	let taken = disk.0.path().join("Filen (not synced 2026-11-12)");
	platform.occupy(&taken);
	let report = ok(&rig.request(unregister_request()).await).clone();
	assert_eq!(platform.aside(&taken), Some(Vec::new()));
	assert_eq!(
		report["keptIn"],
		json!(format!("{} 2", taken.to_str().unwrap()))
	);
	assert_eq!(
		kept_place(&disk, &platform),
		report["keptIn"].as_str().map(str::to_owned)
	);
}

#[tokio::test(start_paused = true)]
async fn a_root_that_cannot_be_renamed_aside_is_reported_where_it_stays() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = served_with_changes(&disk, &platform, &drive).await;
	platform.fail_set_asides(1);
	let report = ok(&rig.request(unregister_request()).await).clone();
	assert_eq!(report["keptCount"], 3);
	assert_eq!(report["keptIn"], json!(disk.root().to_str().unwrap()));
	assert_eq!(
		kept_place(&disk, &platform),
		Some(disk.root().to_str().unwrap().to_owned())
	);
	reported_once(&disk, &platform, rig).await;
}

#[tokio::test(start_paused = true)]
async fn a_rename_refused_in_the_recovery_reports_the_root_where_it_stays() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let rig = served_with_changes(&disk, &platform, &drive).await;
	platform.kill_at(KillPoint::After(FakeStep::Unregister));
	killed_unregister(rig, &platform).await;
	platform.fail_set_asides(1);
	let mut rig = Rig::start(&disk, &platform, &drive);
	let report = ok(&rig.request(unregister_request()).await).clone();
	assert_eq!(report["keptIn"], json!(disk.root().to_str().unwrap()));
	reported_once(&disk, &platform, rig).await;
}

#[tokio::test(start_paused = true)]
async fn unregister_all_killed_after_the_rename_aside_reports_it_on_its_next_run() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let rig = served_with_changes(&disk, &platform, &drive).await;
	rig.close().await;
	let state = StateDir::new(disk.state()).unwrap();
	platform.kill_at(KillPoint::After(FakeStep::SetAside));
	let killed = {
		let (platform, state) = (platform.clone(), state.clone());
		tokio::spawn(async move { unregister_all(platform, &state, true).await })
	};
	assert!(killed.await.unwrap_err().is_panic());
	let done = unregister_all(platform.clone(), &state, true).await;
	let report = done.roots[0].outcome.as_ref().unwrap();
	assert_eq!(report.kept_count, 3);
	let place = kept_place(&disk, &platform).unwrap();
	assert_eq!(
		report.kept_in.as_deref(),
		Some(std::path::Path::new(&place))
	);
	assert_eq!(done.exit_code(), 20);
	// The line reaches no user: reported again, under the same id, until the app reports it.
	let again = unregister_all(platform.clone(), &state, true).await;
	let earlier = &again.roots[0].outcome.as_ref().unwrap().kept_earlier;
	assert_eq!(earlier.len(), 1);
	assert_eq!(Some(earlier[0].id), report.kept_id);
	assert_eq!(again.exit_code(), 20);
	let mut rig = Rig::start(&disk, &platform, &drive);
	let swept = ok(&rig.request(unregister_request()).await).clone();
	assert_eq!(
		json!(swept["keptEarlier"][0]["id"]),
		json!(report.kept_id),
		"{swept}"
	);
	reported_once(&disk, &platform, rig).await;
}

#[tokio::test(start_paused = true)]
async fn an_elevated_unregister_all_reports_what_was_kept_but_leaves_it_for_the_user() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let rig = served_with_changes(&disk, &platform, &drive).await;
	rig.close().await;
	let state = StateDir::new(disk.state()).unwrap();
	platform.set_elevated(true);
	let done = unregister_all(platform.clone(), &state, true).await;
	assert_eq!(done.roots[0].outcome.as_ref().unwrap().kept_count, 3);
	platform.set_elevated(false);
	let mut rig = Rig::start(&disk, &platform, &drive);
	let report = ok(&rig.request(unregister_request()).await).clone();
	assert_eq!(report["keptEarlier"][0]["keptCount"], 3, "{report}");
	reported_once(&disk, &platform, rig).await;
}

#[tokio::test(start_paused = true)]
async fn another_user_takes_the_root_of_one_kept_only_for_its_report() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = served_with_changes(&disk, &platform, &drive).await;
	ok(&rig.request(unregister_request()).await);
	rig.task.abort();
	let _ = (&mut rig.task).await;
	let mut rig = Rig::start(&disk, &platform, &drive);
	let mut other = register(&disk, "43");
	other["dataDir"] = json!(disk.0.path().join("data43").to_str().unwrap());
	ok(&rig.request(other).await);
	let roots = ok(&rig.request(json!({ "t": "listRoots" })).await).clone();
	assert_eq!(
		roots["roots"][0]["userId"], "42",
		"still to report: {roots}"
	);
	assert_eq!(
		roots["roots"][0]["keptEarlier"][0]["keptCount"], 3,
		"{roots}"
	);
	// The listing reported it, and the parent's next request read it.
	let report = ok(&rig.request(unregister_request()).await).clone();
	assert_eq!(report["unregistered"], false, "{report}");
}

#[tokio::test(start_paused = true)]
async fn a_root_another_provider_registered_since_the_kill_is_left_alone_by_the_register() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let rig = served_with_changes(&disk, &platform, &drive).await;
	platform.kill_at(KillPoint::After(FakeStep::Unregister));
	killed_unregister(rig, &platform).await;
	platform.register_foreign(&disk.root());
	let mut rig = Rig::start(&disk, &platform, &drive);
	assert_eq!(code(&rig.register(&disk).await), "rootInUse");
	assert!(platform.asides().is_empty(), "{:?}", platform.asides());
	assert_eq!(
		kept_place(&disk, &platform),
		Some(disk.root().to_str().unwrap().to_owned())
	);
	assert_eq!(platform.registration(&disk.root()), Some(None));
}

#[tokio::test(start_paused = true)]
async fn a_folder_made_at_the_root_after_a_kill_past_the_rename_stays_where_it_is() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let rig = served_with_changes(&disk, &platform, &drive).await;
	platform.kill_at(KillPoint::After(FakeStep::SetAside));
	killed_unregister(rig, &platform).await;
	let renamed_to = planned_aside(&disk);
	platform
		.local()
		.user_create(&root_path("later.txt"), b"later");
	let mut rig = Rig::start(&disk, &platform, &drive);
	let report = ok(&rig.request(unregister_request()).await).clone();
	assert_eq!(platform.asides(), std::slice::from_ref(&renamed_to));
	assert!(
		platform
			.local()
			.entries()
			.iter()
			.any(|entry| entry.path.to_string() == "later.txt"),
		"the folder made since was moved"
	);
	assert_eq!(report["keptIn"], json!(renamed_to.to_str().unwrap()));
	reported_once(&disk, &platform, rig).await;
}

/// Makes the state directory read-only, so the roots index cannot be written, when the next
/// unregister reaches `point`.
#[cfg(unix)]
fn index_unwritable_at(disk: &Disk, platform: &FakePlatform, point: KillPoint) {
	use std::os::unix::fs::PermissionsExt;
	let state = disk.state();
	platform.run_at(point, move || {
		std::fs::set_permissions(state, std::fs::Permissions::from_mode(0o500)).unwrap();
	});
}

#[cfg(unix)]
fn index_writable(disk: &Disk) {
	use std::os::unix::fs::PermissionsExt;
	std::fs::set_permissions(disk.state(), std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn a_kept_place_that_cannot_be_recorded_stops_the_unregister_until_the_next_one() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = served_with_changes(&disk, &platform, &drive).await;
	index_unwritable_at(&disk, &platform, KillPoint::After(FakeStep::SetAside));
	let reply = rig.request(unregister_request()).await;
	index_writable(&disk);
	assert_eq!(code(&reply), "io", "{reply}");
	let (_, lines) = rig.close().await;
	assert!(kept_reports(&lines).is_empty(), "{lines:?}");
	assert!(disk.data().exists(), "wiped before the record was settled");
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.request(unregister_request()).await);
	reported_once(&disk, &platform, rig).await;
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn a_record_that_cannot_be_written_leaves_the_root_registered_and_served() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = served_with_changes(&disk, &platform, &drive).await;
	index_unwritable_at(&disk, &platform, KillPoint::After(FakeStep::Disconnect));
	let reply = rig.request(unregister_request()).await;
	index_writable(&disk);
	assert_eq!(code(&reply), "io", "{reply}");
	assert_eq!(
		platform.registration(&disk.root()),
		Some(Some(ACCOUNT_ROOT))
	);
	assert!(platform.asides().is_empty());
	ok(&rig.request(unregister_request()).await);
	reported_once(&disk, &platform, rig).await;
}

#[tokio::test(start_paused = true)]
async fn an_unregister_the_os_refuses_keeps_no_record_and_serves_the_root_again() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = served_with_changes(&disk, &platform, &drive).await;
	platform.fail_unregisters(1);
	let reply = rig.request(unregister_request()).await;
	assert_eq!(code(&reply), "os", "{reply}");
	assert!(
		!index_text(&disk).contains("\"kept\""),
		"{}",
		index_text(&disk)
	);
	assert!(platform.is_connected(&disk.root()));
	ok(&rig.request(unregister_request()).await);
	reported_once(&disk, &platform, rig).await;
}

#[tokio::test(start_paused = true)]
async fn a_kill_once_the_kept_items_are_recorded_is_reported_by_the_register_replayed() {
	killed_then_registered(at(KillPoint::Before(FakeStep::Unregister))).await;
}

#[tokio::test(start_paused = true)]
async fn a_kill_before_the_root_refuses_requests_is_reported_by_the_register_replayed() {
	killed_then_registered(at(KillPoint::Before(FakeStep::ConnectRefusing))).await;
}

#[tokio::test(start_paused = true)]
async fn a_kill_once_the_root_is_disconnected_is_reported_by_the_register_replayed() {
	killed_then_registered(at(KillPoint::After(FakeStep::Disconnect))).await;
}

#[tokio::test(start_paused = true)]
async fn a_kill_just_before_the_rename_aside_is_reported_by_the_register_replayed() {
	killed_then_registered(at(KillPoint::Before(FakeStep::SetAside))).await;
}

/// `--unregister-all` killed at `point`, then run again: it reports what was kept, which a
/// helper's unregister, as the app's sweep runs it, then reports once.
async fn unregister_all_killed_at(point: KillPoint) {
	let disk = Disk::new();
	let (platform, drive) = world();
	let rig = served_with_changes(&disk, &platform, &drive).await;
	rig.close().await;
	let state = StateDir::new(disk.state()).unwrap();
	platform.kill_at(point);
	let killed = {
		let (platform, state) = (platform.clone(), state.clone());
		tokio::spawn(async move { unregister_all(platform, &state, true).await })
	};
	assert!(killed.await.unwrap_err().is_panic());
	let done = unregister_all(platform.clone(), &state, true).await;
	let report = done.roots[0].outcome.as_ref().unwrap();
	assert_eq!(report.kept_count, 3, "{report:?}");
	assert_eq!(
		report.kept_in.as_deref().and_then(std::path::Path::to_str),
		kept_place(&disk, &platform).as_deref()
	);
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.request(unregister_request()).await);
	reported_once(&disk, &platform, rig).await;
}

#[tokio::test(start_paused = true)]
async fn unregister_all_killed_once_the_kept_items_are_recorded_reports_them_on_its_next_run() {
	unregister_all_killed_at(KillPoint::Before(FakeStep::Unregister)).await;
}

#[tokio::test(start_paused = true)]
async fn unregister_all_killed_once_the_os_let_the_root_go_reports_them_on_its_next_run() {
	unregister_all_killed_at(KillPoint::After(FakeStep::Unregister)).await;
}

/// A root left where it is, which a register then refuses as not empty, and so cannot report: the
/// listing of the roots, which the desktop asks for before it registers, reports it.
#[tokio::test(start_paused = true)]
async fn a_report_not_known_read_is_listed_with_the_roots() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = served_with_changes(&disk, &platform, &drive).await;
	platform.fail_set_asides(1);
	let first = ok(&rig.request(unregister_request()).await).clone();
	rig.task.abort();
	let _ = (&mut rig.task).await;
	let mut rig = Rig::start(&disk, &platform, &drive);
	let roots = ok(&rig.request(json!({ "t": "listRoots" })).await).clone();
	let listed = &roots["roots"][0]["keptEarlier"][0];
	assert_eq!(listed["id"], first["keptId"], "{roots}");
	assert_eq!(listed["keptCount"], 3, "{roots}");
	assert_eq!(listed["keptIn"], json!(disk.root().to_str().unwrap()));
	ok(&rig.request(json!({ "t": "getState" })).await);
	let (_, lines) = rig.close().await;
	assert_eq!(
		kept_reports(&lines).len(),
		0,
		"the listing's are not counted here"
	);
	assert!(
		!index_text(&disk).contains("\"42\""),
		"{}",
		index_text(&disk)
	);
}

/// A `register` of user 42's root naming a display name and an icon.
fn register_branded(disk: &Disk, name: &str) -> Value {
	let mut message = register(disk, "42");
	message["displayName"] = json!(name);
	message["iconPath"] = json!("C:\\Program Files\\Filen\\Filen.exe");
	message["iconIndex"] = json!(0);
	message
}

/// The branding [`register_branded`] names.
fn branding(name: &str) -> Branding {
	Branding::parse(
		Some(name),
		Some("C:\\Program Files\\Filen\\Filen.exe"),
		Some(0),
		None,
	)
	.unwrap()
	.unwrap()
}

fn brand_call(disk: &Disk, branding: Branding) -> BrandingCall {
	BrandingCall::Brand {
		root: disk.root(),
		user: UserId::from(42),
		branding,
	}
}

fn unbrand_call(disk: &Disk, scope: BrandingScope) -> BrandingCall {
	BrandingCall::Unbrand {
		root: disk.root(),
		user: UserId::from(42),
		scope,
	}
}

#[tokio::test(start_paused = true)]
async fn serving_a_branded_root_shows_it_once_and_says_so() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	assert_eq!(
		ok(&rig.request(register_branded(&disk, "Filen (2)")).await),
		&json!({ "state": "registered", "resumed": false })
	);
	assert!(
		platform.branding_calls().is_empty(),
		"nothing shown before it is served"
	);
	assert_eq!(
		ok(&rig.set_auth("42", ACCOUNT_ROOT).await),
		&json!({ "state": "serving", "adopted": false, "branded": true })
	);
	assert_eq!(
		platform.branding_calls(),
		[brand_call(&disk, branding("Filen (2)"))]
	);
	assert_eq!(
		platform.machine_branding(&disk.root()),
		Some(UserId::from(42))
	);
	assert_eq!(
		platform
			.user_branding(&disk.root())
			.map(|entry| entry.branding),
		Some(branding("Filen (2)"))
	);
	// Shown between the OS's registration and the connection.
	assert_eq!(platform.connected_when_branded(), [false]);
	assert_eq!(
		platform.calls()[..2],
		[
			PlatformCall::Register {
				root: disk.root(),
				account_root: ACCOUNT_ROOT,
			},
			PlatformCall::Connect(disk.root()),
		]
	);
}

#[tokio::test(start_paused = true)]
async fn a_root_whose_register_names_no_branding_is_shown_as_filen_and_the_reply_says_nothing() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	assert_eq!(
		ok(&rig.set_auth("42", ACCOUNT_ROOT).await),
		&json!({ "state": "serving", "adopted": false })
	);
	assert_eq!(
		platform.branding_calls(),
		[brand_call(&disk, Branding::default())]
	);
}

#[tokio::test(start_paused = true)]
async fn a_branding_that_fails_is_logged_and_the_root_is_served_all_the_same() {
	let disk = Disk::new();
	let (platform, drive) = world();
	platform.fail_brands(1);
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.request(register_branded(&disk, "Filen")).await);
	assert_eq!(
		ok(&rig.set_auth("42", ACCOUNT_ROOT).await),
		&json!({ "state": "serving", "adopted": false, "branded": false })
	);
	assert!(platform.is_connected(&disk.root()));
	rig.until(|line| line["t"] == "status" && line["watching"] == true)
		.await;
}

#[tokio::test(start_paused = true)]
async fn a_root_served_again_after_a_restart_is_shown_with_the_same_values() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.request(register_branded(&disk, "Filen (2)")).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	rig.close().await;
	let mut rig = Rig::start(&disk, &platform, &drive);
	assert_eq!(
		ok(&rig.request(register_branded(&disk, "Filen (2)")).await),
		&json!({ "state": "serving", "resumed": true, "branded": true })
	);
	assert_eq!(
		platform.branding_calls(),
		[
			brand_call(&disk, branding("Filen (2)")),
			brand_call(&disk, branding("Filen (2)")),
		]
	);
	assert_eq!(platform.branding_entries(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_repeated_register_naming_a_new_name_shows_it_at_once() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.request(register_branded(&disk, "Filen")).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	assert_eq!(
		ok(&rig.request(register_branded(&disk, "Filen (2)")).await),
		&json!({ "state": "serving", "resumed": false, "branded": true })
	);
	assert_eq!(
		platform
			.user_branding(&disk.root())
			.map(|entry| entry.branding),
		Some(branding("Filen (2)"))
	);
	assert!(index_text(&disk).contains("\"displayName\":\"Filen (2)\""));
}

#[tokio::test(start_paused = true)]
async fn an_unregister_removes_the_branding_only_once_the_os_let_the_root_go() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.request(register_branded(&disk, "Filen")).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	platform.fail_unregisters(1);
	assert_eq!(code(&rig.request(unregister_request()).await), "os");
	assert_eq!(
		platform.branding_entries(),
		2,
		"a root still registered stays shown"
	);
	assert!(
		!platform
			.branding_calls()
			.iter()
			.any(|call| matches!(call, BrandingCall::Unbrand { .. }))
	);
	let report = ok(&rig.request(unregister_request()).await).clone();
	assert_eq!(report["unregistered"], true);
	assert!(report.get("brandingLeft").is_none(), "{report}");
	assert_eq!(platform.branding_entries(), 0);
	assert_eq!(
		platform.branding_calls().last(),
		Some(&unbrand_call(&disk, BrandingScope::User))
	);
}

#[tokio::test(start_paused = true)]
async fn unregister_all_removes_the_branding() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.request(register_branded(&disk, "Filen")).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	rig.close().await;
	let state = StateDir::new(disk.state()).unwrap();
	let done = unregister_all(platform.clone(), &state, true).await;
	assert!(!done.roots[0].branding_left);
	assert!(!done.line().to_line().contains("brandingLeft"));
	assert_eq!(platform.branding_entries(), 0);
}

#[tokio::test(start_paused = true)]
async fn an_elevated_unregister_all_removes_only_the_machine_entry_and_reports_the_rest_left() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.request(register_branded(&disk, "Filen")).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	rig.close().await;
	let calls_before = platform.branding_calls().len();
	let state = StateDir::new(disk.state()).unwrap();
	platform.set_elevated(true);
	let done = unregister_all(platform.clone(), &state, true).await;
	assert!(done.roots[0].branding_left);
	assert!(done.line().to_line().contains("\"brandingLeft\":true"));
	assert_eq!(platform.machine_branding(&disk.root()), None);
	assert!(platform.user_branding(&disk.root()).is_some());
	// Nothing is shown, so no icon path is read, by an elevated run.
	assert_eq!(
		platform.branding_calls()[calls_before..],
		[unbrand_call(&disk, BrandingScope::MachineOnly)]
	);
}

#[tokio::test(start_paused = true)]
async fn a_failed_removal_of_the_branding_is_reported_left_by_unregister_all() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	rig.close().await;
	platform.fail_unbrands(1);
	let state = StateDir::new(disk.state()).unwrap();
	let done = unregister_all(platform.clone(), &state, true).await;
	assert!(done.roots[0].outcome.as_ref().unwrap().unregistered);
	assert!(done.roots[0].branding_left);
	// What it left, the sweep that ends the run removed.
	assert_eq!(platform.branding_entries(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_kill_between_the_os_unregister_and_the_clean_up_is_swept_when_the_helper_starts() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let rig = served_with_changes(&disk, &platform, &drive).await;
	platform.kill_at(KillPoint::After(FakeStep::Unregister));
	killed_unregister(rig, &platform).await;
	assert_eq!(platform.registration(&disk.root()), None);
	assert_eq!(platform.branding_entries(), 2, "left by the killed run");
	let mut rig = Rig::start(&disk, &platform, &drive);
	assert_eq!(rig.line().await["t"], "hello");
	ok(&rig.request(json!({ "t": "getState" })).await);
	assert_eq!(platform.branding_entries(), 0);
	ok(&rig.request(unregister_request()).await);
	reported_once(&disk, &platform, rig).await;
}

#[tokio::test(start_paused = true)]
async fn the_sweep_leaves_a_root_the_os_still_holds_shown_and_a_restart_writes_no_second_entry() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	let (reason, _) = rig.close().await;
	assert_eq!(reason, ExitReason::InputClosed);
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	// One at each start and one at each register.
	assert_eq!(platform.branding_sweeps(), 4);
	assert_eq!(platform.branding_entries(), 2);
	assert_eq!(
		platform.machine_branding(&disk.root()),
		Some(UserId::from(42))
	);
}

#[tokio::test(start_paused = true)]
async fn left_over_entries_of_another_root_are_swept_by_unregister_all_but_not_when_elevated() {
	let disk = Disk::new();
	let (platform, _drive) = world();
	let gone = disk.0.path().join("Gone");
	platform.leave_branding(&gone, UserId::from(7));
	let state = StateDir::new(disk.state()).unwrap();
	platform.set_elevated(true);
	unregister_all(platform.clone(), &state, true).await;
	assert_eq!(platform.branding_sweeps(), 0);
	assert_eq!(platform.branding_entries(), 2);
	platform.set_elevated(false);
	unregister_all(platform.clone(), &state, true).await;
	assert_eq!(platform.branding_sweeps(), 1);
	assert_eq!(platform.branding_entries(), 0);
}

/// A `register` of user 42's root naming the context menu commands, one label translated.
fn register_with_commands(disk: &Disk) -> Value {
	let mut message = register(disk, "42");
	message["explorerCommands"] = json!({
		"appPath": "C:\\Program Files\\Filen\\Filen.exe",
		"share": "Teilen",
	});
	message
}

/// The commands [`register_with_commands`] names.
fn explorer_commands() -> ExplorerCommands {
	ExplorerCommands::parse(
		"C:\\Program Files\\Filen\\Filen.exe",
		None,
		Some("Teilen"),
		None,
		None,
	)
	.unwrap()
}

#[tokio::test(start_paused = true)]
async fn serving_a_root_whose_register_names_commands_offers_them_in_file_explorer() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.request(register_with_commands(&disk)).await);
	assert_eq!(platform.explorer_commands(), None, "nothing before serving");
	assert_eq!(
		ok(&rig.set_auth("42", ACCOUNT_ROOT).await),
		&json!({ "state": "serving", "adopted": false, "branded": true })
	);
	assert_eq!(
		platform.branding_calls(),
		[brand_call(
			&disk,
			Branding::parse(None, None, None, Some(explorer_commands()))
				.unwrap()
				.unwrap()
		)]
	);
	assert_eq!(platform.explorer_commands(), Some(explorer_commands()));
	assert!(!index_text(&disk).contains("Filen.exe"), "not stored");
}

#[tokio::test(start_paused = true)]
async fn a_register_naming_no_commands_leaves_those_an_earlier_one_offered() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.request(register_with_commands(&disk)).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	rig.close().await;
	let mut rig = Rig::start(&disk, &platform, &drive);
	assert_eq!(
		ok(&rig.register(&disk).await),
		&json!({ "state": "serving", "resumed": true })
	);
	assert_eq!(platform.branding_calls().len(), 2);
	assert_eq!(platform.explorer_commands(), Some(explorer_commands()));
}

#[tokio::test(start_paused = true)]
async fn an_unregister_of_the_last_root_removes_the_commands() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.request(register_with_commands(&disk)).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	assert_eq!(
		ok(&rig.request(unregister_request()).await)["unregistered"],
		true
	);
	assert_eq!(platform.explorer_commands(), None);
}

#[tokio::test(start_paused = true)]
async fn an_elevated_unregister_all_leaves_the_commands_and_one_as_the_user_removes_them() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.request(register_with_commands(&disk)).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	rig.close().await;
	let state = StateDir::new(disk.state()).unwrap();
	platform.set_elevated(true);
	unregister_all(platform.clone(), &state, true).await;
	assert_eq!(platform.machine_branding(&disk.root()), None);
	assert_eq!(platform.explorer_commands(), Some(explorer_commands()));
	platform.set_elevated(false);
	unregister_all(platform.clone(), &state, true).await;
	assert_eq!(platform.explorer_commands(), None);
}

#[tokio::test(start_paused = true)]
async fn the_commands_a_run_killed_after_the_os_unregister_left_are_swept_when_the_helper_starts() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.request(register_with_commands(&disk)).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	platform.kill_at(KillPoint::After(FakeStep::Unregister));
	killed_unregister(rig, &platform).await;
	assert_eq!(platform.registration(&disk.root()), None);
	assert_eq!(
		platform.explorer_commands(),
		Some(explorer_commands()),
		"left by the killed run"
	);
	let mut rig = Rig::start(&disk, &platform, &drive);
	assert_eq!(rig.line().await["t"], "hello");
	ok(&rig.request(json!({ "t": "getState" })).await);
	assert_eq!(platform.explorer_commands(), None);
}

const DIR_A: Uuid = Uuid::from_u128(0x201);
const DIR_B: Uuid = Uuid::from_u128(0x202);
const FILE_STABLE: Uuid = Uuid::from_u128(0x302);
const FILE_VERSION: Uuid = Uuid::from_u128(0x303);

/// `a/b/f.txt` on the drive and in the root, as placeholders: the directories `a` ([`DIR_A`]) and
/// `a/b` ([`DIR_B`]) and the file ([`FILE_STABLE`] at [`FILE_VERSION`]), not on disk.
fn identify_world() -> (FakePlatform, Arc<FakeDrive>) {
	let (platform, drive) = world();
	let mut a = ffi_dir(DIR_A, "a");
	a.parent = ACCOUNT_ROOT.to_string();
	let mut b = ffi_dir(DIR_B, "b");
	b.parent = DIR_A.to_string();
	let mut file = ffi_file(FILE_STABLE, FILE_VERSION, "f.txt", 4);
	file.parent = DIR_B.to_string();
	drive.list(&ACCOUNT_ROOT.to_string(), vec![FfiNonRootObject::Dir(a)]);
	drive.list(&format!("stable/{DIR_A}"), vec![FfiNonRootObject::Dir(b)]);
	drive.list(
		&format!("stable/{DIR_B}"),
		vec![FfiNonRootObject::File(file)],
	);
	drive.add_file(FILE_STABLE, FILE_VERSION, b"data");
	let local = platform.local();
	local.place_dir(&root_path("a"), PlaceholderIdentity::Dir { uuid: DIR_A });
	local.place_dir(&root_path("a\\b"), PlaceholderIdentity::Dir { uuid: DIR_B });
	local.place_file_with(
		&root_path("a\\b\\f.txt"),
		PlaceholderIdentity::File {
			stable: FILE_STABLE,
			version: FILE_VERSION,
		},
		b"data",
		false,
	);
	(platform, drive)
}

/// A helper serving the root of [`identify_world`].
async fn identifying(disk: &Disk, platform: &FakePlatform, drive: &Arc<FakeDrive>) -> Rig {
	let mut rig = Rig::start(disk, platform, drive);
	ok(&rig.register(disk).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	rig
}

/// `identify` of `path`, as the parent words it.
fn identify(path: &std::path::Path) -> Value {
	json!({ "t": "identify", "path": path.to_str().unwrap() })
}

#[tokio::test(start_paused = true)]
async fn identify_before_register_is_refused_as_not_registered() {
	let disk = Disk::new();
	let (platform, drive) = identify_world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	let reply = rig.request(identify(&disk.root().join("a"))).await;
	assert_eq!(code(&reply), "notRegistered");
}

#[tokio::test(start_paused = true)]
async fn identify_of_a_root_registered_but_not_served_is_refused_as_not_serving() {
	let disk = Disk::new();
	let (platform, drive) = identify_world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	let reply = rig.request(identify(&disk.root().join("a"))).await;
	assert_eq!(code(&reply), "notServing");
	assert_eq!(reply["error"]["message"], "the root is not served");
}

#[tokio::test(start_paused = true)]
async fn identify_of_a_nested_file_names_its_version_its_stable_id_and_its_directories() {
	let disk = Disk::new();
	let (platform, drive) = identify_world();
	let mut rig = identifying(&disk, &platform, &drive).await;
	let reply = rig
		.request(identify(&disk.root().join("a").join("b").join("f.txt")))
		.await;
	assert_eq!(
		ok(&reply),
		&json!({
			"kind": "file",
			"uuid": FILE_VERSION.to_string(),
			"stableUuid": FILE_STABLE.to_string(),
			"parents": [DIR_A.to_string(), DIR_B.to_string()],
		})
	);
	// Answered from the placeholder and the cache: nothing read, nothing downloaded.
	assert_eq!(drive.reads(), Vec::<(u64, u64)>::new());
	assert_eq!(
		platform
			.local()
			.entry(&root_path("a\\b\\f.txt"))
			.map(|entry| entry.on_disk),
		Some(0)
	);
}

#[tokio::test(start_paused = true)]
async fn identify_of_a_directory_names_the_directories_above_it() {
	let disk = Disk::new();
	let (platform, drive) = identify_world();
	let mut rig = identifying(&disk, &platform, &drive).await;
	let reply = rig
		.request(identify(&disk.root().join("a").join("b")))
		.await;
	assert_eq!(
		ok(&reply),
		&json!({ "kind": "dir", "uuid": DIR_B.to_string(), "parents": [DIR_A.to_string()] })
	);
}

#[tokio::test(start_paused = true)]
async fn identify_of_the_root_names_the_account_root() {
	let disk = Disk::new();
	let (platform, drive) = identify_world();
	let mut rig = identifying(&disk, &platform, &drive).await;
	let reply = rig.request(identify(&disk.root())).await;
	assert_eq!(
		ok(&reply),
		&json!({ "kind": "root", "uuid": ACCOUNT_ROOT.to_string(), "parents": [] })
	);
}

#[tokio::test(start_paused = true)]
async fn identify_of_a_path_outside_the_root_is_refused_as_not_in_root() {
	let disk = Disk::new();
	let (platform, drive) = identify_world();
	let mut rig = identifying(&disk, &platform, &drive).await;
	let root = disk.root();
	let sibling = format!("{}2/a", root.to_str().unwrap());
	let dotted = format!("{}/a/../a", root.to_str().unwrap());
	for path in [
		disk.0.path().join("elsewhere.txt").to_str().unwrap(),
		sibling.as_str(),
		dotted.as_str(),
		"a/b/f.txt",
	] {
		let reply = rig.request(json!({ "t": "identify", "path": path })).await;
		assert_eq!(code(&reply), "notInRoot", "{path}");
	}
}

#[tokio::test(start_paused = true)]
async fn identify_of_an_ordinary_file_is_refused_as_not_synced() {
	let disk = Disk::new();
	let (platform, drive) = identify_world();
	let mut rig = identifying(&disk, &platform, &drive).await;
	platform.local().user_create(&root_path("new.txt"), b"new");
	let reply = rig.request(identify(&disk.root().join("new.txt"))).await;
	assert_eq!(code(&reply), "notSynced");
}

#[tokio::test(start_paused = true)]
async fn identify_of_a_file_set_apart_from_the_drive_is_refused_as_not_synced() {
	let disk = Disk::new();
	let (platform, drive) = identify_world();
	platform.local().place_file(
		&root_path("a\\copy.txt"),
		PlaceholderIdentity::Detached {
			nonce: Uuid::from_u128(0x900),
		},
		b"mine",
	);
	let mut rig = identifying(&disk, &platform, &drive).await;
	let reply = rig
		.request(identify(&disk.root().join("a").join("copy.txt")))
		.await;
	assert_eq!(code(&reply), "notSynced");
}

#[tokio::test(start_paused = true)]
async fn identify_of_nothing_is_refused_as_not_found() {
	let disk = Disk::new();
	let (platform, drive) = identify_world();
	let mut rig = identifying(&disk, &platform, &drive).await;
	let reply = rig.request(identify(&disk.root().join("gone.txt"))).await;
	assert_eq!(code(&reply), "notFound");
}

#[tokio::test(start_paused = true)]
async fn identify_of_a_placeholder_the_cache_does_not_hold_is_refused_as_unknown() {
	let disk = Disk::new();
	let (platform, drive) = identify_world();
	platform.local().place_file_with(
		&root_path("a\\stray.txt"),
		PlaceholderIdentity::File {
			stable: Uuid::from_u128(0x700),
			version: Uuid::from_u128(0x701),
		},
		b"x",
		false,
	);
	let mut rig = identifying(&disk, &platform, &drive).await;
	let reply = rig
		.request(identify(&disk.root().join("a").join("stray.txt")))
		.await;
	assert_eq!(code(&reply), "unknownItem");
}

#[tokio::test(start_paused = true)]
async fn identify_of_a_file_the_drive_holds_in_the_trash_is_refused_as_unknown() {
	let disk = Disk::new();
	let (platform, drive) = identify_world();
	drive.remote_trash(FILE_STABLE);
	let mut rig = identifying(&disk, &platform, &drive).await;
	let reply = rig
		.request(identify(&disk.root().join("a").join("b").join("f.txt")))
		.await;
	assert_eq!(code(&reply), "unknownItem");
}

#[tokio::test(start_paused = true)]
async fn identify_of_a_file_in_a_trashed_directory_is_refused_as_unknown() {
	let disk = Disk::new();
	let (platform, drive) = identify_world();
	drive.remote_trash(DIR_A);
	let mut rig = identifying(&disk, &platform, &drive).await;
	let reply = rig
		.request(identify(&disk.root().join("a").join("b").join("f.txt")))
		.await;
	assert_eq!(code(&reply), "unknownItem");
}

#[tokio::test(start_paused = true)]
async fn identify_when_the_cache_cannot_be_read_answers_io_without_its_cause() {
	let disk = Disk::new();
	let (platform, drive) = identify_world();
	let mut rig = identifying(&disk, &platform, &drive).await;
	drive.fail_query(CacheError::remote("the server said secret"));
	let reply = rig
		.request(identify(&disk.root().join("a").join("b").join("f.txt")))
		.await;
	assert_eq!(code(&reply), "io");
	assert_eq!(reply["error"]["message"], "reading the cache failed");
}

#[tokio::test(start_paused = true)]
async fn identify_in_a_directory_cycle_stops_at_the_hop_bound_as_internal() {
	let disk = Disk::new();
	let (platform, drive) = identify_world();
	let (c, d) = (Uuid::from_u128(0x501), Uuid::from_u128(0x502));
	let mut dir_c = ffi_dir(c, "c");
	dir_c.parent = d.to_string();
	let mut dir_d = ffi_dir(d, "d");
	dir_d.parent = c.to_string();
	drive.list(
		&format!("stable/{}", Uuid::from_u128(0x500)),
		vec![FfiNonRootObject::Dir(dir_c), FfiNonRootObject::Dir(dir_d)],
	);
	platform
		.local()
		.place_dir(&root_path("a\\c"), PlaceholderIdentity::Dir { uuid: c });
	let mut rig = identifying(&disk, &platform, &drive).await;
	let reply = rig
		.request(identify(&disk.root().join("a").join("c")))
		.await;
	assert_eq!(code(&reply), "internal");
	assert_eq!(
		reply["error"]["message"],
		"the item is deeper than any directory tree"
	);
}

#[tokio::test(start_paused = true)]
async fn identify_below_a_parent_that_is_a_file_answers_internal() {
	let disk = Disk::new();
	let (platform, drive) = identify_world();
	let odd = Uuid::from_u128(0x601);
	let mut dir = ffi_dir(odd, "odd");
	dir.parent = FILE_STABLE.to_string();
	drive.list(
		&format!("stable/{}", Uuid::from_u128(0x600)),
		vec![FfiNonRootObject::Dir(dir)],
	);
	platform
		.local()
		.place_dir(&root_path("a\\odd"), PlaceholderIdentity::Dir { uuid: odd });
	let mut rig = identifying(&disk, &platform, &drive).await;
	let reply = rig
		.request(identify(&disk.root().join("a").join("odd")))
		.await;
	assert_eq!(code(&reply), "internal");
	assert_eq!(
		reply["error"]["message"],
		"an item's parent is not a directory"
	);
}

#[tokio::test(start_paused = true)]
async fn identify_of_a_root_identity_below_the_root_is_refused_as_not_synced() {
	let disk = Disk::new();
	let (platform, drive) = identify_world();
	platform.local().place_dir(
		&root_path("a\\nested"),
		PlaceholderIdentity::Root { root: ACCOUNT_ROOT },
	);
	let mut rig = identifying(&disk, &platform, &drive).await;
	let reply = rig
		.request(identify(&disk.root().join("a").join("nested")))
		.await;
	assert_eq!(code(&reply), "notSynced");
}

#[tokio::test(start_paused = true)]
async fn identify_of_a_file_placeholder_the_cache_holds_as_a_directory_answers_internal() {
	let disk = Disk::new();
	let (platform, drive) = identify_world();
	platform.local().place_file_with(
		&root_path("a\\posing.txt"),
		PlaceholderIdentity::File {
			stable: DIR_B,
			version: Uuid::from_u128(0x801),
		},
		b"x",
		false,
	);
	let mut rig = identifying(&disk, &platform, &drive).await;
	let reply = rig
		.request(identify(&disk.root().join("a").join("posing.txt")))
		.await;
	assert_eq!(code(&reply), "internal");
	assert_eq!(
		reply["error"]["message"],
		"the cache holds the placeholder's item as another kind"
	);
}

#[tokio::test(start_paused = true)]
async fn a_build_without_the_com_layer_serves_and_names_no_thumbnail_class() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	assert_eq!(platform.thumbnail_securings(), 0);
	assert_eq!(platform.thumbnail_servers(), (0, 0));
	assert_eq!(platform.thumbnail_withdrawals(), 0);
	assert!(!platform.thumbnail_provider(&disk.root()));
	assert!(!platform.thumbnail_class());
}

/// Yields until `done` holds, for a real minute at most, then fails saying `what`. Yields rather
/// than sleeps: while a thumbnail server's start is held on the blocking pool, the paused clock
/// does not move.
async fn eventually(what: &str, done: impl Fn() -> bool) {
	let started = std::time::Instant::now();
	while !done() {
		assert!(
			started.elapsed() < Duration::from_secs(60),
			"never happened: {what}"
		);
		yield_now().await;
	}
}

#[tokio::test(start_paused = true)]
async fn a_helper_whose_thumbnail_server_fails_withdraws_the_class_from_every_root() {
	let disk = Disk::new();
	let (platform, drive) = world();
	platform.implement_thumbnails(true);
	platform.fail_thumbnail_servers();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	eventually("the class was withdrawn", || {
		platform.thumbnail_withdrawals() == 1
	})
	.await;
	assert_eq!(platform.thumbnail_servers(), (0, 0));
	assert!(!platform.thumbnail_provider(&disk.root()));
}

#[tokio::test(start_paused = true)]
async fn a_helper_whose_thumbnail_security_failed_starts_no_server_and_withdraws_the_class() {
	let disk = Disk::new();
	let (platform, drive) = world();
	platform.implement_thumbnails(true);
	platform.fail_thumbnail_security();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	eventually("the class was withdrawn", || {
		platform.thumbnail_withdrawals() == 1
	})
	.await;
	assert_eq!(platform.thumbnail_securings(), 1);
	assert_eq!(platform.thumbnail_servers(), (0, 0));
	assert!(platform.thumbnail_handler().is_none());
	assert!(!platform.thumbnail_provider(&disk.root()));
	assert!(!platform.thumbnail_class());
	ok(&rig.request(json!({ "t": "shutdown" })).await);
	assert_eq!(rig.task.await.unwrap(), ExitReason::Shutdown);
	assert_eq!(platform.thumbnail_servers(), (0, 0));
	assert_eq!(platform.thumbnail_withdrawals(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_panic_answering_the_thumbnail_servers_start_stops_the_helper_in_order() {
	let disk = Disk::new();
	let (platform, drive) = world();
	platform.implement_thumbnails(true);
	platform.fail_thumbnail_servers();
	platform.panic_on_next_thumbnail_withdrawal();
	let rig = Rig::start(&disk, &platform, &drive);
	assert_eq!(rig.task.await.unwrap(), ExitReason::Internal);
}

#[tokio::test(start_paused = true)]
async fn the_parent_is_answered_while_the_thumbnail_server_still_starts() {
	let disk = Disk::new();
	let (platform, drive) = world();
	platform.implement_thumbnails(true);
	let hold = platform.hold_thumbnail_starts();
	let mut rig = Rig::start(&disk, &platform, &drive);
	assert_eq!(rig.line().await["t"], "hello");
	eventually("the start reached the hold", || {
		platform.thumbnail_starts_held() == 1
	})
	.await;

	ok(&rig.request(json!({ "t": "listRoots" })).await);
	ok(&rig.register(&disk).await);
	assert_eq!(platform.thumbnail_starts_held(), 1, "still starting");
	assert_eq!(platform.thumbnail_servers(), (0, 0));

	hold.release();
	ok(&rig.request(json!({ "t": "shutdown" })).await);
	assert_eq!(rig.task.await.unwrap(), ExitReason::Shutdown);
	// Stopped by the helper's stop, or by the start itself when it answered after the stop.
	eventually("the server was started and stopped", || {
		platform.thumbnail_servers() == (1, 1)
	})
	.await;
}

#[tokio::test(start_paused = true)]
async fn a_root_served_while_the_thumbnail_server_starts_names_its_class_once_it_started() {
	let disk = Disk::new();
	let (platform, drive) = world();
	platform.implement_thumbnails(true);
	let hold = platform.hold_thumbnail_starts();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	// Branded with no server running: no value naming the class yet.
	assert!(platform.user_branding(&disk.root()).is_some());
	assert!(!platform.thumbnail_provider(&disk.root()));
	assert!(!platform.thumbnail_class());

	hold.release();
	eventually("the served root names the class", || {
		platform.thumbnail_provider(&disk.root())
	})
	.await;
	assert!(platform.thumbnail_class());
	assert_eq!(platform.thumbnail_servers(), (1, 0));
	assert_eq!(platform.thumbnail_withdrawals(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_thumbnail_server_start_that_fails_late_withdraws_the_class() {
	let disk = Disk::new();
	let (platform, drive) = world();
	platform.implement_thumbnails(true);
	platform.fail_thumbnail_servers();
	let hold = platform.hold_thumbnail_starts();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	assert_eq!(platform.thumbnail_withdrawals(), 0, "not answered yet");

	hold.release();
	eventually("the class was withdrawn", || {
		platform.thumbnail_withdrawals() == 1
	})
	.await;
	assert_eq!(platform.thumbnail_servers(), (0, 0));
	assert!(!platform.thumbnail_provider(&disk.root()));
	ok(&rig.request(json!({ "t": "shutdown" })).await);
	assert_eq!(rig.task.await.unwrap(), ExitReason::Shutdown);
	assert_eq!(platform.thumbnail_withdrawals(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_stop_while_the_thumbnail_server_starts_ends_and_stops_the_server_started_late() {
	let disk = Disk::new();
	let (platform, drive) = world();
	platform.implement_thumbnails(true);
	let hold = platform.hold_thumbnail_starts();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	eventually("the start reached the hold", || {
		platform.thumbnail_starts_held() == 1
	})
	.await;

	ok(&rig.request(json!({ "t": "shutdown" })).await);
	assert_eq!(rig.task.await.unwrap(), ExitReason::Shutdown);
	assert_eq!(
		platform.thumbnail_servers(),
		(0, 0),
		"the stop did not wait"
	);

	hold.release();
	eventually("the late server was stopped", || {
		platform.thumbnail_servers() == (1, 1)
	})
	.await;
	assert!(platform.thumbnail_handler().is_none());
	assert!(!platform.thumbnail_provider(&disk.root()));
}

#[tokio::test(start_paused = true)]
async fn a_build_serving_thumbnails_names_its_class_for_the_root_until_it_is_unregistered() {
	let disk = Disk::new();
	let (platform, drive) = world();
	platform.implement_thumbnails(true);
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	eventually("the served root names the class", || {
		platform.thumbnail_provider(&disk.root())
	})
	.await;
	assert_eq!(platform.thumbnail_servers(), (1, 0));
	assert!(platform.thumbnail_handler().is_some());
	assert!(platform.thumbnail_provider(&disk.root()));
	assert!(platform.thumbnail_class());

	ok(&rig.request(unregister_request()).await);
	assert!(!platform.thumbnail_provider(&disk.root()));
	assert!(!platform.thumbnail_class());
	assert_eq!(
		ok(&rig.request(json!({ "t": "shutdown" })).await),
		&json!({})
	);
	assert_eq!(rig.task.await.unwrap(), ExitReason::Shutdown);
	assert_eq!(platform.thumbnail_servers(), (1, 1));
	assert!(platform.thumbnail_handler().is_none());
}

#[tokio::test(start_paused = true)]
async fn a_thumbnail_class_left_by_a_stopped_run_is_swept_at_the_next_start() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let gone = disk.0.path().join("Gone");
	platform.leave_branding(&gone, UserId::from(7));
	platform.leave_thumbnails(&gone);
	let mut rig = Rig::start(&disk, &platform, &drive);
	assert_eq!(rig.line().await["t"], "hello");
	ok(&rig.request(json!({ "t": "listRoots" })).await);
	assert!(!platform.thumbnail_provider(&gone));
	assert!(!platform.thumbnail_class());
}

/// A helper serving thumbnails for the root, the class named for it.
async fn serving_thumbnails(disk: &Disk, platform: &FakePlatform, drive: &Arc<FakeDrive>) -> Rig {
	platform.implement_thumbnails(true);
	let mut rig = Rig::start(disk, platform, drive);
	ok(&rig.register(disk).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	eventually("the served root names the class", || {
		platform.thumbnail_provider(&disk.root())
	})
	.await;
	rig
}

#[tokio::test(start_paused = true)]
async fn signing_out_withdraws_the_thumbnail_class_from_the_root_until_a_sign_in() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = serving_thumbnails(&disk, &platform, &drive).await;

	ok(&rig
		.request(json!({ "t": "clearAuth", "userId": "42" }))
		.await);
	assert!(
		!platform.thumbnail_provider(&disk.root()),
		"File Explorer's own thumbnails while signed out"
	);
	assert!(platform.thumbnail_class(), "the class stays with the entry");
	assert_eq!(platform.root_thumbnail_withdrawals(), 1);
	assert_eq!(platform.thumbnail_withdrawals(), 0, "only this root's");

	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	assert!(platform.thumbnail_provider(&disk.root()), "named again");
}

#[tokio::test(start_paused = true)]
async fn a_helper_that_only_stops_leaves_the_thumbnail_class_named_for_the_root() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = serving_thumbnails(&disk, &platform, &drive).await;

	ok(&rig.request(json!({ "t": "shutdown" })).await);
	assert_eq!(rig.task.await.unwrap(), ExitReason::Shutdown);
	assert!(
		platform.thumbnail_provider(&disk.root()),
		"the instance COM starts answers until the next helper serves"
	);
	assert!(platform.thumbnail_class());
	assert_eq!(platform.root_thumbnail_withdrawals(), 0);
}

#[tokio::test(start_paused = true)]
async fn signing_out_of_a_root_not_served_withdraws_the_class_an_earlier_run_named() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = serving_thumbnails(&disk, &platform, &drive).await;
	ok(&rig.request(json!({ "t": "shutdown" })).await);
	assert_eq!(rig.task.await.unwrap(), ExitReason::Shutdown);
	// The session went while no helper ran: the next one registers the root without serving it.
	std::fs::remove_file(disk.data().join("auth.json")).unwrap();
	let mut rig = Rig::start(&disk, &platform, &drive);
	let registered = ok(&rig.register(&disk).await).clone();
	assert_eq!(registered["state"], "registered");
	assert!(platform.thumbnail_provider(&disk.root()));

	ok(&rig
		.request(json!({ "t": "clearAuth", "userId": "42" }))
		.await);
	assert!(!platform.thumbnail_provider(&disk.root()));
	assert_eq!(platform.root_thumbnail_withdrawals(), 1);
}

// ---- a stop during an unregister ----
//
// The parent's `shutdown` (or its closed input) reaches the helper while an unregister walks the
// root or fetches what it keeps: the unregister answers `shuttingDown` at its next safe point, the
// root stays registered with its session, and nothing is kept aside; once the items are being
// settled it runs to its end instead.

/// The changed files not all on disk the stop tests put in the served root, in the order the
/// unregister fetches them.
const PARTIAL_EDITS: [&str; 2] = ["a.txt", "b.txt"];

/// A served root whose engine walked it, then holding [`PARTIAL_EDITS`], which the engine never
/// sees: only the unregister fetches them.
async fn served_with_partial_edits(
	disk: &Disk,
	platform: &FakePlatform,
	drive: &Arc<FakeDrive>,
) -> Rig {
	let mut rig = Rig::start(disk, platform, drive);
	ok(&rig.register(disk).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	rig.until(|line| line["t"] == "pendingUploads" && line["count"] == 0)
		.await;
	let local = platform.local();
	for (n, name) in (10..).zip(PARTIAL_EDITS) {
		let path = root_path(name);
		local.swallow_events(&path);
		local.place_file(&path, file_identity(n), b"old data");
		local.user_write(&path, b"new data").unwrap();
		local.set_on_disk(&path, 3);
	}
	rig
}

/// Sends an unregister held at `call` and, once it reached it, a shutdown; lets the reader take
/// the shutdown before the gate opens. The two requests' ids.
async fn unregister_then_shut_down(
	rig: &mut Rig,
	platform: &FakePlatform,
	call: Gated,
) -> (u64, u64) {
	let gate = platform.local().gate(call);
	let unregister = rig.send(unregister_request()).await;
	{
		let gate = gate.clone();
		blocking(move || gate.wait_entered()).await;
	}
	let shutdown = rig.send(json!({ "t": "shutdown" })).await;
	// The runtime has one thread: the reader takes the shutdown while this task yields.
	for _ in 0..100 {
		yield_now().await;
	}
	gate.open();
	(unregister, shutdown)
}

/// The unregister answered `shuttingDown` and the helper stopped, leaving the root registered
/// with its session, nothing kept and no item made ordinary.
async fn stopped_before_settling(
	mut rig: Rig,
	disk: &Disk,
	platform: &FakePlatform,
	(unregister, shutdown): (u64, u64),
) {
	assert_eq!(code(&rig.reply_to(unregister).await), "shuttingDown");
	ok(&rig.reply_to(shutdown).await);
	assert_eq!(rig.task.await.unwrap(), ExitReason::Shutdown);
	assert!(
		!platform.calls().iter().any(|call| matches!(
			call,
			PlatformCall::ConnectRefusing(_) | PlatformCall::Unregister(_)
		)),
		"{:?}",
		platform.calls()
	);
	assert_eq!(
		platform.registration(&disk.root()),
		Some(Some(ACCOUNT_ROOT))
	);
	assert!(disk.has_session());
	assert!(!index_text(disk).contains("kept"), "{}", index_text(disk));
}

/// Whether every one of [`PARTIAL_EDITS`] is still a placeholder not all on disk.
fn partial_edits_untouched(platform: &FakePlatform) -> bool {
	PARTIAL_EDITS.iter().all(|name| {
		platform
			.local()
			.entry(&root_path(name))
			.is_some_and(|entry| entry.placeholder && entry.on_disk == 3)
	})
}

/// The files the helper fetched, in order.
fn hydrated(platform: &FakePlatform) -> Vec<String> {
	platform
		.local()
		.ops()
		.iter()
		.filter_map(|op| match op {
			LocalOp::Hydrated(path) => Some(path.to_string()),
			_ => None,
		})
		.collect()
}

#[tokio::test(start_paused = true)]
async fn a_shutdown_during_the_fetches_before_an_unregister_answers_shutting_down() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = served_with_partial_edits(&disk, &platform, &drive).await;
	let ids = unregister_then_shut_down(&mut rig, &platform, Gated::Hydrate).await;
	stopped_before_settling(rig, &disk, &platform, ids).await;
	assert_eq!(hydrated(&platform), ["a.txt"], "no file after the stop");
}

#[tokio::test(start_paused = true)]
async fn a_shutdown_during_the_walk_before_an_unregister_answers_shutting_down() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = served_with_partial_edits(&disk, &platform, &drive).await;
	let ids = unregister_then_shut_down(&mut rig, &platform, Gated::ListDir).await;
	stopped_before_settling(rig, &disk, &platform, ids).await;
	assert_eq!(hydrated(&platform), Vec::<String>::new());
	assert!(partial_edits_untouched(&platform));
}

#[tokio::test(start_paused = true)]
async fn a_shutdown_while_the_unregister_waits_for_the_sign_in_unregisters_nothing() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	rig.close().await;
	drive.fail_root_always(Some(unauthenticated));
	let mut rig = Rig::start(&disk, &platform, &drive);
	let started = tokio::time::Instant::now();
	let unregister = rig.send(unregister_request()).await;
	sleep(Duration::from_secs(2)).await;
	let shutdown = rig.send(json!({ "t": "shutdown" })).await;
	stopped_before_settling(rig, &disk, &platform, (unregister, shutdown)).await;
	assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test(start_paused = true)]
async fn a_shutdown_once_the_items_are_settled_lets_the_unregister_finish() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = served_with_changes(&disk, &platform, &drive).await;
	let (unregister, shutdown) =
		unregister_then_shut_down(&mut rig, &platform, Gated::Revert).await;
	let report = ok(&rig.reply_to(unregister).await).clone();
	assert_eq!(report["unregistered"], true, "{report}");
	assert_eq!(report["keptCount"], 3, "{report}");
	ok(&rig.reply_to(shutdown).await);
	assert_eq!(rig.task.await.unwrap(), ExitReason::Shutdown);
	assert_eq!(platform.registration(&disk.root()), None);
	assert!(
		kept_place(&disk, &platform).is_some(),
		"the kept files were lost"
	);
}

#[tokio::test(start_paused = true)]
async fn an_unregister_a_shutdown_stopped_finishes_on_the_next_run() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = served_with_partial_edits(&disk, &platform, &drive).await;
	let ids = unregister_then_shut_down(&mut rig, &platform, Gated::Hydrate).await;
	stopped_before_settling(rig, &disk, &platform, ids).await;
	let mut rig = Rig::start(&disk, &platform, &drive);
	let report = ok(&rig.request(unregister_request()).await).clone();
	assert_eq!(report["unregistered"], true, "{report}");
	assert_eq!(
		report["kept"],
		json!([
			{ "path": "a.txt", "reason": "modified" },
			{ "path": "b.txt", "reason": "modified" },
		]),
		"{report}"
	);
	assert_eq!(platform.registration(&disk.root()), None);
	assert!(!disk.has_session());
}

// ---- a held root ----
//
// An unregister asked to hold the root holds it before anything else: still served with the
// signed-out user's session, so its changes go on up to the drive, but closed to every other
// process's download, and so until a sign-in of the user or the unregister succeeding.

fn held_unregister() -> Value {
	json!({ "t": "unregister", "userId": "42", "deleteHydrated": true, "hold": true })
}

/// A served root holding an edit not all on disk that no fetch completes, held by an unregister
/// that answered `unsyncedChanges`.
async fn held_with_an_unfetched_edit(
	disk: &Disk,
	platform: &FakePlatform,
	drive: &Arc<FakeDrive>,
) -> Rig {
	let mut rig = Rig::start(disk, platform, drive);
	ok(&rig.register(disk).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	rig.until(|line| line["t"] == "pendingUploads" && line["count"] == 0)
		.await;
	let local = platform.local();
	local.place_file(&root_path("edited.txt"), file_identity(3), b"old data");
	local
		.user_write(&root_path("edited.txt"), b"new data")
		.unwrap();
	local.set_on_disk(&root_path("edited.txt"), 3);
	local.fail_hydrations(Some(LocalError::Busy));
	assert_eq!(
		code(&rig.request(held_unregister()).await),
		"unsyncedChanges"
	);
	rig
}

fn is_status(state: &'static str) -> impl Fn(&Value) -> bool {
	move |line| line["t"] == "status" && line["state"] == state
}

#[tokio::test(start_paused = true)]
async fn an_unregister_that_holds_a_root_it_cannot_unregister_keeps_it_served_and_held() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = held_with_an_unfetched_edit(&disk, &platform, &drive).await;
	rig.until(is_status("held")).await;
	let state = ok(&rig.request(json!({ "t": "getState" })).await).clone();
	assert_eq!(state["state"], "held", "{state}");
	assert_eq!(state["userId"], "42");
	let roots = ok(&rig.request(json!({ "t": "listRoots" })).await).clone();
	assert_eq!(roots["roots"][0]["serving"], true, "{roots}");
	assert_eq!(roots["roots"][0]["held"], true, "{roots}");
	assert!(
		index_text(&disk).contains(r#""held":true"#),
		"{}",
		index_text(&disk)
	);
	assert!(platform.is_connected(&disk.root()));
	assert!(
		!platform.calls().iter().any(|call| matches!(
			call,
			PlatformCall::ConnectRefusing(_) | PlatformCall::Disconnect(_)
		)),
		"still served: {:?}",
		platform.calls()
	);
	assert!(disk.has_session());
	let identified = rig
		.request(json!({
			"t": "identify",
			"path": disk.root().join("edited.txt").to_str().unwrap(),
		}))
		.await;
	assert_eq!(code(&identified), "notServing");
	assert_eq!(identified["error"]["message"], "the root is held");
}

#[tokio::test(start_paused = true)]
async fn a_held_root_refuses_another_users_register_and_answers_its_own_as_held() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = held_with_an_unfetched_edit(&disk, &platform, &drive).await;
	let mut other = register(&disk, "43");
	other["root"] = json!(disk.0.path().join("Other").to_str().unwrap());
	other["dataDir"] = json!(disk.0.path().join("data43").to_str().unwrap());
	assert_eq!(code(&rig.request(other).await), "alreadyRegistered");
	assert_eq!(
		ok(&rig.register(&disk).await),
		&json!({ "state": "held", "resumed": false })
	);
}

#[tokio::test(start_paused = true)]
async fn a_helper_started_anew_serves_a_held_root_held_with_no_request() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let rig = held_with_an_unfetched_edit(&disk, &platform, &drive).await;
	rig.close().await;
	let connects = |platform: &FakePlatform| {
		platform
			.calls()
			.iter()
			.filter(|call| matches!(call, PlatformCall::Connect(_)))
			.count()
	};
	assert_eq!(connects(&platform), 1);
	let mut rig = Rig::start(&disk, &platform, &drive);
	rig.until(is_status("held")).await;
	assert_eq!(connects(&platform), 2);
	assert!(platform.is_connected(&disk.root()));
	let state = ok(&rig.request(json!({ "t": "getState" })).await).clone();
	assert_eq!(state["state"], "held", "{state}");
}

#[tokio::test(start_paused = true)]
async fn a_held_root_whose_session_does_not_sign_in_stays_registered_held_after_a_restart() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let rig = held_with_an_unfetched_edit(&disk, &platform, &drive).await;
	rig.close().await;
	drive.fail_root_always(Some(unauthenticated));
	let started = tokio::time::Instant::now();
	let mut rig = Rig::start(&disk, &platform, &drive);
	let state = ok(&rig.request(json!({ "t": "getState" })).await).clone();
	assert!(
		started.elapsed() <= Duration::from_secs(6),
		"waited {:?}",
		started.elapsed()
	);
	assert_eq!(state["state"], "held", "{state}");
	assert!(!platform.is_connected(&disk.root()));
	assert!(index_text(&disk).contains(r#""held":true"#));
}

#[tokio::test(start_paused = true)]
async fn a_request_at_start_ends_the_wait_for_a_held_roots_sign_in() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let rig = held_with_an_unfetched_edit(&disk, &platform, &drive).await;
	rig.close().await;
	drive.fail_root_always(Some(unauthenticated));
	let started = tokio::time::Instant::now();
	let mut rig = Rig::start(&disk, &platform, &drive);
	let state = ok(&rig.request(json!({ "t": "getState" })).await).clone();
	assert!(
		started.elapsed() < Duration::from_secs(1),
		"waited {:?}",
		started.elapsed()
	);
	assert_eq!(state["state"], "held", "{state}");
	assert!(!platform.is_connected(&disk.root()));
}

/// A file of 4 KiB the drive holds, which another process reads in [`foreign_read`].
fn readable_file(drive: &FakeDrive) -> PlaceholderIdentity {
	drive.add_file(Uuid::from_u128(7), Uuid::from_u128(1007), &[7; 4096]);
	file_identity(7)
}

/// How another process's read of `file` at the served root ends, as the dispatcher the root was
/// connected with answers it.
async fn foreign_read(
	platform: &FakePlatform,
	disk: &Disk,
	file: PlaceholderIdentity,
) -> FakeEvent {
	let (os, key) = platform
		.foreign_read(&disk.root(), file)
		.expect("the root is served");
	for _ in 0..1000 {
		let end = os.events().into_iter().find(|event| match event {
			FakeEvent::Completed(k)
			| FakeEvent::Incomplete(k)
			| FakeEvent::Failed(k, _)
			| FakeEvent::Cancelled(k)
			| FakeEvent::Abandoned(k) => *k == key,
			_ => false,
		});
		if let Some(end) = end {
			return end;
		}
		sleep(Duration::from_millis(10)).await;
	}
	panic!("the read never ended");
}

#[tokio::test(start_paused = true)]
async fn another_processs_read_of_a_held_root_is_refused_until_a_sign_in_lifts_the_hold() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let file = readable_file(&drive);
	let mut rig = held_with_an_unfetched_edit(&disk, &platform, &drive).await;
	assert!(matches!(
		foreign_read(&platform, &disk, file).await,
		FakeEvent::Failed(_, Failure::NotAuthenticated)
	));
	assert_eq!(
		drive.reads(),
		Vec::new(),
		"the refused read reached no drive"
	);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	// Served: read from the drive (the fake OS then refuses a transfer to a file it never placed).
	foreign_read(&platform, &disk, file).await;
	assert_eq!(drive.reads(), vec![(0, 4096)]);
}

#[tokio::test(start_paused = true)]
async fn a_held_root_served_anew_refuses_another_processs_read() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let file = readable_file(&drive);
	let rig = held_with_an_unfetched_edit(&disk, &platform, &drive).await;
	rig.close().await;
	let mut rig = Rig::start(&disk, &platform, &drive);
	rig.until(is_status("held")).await;
	assert!(matches!(
		foreign_read(&platform, &disk, file).await,
		FakeEvent::Failed(_, Failure::NotAuthenticated)
	));
}

#[tokio::test(start_paused = true)]
async fn a_sign_in_that_does_not_complete_leaves_the_root_held() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = held_with_an_unfetched_edit(&disk, &platform, &drive).await;
	drive.fail_root_always(Some(unauthenticated));
	assert_eq!(
		code(&rig.set_auth("42", ACCOUNT_ROOT).await),
		"sessionRejected"
	);
	let state = ok(&rig.request(json!({ "t": "getState" })).await).clone();
	assert_eq!(state["state"], "held", "{state}");
	assert!(index_text(&disk).contains(r#""held":true"#));
}

#[tokio::test(start_paused = true)]
async fn a_root_registered_again_after_a_kill_once_the_os_let_its_held_root_go_is_not_held() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let rig = held_with_an_unfetched_edit(&disk, &platform, &drive).await;
	platform.local().fail_hydrations(None);
	platform.kill_at(KillPoint::After(FakeStep::Unregister));
	killed_unregister(rig, &platform).await;
	assert!(index_text(&disk).contains(r#""held":true"#));
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	assert!(!index_text(&disk).contains("held"), "{}", index_text(&disk));
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	let state = ok(&rig.request(json!({ "t": "getState" })).await).clone();
	assert_eq!(state["state"], "serving", "{state}");
}

#[tokio::test(start_paused = true)]
async fn an_unregistered_held_root_whose_data_stays_holds_up_no_other_users_register() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = held_with_an_unfetched_edit(&disk, &platform, &drive).await;
	platform.local().fail_hydrations(None);
	// A data directory without its marker is never wiped.
	std::fs::remove_file(disk.data().join(DATA_DIR_MARKER_NAME)).unwrap();
	let report = ok(&rig.request(held_unregister()).await).clone();
	assert_eq!(report["unregistered"], true, "{report}");
	assert_eq!(report["dataDeleted"], false, "{report}");
	assert!(!index_text(&disk).contains("held"), "{}", index_text(&disk));
	let other = Disk::new();
	let reply = rig.request(register(&other, "43")).await;
	assert_ne!(reply["error"]["code"], "alreadyRegistered", "{reply}");
}

#[tokio::test(start_paused = true)]
async fn a_sign_in_of_the_same_user_lifts_the_hold_and_the_unregister_then_finishes() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = held_with_an_unfetched_edit(&disk, &platform, &drive).await;
	assert_eq!(
		ok(&rig.set_auth("42", ACCOUNT_ROOT).await),
		&json!({ "state": "serving", "adopted": true })
	);
	let lines_before = rig.lines.len();
	let state = ok(&rig.request(json!({ "t": "getState" })).await).clone();
	assert_eq!(state["state"], "serving", "{state}");
	assert!(
		rig.lines[..lines_before]
			.iter()
			.rev()
			.find(|line| line["t"] == "status")
			.is_some_and(|line| line["state"] == "serving"),
		"the last status before is serving"
	);
	assert!(!index_text(&disk).contains("held"), "{}", index_text(&disk));
	platform.local().fail_hydrations(None);
	let report = ok(&rig
		.request(json!({ "t": "unregister", "userId": "42", "deleteHydrated": true }))
		.await)
	.clone();
	assert_eq!(report["unregistered"], true, "{report}");
	assert_eq!(report["kept"][0]["path"], "edited.txt");
	assert_eq!(platform.registration(&disk.root()), None);
	assert_eq!(
		ok(&rig.request(json!({ "t": "listRoots" })).await),
		&json!({ "roots": [] })
	);
}

#[tokio::test(start_paused = true)]
async fn an_unregister_that_succeeds_forgets_the_hold_with_the_root() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = held_with_an_unfetched_edit(&disk, &platform, &drive).await;
	platform.local().fail_hydrations(None);
	let report = ok(&rig.request(held_unregister()).await).clone();
	assert_eq!(report["unregistered"], true, "{report}");
	assert_eq!(platform.registration(&disk.root()), None);
	assert_eq!(
		ok(&rig.request(json!({ "t": "listRoots" })).await),
		&json!({ "roots": [] })
	);
	assert_eq!(rig.until(is_status("idle")).await["state"], "idle");
}

#[tokio::test(start_paused = true)]
async fn clearing_the_session_of_a_held_root_stops_serving_it_and_leaves_it_held() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = held_with_an_unfetched_edit(&disk, &platform, &drive).await;
	assert_eq!(
		ok(&rig
			.request(json!({ "t": "clearAuth", "userId": "42" }))
			.await),
		&json!({ "cleared": true })
	);
	assert!(!disk.has_session());
	assert!(!platform.is_connected(&disk.root()));
	assert_eq!(
		platform.registration(&disk.root()),
		Some(Some(ACCOUNT_ROOT))
	);
	let state = ok(&rig.request(json!({ "t": "getState" })).await).clone();
	assert_eq!(state["state"], "held", "{state}");
	assert!(index_text(&disk).contains(r#""held":true"#));
	assert!(
		platform
			.local()
			.entry(&root_path("edited.txt"))
			.unwrap()
			.placeholder,
		"the edit stays in place"
	);
	// A sign-in serves it again and lifts the hold.
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	let state = ok(&rig.request(json!({ "t": "getState" })).await).clone();
	assert_eq!(state["state"], "serving", "{state}");
}

#[tokio::test(start_paused = true)]
async fn a_held_root_goes_on_uploading_through_the_unregisters_retries() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let gate = drive.gate_uploads();
	let mut rig = held_with_an_unfetched_edit(&disk, &platform, &drive).await;
	let pump = {
		let platform = platform.clone();
		tokio::spawn(async move {
			loop {
				platform.deliver();
				sleep(Duration::from_secs(1)).await;
			}
		})
	};
	platform
		.local()
		.user_create(&root_path("new.txt"), b"hello");
	sleep(Duration::from_secs(10)).await;
	assert_eq!(
		code(&rig.request(held_unregister()).await),
		"unsyncedChanges"
	);
	gate.add_permits(100);
	rig.until(|line| {
		line["t"] == "activity"
			&& line["entries"]
				.as_array()
				.unwrap()
				.iter()
				.any(|entry| entry["kind"] == "uploaded" && entry["path"] == "new.txt")
	})
	.await;
	pump.abort();
	assert!(drive.child(&ACCOUNT_ROOT.to_string(), "new.txt").is_some());
}

#[tokio::test(start_paused = true)]
async fn a_held_root_serves_no_thumbnail_until_the_hold_is_lifted() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = serving_thumbnails(&disk, &platform, &drive).await;
	// From a thread outside the runtime, as File Explorer's COM threads ask.
	let ask = |platform: &FakePlatform, path: PathBuf| {
		let handler = platform.thumbnail_handler().unwrap();
		let asking = std::thread::spawn(move || handler(&path, 96, &ThumbnailCancel::default()));
		blocking(move || asking.join().unwrap())
	};
	let local = platform.local();
	local.place_file(&root_path("edited.txt"), file_identity(3), b"old data");
	local
		.user_write(&root_path("edited.txt"), b"new data")
		.unwrap();
	local.set_on_disk(&root_path("edited.txt"), 3);
	local.fail_hydrations(Some(LocalError::Busy));
	let path = disk.root().join("photo.jpg");
	let answer = ask(&platform, path.clone()).await;
	assert!(
		!matches!(
			answer,
			Err(ThumbnailRefusal::SignedOut | ThumbnailRefusal::Busy)
		),
		"{answer:?}"
	);
	assert_eq!(
		code(&rig.request(held_unregister()).await),
		"unsyncedChanges"
	);
	let answer = ask(&platform, path.clone()).await;
	assert!(
		matches!(answer, Err(ThumbnailRefusal::SignedOut)),
		"{answer:?}"
	);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	let answer = ask(&platform, path).await;
	assert!(
		!matches!(
			answer,
			Err(ThumbnailRefusal::SignedOut | ThumbnailRefusal::Busy)
		),
		"{answer:?}"
	);
}

// ---- abandon ----
//
// An abandon unregisters a held root whose changed files not all on disk cannot be fetched,
// saving what is on disk of each beside it, and so only on the user's explicit request.

fn abandon_request() -> Value {
	json!({ "t": "abandon", "userId": "42", "deleteHydrated": false })
}

/// The items the aside `report` names holds.
fn aside_of(platform: &FakePlatform, report: &Value) -> Vec<FakeEntry> {
	let kept_in = PathBuf::from(report["keptIn"].as_str().expect("kept aside"));
	platform.aside(&kept_in).expect("an aside")
}

fn bytes_at(entries: &[FakeEntry], path: &str) -> Option<Vec<u8>> {
	entries
		.iter()
		.find(|entry| entry.path.to_string() == path)
		.map(|entry| entry.bytes.clone())
}

#[tokio::test(start_paused = true)]
async fn an_abandon_of_a_held_root_saves_what_is_on_disk_of_an_edit_it_cannot_fetch() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = held_with_an_unfetched_edit(&disk, &platform, &drive).await;
	let report = ok(&rig.request(abandon_request()).await).clone();
	assert_eq!(report["unregistered"], true, "{report}");
	assert_eq!(
		report["kept"],
		json!([{ "path": "edited.txt.incomplete", "reason": "incomplete" }]),
		"{report}"
	);
	assert_eq!(
		report["abandoned"]["incomplete"],
		json!([{
			"path": "edited.txt.incomplete",
			"from": "edited.txt",
			"size": 8,
			"onDisk": 3,
			"modified": 8,
		}]),
		"{report}"
	);
	assert_eq!(report["abandoned"]["incompleteCount"], 1);
	assert_eq!(report["abandoned"]["manifest"], "Incomplete files.txt");
	let entries = aside_of(&platform, &report);
	assert_eq!(
		bytes_at(&entries, "edited.txt.incomplete").as_deref(),
		Some(&b"new\0\0\0\0\0"[..])
	);
	assert!(bytes_at(&entries, "Incomplete files.txt").is_some());
	assert_eq!(platform.registration(&disk.root()), None);
	assert!(!disk.has_session());
	assert_eq!(
		ok(&rig.request(json!({ "t": "listRoots" })).await),
		&json!({ "roots": [] })
	);
	assert_eq!(rig.until(is_status("idle")).await["state"], "idle");
}

#[tokio::test(start_paused = true)]
async fn an_abandon_reports_how_many_changes_were_pending_and_files_dropped() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = held_with_an_unfetched_edit(&disk, &platform, &drive).await;
	let local = platform.local();
	local.place_file_with(&root_path("cloud.txt"), file_identity(2), b"cloud", false);
	// A new file the drive, unreachable, cannot take.
	drive.set_offline(true);
	local.user_create(&root_path("new.txt"), b"never uploaded");
	platform.deliver();
	sleep(Duration::from_secs(10)).await;
	let report = ok(&rig.request(abandon_request()).await).clone();
	assert_eq!(report["abandoned"]["dropped"], 1, "{report}");
	// The edit not all on disk and the new file, neither on the drive.
	assert_eq!(report["abandoned"]["pendingOperations"], 2, "{report}");
}

#[tokio::test(start_paused = true)]
async fn an_abandon_fetches_again_what_its_first_try_could_not() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = held_with_an_unfetched_edit(&disk, &platform, &drive).await;
	let local = platform.local();
	local.fail_hydrations(None);
	local.fail_hydrate(LocalError::Busy);
	let report = ok(&rig.request(abandon_request()).await).clone();
	assert_eq!(
		report["kept"],
		json!([{ "path": "edited.txt", "reason": "modified" }]),
		"{report}"
	);
	assert_eq!(report["abandoned"]["incompleteCount"], 0, "{report}");
	assert_eq!(
		bytes_at(&aside_of(&platform, &report), "edited.txt").as_deref(),
		Some(&b"new data"[..])
	);
}

#[tokio::test(start_paused = true)]
async fn an_abandon_that_finishes_a_killed_one_reports_its_copies_as_kept_only() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = held_with_an_unfetched_edit(&disk, &platform, &drive).await;
	platform.kill_at(KillPoint::After(FakeStep::Unregister));
	assert_eq!(code(&rig.request(abandon_request()).await), "internal");
	let (reason, _) = rig.close().await;
	assert_eq!(reason, ExitReason::Internal);
	let mut rig = Rig::start(&disk, &platform, &drive);
	let report = ok(&rig.request(abandon_request()).await).clone();
	assert_eq!(report["unregistered"], true, "{report}");
	assert!(report.get("abandoned").is_none(), "{report}");
	let kept = report["kept"].as_array().expect("kept");
	assert_eq!(kept.len(), 1, "{report}");
	assert_eq!(kept[0]["reason"], "incomplete", "{report}");
	// Killed before it took its name: it stays under the one it was written under.
	let path = kept[0]["path"].as_str().unwrap();
	assert!(path.starts_with(".filen-upload-copy-"), "{report}");
	assert_eq!(
		bytes_at(&aside_of(&platform, &report), path).as_deref(),
		Some(&b"new\0\0\0\0\0"[..])
	);
}

#[tokio::test(start_paused = true)]
async fn an_abandon_of_a_root_that_is_not_held_is_refused_and_changes_nothing() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = Rig::start(&disk, &platform, &drive);
	ok(&rig.register(&disk).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	let refused = rig.request(abandon_request()).await;
	assert_eq!(code(&refused), "notHeld");
	assert_eq!(
		refused["error"]["message"],
		"the root is not held; an unregister with hold holds it"
	);
	assert!(platform.is_connected(&disk.root()));
	assert!(disk.has_session());
	assert_eq!(
		code(
			&rig.request(json!({ "t": "abandon", "userId": "43", "deleteHydrated": false }))
				.await
		),
		"notRegistered"
	);
}

#[tokio::test(start_paused = true)]
async fn an_abandon_fetches_what_it_can_first() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = held_with_an_unfetched_edit(&disk, &platform, &drive).await;
	platform.local().fail_hydrations(None);
	let report = ok(&rig.request(abandon_request()).await).clone();
	assert_eq!(
		report["kept"],
		json!([{ "path": "edited.txt", "reason": "modified" }]),
		"{report}"
	);
	assert_eq!(report["abandoned"]["incompleteCount"], 0, "{report}");
	assert_eq!(report["abandoned"]["manifest"], Value::Null, "{report}");
	assert_eq!(
		bytes_at(&aside_of(&platform, &report), "edited.txt").as_deref(),
		Some(&b"new data"[..])
	);
}

#[tokio::test(start_paused = true)]
async fn an_abandon_whose_copy_fails_unregisters_nothing_and_keeps_the_root_held() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = held_with_an_unfetched_edit(&disk, &platform, &drive).await;
	platform
		.local()
		.fail_copy(&root_path("edited.txt"), Some(LocalError::Busy));
	assert_eq!(code(&rig.request(abandon_request()).await), "os");
	assert_eq!(
		platform.registration(&disk.root()),
		Some(Some(ACCOUNT_ROOT))
	);
	assert!(index_text(&disk).contains(r#""held":true"#));
	assert!(disk.has_session());
	let edit = platform.local().entry(&root_path("edited.txt")).unwrap();
	assert!(edit.placeholder && edit.on_disk == 3, "{edit:?}");
	assert!(
		platform
			.local()
			.entry(&root_path("edited.txt.incomplete"))
			.is_none()
	);
	let state = ok(&rig.request(json!({ "t": "getState" })).await).clone();
	assert_eq!(state["state"], "held", "{state}");
}

#[tokio::test(start_paused = true)]
async fn a_kill_after_an_incomplete_copy_loses_nothing_and_a_resent_abandon_finishes() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = held_with_an_unfetched_edit(&disk, &platform, &drive).await;
	platform.local().kill_after_copies(1);
	assert_eq!(code(&rig.request(abandon_request()).await), "internal");
	let (reason, _) = rig.close().await;
	assert_eq!(reason, ExitReason::Internal);
	assert_eq!(
		platform.registration(&disk.root()),
		Some(Some(ACCOUNT_ROOT))
	);
	let edit = platform.local().entry(&root_path("edited.txt")).unwrap();
	assert!(edit.placeholder && edit.on_disk == 3, "{edit:?}");
	let uploads = || {
		drive
			.ops()
			.iter()
			.filter(|op| matches!(op, DriveOp::Upload { .. }))
			.count()
	};
	let uploaded = uploads();
	let mut rig = Rig::start(&disk, &platform, &drive);
	// Served again, held, the engine walks the root: the copy goes up to no drive.
	rig.until(is_status("held")).await;
	sleep(Duration::from_secs(120)).await;
	assert_eq!(uploads(), uploaded, "{:?}", drive.ops());
	let report = ok(&rig.request(abandon_request()).await).clone();
	assert_eq!(report["unregistered"], true, "{report}");
	assert_eq!(report["abandoned"]["incompleteCount"], 1, "{report}");
	// The copy the killed run made stays, under the name it was written under, beside the new one.
	let entries = aside_of(&platform, &report);
	let copies: Vec<&FakeEntry> = entries
		.iter()
		.filter(|entry| entry.bytes == b"new\0\0\0\0\0")
		.collect();
	assert_eq!(copies.len(), 2, "{entries:?}");
	assert!(
		copies
			.iter()
			.any(|entry| entry.path.to_string() == "edited.txt.incomplete")
	);
	assert_eq!(platform.registration(&disk.root()), None);
}

/// [`held_with_an_unfetched_edit`] with the edit in `d`, a folder the drive holds beside
/// `remote.txt`, which is not on disk; the folder's uuid.
async fn held_with_an_unfetched_edit_in_a_folder(
	disk: &Disk,
	platform: &FakePlatform,
	drive: &Arc<FakeDrive>,
) -> (Rig, Uuid) {
	let mut rig = Rig::start(disk, platform, drive);
	ok(&rig.register(disk).await);
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	rig.until(|line| line["t"] == "pendingUploads" && line["count"] == 0)
		.await;
	let remote = drive.add_remote_dir(&ACCOUNT_ROOT.to_string(), "d");
	let uuid = Uuid::parse_str(&remote.uuid).unwrap();
	drive.add_remote_file(&format!("stable/{uuid}"), "remote.txt", b"remote");
	let local = platform.local();
	local.place_dir(&root_path("d"), PlaceholderIdentity::Dir { uuid });
	local.place_file(&root_path(r"d\edited.txt"), file_identity(3), b"old data");
	local
		.user_write(&root_path(r"d\edited.txt"), b"new data")
		.unwrap();
	local.set_on_disk(&root_path(r"d\edited.txt"), 3);
	local.fail_hydrations(Some(LocalError::Busy));
	assert_eq!(
		code(&rig.request(held_unregister()).await),
		"unsyncedChanges"
	);
	(rig, uuid)
}

/// Whether the engine took the folder `d`, made ordinary, in again as a populated directory of the
/// drive's `uuid`.
fn adopted_populated(platform: &FakePlatform, uuid: Uuid) -> bool {
	platform.local().ops().contains(&LocalOp::ConvertedDir {
		path: root_path("d"),
		identity: PlaceholderIdentity::Dir { uuid },
		populated: true,
	})
}

#[tokio::test(start_paused = true)]
async fn an_abandon_the_os_refuses_past_its_settle_leaves_the_folder_listable_and_a_retry_finishes()
{
	let disk = Disk::new();
	let (platform, drive) = world();
	let (mut rig, uuid) = held_with_an_unfetched_edit_in_a_folder(&disk, &platform, &drive).await;
	platform.fail_unregisters(1);
	let refused = rig.request(abandon_request()).await;
	assert_eq!(refused["ok"], false, "{refused}");
	assert_eq!(
		platform.registration(&disk.root()),
		Some(Some(ACCOUNT_ROOT))
	);
	assert!(index_text(&disk).contains(r#""held":true"#));
	// Served again, held: the folder the settle made ordinary is taken in again, populated, so it
	// lists what it holds while held, and nothing of the drive's is placed in it.
	sleep(Duration::from_secs(60)).await;
	assert!(
		adopted_populated(&platform, uuid),
		"{:?}",
		platform.local().ops()
	);
	assert!(
		platform
			.local()
			.entry(&root_path(r"d\remote.txt"))
			.is_none()
	);
	let report = ok(&rig.request(abandon_request()).await).clone();
	assert_eq!(report["unregistered"], true, "{report}");
	assert_eq!(report["abandoned"]["incompleteCount"], 1, "{report}");
	let entries = aside_of(&platform, &report);
	assert_eq!(
		bytes_at(&entries, r"d\edited.txt.incomplete").as_deref(),
		Some(&b"new\0\0\0\0\0"[..])
	);
	// The first try's copy stays beside it, under the name nothing uploads.
	let copies = entries
		.iter()
		.filter(|entry| entry.bytes == b"new\0\0\0\0\0")
		.count();
	assert_eq!(copies, 2, "{entries:?}");
	assert_eq!(platform.registration(&disk.root()), None);
}

#[tokio::test(start_paused = true)]
async fn a_sign_in_after_an_abandon_refused_past_its_settle_serves_the_folder_whole() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let (mut rig, uuid) = held_with_an_unfetched_edit_in_a_folder(&disk, &platform, &drive).await;
	platform.fail_unregisters(1);
	assert_eq!(rig.request(abandon_request()).await["ok"], false);
	sleep(Duration::from_secs(60)).await;
	assert!(adopted_populated(&platform, uuid));
	ok(&rig.set_auth("42", ACCOUNT_ROOT).await);
	assert_eq!(rig.until(is_status("serving")).await["state"], "serving");
	sleep(Duration::from_secs(60)).await;
	let local = platform.local();
	let placed = local.entry(&root_path(r"d\remote.txt")).unwrap();
	assert!(placed.placeholder && placed.in_sync, "{placed:?}");
	let edit = local.entry(&root_path(r"d\edited.txt")).unwrap();
	assert!(edit.placeholder && edit.on_disk == 3, "{edit:?}");
	assert_eq!(&edit.bytes[..3], b"new");
	assert!(!index_text(&disk).contains(r#""held":true"#));
	assert!(disk.has_session());
}

#[tokio::test(start_paused = true)]
async fn a_kill_past_an_abandons_settle_loses_nothing_and_a_resent_abandon_finishes() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let (mut rig, uuid) = held_with_an_unfetched_edit_in_a_folder(&disk, &platform, &drive).await;
	// The folder is the one item the settle makes ordinary.
	platform.local().kill_after_reverts(1);
	assert_eq!(code(&rig.request(abandon_request()).await), "internal");
	let (reason, _) = rig.close().await;
	assert_eq!(reason, ExitReason::Internal);
	assert!(!platform.local().entry(&root_path("d")).unwrap().placeholder);
	let mut rig = Rig::start(&disk, &platform, &drive);
	rig.until(is_status("held")).await;
	sleep(Duration::from_secs(60)).await;
	assert!(adopted_populated(&platform, uuid));
	let report = ok(&rig.request(abandon_request()).await).clone();
	assert_eq!(report["unregistered"], true, "{report}");
	assert_eq!(
		bytes_at(&aside_of(&platform, &report), r"d\edited.txt.incomplete").as_deref(),
		Some(&b"new\0\0\0\0\0"[..])
	);
	assert_eq!(platform.registration(&disk.root()), None);
}

#[tokio::test(start_paused = true)]
async fn a_shutdown_during_an_abandons_fetches_answers_shutting_down() {
	let disk = Disk::new();
	let (platform, drive) = world();
	let mut rig = held_with_an_unfetched_edit(&disk, &platform, &drive).await;
	let gate = platform.local().gate(Gated::Hydrate);
	let abandon = rig.send(abandon_request()).await;
	{
		let gate = gate.clone();
		blocking(move || gate.wait_entered()).await;
	}
	let shutdown = rig.send(json!({ "t": "shutdown" })).await;
	for _ in 0..100 {
		yield_now().await;
	}
	gate.open();
	assert_eq!(code(&rig.reply_to(abandon).await), "shuttingDown");
	ok(&rig.reply_to(shutdown).await);
	assert_eq!(rig.task.await.unwrap(), ExitReason::Shutdown);
	assert_eq!(
		platform.registration(&disk.root()),
		Some(Some(ACCOUNT_ROOT))
	);
	assert!(disk.has_session());
	assert!(index_text(&disk).contains(r#""held":true"#));
	assert!(
		!platform
			.local()
			.ops()
			.iter()
			.any(|op| matches!(op, LocalOp::CopiedOnDisk { .. })),
		"nothing copied"
	);
}

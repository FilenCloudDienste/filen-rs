//! No line the helper writes and no log it makes shows a pushed session's secrets. Its own test
//! binary, so its logger is the process's global one and catches every thread's logs, the blocking
//! pool's included.

use std::{
	io::Write,
	sync::{Arc, Mutex},
	time::Duration,
};

use filen_cloud_files::{
	HelperConfig, PlaceholderIdentity, StateDir, run_helper,
	test_support::{FakeDrive, FakeLocal, FakeOpener, FakePlatform},
};
use filen_types::fs::Uuid;
use serde_json::{Value, json};
use tokio::{
	io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
	time::timeout,
};

const ACCOUNT_ROOT: Uuid = Uuid::from_u128(0x100);

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

#[tokio::test(start_paused = true)]
async fn no_line_and_no_log_shows_a_secret() {
	let capture = Capture::default();
	let writer = capture.clone();
	tracing_subscriber::fmt()
		.with_max_level(tracing::Level::TRACE)
		.with_ansi(false)
		.with_writer(move || writer.clone())
		.init();

	let temp = tempfile::tempdir().unwrap();
	let local = FakeLocal::new(PlaceholderIdentity::Root { root: ACCOUNT_ROOT });
	let drive = Arc::new(FakeDrive::new(ACCOUNT_ROOT));
	drive.list(&ACCOUNT_ROOT.to_string(), Vec::new());
	let platform = FakePlatform::new(local);
	let (mut input, helper_input) = tokio::io::duplex(1 << 22);
	let (helper_output, output) = tokio::io::duplex(1 << 22);
	let helper = tokio::spawn(run_helper(
		platform,
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
	let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
	assert!(logs.contains("request refused"), "{logs}");
	for text in lines.iter().map(Value::to_string).chain([logs]) {
		assert!(!text.contains("marker-"), "{text}");
	}
}

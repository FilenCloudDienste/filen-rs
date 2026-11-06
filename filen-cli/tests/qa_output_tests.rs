//! What the CLI prints with `--json` and `--quiet`, which of stdout and stderr it prints a failure
//! to, and the exit codes not covered by `output_tests` and `closed_stdout_tests`.
//!
//! The JSON tests check only that stdout is one JSON document and that a few keys are there with
//! the right types, not the whole document, so that a key added to it does not fail them.

/// Needs no account: each process stops before it would log in.
mod offline {
	#[cfg(unix)]
	use std::{
		io::{Read as _, Write as _},
		process::{Command, Stdio},
		sync::mpsc,
		time::{Duration, Instant},
	};

	#[cfg(unix)]
	use assert_fs::TempDir;

	/// Ctrl-C while the CLI waits for the next command piped into it quits it with 130, the code a
	/// shell reports for a process that SIGINT ended.
	#[cfg(unix)]
	#[test]
	fn ctrl_c_between_piped_commands_exits_with_130() {
		let config_dir = TempDir::new().unwrap();
		let mut cli = Command::new(assert_cmd::cargo::cargo_bin!())
			.arg("--config-dir")
			.arg(config_dir.path())
			.arg("--skip-update")
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.spawn()
			.unwrap();

		// A first command, whose output shows that the CLI has installed its Ctrl-C handler and is
		// reading the next line. stdin stays open, so it then waits for one.
		let mut stdin = cli.stdin.take().unwrap();
		stdin.write_all(b"help\n").unwrap();
		let mut stdout = cli.stdout.take().unwrap();
		let (printed, printed_rx) = mpsc::channel();
		std::thread::spawn(move || {
			let mut first = [0u8; 1];
			let read = stdout.read(&mut first);
			let _ = printed.send(read.map(|n| n > 0).unwrap_or(false));
			// keep reading, so the CLI never blocks on a full pipe
			let _ = std::io::copy(&mut stdout, &mut std::io::sink());
		});
		// read as it comes, so the CLI never blocks on a full pipe either
		let mut stderr = cli.stderr.take().unwrap();
		let stderr = std::thread::spawn(move || {
			let mut read = Vec::new();
			let _ = stderr.read_to_end(&mut read);
			String::from_utf8_lossy(&read).into_owned()
		});
		let printed = printed_rx.recv_timeout(Duration::from_secs(60));
		if printed != Ok(true) {
			let _ = cli.kill();
			panic!("the CLI printed nothing for the piped `help`: {printed:?}");
		}

		let interrupted = Command::new("kill")
			.args(["-INT", &cli.id().to_string()])
			.status()
			.unwrap();
		assert!(interrupted.success());
		// The CLI exits within a second or two of Ctrl-C. stdin stays open while it waits, so a
		// Ctrl-C it loses leaves it reading the next command for good.
		let deadline = Instant::now() + Duration::from_secs(30);
		let status = loop {
			if let Some(status) = cli.try_wait().unwrap() {
				break status;
			}
			if Instant::now() >= deadline {
				let _ = cli.kill();
				let _ = cli.wait();
				panic!(
					"the CLI did not exit within 30 s of Ctrl-C\nstderr:\n{}",
					stderr.join().unwrap()
				);
			}
			std::thread::sleep(Duration::from_millis(50));
		};
		drop(stdin);
		let stderr = stderr.join().unwrap();

		assert_eq!(status.code(), Some(130), "{status}\nstderr:\n{stderr}");
	}
}

/// Logs into the test account.
mod live {
	use filen_macros::shared_test_runtime;
	use filen_sdk_rs::fs::{HasName as _, HasUUID as _};
	use test_utils::authenticated_cli_with_args;

	/// Parses stdout as JSON, failing unless it is exactly one JSON document with nothing but
	/// whitespace around it.
	fn single_json_document(stdout: &[u8]) -> serde_json::Value {
		serde_json::from_slice(stdout).unwrap_or_else(|e| {
			panic!(
				"stdout is not one JSON document ({e}):\n{}",
				String::from_utf8_lossy(stdout)
			)
		})
	}

	fn assert_number(value: &serde_json::Value, key: &str) {
		assert!(value[key].is_u64(), "{key} is not a number: {value}");
	}

	#[shared_test_runtime]
	async fn ls_json_lists_the_directories_and_files() {
		let resources = test_utils::RESOURCES.get_resources().await;
		let client = &resources.client;
		let test_dir = &resources.dir;
		client
			.create_dir(&test_dir.into(), "listed_dir")
			.await
			.unwrap();
		let file = client
			.make_file_builder("listed_file.txt", test_dir.uuid)
			.unwrap();
		client.upload_file(file, b"content").await.unwrap();

		let assert =
			authenticated_cli_with_args!("ls", "--json", test_dir.name().unwrap()).success();
		let listing = single_json_document(&assert.get_output().stdout);

		// one object per item, directories first
		let items = listing
			.as_array()
			.unwrap_or_else(|| panic!("not an array of items: {listing}"));
		let names_and_types = items
			.iter()
			.map(|item| (item["name"].as_str(), item["type"].as_str()))
			.collect::<Vec<_>>();
		assert_eq!(
			names_and_types,
			[
				(Some("listed_dir"), Some("directory")),
				(Some("listed_file.txt"), Some("file")),
			],
			"{listing}"
		);
	}

	#[shared_test_runtime]
	async fn stat_json_describes_a_file() {
		let resources = test_utils::RESOURCES.get_resources().await;
		let client = &resources.client;
		let test_dir = &resources.dir;
		let file = client
			.make_file_builder("stat_file.txt", test_dir.uuid)
			.unwrap();
		client.upload_file(file, &[7u8; 1024]).await.unwrap();

		let path = format!("{}/stat_file.txt", test_dir.name().unwrap());
		let assert = authenticated_cli_with_args!("stat", "--json", &path).success();
		let stat = single_json_document(&assert.get_output().stdout);

		assert_eq!(stat["type"], "file", "{stat}");
		assert_eq!(stat["name"], "stat_file.txt", "{stat}");
		assert_eq!(stat["size"], 1024, "{stat}");
	}

	#[shared_test_runtime]
	async fn stat_json_describes_a_directory() {
		let resources = test_utils::RESOURCES.get_resources().await;
		let client = &resources.client;
		let test_dir = &resources.dir;
		let dir = client
			.create_dir(&test_dir.into(), "stat_dir")
			.await
			.unwrap();
		let file = client.make_file_builder("inside.txt", dir.uuid()).unwrap();
		client.upload_file(file, b"content").await.unwrap();

		let path = format!("{}/stat_dir", test_dir.name().unwrap());
		let assert = authenticated_cli_with_args!("stat", "--json", &path).success();
		let stat = single_json_document(&assert.get_output().stdout);

		assert_eq!(stat["type"], "directory", "{stat}");
		assert_eq!(stat["name"], "stat_dir", "{stat}");
		// the server recomputes directory sizes lazily, so only their types are certain
		assert_number(&stat, "size");
		assert_number(&stat, "files");
		assert_number(&stat, "directories");
	}

	#[shared_test_runtime]
	async fn stat_json_describes_the_drive() {
		let assert = authenticated_cli_with_args!("stat", "--json", "/").success();
		let stat = single_json_document(&assert.get_output().stdout);

		assert_eq!(stat["type"], "drive", "{stat}");
		assert_number(&stat, "usedStorage");
		assert_number(&stat, "totalStorage");
	}

	#[shared_test_runtime]
	// With a fresh cache, `search` lists the directory and still prints `{"results":[]}`. Run it
	// with `--ignored` once that is fixed, and drop this.
	#[ignore = "search prints no results for a directory its cache has to list first"]
	async fn search_json_finds_an_uploaded_file() {
		let resources = test_utils::RESOURCES.get_resources().await;
		let client = &resources.client;
		let test_dir = &resources.dir;
		// unique, so only this file matches
		let name = format!("searched_{}.txt", test_dir.uuid);
		let file = client.make_file_builder(&name, test_dir.uuid).unwrap();
		client.upload_file(file, b"content").await.unwrap();
		// its own config dir, so the search cache starts empty rather than from another run's
		let config_dir = assert_fs::TempDir::new().unwrap();

		let assert = authenticated_cli_with_args!(
			"--config-dir",
			config_dir.path().to_str().unwrap(),
			"--skip-update",
			"--working-path",
			test_dir.name().unwrap(),
			"search",
			"--json",
			&name
		)
		.success();
		let found = single_json_document(&assert.get_output().stdout);

		assert!(found.is_object(), "{found}");
		assert!(found.to_string().contains(&name), "{found}");
	}

	#[shared_test_runtime]
	async fn export_api_key_json_has_the_email_and_the_key() {
		let assert = authenticated_cli_with_args!("export-api-key", "--json").success();
		let exported = single_json_document(&assert.get_output().stdout);

		// Never print the document: it holds the account's API key.
		let email = exported["email"].as_str();
		assert!(email.is_some_and(|email| email.contains('@')), "no email");
		let api_key = exported["apiKey"].as_str();
		assert!(api_key.is_some_and(|key| !key.is_empty()), "no apiKey");
	}

	#[shared_test_runtime]
	async fn quiet_mkdir_prints_nothing() {
		let resources = test_utils::RESOURCES.get_resources().await;
		let path = format!("{}/quiet_dir", resources.dir.name().unwrap());

		let assert = authenticated_cli_with_args!("mkdir", "--quiet", &path).success();

		assert_eq!(String::from_utf8_lossy(&assert.get_output().stdout), "");
		assert!(
			resources
				.client
				.find_item_at_path(&path)
				.await
				.unwrap()
				.is_some(),
			"{path} was not created"
		);
	}

	#[shared_test_runtime]
	async fn quiet_favorite_prints_nothing() {
		let resources = test_utils::RESOURCES.get_resources().await;
		let client = &resources.client;
		let test_dir = &resources.dir;
		let file = client
			.make_file_builder("quiet_favorite.txt", test_dir.uuid)
			.unwrap();
		client.upload_file(file, b"content").await.unwrap();
		let path = format!("{}/quiet_favorite.txt", test_dir.name().unwrap());

		let assert = authenticated_cli_with_args!("favorite", "--quiet", &path).success();

		assert_eq!(String::from_utf8_lossy(&assert.get_output().stdout), "");
	}

	#[shared_test_runtime]
	async fn a_failing_command_prints_its_error_to_stderr_only() {
		let resources = test_utils::RESOURCES.get_resources().await;
		let path = format!("{}/does_not_exist", resources.dir.name().unwrap());

		let assert = authenticated_cli_with_args!("stat", &path).code(1);

		let output = assert.get_output();
		assert_eq!(String::from_utf8_lossy(&output.stdout), "");
		let stderr = String::from_utf8_lossy(&output.stderr);
		assert!(stderr.contains("does_not_exist"), "{stderr}");
	}

	#[shared_test_runtime]
	async fn a_failing_command_prints_no_json() {
		let resources = test_utils::RESOURCES.get_resources().await;
		let path = format!("{}/does_not_exist", resources.dir.name().unwrap());

		let assert = authenticated_cli_with_args!("stat", "--json", &path).code(1);

		let output = assert.get_output();
		assert_eq!(String::from_utf8_lossy(&output.stdout), "");
		let stderr = String::from_utf8_lossy(&output.stderr);
		assert!(stderr.contains("does_not_exist"), "{stderr}");
	}

	/// `rclone` exits with rclone's own code: 3 when the directory to list does not exist.
	///
	/// Runs only with `FILEN_TEST_RCLONE` set, as the other rclone tests do: the CLI downloads its
	/// managed rclone for it.
	#[shared_test_runtime]
	async fn rclone_exits_with_the_code_rclone_exits_with() {
		if std::env::var_os("FILEN_TEST_RCLONE").is_none() {
			eprintln!("skipped: FILEN_TEST_RCLONE is not set");
			return;
		}
		let resources = test_utils::RESOURCES.get_resources().await;
		let config_dir = assert_fs::TempDir::new().unwrap();
		let remote = format!("filen:{}/does_not_exist", resources.dir.name().unwrap());

		authenticated_cli_with_args!(
			"--config-dir",
			config_dir.path().to_str().unwrap(),
			"--skip-update",
			"rclone",
			"lsf",
			&remote
		)
		.code(3);
	}
}

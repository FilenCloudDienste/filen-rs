//! Commands no other test runs (`export-api-key`, `list-recents`, `view-html-docs`), paths with
//! spaces in them from the command line and from piped REPL commands, and snapshots of what
//! `stat` prints and of the `--json` listings of `list-trash` and `list-recents`, with what
//! differs between runs redacted.
//!
//! `export-auth-config` is covered in `auth_tests`, and `logout` in `output_tests`.

use std::{
	io::Write as _,
	process::{Output, Stdio},
};

use test_utils::cli::logged_out_command;

/// Runs the CLI on `script` piped into it, with a fresh config directory and none of the
/// credentials of whoever runs the test.
fn run_script(args: &[&str], script: &str) -> Output {
	let config_dir = assert_fs::TempDir::new().unwrap();
	let mut cli = logged_out_command(assert_cmd::cargo::cargo_bin!(), config_dir.path())
		.arg("--skip-update")
		.args(args)
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()
		.unwrap();
	cli.stdin
		.take()
		.unwrap()
		.write_all(script.as_bytes())
		.unwrap();
	cli.wait_with_output().unwrap()
}

/// Needs no account.
mod offline {
	use std::{
		io::{BufRead as _, BufReader, Read as _, Write as _},
		net::TcpStream,
		process::{Child, Stdio},
		sync::mpsc,
		time::Duration,
	};

	use assert_fs::TempDir;
	use regex::Regex;
	use test_utils::cli::logged_out_command;

	/// Kills the CLI when the test ends, passed or failed: `view-html-docs` serves until it is
	/// stopped.
	struct KillOnDrop(Child);

	impl Drop for KillOnDrop {
		fn drop(&mut self) {
			let _ = self.0.kill();
			let _ = self.0.wait();
		}
	}

	/// Sends `GET <path>` to the docs server as an HTTP/1.0 request, which the server answers and
	/// then closes, and returns the status code and the body.
	fn get(port: u16, path: &str) -> (u16, String) {
		let mut stream = TcpStream::connect(("localhost", port)).unwrap();
		stream
			.set_read_timeout(Some(Duration::from_secs(30)))
			.unwrap();
		write!(stream, "GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n").unwrap();
		let mut response = String::new();
		stream.read_to_string(&mut response).unwrap();
		let (head, body) = response
			.split_once("\r\n\r\n")
			.unwrap_or_else(|| panic!("no end of the headers in:\n{response}"));
		let status = head
			.split_whitespace()
			.nth(1)
			.and_then(|status| status.parse().ok())
			.unwrap_or_else(|| panic!("no status in:\n{head}"));
		(status, body.to_owned())
	}

	#[test]
	fn view_html_docs_serves_each_section_as_html() {
		let config_dir = TempDir::new().unwrap();
		let mut cli = KillOnDrop(
			logged_out_command(assert_cmd::cargo::cargo_bin!(), config_dir.path())
				.env("FILEN_CLI_TESTING_DISABLE_BROWSER", "1")
				.arg("--skip-update")
				.arg("view-html-docs")
				.stdin(Stdio::null())
				.stdout(Stdio::piped())
				.stderr(Stdio::null())
				.spawn()
				.unwrap(),
		);

		// The CLI prints the address once the server listens.
		let stdout = BufReader::new(cli.0.stdout.take().unwrap());
		let (port_found, port_rx) = mpsc::channel();
		std::thread::spawn(move || {
			let address = Regex::new(r"http://localhost:(\d+)").unwrap();
			for line in stdout.lines() {
				let Ok(line) = line else { return };
				if let Some(port) = address.captures(&line) {
					let _ = port_found.send(port[1].parse::<u16>().unwrap());
				}
			}
		});
		let port = port_rx
			.recv_timeout(Duration::from_secs(60))
			.expect("the CLI printed no docs address");

		let (status, main) = get(port, "/");
		assert_eq!(status, 200, "{main}");
		assert!(main.contains("<body>"), "{main}");
		assert!(main.contains("view-html-docs"), "{main}");
		// the main section links the others
		assert!(main.contains("/transfer-files"), "{main}");

		let (status, transfers) = get(port, "/transfer-files");
		assert_eq!(status, 200, "{transfers}");
		assert!(transfers.contains("<body>"), "{transfers}");
		assert!(transfers.contains("upload"), "{transfers}");

		let (status, unknown) = get(port, "/no-such-section");
		assert_eq!(status, 404, "{unknown}");
		assert_eq!(unknown, "Section not found");
	}
}

/// Logs into the test account.
mod live {
	use std::time::{Duration, Instant};

	use filen_macros::shared_test_runtime;
	use filen_sdk_rs::fs::{HasName as _, HasUUID as _};
	use rand::TryRng as _;
	use regex::Regex;
	use test_utils::{
		authenticated_cli_with_args,
		cli::{exists, is_file},
	};

	use super::run_script;

	/// How long the server may take to list a new upload in the recents.
	const RECENTS_DEADLINE: Duration = Duration::from_secs(120);

	/// Replaces what changes between runs in `output`: the test directory's name, UUIDs,
	/// timestamps (as printed and as in JSON), sizes and counts. Color codes are stripped first.
	fn redact(output: &str, test_dir_name: &str) -> String {
		let ansi = Regex::new(r"\x1b\[[0-9;]*m").unwrap();
		let uuid =
			Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").unwrap();
		let time = Regex::new(
			r"\d{4}-\d{2}-\d{2}( \d{2}:\d{2}:\d{2} \(UTC\)|T\d{2}:\d{2}:\d{2}(\.\d+)?Z)",
		)
		.unwrap();
		let size = Regex::new(r"\d+(\.\d+)? (B|KiB|MiB|GiB|TiB)\b").unwrap();
		let count = Regex::new(r"(?m) \d+$").unwrap();

		let output = ansi.replace_all(output, "");
		let output = output.replace(test_dir_name, "<dir>");
		let output = uuid.replace_all(&output, "<uuid>");
		let output = time.replace_all(&output, "<time>");
		let output = size.replace_all(&output, "<size>");
		count.replace_all(&output, " <count>").into_owned()
	}

	/// A `--json` listing of account-wide items, an array of item objects, cut down to this
	/// test's items: those with a string field that contains `marker`. Each item is kept as
	/// printed, so the snapshot still shows the listing's shape.
	fn json_listing_containing(stdout: &[u8], marker: &str) -> String {
		let listing = json_listing(stdout);
		let items = listing
			.into_iter()
			.filter(|item| {
				item.as_object().is_some_and(|fields| {
					fields
						.values()
						.any(|value| value.as_str().is_some_and(|value| value.contains(marker)))
				})
			})
			.collect::<Vec<_>>();
		serde_json::to_string_pretty(&items).unwrap()
	}

	/// The items of a `--json` listing.
	fn json_listing(stdout: &[u8]) -> Vec<serde_json::Value> {
		serde_json::from_slice(stdout).unwrap_or_else(|e| {
			panic!(
				"not a JSON array of items ({e}):\n{}",
				String::from_utf8_lossy(stdout)
			)
		})
	}

	/// Polls `list-recents` until it lists `path`, and returns its stdout then.
	async fn list_recents_until_listed(path: &str) -> Vec<u8> {
		let deadline = Instant::now() + RECENTS_DEADLINE;
		loop {
			let assert = authenticated_cli_with_args!("list-recents").success();
			let stdout = assert.get_output().stdout.clone();
			if String::from_utf8_lossy(&stdout)
				.lines()
				.any(|line| line.contains(path))
			{
				return stdout;
			}
			assert!(
				Instant::now() < deadline,
				"{path} not in the recents after {RECENTS_DEADLINE:?}:\n{}",
				String::from_utf8_lossy(&stdout)
			);
			tokio::time::sleep(Duration::from_secs(5)).await;
		}
	}

	#[shared_test_runtime]
	async fn export_api_key_prints_the_key_of_the_account() {
		let client = test_utils::RESOURCES.client().await;
		let expected = client.to_stringified();

		let assert = authenticated_cli_with_args!("export-api-key").success();

		// Never print stdout: it holds the account's API key.
		let output = assert.get_output();
		let stdout = String::from_utf8_lossy(&output.stdout);
		let line = stdout
			.lines()
			.find(|line| line.contains(&expected.email))
			.expect("stdout names no account email");
		let (_, api_key) = line
			.split_once(&format!("{}:", expected.email))
			.expect("no key after the account email");
		assert!(
			api_key.trim() == expected.api_key,
			"stdout does not hold the account's API key"
		);
		// the warning to keep it secret goes to stderr, with the other warnings (marked "⚠")
		assert!(
			String::from_utf8_lossy(&output.stderr).contains('⚠'),
			"no warning on stderr"
		);
		assert!(!stdout.contains('⚠'), "a warning on stdout");
	}

	#[shared_test_runtime]
	async fn list_recents_lists_an_uploaded_file_by_its_full_path() {
		let resources = test_utils::RESOURCES.get_resources().await;
		let client = &resources.client;
		let test_dir = &resources.dir;
		let file = client
			.make_file_builder("recent_file.txt", test_dir.uuid)
			.unwrap();
		client.upload_file(file, b"content").await.unwrap();
		let path = format!("/{}/recent_file.txt", test_dir.name().unwrap());

		list_recents_until_listed(&path).await;

		let assert = authenticated_cli_with_args!("list-recents", "--json").success();
		let recents = json_listing(&assert.get_output().stdout);
		assert!(
			recents
				.iter()
				.any(|item| item["path"].as_str() == Some(path.as_str())
					&& item["type"].as_str() == Some("file")),
			"{recents:?}"
		);
	}

	/// `upload`, `download` and `mv` with spaces in the local and the remote paths, passed as one
	/// argument each, as a shell passes a quoted path.
	#[shared_test_runtime]
	async fn transfers_and_mv_take_paths_with_spaces() {
		let resources = test_utils::RESOURCES.get_resources().await;
		let client = &resources.client;
		let test_dir = &resources.dir;
		let test_dir_name = test_dir.name().unwrap();
		client.create_dir(&test_dir.into(), "a b").await.unwrap();
		let local = assert_fs::TempDir::new().unwrap();
		let source_dir = local.path().join("local source");
		let download_dir = local.path().join("local download");
		std::fs::create_dir(&source_dir).unwrap();
		std::fs::create_dir(&download_dir).unwrap();
		let mut contents = vec![0u8; 4096];
		rand::rng().try_fill_bytes(&mut contents).unwrap();
		let source = source_dir.join("x y.txt");
		std::fs::write(&source, &contents).unwrap();
		let remote_dir = format!("{test_dir_name}/a b");

		authenticated_cli_with_args!("upload", source.to_str().unwrap(), &remote_dir).success();
		authenticated_cli_with_args!(
			"download",
			&format!("{remote_dir}/x y.txt"),
			download_dir.to_str().unwrap()
		)
		.success();
		assert_eq!(
			std::fs::read(download_dir.join("x y.txt")).unwrap(),
			contents
		);

		authenticated_cli_with_args!(
			"mv",
			&format!("{remote_dir}/x y.txt"),
			&format!("{remote_dir}/z w.txt")
		)
		.success();
		assert!(is_file(client, &format!("{remote_dir}/z w.txt")).await);
		assert!(!exists(client, &format!("{remote_dir}/x y.txt")).await);
	}

	/// `cd` and `mv` read quoted paths with spaces in commands piped into the CLI, in either kind
	/// of quotes where the platform has both: on Windows only double quotes group words.
	#[shared_test_runtime]
	async fn piped_commands_take_quoted_paths_with_spaces() {
		let resources = test_utils::RESOURCES.get_resources().await;
		let client = &resources.client;
		let test_dir = &resources.dir;
		let dir = client
			.create_dir(&test_dir.into(), "dir with spaces")
			.await
			.unwrap();
		let file = client.make_file_builder("x y.txt", dir.uuid()).unwrap();
		client.upload_file(file, b"content").await.unwrap();
		let remote_dir = format!("/{}/dir with spaces", test_dir.name().unwrap());
		let (_auth_config_dir, auth_config) = test_utils::cli::prepare_cli_auth_config().await;
		let source = if cfg!(windows) {
			"\"x y.txt\""
		} else {
			"'x y.txt'"
		};

		let output = run_script(
			&["--auth-config-path", auth_config.to_str().unwrap()],
			&format!("cd \"{remote_dir}\"\nls\nmv {source} \"z w.txt\"\n"),
		);

		let stdout = String::from_utf8_lossy(&output.stdout);
		let stderr = String::from_utf8_lossy(&output.stderr);
		assert_eq!(output.status.code(), Some(0), "{stdout}\n{stderr}");
		// `ls` ran in the directory `cd` changed to
		assert!(stdout.contains("x y.txt"), "{stdout}");
		assert!(is_file(client, &format!("{remote_dir}/z w.txt")).await);
		assert!(!exists(client, &format!("{remote_dir}/x y.txt")).await);
	}

	#[shared_test_runtime]
	async fn stat_of_a_file_snapshot() {
		let resources = test_utils::RESOURCES.get_resources().await;
		let client = &resources.client;
		let test_dir = &resources.dir;
		let test_dir_name = test_dir.name().unwrap();
		let file = client
			.make_file_builder("stat_snapshot.txt", test_dir.uuid)
			.unwrap();
		client.upload_file(file, &[7u8; 1024]).await.unwrap();

		let assert =
			authenticated_cli_with_args!("stat", &format!("{test_dir_name}/stat_snapshot.txt"))
				.success();

		insta::assert_snapshot!(
			"stat_file",
			redact(
				&String::from_utf8_lossy(&assert.get_output().stdout),
				test_dir_name
			)
		);
	}

	#[shared_test_runtime]
	async fn stat_of_a_directory_snapshot() {
		let resources = test_utils::RESOURCES.get_resources().await;
		let client = &resources.client;
		let test_dir = &resources.dir;
		let test_dir_name = test_dir.name().unwrap();
		client
			.create_dir(&test_dir.into(), "stat_snapshot_dir")
			.await
			.unwrap();

		let assert =
			authenticated_cli_with_args!("stat", &format!("{test_dir_name}/stat_snapshot_dir"))
				.success();

		insta::assert_snapshot!(
			"stat_directory",
			redact(
				&String::from_utf8_lossy(&assert.get_output().stdout),
				test_dir_name
			)
		);
	}

	#[shared_test_runtime]
	async fn list_trash_snapshot() {
		let resources = test_utils::RESOURCES.get_resources().await;
		let client = &resources.client;
		let test_dir = &resources.dir;
		// so that no other test empties the trash between trashing the file and listing it
		let _trash_lock = client
			.acquire_lock_with_default(test_utils::locks::TRASH)
			.await
			.unwrap();
		// The trash lists bare names and holds every test's items, so the test directory's UUID
		// in the names tells this test's items apart.
		let marker = test_dir.uuid().to_string();
		let file = client
			.make_file_builder(&format!("trashed_{marker}.txt"), test_dir.uuid)
			.unwrap();
		let mut file = client.upload_file(file, b"content").await.unwrap();
		client.trash_file(&mut file).await.unwrap();
		let mut dir = client
			.create_dir(&test_dir.into(), &format!("trashed_dir_{marker}"))
			.await
			.unwrap();
		client.trash_dir(&mut dir).await.unwrap();

		let assert = authenticated_cli_with_args!("list-trash", "--json").success();
		let listed = json_listing_containing(&assert.get_output().stdout, &marker);
		client.delete_file_permanently(file).await.unwrap();
		client.delete_dir_permanently(dir).await.unwrap();

		insta::assert_snapshot!("list_trash", redact(&listed, test_dir.name().unwrap()));
	}

	#[shared_test_runtime]
	async fn list_recents_snapshot() {
		let resources = test_utils::RESOURCES.get_resources().await;
		let client = &resources.client;
		let test_dir = &resources.dir;
		let test_dir_name = test_dir.name().unwrap();
		let file = client
			.make_file_builder("recents_snapshot.txt", test_dir.uuid)
			.unwrap();
		client.upload_file(file, b"content").await.unwrap();

		let path = format!("/{test_dir_name}/recents_snapshot.txt");
		list_recents_until_listed(&path).await;

		let assert = authenticated_cli_with_args!("list-recents", "--json").success();
		// only the uploaded file's path: the test directory itself may be listed or not
		let listed = json_listing_containing(&assert.get_output().stdout, &path);
		insta::assert_snapshot!("list_recents", redact(&listed, test_dir_name));
	}
}

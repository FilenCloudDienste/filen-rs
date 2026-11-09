use std::{path::Path, process::Command, sync::Arc};

use filen_macros::shared_test_runtime;
use filen_sdk_rs::fs::{HasName, categories::NonRootFileType};
use filen_sdk_rs::{
	auth::Client,
	sync_engine::{DeleteGuard, SyncEngine},
};
use filen_types::api::v3::notes::NoteType;
use rand::TryRngCore;
use test_utils::authenticated_cli_with_args;
use tokio::sync::Mutex;

// keep this, would need to have a way to redact uuids, timestamps and drive size from the output to make it deterministic
#[shared_test_runtime]
async fn cmd_stat() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	// create test file to call stat on
	let file = client
		.make_file_builder("testfile.txt", test_dir.uuid)
		.unwrap();
	let mut contents = vec![0u8; 1024];
	rand::rng().try_fill_bytes(&mut contents).unwrap();
	client.upload_file(file, &contents).await.unwrap();

	// stat
	authenticated_cli_with_args!(
		"stat",
		&format!("{}/testfile.txt", test_dir.name().unwrap())
	)
	.success()
	.stdout(predicates::str::contains("1 KiB"));

	// stat on root drive
	authenticated_cli_with_args!("stat", "/")
		.success()
		.stdout(predicates::str::contains("Drive"));
}

// todo: verify results in manuel test
#[shared_test_runtime]
async fn cmd_favorite_unfavorite() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	// create test file to call favorite on
	let file = client
		.make_file_builder("testfile.txt", test_dir.uuid)
		.unwrap();
	let content = "Hello, Filen!";
	client.upload_file(file, content.as_bytes()).await.unwrap();

	let file_path = format!("{}/testfile.txt", test_dir.name().unwrap());

	// favorite
	authenticated_cli_with_args!("favorite", &file_path)
		.success()
		.stdout(predicates::str::contains("Favorited"));

	// verify file is favorited
	match client.find_item_at_path(&file_path).await.unwrap().unwrap() {
		NonRootFileType::File(file) => assert!(file.favorited),
		_ => panic!("Expected a file"),
	}

	// unfavorite
	authenticated_cli_with_args!("unfavorite", &file_path)
		.success()
		.stdout(predicates::str::contains("Unfavorited"));

	// verify file is unfavorited
	match client.find_item_at_path(&file_path).await.unwrap().unwrap() {
		NonRootFileType::File(file) => assert!(!file.favorited),
		_ => panic!("Expected a file"),
	}
}

// this cannot be tested in manuel recordings because of the custom timeout logic
#[shared_test_runtime]
async fn cmd_list_trash_empty_trash() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;

	// empty-trash is account-global and permanently deletes whatever other test binaries on this
	// shared account have sitting in trash mid-restore (nightly 2026-08-14), so serialize on the
	// trash lock the sdk's own trash tests use.
	let _trash_lock = client
		.acquire_lock_with_default(test_utils::locks::TRASH)
		.await
		.unwrap();

	// create test file to trash
	let test_dir = &resources.dir;
	let file = client
		.make_file_builder("testfile_from_cli_test_list_trash.txt", test_dir.uuid)
		.unwrap();
	let content = "Hello, Filen!";
	let mut file = client.upload_file(file, content.as_bytes()).await.unwrap();

	// trash the file
	client.trash_file(&mut file).await.unwrap();

	// list-trash
	authenticated_cli_with_args!("list-trash")
		.success()
		.stdout(predicates::str::contains(
			"testfile_from_cli_test_list_trash.txt",
		));

	// empty-trash, which says how many items it deleted: at least the file trashed above, as
	// other test binaries on the account may have trashed more
	let emptied = authenticated_cli_with_args!("empty-trash", "--yes").success();
	let stdout = String::from_utf8_lossy(&emptied.get_output().stdout).into_owned();
	let count = stdout
		.split_once("Emptied trash (")
		.and_then(|(_, rest)| rest.split_once(' '))
		.and_then(|(count, unit)| {
			let count = count.parse::<usize>().ok()?;
			let expected_unit = if count == 1 { "item)" } else { "items)" };
			unit.starts_with(expected_unit).then_some(count)
		});
	assert!(count.is_some_and(|count| count >= 1), "stdout:\n{stdout}");

	// Verify our own file eventually leaves the trash listing. Asserting a globally
	// empty trash is impossible on the shared account (concurrent test binaries keep
	// trashing items), and server-side emptying is async and can lag by minutes.
	let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
	loop {
		let assert = authenticated_cli_with_args!("list-trash").success();
		let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
		if !stdout.contains("testfile_from_cli_test_list_trash.txt") {
			break;
		}
		if std::time::Instant::now() >= deadline {
			panic!("file still listed in trash 300s after empty-trash:\n{stdout}");
		}
		tokio::time::sleep(std::time::Duration::from_secs(5)).await;
	}
}

#[shared_test_runtime]
async fn working_path_must_be_an_existing_directory() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let test_dir_name = test_dir.name().unwrap();

	let file = client
		.make_file_builder("file_in_working_path.txt", test_dir.uuid)
		.unwrap();
	client.upload_file(file, b"content").await.unwrap();

	authenticated_cli_with_args!("--working-path", test_dir_name, "ls")
		.success()
		.stdout(predicates::str::contains("file_in_working_path.txt"));

	authenticated_cli_with_args!(
		"--working-path",
		&format!("{test_dir_name}/does_not_exist"),
		"ls"
	)
	.code(1)
	.stderr(predicates::str::contains(format!(
		"Invalid --working-path: No such directory: /{test_dir_name}/does_not_exist"
	)));

	authenticated_cli_with_args!(
		"--working-path",
		&format!("{test_dir_name}/file_in_working_path.txt"),
		"ls"
	)
	.code(1)
	.stderr(predicates::str::contains(
		"Invalid --working-path: Not a directory",
	));
}

#[shared_test_runtime]
async fn options_after_the_command_apply_to_it() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let file = client
		.make_file_builder("file_listed_as_json.txt", test_dir.uuid)
		.unwrap();
	client.upload_file(file, b"content").await.unwrap();

	let assert = authenticated_cli_with_args!("ls", test_dir.name().unwrap(), "--json").success();
	let listing: serde_json::Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
	assert!(
		listing.to_string().contains("file_listed_as_json.txt"),
		"{listing}"
	);
}

#[shared_test_runtime]
async fn cmd_export_notes() {
	let client = test_utils::RESOURCES.client().await;

	// delete existing notes and lock
	let _lock = client
		.acquire_lock_with_default(test_utils::locks::NOTES)
		.await
		.unwrap();
	for note in client.list_notes().await.unwrap() {
		client.delete_note(note).await.unwrap();
	}

	let suffix = rand::random::<u32>();
	let checklist_html = "<ul data-checked=\"false\"><li>Item 1</li><li>Item 2</li></ul>\
		<ul data-checked=\"true\"><li>Checked item</li></ul>\
		<ul data-checked=\"false\"><li>other</li></ul>";
	let rich_html = "<p>This is a te<u>st with </u><strong><u>form</u>atting</strong>.</p>";
	let mock_notes = [
		("Plain Text", NoteType::Text, "This is some text"),
		("Markdown", NoteType::Md, "# Title\nSome **formatting**."),
		("Same Title", NoteType::Text, "This is same title note 1"),
		("Same Title", NoteType::Text, "This is same title note 2"),
		("Checklist", NoteType::Checklist, checklist_html),
		("Rich Text", NoteType::Rich, rich_html),
		("Code", NoteType::Code, "<h1>Code note</h1>"),
		("Trashed", NoteType::Text, "This note is trashed"),
		("Archived", NoteType::Text, "This note is archived"),
	];

	let mut notes = Vec::new();
	for (title, note_type, content) in mock_notes {
		let mut note = client
			.create_note(Some(format!("{title} {suffix}")))
			.await
			.unwrap();
		client
			.set_note_type(&mut note, note_type, None)
			.await
			.unwrap();
		client
			.set_note_content(&mut note, content, String::new())
			.await
			.unwrap();
		match title {
			"Trashed" => client.trash_note(&mut note).await.unwrap(),
			"Archived" => client.archive_note(&mut note).await.unwrap(),
			_ => {}
		}
		notes.push(note);
	}

	// a directory that already holds something gets a fresh timestamped directory for the export
	let parent_dir = assert_fs::TempDir::new().unwrap();
	let existing_file = parent_dir.path().join("existing.txt");
	std::fs::write(&existing_file, "kept").unwrap();
	authenticated_cli_with_args!("export-notes", parent_dir.path().to_str().unwrap())
		.success()
		.stdout(predicates::str::contains("Exported"));
	assert_eq!(std::fs::read_to_string(&existing_file).unwrap(), "kept");

	let mut created = std::fs::read_dir(parent_dir.path())
		.unwrap()
		.map(|entry| entry.unwrap().path())
		.filter(|path| *path != existing_file)
		.collect::<Vec<_>>();
	assert_eq!(created.len(), 1, "expected exactly one export directory");
	let export_dir = created.pop().unwrap();
	assert!(
		export_dir
			.file_name()
			.unwrap()
			.to_string_lossy()
			.starts_with("filen-notes-export-"),
		"unexpected export directory name: {}",
		export_dir.display()
	);

	let exported = |relative_path: &str| {
		std::fs::read_to_string(export_dir.join(relative_path))
			.unwrap_or_else(|e| panic!("Failed to read exported note {relative_path}: {e}"))
	};

	// file extension and content per note type
	assert_eq!(
		exported(&format!("Plain Text {suffix}.txt")),
		"This is some text"
	);
	assert_eq!(
		exported(&format!("Markdown {suffix}.md")),
		"# Title\nSome **formatting**."
	);
	assert_eq!(
		exported(&format!("Code {suffix}.txt")),
		"<h1>Code note</h1>"
	);
	assert_eq!(exported(&format!("Rich Text {suffix}.html")), rich_html);

	// checklists are converted to markdown
	assert_eq!(
		exported(&format!("Checklist {suffix}.md")),
		"- [ ] Item 1\n- [ ] Item 2\n- [x] Checked item\n- [ ] other"
	);

	// notes sharing a title are disambiguated with a numeric suffix
	let mut same_title = [
		exported(&format!("Same Title {suffix}.txt")),
		exported(&format!("Same Title {suffix}-1.txt")),
	];
	same_title.sort();
	assert_eq!(
		same_title,
		["This is same title note 1", "This is same title note 2"]
	);

	// trashed and archived notes go into their own subdirectories
	assert_eq!(
		exported(&format!("trash/Trashed {suffix}.txt")),
		"This note is trashed"
	);
	assert_eq!(
		exported(&format!("archive/Archived {suffix}.txt")),
		"This note is archived"
	);

	for note in notes {
		client.delete_note(note).await.unwrap();
	}
}

#[shared_test_runtime]
async fn cmd_sync() {
	// Not `get_resources_with_lock`: the CLI's sync engine takes the drive lock for every pass, and
	// would wait on this test holding it.
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let test_dir_name = test_dir.name().unwrap();

	// one file on each side, which a two-way sync must bring across to the other
	let file = client
		.make_file_builder("from_remote.txt", test_dir.uuid)
		.unwrap();
	client
		.upload_file(file, b"from the remote side")
		.await
		.unwrap();
	let local = assert_fs::TempDir::new().unwrap();
	std::fs::write(local.path().join("from_local.txt"), "from the local side").unwrap();

	// its own config dir, so the engine's database starts empty and is deleted afterwards
	let config_dir = assert_fs::TempDir::new().unwrap();
	// `<local>:<remote>`, which syncs two-way
	let pair = format!("{}:{test_dir_name}", local.path().to_str().unwrap());
	let stdout = sync_once(config_dir.path(), &pair).await;
	assert_eq!(
		std::fs::read_to_string(local.path().join("from_remote.txt")).unwrap(),
		"from the remote side",
		"{stdout}"
	);
	assert!(
		is_remote_file(client, &format!("{test_dir_name}/from_local.txt")).await,
		"{stdout}"
	);
	assert!(stdout.contains("Uploaded from_local.txt"), "{stdout}");
	assert!(stdout.contains("Downloaded from_remote.txt"), "{stdout}");
}

/// A pair an earlier version of the CLI registered kept the delete guard it stored, which held
/// every deletion of a pair with fewer than a million items. Running the pair again puts it back on
/// the default guard.
#[shared_test_runtime]
async fn cmd_sync_puts_a_registered_pair_back_on_the_default_delete_guard() {
	// Not `get_resources_with_lock`, for the reason `cmd_sync` gives.
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir_name = resources.dir.name().unwrap();
	let local = assert_fs::TempDir::new().unwrap();
	let config_dir = assert_fs::TempDir::new().unwrap();
	let db_path = config_dir.path().join("sync-engine.db");
	let pair = format!("{}:{test_dir_name}", local.path().to_str().unwrap());

	std::fs::write(local.path().join("first.txt"), "first").unwrap();
	let stdout = sync_once(config_dir.path(), &pair).await;
	assert!(
		is_remote_file(client, &format!("{test_dir_name}/first.txt")).await,
		"{stdout}"
	);

	// the guard those versions stored
	let engine = SyncEngine::open(Arc::clone(client), db_path.clone())
		.await
		.unwrap();
	let pairs = engine.list_pairs().await.unwrap();
	let [registered] = pairs.as_slice() else {
		panic!("expected the one pair the CLI registered, got {pairs:?}");
	};
	engine
		.set_delete_guard(registered.id, DeleteGuard::new(0, 0.000001).unwrap())
		.await
		.unwrap();
	// releases the database's lock for the CLI
	drop(engine);

	std::fs::write(local.path().join("second.txt"), "second").unwrap();
	let stdout = sync_once(config_dir.path(), &pair).await;
	assert!(
		is_remote_file(client, &format!("{test_dir_name}/second.txt")).await,
		"{stdout}"
	);

	let engine = SyncEngine::open(Arc::clone(client), db_path).await.unwrap();
	let guards = engine
		.list_pairs()
		.await
		.unwrap()
		.into_iter()
		.map(|pair| pair.delete_guard)
		.collect::<Vec<_>>();
	assert_eq!(guards, [DeleteGuard::default()]);
}

/// Held by every CLI sync of these tests, so that they run one at a time: each pass needs the
/// account's drive lock, which the server hands to whichever poll reaches it first after a
/// release, and syncs of these tests running at once would only compete for it.
static CLI_SYNCS: Mutex<()> = Mutex::const_new(());

/// Runs `sync --once --pair <pair>` with `config_dir`, checks that it exits with 0 and returns its
/// stdout.
async fn sync_once(config_dir: &Path, pair: &str) -> String {
	let _syncs = CLI_SYNCS.lock().await;
	let (_auth_config_dir, auth_config) = test_utils::cli::prepare_cli_auth_config().await;
	let output = Command::new(env!("CARGO_BIN_EXE_filen-cli"))
		.args([
			"--auth-config-path",
			auth_config.to_str().unwrap(),
			"--config-dir",
			config_dir.to_str().unwrap(),
			"--skip-update",
			"sync",
			"--once",
			"--pair",
			pair,
		])
		.output()
		.unwrap();
	let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
	assert!(
		output.status.success(),
		"exit status: {}\nstdout:\n{stdout}\nstderr:\n{}",
		output.status,
		String::from_utf8_lossy(&output.stderr)
	);
	stdout
}

async fn is_remote_file(client: &Client, path: &str) -> bool {
	matches!(
		client.find_item_at_path(path).await.unwrap(),
		Some(NonRootFileType::File(_))
	)
}

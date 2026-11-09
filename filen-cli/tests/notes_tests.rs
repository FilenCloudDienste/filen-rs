//! Live tests of the notes commands. Each test creates notes with titles of its own, runs the CLI
//! on them, checks them through the SDK, and deletes every note it made, whether it passes or not.

use std::{future::Future, panic::AssertUnwindSafe};

use assert_fs::TempDir;
use filen_macros::shared_test_runtime;
use filen_sdk_rs::{auth::Client, notes::Note};
use filen_types::{api::v3::notes::NoteType, fs::Uuid};
use futures::FutureExt as _;
use serde_json::Value;
use test_utils::{
	authenticated_cli_with_args,
	cli::{stderr_of, stdout_of},
};

fn json_of(assert: &assert_cmd::assert::Assert) -> Value {
	let stdout = stdout_of(assert);
	serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout}"))
}

/// Runs the CLI with `args`, with `stdin` as its standard input.
async fn cli_with_stdin(stdin: &str, args: &[&str]) -> assert_cmd::assert::Assert {
	let mut command = assert_cmd::cargo::cargo_bin_cmd!("filen-cli");
	command.write_stdin(stdin);
	test_utils::cli::run_authenticated_cli_with_args(&mut command, args).await
}

/// A title prefix no other test run uses.
fn unique_prefix(test: &str) -> String {
	format!("cli-{test}-{}", Uuid::new_v4())
}

/// The notes whose title starts with `prefix`.
async fn notes_with_prefix(client: &Client, prefix: &str) -> Vec<Note> {
	client
		.list_notes()
		.await
		.unwrap()
		.into_iter()
		.filter(|note| note.title().is_some_and(|title| title.starts_with(prefix)))
		.collect()
}

/// Runs `test`, then deletes every note whose title starts with `prefix`, even when the test
/// panicked, and passes the panic on. It holds the notes test lock throughout, since other tests
/// on the account delete every note.
async fn with_notes_cleaned_up(client: &Client, prefix: &str, test: impl Future<Output = ()>) {
	let _lock = client
		.acquire_lock_with_default(test_utils::locks::NOTES)
		.await
		.unwrap();
	let result = AssertUnwindSafe(test).catch_unwind().await;
	for note in notes_with_prefix(client, prefix).await {
		client.delete_note(note).await.unwrap();
	}
	if let Err(panic) = result {
		std::panic::resume_unwind(panic);
	}
}

async fn content_of(client: &Client, uuid: Uuid) -> String {
	let mut note = client.get_note(uuid).await.unwrap().unwrap();
	client.get_note_content(&mut note).await.unwrap().unwrap()
}

/// The note in a `notes --json` array with this UUID.
fn listed(json: &Value, uuid: Uuid) -> Option<&Value> {
	json.as_array()
		.unwrap()
		.iter()
		.find(|note| note["uuid"] == uuid.to_string())
}

#[shared_test_runtime]
async fn note_commands_create_print_edit_and_trash_a_note() {
	let client = test_utils::RESOURCES.client().await;
	let title = unique_prefix("note-lifecycle");
	with_notes_cleaned_up(&client, &title, async {
		let created = json_of(
			&cli_with_stdin(
				"# Heading\nbody\n",
				&["note-new", &title, "--type", "md", "--json"],
			)
			.await
			.success(),
		);
		assert_eq!(created["title"], title.as_str());
		assert_eq!(created["type"], "md");
		let uuid: Uuid = created["uuid"].as_str().unwrap().parse().unwrap();
		let note = client.get_note(uuid).await.unwrap().unwrap();
		assert_eq!(note.note_type(), NoteType::Md);
		assert_eq!(note.preview(), Some("# Heading"));
		assert_eq!(content_of(&client, uuid).await, "# Heading\nbody\n");

		let listing = json_of(&authenticated_cli_with_args!("notes", "--json").success());
		let entry = listed(&listing, uuid).expect("the new note is listed");
		assert_eq!(entry["title"], title.as_str());
		assert_eq!(entry["type"], "md");
		assert_eq!(entry["trashed"], false);
		assert_eq!(entry["archived"], false);
		let plain = stdout_of(&authenticated_cli_with_args!("notes").success());
		let line = plain
			.lines()
			.find(|line| line.contains(&uuid.to_string()))
			.expect("the new note has a line");
		assert!(line.ends_with(&format!("  {title}")), "{line}");
		assert!(line.contains("  md  "), "{line}");

		let printed = authenticated_cli_with_args!("note-cat", &title).success();
		assert_eq!(stdout_of(&printed), "# Heading\nbody\n");

		let dir = TempDir::new().unwrap();
		let edited_file = dir.path().join("edited.md");
		std::fs::write(&edited_file, "# Heading\nedited body").unwrap();
		authenticated_cli_with_args!(
			"note-edit",
			&uuid.to_string(),
			"--content-file",
			edited_file.to_str().unwrap()
		)
		.success()
		.stdout(predicates::str::contains("Saved note"));
		assert_eq!(content_of(&client, uuid).await, "# Heading\nedited body");
		let printed = authenticated_cli_with_args!("note-cat", &uuid.to_string()).success();
		assert_eq!(stdout_of(&printed), "# Heading\nedited body\n");

		// the same content again changes nothing
		authenticated_cli_with_args!(
			"note-edit",
			&title,
			"--content-file",
			edited_file.to_str().unwrap()
		)
		.success()
		.stdout(predicates::str::contains("is unchanged"));

		// replaced from stdin
		cli_with_stdin("from stdin", &["note-edit", &title, "--content-file", "-"])
			.await
			.success();
		assert_eq!(content_of(&client, uuid).await, "from stdin");

		authenticated_cli_with_args!("note-rm", &title)
			.success()
			.stdout(predicates::str::contains("to the trash"));
		let note = client.get_note(uuid).await.unwrap().unwrap();
		assert!(note.trashed());
		let listing = json_of(&authenticated_cli_with_args!("notes", "--json").success());
		assert!(listed(&listing, uuid).is_none(), "a trashed note is listed");
		let listing = json_of(&authenticated_cli_with_args!("notes", "--all", "--json").success());
		assert_eq!(listed(&listing, uuid).unwrap()["trashed"], true);

		// a permanent removal needs --yes without a terminal
		let refused = authenticated_cli_with_args!("note-rm", &title, "--permanent").code(2);
		assert!(
			stderr_of(&refused).contains("Pass --yes to answer yes."),
			"{}",
			stderr_of(&refused)
		);
		assert!(client.get_note(uuid).await.unwrap().is_some());
		authenticated_cli_with_args!("note-rm", &title, "--permanent", "--yes").success();
		assert!(client.get_note(uuid).await.unwrap().is_none());
	})
	.await;
}

#[shared_test_runtime]
async fn a_checklist_note_is_written_and_printed_as_a_task_list() {
	let client = test_utils::RESOURCES.client().await;
	let title = unique_prefix("note-checklist");
	with_notes_cleaned_up(&client, &title, async {
		let dir = TempDir::new().unwrap();
		let content_file = dir.path().join("list.md");
		std::fs::write(&content_file, "- [ ] milk\n- [x] eggs & ham\n- [ ] bread\n").unwrap();
		let created = json_of(
			&authenticated_cli_with_args!(
				"note-new",
				&title,
				"--type",
				"checklist",
				"--content-file",
				content_file.to_str().unwrap(),
				"--json"
			)
			.success(),
		);
		let uuid: Uuid = created["uuid"].as_str().unwrap().parse().unwrap();
		assert_eq!(
			content_of(&client, uuid).await,
			"<ul data-checked=\"false\"><li>milk</li></ul>\
			<ul data-checked=\"true\"><li>eggs &amp; ham</li></ul>\
			<ul data-checked=\"false\"><li>bread</li></ul>"
		);
		let note = client.get_note(uuid).await.unwrap().unwrap();
		assert_eq!(note.note_type(), NoteType::Checklist);
		assert_eq!(note.preview(), Some("milk"));

		let printed = authenticated_cli_with_args!("note-cat", &title).success();
		assert_eq!(
			stdout_of(&printed),
			"- [ ] milk\n- [x] eggs & ham\n- [ ] bread\n"
		);
	})
	.await;
}

#[shared_test_runtime]
async fn an_ambiguous_title_lists_the_notes_instead_of_picking_one() {
	let client = test_utils::RESOURCES.client().await;
	let title = unique_prefix("note-ambiguous");
	with_notes_cleaned_up(&client, &title, async {
		let first = client.create_note(Some(title.clone())).await.unwrap();
		let second = client.create_note(Some(title.clone())).await.unwrap();

		let failed = authenticated_cli_with_args!("note-cat", &title).code(1);
		let stderr = stderr_of(&failed);
		assert!(stderr.contains("2 notes are titled"), "{stderr}");
		assert!(stderr.contains(&first.uuid().to_string()), "{stderr}");
		assert!(stderr.contains(&second.uuid().to_string()), "{stderr}");
		assert_eq!(stdout_of(&failed), "");

		authenticated_cli_with_args!("note-rm", &title).code(1);
		for note in [&first, &second] {
			let note = client.get_note(*note.uuid()).await.unwrap().unwrap();
			assert!(!note.trashed());
		}
	})
	.await;
}

#[shared_test_runtime]
async fn import_notes_creates_a_note_from_each_file() {
	let client = test_utils::RESOURCES.client().await;
	let prefix = unique_prefix("note-import");
	with_notes_cleaned_up(&client, &prefix, async {
		let dir = TempDir::new().unwrap();
		let files = [
			(format!("{prefix}-md.md"), "# Markdown"),
			(format!("{prefix}-text.txt"), "plain text"),
			(format!("{prefix}-rich.html"), "<p>rich</p>"),
			("ignored.pdf".to_owned(), "not a note"),
			(format!("trash/{prefix}-trashed.txt"), "trashed"),
		];
		for (name, content) in &files {
			let path = dir.path().join(name);
			std::fs::create_dir_all(path.parent().unwrap()).unwrap();
			std::fs::write(path, content).unwrap();
		}

		authenticated_cli_with_args!("import-notes", dir.path().to_str().unwrap())
			.success()
			.stdout(predicates::str::contains("Imported 3 notes"));
		let mut imported = Vec::new();
		for note in notes_with_prefix(&client, &prefix).await {
			let mut note = note;
			let content = client.get_note_content(&mut note).await.unwrap().unwrap();
			imported.push((
				note.title().unwrap().to_owned(),
				note.note_type(),
				content,
				note.trashed(),
			));
		}
		imported.sort_by(|a, b| a.0.cmp(&b.0));
		assert_eq!(
			imported,
			[
				(
					format!("{prefix}-md"),
					NoteType::Md,
					"# Markdown".to_owned(),
					false
				),
				(
					format!("{prefix}-rich"),
					NoteType::Rich,
					"<p>rich</p>".to_owned(),
					false
				),
				(
					format!("{prefix}-text"),
					NoteType::Text,
					"plain text".to_owned(),
					false
				),
			]
		);

		// with --include-trashed, the trash/ subdirectory is imported into the trash
		let trash_only = TempDir::new().unwrap();
		std::fs::create_dir(trash_only.path().join("trash")).unwrap();
		std::fs::write(
			trash_only
				.path()
				.join("trash")
				.join(format!("{prefix}-trashed.txt")),
			"trashed",
		)
		.unwrap();
		authenticated_cli_with_args!("import-notes", trash_only.path().to_str().unwrap())
			.success()
			.stdout(predicates::str::contains("No notes to import"));
		authenticated_cli_with_args!(
			"import-notes",
			trash_only.path().to_str().unwrap(),
			"--include-trashed"
		)
		.success()
		.stdout(predicates::str::contains("Imported 1 notes"));
		let trashed = notes_with_prefix(&client, &format!("{prefix}-trashed")).await;
		assert_eq!(trashed.len(), 1);
		assert!(trashed[0].trashed());
	})
	.await;
}

use filen_macros::shared_test_runtime;
use filen_sdk_rs::fs::{HasName, categories::NonRootFileType};
use rand::TryRngCore;
use test_utils::authenticated_cli_with_args;

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
		.acquire_lock_with_default("test:rs:trash")
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

	// empty-trash
	authenticated_cli_with_args!("empty-trash")
		.success()
		.stdout(predicates::str::contains("Emptied trash"));

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

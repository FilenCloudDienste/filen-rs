use std::sync::Arc;
#[cfg(unix)]
use std::{
	process::{Command, Stdio},
	time::{Duration, Instant},
};

use assert_cmd::cargo::cargo_bin_cmd;
use chrono::{TimeZone as _, Utc};
use filen_macros::shared_test_runtime;
use filen_sdk_rs::consts::CHUNK_SIZE;
use filen_sdk_rs::{
	auth::Client,
	fs::{
		HasName, HasUUID,
		categories::NonRootFileType,
		dir::RemoteDirectory,
		file::{RemoteFile, traits::HasFileInfo as _},
	},
	io::client_impl::IoSharedClientExt,
	sync::lock::ResourceLock,
};
use test_utils::{authenticated_cli_with_args, cli::run_authenticated_cli_with_args};

/// Uploads `content` as `name` into `parent`.
async fn upload(
	client: &Client,
	parent: &RemoteDirectory,
	name: &str,
	content: &[u8],
) -> RemoteFile {
	let builder = client.make_file_builder(name, parent.uuid()).unwrap();
	client.upload_file(builder, content).await.unwrap()
}

/// The content of the file at `path`.
async fn content_at(client: &Client, path: &str) -> Vec<u8> {
	let Some(NonRootFileType::File(file)) = client.find_item_at_path(path).await.unwrap() else {
		panic!("Expected a file at {path}");
	};
	client.download_file(file.as_ref()).await.unwrap()
}

#[shared_test_runtime]
async fn mv_into_a_directory_named_with_a_trailing_slash_keeps_the_source_name() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = format!("/{}", resources.dir.name().unwrap());
	test_utils::create_remote_file_structure_outline(
		client,
		resources.dir.clone(),
		&["a.txt", "dest/"],
	)
	.await
	.unwrap();

	authenticated_cli_with_args!(
		"mv",
		&format!("{test_dir}/a.txt"),
		&format!("{test_dir}/dest/")
	)
	.success();

	assert!(matches!(
		client
			.find_item_at_path(&format!("{test_dir}/dest/a.txt"))
			.await
			.unwrap(),
		Some(NonRootFileType::File(_))
	));
}

#[shared_test_runtime]
async fn mv_to_a_missing_directory_named_with_a_trailing_slash_fails_without_renaming() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = format!("/{}", resources.dir.name().unwrap());
	test_utils::create_remote_file_structure_outline(client, resources.dir.clone(), &["a.txt"])
		.await
		.unwrap();

	authenticated_cli_with_args!(
		"mv",
		&format!("{test_dir}/a.txt"),
		&format!("{test_dir}/missing/")
	)
	.failure()
	.stderr(predicates::str::contains(format!(
		"No such destination directory: {test_dir}/missing"
	)));

	assert!(matches!(
		client
			.find_item_at_path(&format!("{test_dir}/a.txt"))
			.await
			.unwrap(),
		Some(NonRootFileType::File(_))
	));
	assert!(
		client
			.find_item_at_path(&format!("{test_dir}/missing"))
			.await
			.unwrap()
			.is_none()
	);
}

#[shared_test_runtime]
async fn mkdir_recursive_creates_a_path_with_a_trailing_slash() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = format!("/{}", resources.dir.name().unwrap());

	authenticated_cli_with_args!("mkdir", "-r", &format!("{test_dir}/x/y/"))
		.success()
		.stdout(predicates::str::contains(format!(
			"Directory created: {test_dir}/x/y"
		)));

	assert!(matches!(
		client
			.find_item_at_path(&format!("{test_dir}/x/y"))
			.await
			.unwrap(),
		Some(NonRootFileType::Dir(_))
	));
}

#[shared_test_runtime]
async fn mkdir_of_an_existing_directory_says_it_exists() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let test_dir = format!("/{}", resources.dir.name().unwrap());

	authenticated_cli_with_args!("mkdir", &format!("{test_dir}/x"))
		.success()
		.stdout(predicates::str::contains(format!(
			"Directory created: {test_dir}/x"
		)));
	authenticated_cli_with_args!("mkdir", &format!("{test_dir}/x"))
		.success()
		.stdout(predicates::str::contains(format!(
			"Directory already exists: {test_dir}/x"
		)));
}

#[shared_test_runtime]
async fn mkdir_of_a_name_differing_only_in_case_says_which_directory_exists() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = format!("/{}", resources.dir.name().unwrap());
	test_utils::create_remote_file_structure_outline(client, resources.dir.clone(), &["Docs/"])
		.await
		.unwrap();

	authenticated_cli_with_args!("mkdir", &format!("{test_dir}/docs"))
		.success()
		.stdout(predicates::str::contains(format!(
			"Directory already exists: {test_dir}/Docs"
		)));
}

#[shared_test_runtime]
async fn mkdir_of_a_file_fails() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = format!("/{}", resources.dir.name().unwrap());
	test_utils::create_remote_file_structure_outline(client, resources.dir.clone(), &["a.txt"])
		.await
		.unwrap();

	authenticated_cli_with_args!("mkdir", &format!("{test_dir}/a.txt"))
		.failure()
		.stderr(predicates::str::contains(format!(
			"Not a directory: {test_dir}/a.txt"
		)));
}

#[shared_test_runtime]
async fn cp_refuses_a_file_whose_name_exists_in_the_destination() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = format!("/{}", resources.dir.name().unwrap());
	test_utils::create_remote_file_structure_outline(
		client,
		resources.dir.clone(),
		&["a.txt", "dest/A.TXT"],
	)
	.await
	.unwrap();
	let Some(NonRootFileType::File(existing)) = client
		.find_item_at_path(&format!("{test_dir}/dest/A.TXT"))
		.await
		.unwrap()
	else {
		panic!("Expected a file");
	};
	let existing_uuid = existing.uuid;

	authenticated_cli_with_args!(
		"cp",
		&format!("{test_dir}/a.txt"),
		&format!("{test_dir}/dest")
	)
	.failure()
	.stderr(predicates::str::contains(format!(
		"Destination already exists: {test_dir}/dest/A.TXT"
	)));

	// an upload under the same name would have replaced the file with a new uuid
	let Some(NonRootFileType::File(current)) = client
		.find_item_at_path(&format!("{test_dir}/dest/A.TXT"))
		.await
		.unwrap()
	else {
		panic!("Expected a file");
	};
	assert_eq!(current.uuid, existing_uuid);
}

#[shared_test_runtime]
async fn cp_refuses_a_directory_whose_name_exists_in_the_destination() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = format!("/{}", resources.dir.name().unwrap());
	test_utils::create_remote_file_structure_outline(
		client,
		resources.dir.clone(),
		&["src/inner.txt", "dest/src/"],
	)
	.await
	.unwrap();

	authenticated_cli_with_args!(
		"cp",
		&format!("{test_dir}/src"),
		&format!("{test_dir}/dest")
	)
	.failure()
	.stderr(predicates::str::contains(format!(
		"Destination already exists: {test_dir}/dest/src"
	)));

	assert!(
		client
			.find_item_at_path(&format!("{test_dir}/dest/src/inner.txt"))
			.await
			.unwrap()
			.is_none()
	);
}

#[shared_test_runtime]
async fn mv_refuses_a_destination_whose_name_differs_only_in_case() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = format!("/{}", resources.dir.name().unwrap());
	test_utils::create_remote_file_structure_outline(
		client,
		resources.dir.clone(),
		&["a.txt", "dest/A.TXT"],
	)
	.await
	.unwrap();

	authenticated_cli_with_args!(
		"mv",
		&format!("{test_dir}/a.txt"),
		&format!("{test_dir}/dest")
	)
	.failure()
	.stderr(predicates::str::contains(format!(
		"Destination already exists: {test_dir}/dest/A.TXT"
	)));

	assert!(matches!(
		client
			.find_item_at_path(&format!("{test_dir}/a.txt"))
			.await
			.unwrap(),
		Some(NonRootFileType::File(_))
	));
}

#[shared_test_runtime]
async fn mv_renames_an_item_to_a_name_differing_only_in_case() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = format!("/{}", resources.dir.name().unwrap());
	test_utils::create_remote_file_structure_outline(client, resources.dir.clone(), &["a.txt"])
		.await
		.unwrap();

	authenticated_cli_with_args!(
		"mv",
		&format!("{test_dir}/a.txt"),
		&format!("{test_dir}/A.txt")
	)
	.success();

	assert!(matches!(
		client
			.find_item_at_path(&format!("{test_dir}/A.txt"))
			.await
			.unwrap(),
		Some(NonRootFileType::File(_))
	));
}

#[shared_test_runtime]
async fn cp_copies_a_file_into_a_directory() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = format!("/{}", resources.dir.name().unwrap());
	test_utils::create_remote_file_structure_outline(client, resources.dir.clone(), &["dest/"])
		.await
		.unwrap();
	upload(client, &resources.dir, "a.txt", b"the file to copy").await;

	authenticated_cli_with_args!(
		"cp",
		&format!("{test_dir}/a.txt"),
		&format!("{test_dir}/dest")
	)
	.success()
	.stdout(predicates::str::contains(format!(
		"Copied {test_dir}/a.txt to {test_dir}/dest/a.txt"
	)));

	assert_eq!(
		content_at(client, &format!("{test_dir}/dest/a.txt")).await,
		b"the file to copy"
	);
	assert_eq!(
		content_at(client, &format!("{test_dir}/a.txt")).await,
		b"the file to copy"
	);
}

#[shared_test_runtime]
async fn cp_copies_a_nested_directory_with_identical_content() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = format!("/{}", resources.dir.name().unwrap());
	test_utils::create_remote_file_structure_outline(
		client,
		resources.dir.clone(),
		&["src/sub/deeper/", "dest/"],
	)
	.await
	.unwrap();
	let find_dir = async |path: &str| {
		let Some(NonRootFileType::Dir(dir)) = client.find_item_at_path(path).await.unwrap() else {
			panic!("Expected a directory at {path}");
		};
		dir.into_owned()
	};
	let src = find_dir(&format!("{test_dir}/src")).await;
	let deeper = find_dir(&format!("{test_dir}/src/sub/deeper")).await;
	let binary = (0..=255u8).cycle().take(70_000).collect::<Vec<u8>>();
	upload(client, &src, "top.txt", b"at the top").await;
	upload(client, &deeper, "binary.bin", &binary).await;

	authenticated_cli_with_args!(
		"cp",
		&format!("{test_dir}/src"),
		&format!("{test_dir}/dest")
	)
	.success()
	.stdout(predicates::str::contains(format!(
		"Copied {test_dir}/src to {test_dir}/dest/src"
	)));

	assert_eq!(
		content_at(client, &format!("{test_dir}/dest/src/top.txt")).await,
		b"at the top"
	);
	assert_eq!(
		content_at(
			client,
			&format!("{test_dir}/dest/src/sub/deeper/binary.bin")
		)
		.await,
		binary
	);
}

#[shared_test_runtime]
async fn cp_copies_a_file_to_a_new_name() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = format!("/{}", resources.dir.name().unwrap());
	test_utils::create_remote_file_structure_outline(client, resources.dir.clone(), &["dest/"])
		.await
		.unwrap();
	upload(client, &resources.dir, "a.txt", b"copied under a new name").await;

	authenticated_cli_with_args!(
		"cp",
		&format!("{test_dir}/a.txt"),
		&format!("{test_dir}/dest/b.txt")
	)
	.success()
	.stdout(predicates::str::contains(format!(
		"Copied {test_dir}/a.txt to {test_dir}/dest/b.txt"
	)));

	assert_eq!(
		content_at(client, &format!("{test_dir}/dest/b.txt")).await,
		b"copied under a new name"
	);
	assert!(
		client
			.find_item_at_path(&format!("{test_dir}/dest/a.txt"))
			.await
			.unwrap()
			.is_none()
	);
}

#[shared_test_runtime]
async fn cp_copies_a_directory_to_a_new_name_next_to_it() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = format!("/{}", resources.dir.name().unwrap());
	test_utils::create_remote_file_structure_outline(
		client,
		resources.dir.clone(),
		&["src/inner.txt"],
	)
	.await
	.unwrap();

	authenticated_cli_with_args!(
		"cp",
		&format!("{test_dir}/src"),
		&format!("{test_dir}/copy")
	)
	.success();

	assert!(matches!(
		client
			.find_item_at_path(&format!("{test_dir}/copy/inner.txt"))
			.await
			.unwrap(),
		Some(NonRootFileType::File(_))
	));
}

#[shared_test_runtime]
async fn cp_refuses_a_new_path_whose_name_exists_in_its_directory() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = format!("/{}", resources.dir.name().unwrap());
	test_utils::create_remote_file_structure_outline(
		client,
		resources.dir.clone(),
		&["a.txt", "b.txt"],
	)
	.await
	.unwrap();

	authenticated_cli_with_args!(
		"cp",
		&format!("{test_dir}/a.txt"),
		&format!("{test_dir}/B.TXT")
	)
	.failure()
	.stderr(predicates::str::contains(format!(
		"Destination already exists: {test_dir}/b.txt"
	)));

	assert!(
		client
			.find_item_at_path(&format!("{test_dir}/B.TXT"))
			.await
			.unwrap()
			.is_none()
	);
}

#[shared_test_runtime]
async fn cp_refuses_to_copy_a_directory_into_itself() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = format!("/{}", resources.dir.name().unwrap());
	test_utils::create_remote_file_structure_outline(
		client,
		resources.dir.clone(),
		&["src/inner.txt"],
	)
	.await
	.unwrap();

	authenticated_cli_with_args!(
		"cp",
		&format!("{test_dir}/src"),
		&format!("{test_dir}/src/copy")
	)
	.failure()
	.stderr(predicates::str::contains(format!(
		"Cannot copy {test_dir}/src into itself: {test_dir}/src/copy"
	)));

	assert!(
		client
			.find_item_at_path(&format!("{test_dir}/src/copy"))
			.await
			.unwrap()
			.is_none()
	);
}

/// Unix only: the CLI is interrupted the way a user interrupts it, with SIGINT.
#[cfg(unix)]
#[shared_test_runtime]
async fn cp_interrupted_by_ctrl_c_stops_and_says_what_it_copied() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = format!("/{}", resources.dir.name().unwrap());
	test_utils::create_remote_file_structure_outline(client, resources.dir.clone(), &["src/"])
		.await
		.unwrap();
	let Some(NonRootFileType::Dir(src)) = client
		.find_item_at_path(&format!("{test_dir}/src"))
		.await
		.unwrap()
	else {
		panic!("Expected a directory");
	};
	// enough that the copy is still running when its directory shows up
	let content = (0..=255u8)
		.cycle()
		.take(4 * 1024 * 1024)
		.collect::<Vec<u8>>();
	for i in 0..8 {
		upload(client, &src, &format!("{i}.bin"), &content).await;
	}

	let (_auth_config_dir, auth_config) = test_utils::cli::prepare_cli_auth_config().await;
	let mut cli = Command::new(env!("CARGO_BIN_EXE_filen-cli"))
		.args([
			"--auth-config-path",
			auth_config.to_str().unwrap(),
			"--skip-update",
			"cp",
			&format!("{test_dir}/src"),
			&format!("{test_dir}/copy"),
		])
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()
		.unwrap();
	let deadline = Instant::now() + Duration::from_secs(120);
	while client
		.find_item_at_path(&format!("{test_dir}/copy"))
		.await
		.unwrap()
		.is_none()
	{
		assert!(
			Instant::now() < deadline && cli.try_wait().unwrap().is_none(),
			"the copy never started"
		);
		tokio::time::sleep(Duration::from_millis(100)).await;
	}
	let interrupted = Command::new("kill")
		.args(["-INT", &cli.id().to_string()])
		.status()
		.unwrap();
	assert!(interrupted.success());
	let output = cli.wait_with_output().unwrap();
	let stderr = String::from_utf8_lossy(&output.stderr);

	assert_eq!(output.status.code(), Some(130), "stderr:\n{stderr}");
	assert!(
		stderr.contains("Copied only ") && stderr.contains(&format!("to {test_dir}/copy")),
		"stderr:\n{stderr}"
	);
}

/// Uploads a file named `name` into a new directory `dir` of the test directory, and trashes it.
async fn trashed_file(
	client: &Client,
	test_dir: &RemoteDirectory,
	dir: &str,
	name: &str,
) -> RemoteFile {
	let dir = client.create_dir(&test_dir.into(), dir).await.unwrap();
	let mut file = upload(client, &dir, name, b"trashed").await;
	client.trash_file(&mut file).await.unwrap();
	file
}

/// Holds the trash lock while the test touches the trash: the trash is the whole account's, and
/// other tests on the account empty it.
async fn lock_trash(client: &Client) -> Arc<ResourceLock> {
	client
		.acquire_lock_with_default(test_utils::locks::TRASH)
		.await
		.unwrap()
}

async fn is_in_trash(client: &Client, file: &RemoteFile) -> bool {
	let (_, files) = client
		.list_trash(None::<&fn(u64, Option<u64>)>)
		.await
		.unwrap();
	files.iter().any(|trashed| trashed.uuid() == file.uuid())
}

#[shared_test_runtime]
async fn trash_delete_deletes_the_trashed_item_it_names() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let _trash_lock = lock_trash(client).await;
	// the trash is the whole account's, so the name is this test's own
	let name = format!("{}.txt", resources.dir.name().unwrap());
	let file = trashed_file(client, &resources.dir, "a", &name).await;

	authenticated_cli_with_args!("trash-delete", &name, "--yes")
		.success()
		.stdout(predicates::str::contains(format!(
			"Permanently deleted file: {name}"
		)));

	assert!(!is_in_trash(client, &file).await);
}

#[shared_test_runtime]
async fn trash_delete_refuses_a_name_several_trashed_items_share() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let _trash_lock = lock_trash(client).await;
	let name = format!("{}.txt", resources.dir.name().unwrap());
	let first = trashed_file(client, &resources.dir, "a", &name).await;
	let second = trashed_file(client, &resources.dir, "b", &name).await;

	authenticated_cli_with_args!("trash-delete", &name, "--yes")
		.failure()
		.stderr(predicates::str::contains(format!(
			"2 trashed items are named {name}, so name one by its UUID"
		)))
		.stderr(predicates::str::contains(first.uuid().to_string()))
		.stderr(predicates::str::contains(second.uuid().to_string()));

	assert!(is_in_trash(client, &first).await);
	assert!(is_in_trash(client, &second).await);
	client.delete_file_permanently(first).await.unwrap();
	client.delete_file_permanently(second).await.unwrap();
}

#[shared_test_runtime]
async fn trash_delete_deletes_a_trashed_item_named_by_its_uuid() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let _trash_lock = lock_trash(client).await;
	let name = format!("{}.txt", resources.dir.name().unwrap());
	let first = trashed_file(client, &resources.dir, "a", &name).await;
	let second = trashed_file(client, &resources.dir, "b", &name).await;

	authenticated_cli_with_args!("trash-delete", &second.uuid().to_string(), "--yes").success();

	assert!(is_in_trash(client, &first).await);
	assert!(!is_in_trash(client, &second).await);
	client.delete_file_permanently(first).await.unwrap();
}

#[shared_test_runtime]
async fn trash_restore_restores_the_trashed_item_it_names() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let _trash_lock = lock_trash(client).await;
	let test_dir = format!("/{}", resources.dir.name().unwrap());
	let name = format!("{}.txt", resources.dir.name().unwrap());
	let file = trashed_file(client, &resources.dir, "a", &name).await;

	authenticated_cli_with_args!("trash-restore", &name)
		.success()
		.stdout(predicates::str::contains(format!("Restored file: {name}")));

	assert!(!is_in_trash(client, &file).await);
	let Some(NonRootFileType::File(restored)) = client
		.find_item_at_path(&format!("{test_dir}/a/{name}"))
		.await
		.unwrap()
	else {
		panic!("Expected the restored file at {test_dir}/a/{name}");
	};
	assert_eq!(restored.uuid(), file.uuid());
}

#[shared_test_runtime]
async fn trash_restore_refuses_a_name_several_trashed_items_share() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let _trash_lock = lock_trash(client).await;
	let name = format!("{}.txt", resources.dir.name().unwrap());
	let first = trashed_file(client, &resources.dir, "a", &name).await;
	let second = trashed_file(client, &resources.dir, "b", &name).await;

	authenticated_cli_with_args!("trash-restore", &name)
		.failure()
		.stderr(predicates::str::contains(format!(
			"2 trashed items are named {name}, so name one by its UUID"
		)));

	assert!(is_in_trash(client, &first).await);
	assert!(is_in_trash(client, &second).await);
	client.delete_file_permanently(first).await.unwrap();
	client.delete_file_permanently(second).await.unwrap();
}

#[shared_test_runtime]
async fn trash_restore_restores_every_trashed_item_it_names() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let _trash_lock = lock_trash(client).await;
	let first_name = format!("{}-1.txt", resources.dir.name().unwrap());
	let second_name = format!("{}-2.txt", resources.dir.name().unwrap());
	let first = trashed_file(client, &resources.dir, "a", &first_name).await;
	let second = trashed_file(client, &resources.dir, "b", &second_name).await;

	authenticated_cli_with_args!(
		"trash-restore",
		&first_name,
		&format!("uuid:{}", second.uuid())
	)
	.success()
	.stdout(format!(
		"✔ Restored file: {first_name}\n✔ Restored file: {second_name}\n"
	));

	assert!(!is_in_trash(client, &first).await);
	assert!(!is_in_trash(client, &second).await);
}

#[shared_test_runtime]
async fn trash_delete_deletes_every_trashed_item_it_names_with_yes() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let _trash_lock = lock_trash(client).await;
	let name = format!("{}.txt", resources.dir.name().unwrap());
	let first = trashed_file(client, &resources.dir, "a", &name).await;
	let second = trashed_file(client, &resources.dir, "b", &name).await;

	authenticated_cli_with_args!(
		"trash-delete",
		&first.uuid().to_string(),
		&format!("uuid:{}", second.uuid()),
		"--yes"
	)
	.success()
	.stdout(format!(
		"✔ Permanently deleted file: {name}\n✔ Permanently deleted file: {name}\n"
	));

	assert!(!is_in_trash(client, &first).await);
	assert!(!is_in_trash(client, &second).await);
}

#[shared_test_runtime]
async fn trash_delete_changes_nothing_when_one_item_it_names_is_not_in_the_trash() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let _trash_lock = lock_trash(client).await;
	let name = format!("{}.txt", resources.dir.name().unwrap());
	let missing = format!("{}-missing.txt", resources.dir.name().unwrap());
	let file = trashed_file(client, &resources.dir, "a", &name).await;

	authenticated_cli_with_args!("trash-delete", &name, &missing, "--yes")
		.failure()
		.stdout("")
		.stderr(predicates::str::ends_with(format!(
			"✘ No such item in the trash: {missing}\n"
		)));

	assert!(is_in_trash(client, &file).await);
	client.delete_file_permanently(file).await.unwrap();
}

#[shared_test_runtime]
async fn list_trash_long_shows_the_size_and_modified_date() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let _trash_lock = lock_trash(client).await;
	let name = format!("{}.txt", resources.dir.name().unwrap());
	let builder = client
		.make_file_builder(&name, resources.dir.uuid())
		.unwrap()
		.modified(Utc.with_ymd_and_hms(2024, 3, 9, 10, 20, 30).unwrap());
	let mut file = client.upload_file(builder, b"content").await.unwrap();
	client.trash_file(&mut file).await.unwrap();

	let assert = authenticated_cli_with_args!("list-trash", "-l").success();
	let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
	let line = stdout
		.lines()
		.find(|line| line.ends_with(&format!("  {name}")))
		.unwrap_or_else(|| panic!("No {name} in:\n{stdout}"));
	// the size column is as wide as the trash's widest size
	assert_eq!(
		line.trim_start(),
		format!("7 B  2024-03-09 10:20:30 (UTC)  {name}")
	);
	client.delete_file_permanently(file).await.unwrap();
}

/// Runs the CLI with `args`, logged out and with no terminal on stdin.
fn run_logged_out_without_a_terminal(args: &[&str]) -> std::process::Output {
	let config_dir = assert_fs::TempDir::new().unwrap();
	std::process::Command::new(env!("CARGO_BIN_EXE_filen-cli"))
		.env("FILEN_CLI_TESTING_DISABLE_KEYRING", "1")
		.env("FILEN_CLI_TESTING_DISABLE_AUTODETECTED_AUTH_CONFIG", "1")
		.env_remove("FILEN_CLI_EMAIL")
		.env_remove("FILEN_CLI_PASSWORD")
		.env_remove("FILEN_CLI_2FA_CODE")
		.arg("--config-dir")
		.arg(config_dir.path())
		.arg("--skip-update")
		.args(args)
		.stdin(std::process::Stdio::null())
		.output()
		.unwrap()
}

#[test]
fn trash_delete_of_a_named_item_without_yes_asks_for_it_before_logging_in() {
	let output = run_logged_out_without_a_terminal(&["trash-delete", "a.txt"]);

	assert_eq!(output.status.code(), Some(2));
	assert_eq!(
		console::strip_ansi_codes(&String::from_utf8(output.stderr).unwrap()),
		"✘ Cannot ask \"Permanently delete a.txt?\": stdin is not a terminal. Pass --yes to \
		 answer yes.\n"
	);
}

#[test]
fn trash_delete_of_several_items_without_yes_asks_for_it_before_logging_in() {
	let output = run_logged_out_without_a_terminal(&["trash-delete", "a.txt", "b.txt"]);

	assert_eq!(output.status.code(), Some(2));
	assert_eq!(
		console::strip_ansi_codes(&String::from_utf8(output.stderr).unwrap()),
		"✘ Cannot ask \"Permanently delete 2 items?\": stdin is not a terminal. Pass --yes to \
		 answer yes.\n"
	);
}

#[test]
fn trash_delete_all_without_yes_asks_for_it_before_logging_in() {
	let output = run_logged_out_without_a_terminal(&["trash-delete", "--all"]);

	assert_eq!(output.status.code(), Some(2));
	assert_eq!(
		console::strip_ansi_codes(&String::from_utf8(output.stderr).unwrap()),
		"✘ Cannot ask \"Permanently delete all trashed items?\": stdin is not a terminal. Pass \
		 --yes to answer yes.\n"
	);
}

#[test]
fn trash_restore_refuses_all_together_with_named_items() {
	let output = run_logged_out_without_a_terminal(&["trash-restore", "--all", "a.txt"]);

	assert_eq!(output.status.code(), Some(2));
}

#[test]
fn empty_trash_without_yes_asks_for_it_before_logging_in() {
	let output = run_logged_out_without_a_terminal(&["empty-trash"]);

	assert_eq!(output.status.code(), Some(2));
	assert_eq!(
		console::strip_ansi_codes(&String::from_utf8(output.stderr).unwrap()),
		"✘ Cannot ask \"Permanently delete all trashed items?\": stdin is not a terminal. Pass \
		 --yes to answer yes.\n"
	);
}

/// What the CLI printed on stdout, parsed as JSON.
fn printed_json(assert: &assert_cmd::assert::Assert) -> serde_json::Value {
	serde_json::from_slice(&assert.get_output().stdout).unwrap()
}

/// The printed JSON listing's object for the item with `uuid`.
fn listed<'a>(listing: &'a serde_json::Value, uuid: &str) -> &'a serde_json::Value {
	listing
		.as_array()
		.unwrap()
		.iter()
		.find(|item| item["uuid"] == uuid)
		.unwrap_or_else(|| panic!("No item {uuid} in {listing}"))
}

#[shared_test_runtime]
async fn ls_a_file_prints_that_file() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = format!("/{}", resources.dir.name().unwrap());
	let file = upload(client, &resources.dir, "a.txt", b"content").await;

	authenticated_cli_with_args!("ls", &format!("{test_dir}/a.txt"))
		.success()
		.stdout("a.txt\n");

	let assert =
		authenticated_cli_with_args!("ls", &format!("{test_dir}/a.txt"), "--json").success();
	let listing = printed_json(&assert);
	assert_eq!(listing.as_array().unwrap().len(), 1, "{listing}");
	assert_eq!(listing[0]["name"], "a.txt");
	assert_eq!(listing[0]["type"], "file");
	assert_eq!(listing[0]["size"], 7);
	assert_eq!(listing[0]["uuid"], file.uuid().to_string());
}

#[shared_test_runtime]
async fn ls_long_shows_the_modified_date_and_marks_favorites() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = format!("/{}", resources.dir.name().unwrap());
	let modified = Utc.with_ymd_and_hms(2024, 3, 9, 10, 20, 30).unwrap();
	let builder = client
		.make_file_builder("a.txt", resources.dir.uuid())
		.unwrap()
		.modified(modified);
	let mut file = client.upload_file(builder, b"content").await.unwrap();
	client.set_file_favorite(&mut file, true).await.unwrap();
	upload(client, &resources.dir, "b.txt", b"content").await;

	let assert = authenticated_cli_with_args!("ls", "-l", &test_dir).success();
	let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
	let lines = stdout.lines().collect::<Vec<&str>>();
	assert_eq!(lines.len(), 2, "{stdout}");
	assert_eq!(lines[0], "7 B  2024-03-09 10:20:30 (UTC)  a.txt (*)");
	assert!(lines[1].starts_with("7 B  20"), "{stdout}");
	assert!(lines[1].ends_with("  b.txt"), "{stdout}");
}

#[shared_test_runtime]
async fn ls_json_prints_an_object_per_item_directories_first() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = format!("/{}", resources.dir.name().unwrap());
	let file = upload(client, &resources.dir, "a.txt", b"content").await;
	let dir = client
		.create_dir(&(&resources.dir).into(), "docs")
		.await
		.unwrap();

	let assert = authenticated_cli_with_args!("ls", &test_dir, "--json").success();
	let listing = printed_json(&assert);

	assert_eq!(
		listing,
		serde_json::json!([
			{
				"name": "docs",
				"type": "directory",
				"created": dir.created(),
				"favorited": false,
				"uuid": dir.uuid().to_string(),
			},
			{
				"name": "a.txt",
				"type": "file",
				"size": 7,
				"modified": file.last_modified(),
				"created": file.created(),
				"favorited": false,
				"uuid": file.uuid().to_string(),
			},
		])
	);
}

#[shared_test_runtime]
async fn list_favorites_json_gives_each_items_path() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = format!("/{}", resources.dir.name().unwrap());
	let mut file = upload(client, &resources.dir, "a.txt", b"content").await;
	client.set_file_favorite(&mut file, true).await.unwrap();

	let assert = authenticated_cli_with_args!("list-favorites", "--json").success();
	// the favorites are the whole account's, so the test looks at its own file only
	client.set_file_favorite(&mut file, false).await.unwrap();
	let listing = printed_json(&assert);
	let item = listed(&listing, &file.uuid().to_string());
	assert_eq!(item["name"], "a.txt");
	assert_eq!(item["path"], format!("{test_dir}/a.txt"));
	assert_eq!(item["type"], "file");
	assert_eq!(item["favorited"], true);
}

#[shared_test_runtime]
async fn list_recents_json_gives_each_items_path() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = format!("/{}", resources.dir.name().unwrap());
	let file = upload(client, &resources.dir, "a.txt", b"content").await;

	let assert = authenticated_cli_with_args!("list-recents", "--json").success();
	let listing = printed_json(&assert);
	let item = listed(&listing, &file.uuid().to_string());
	assert_eq!(item["name"], "a.txt");
	assert_eq!(item["path"], format!("{test_dir}/a.txt"));
	assert_eq!(item["type"], "file");
	assert_eq!(item["size"], 7);
}

/// What `args` printed on stdout, as bytes.
async fn printed_bytes(args: &[&str]) -> Vec<u8> {
	run_authenticated_cli_with_args(&mut cargo_bin_cmd!("filen-cli"), args)
		.await
		.success()
		.get_output()
		.stdout
		.clone()
}

#[shared_test_runtime]
async fn cat_prints_a_binary_file_byte_for_byte() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = format!("/{}", resources.dir.name().unwrap());
	// more than two chunks, with every byte value and no newline at the end
	let content = (0..=255u8)
		.cycle()
		.take(2 * CHUNK_SIZE + 1000)
		.collect::<Vec<u8>>();
	upload(client, &resources.dir, "a.bin", &content).await;

	let printed = printed_bytes(&["cat", &format!("{test_dir}/a.bin")]).await;

	assert_eq!(printed.len(), content.len());
	assert!(
		printed == content,
		"the printed bytes differ from the file's"
	);
}

#[shared_test_runtime]
async fn head_and_tail_print_whole_lines_as_they_are() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = format!("/{}", resources.dir.name().unwrap());
	upload(client, &resources.dir, "a.txt", b"one\ntwo\r\nthree").await;
	let path = format!("{test_dir}/a.txt");

	assert_eq!(
		printed_bytes(&["head", "-n", "2", &path]).await,
		b"one\ntwo\r\n"
	);
	assert_eq!(
		printed_bytes(&["head", "-n", "5", &path]).await,
		b"one\ntwo\r\nthree"
	);
	assert_eq!(printed_bytes(&["tail", "-n", "1", &path]).await, b"three");
	assert_eq!(
		printed_bytes(&["tail", "-n", "2", &path]).await,
		b"two\r\nthree"
	);
	assert_eq!(printed_bytes(&["tail", "-n", "0", &path]).await, b"");
}

#[shared_test_runtime]
async fn head_and_tail_of_a_file_of_several_chunks_print_lines_across_chunks() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = format!("/{}", resources.dir.name().unwrap());
	// the long line spans the boundary of the first two chunks
	let long_line = [vec![b'x'; CHUNK_SIZE + 10], b"\n".to_vec()].concat();
	let content = [b"first\n".to_vec(), long_line.clone(), b"last\n".to_vec()].concat();
	upload(client, &resources.dir, "a.txt", &content).await;
	let path = format!("{test_dir}/a.txt");

	assert_eq!(printed_bytes(&["head", "-n", "1", &path]).await, b"first\n");
	assert!(
		printed_bytes(&["head", "-n", "2", &path]).await
			== [b"first\n".as_slice(), &long_line].concat()
	);
	assert!(
		printed_bytes(&["tail", "-n", "2", &path]).await
			== [long_line.as_slice(), b"last\n"].concat()
	);
}

#[shared_test_runtime]
async fn cat_and_head_json_print_the_text() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = format!("/{}", resources.dir.name().unwrap());
	upload(client, &resources.dir, "a.txt", "héllo\nworld".as_bytes()).await;
	let path = format!("{test_dir}/a.txt");

	let assert = authenticated_cli_with_args!("cat", &path, "--json").success();
	assert_eq!(
		printed_json(&assert),
		serde_json::json!({ "text": "héllo\nworld" })
	);
	let assert = authenticated_cli_with_args!("head", "-n", "1", &path, "--json").success();
	assert_eq!(
		printed_json(&assert),
		serde_json::json!({ "text": "héllo\n" })
	);
}

#[shared_test_runtime]
async fn cat_json_of_a_binary_file_fails() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = format!("/{}", resources.dir.name().unwrap());
	upload(client, &resources.dir, "a.bin", b"\x00\xff\xfe").await;
	let path = format!("{test_dir}/a.bin");

	authenticated_cli_with_args!("cat", &path, "--json")
		.failure()
		.stdout("")
		.stderr(predicates::str::contains(format!(
			"Not UTF-8 text: {path}. Use cat without --json for binary content"
		)));
}

#[shared_test_runtime]
async fn write_writes_its_arguments_joined_by_spaces() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = format!("/{}", resources.dir.name().unwrap());
	let path = format!("{test_dir}/a.txt");

	authenticated_cli_with_args!("write", &path, "hello", "world")
		.success()
		.stdout(predicates::str::contains(format!("Wrote to {path}")));
	assert_eq!(content_at(client, &path).await, b"hello world");

	authenticated_cli_with_args!("write", &path, "replaced").success();
	assert_eq!(content_at(client, &path).await, b"replaced");
}

#[shared_test_runtime]
async fn write_without_content_arguments_writes_what_stdin_reads() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = format!("/{}", resources.dir.name().unwrap());
	let path = format!("{test_dir}/a.bin");
	let content = (0..=255u8).cycle().take(300_000).collect::<Vec<u8>>();

	let (_auth_config_dir, auth_config) = test_utils::cli::prepare_cli_auth_config().await;
	cargo_bin_cmd!("filen-cli")
		.args([
			"--auth-config-path",
			auth_config.to_str().unwrap(),
			"--skip-update",
			"write",
			&path,
		])
		.write_stdin(content.clone())
		.assert()
		.success();

	assert!(
		content_at(client, &path).await == content,
		"the written bytes differ"
	);
}

#[shared_test_runtime]
async fn write_into_a_missing_directory_fails() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let test_dir = format!("/{}", resources.dir.name().unwrap());

	authenticated_cli_with_args!("write", &format!("{test_dir}/missing/a.txt"), "x")
		.failure()
		.stderr(predicates::str::contains(format!(
			"No such directory: {test_dir}/missing"
		)));
}

#[shared_test_runtime]
async fn write_to_a_directory_fails() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = format!("/{}", resources.dir.name().unwrap());
	test_utils::create_remote_file_structure_outline(client, resources.dir.clone(), &["Dir/"])
		.await
		.unwrap();

	authenticated_cli_with_args!("touch", &format!("{test_dir}/dir"))
		.failure()
		.stderr(predicates::str::contains(format!(
			"Not a file: {test_dir}/Dir"
		)));
	authenticated_cli_with_args!("write", &format!("{test_dir}/Dir"), "x")
		.failure()
		.stderr(predicates::str::contains(format!(
			"Not a file: {test_dir}/Dir"
		)));
}

#[shared_test_runtime]
async fn touch_on_an_existing_file_keeps_its_content_and_sets_its_modification_date() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = format!("/{}", resources.dir.name().unwrap());
	let modified = Utc.with_ymd_and_hms(2020, 1, 2, 3, 4, 5).unwrap();
	let builder = client
		.make_file_builder("a.txt", resources.dir.uuid())
		.unwrap()
		.modified(modified);
	let file = client.upload_file(builder, b"content").await.unwrap();
	let before_touch = Utc::now() - chrono::Duration::minutes(5);

	// a name differing only in case names the same file, as the server compares names
	authenticated_cli_with_args!("touch", &format!("{test_dir}/A.TXT"))
		.success()
		.stdout(predicates::str::contains(format!(
			"Updated the modification date of {test_dir}/a.txt"
		)));

	let Some(NonRootFileType::File(touched)) = client
		.find_item_at_path(&format!("{test_dir}/a.txt"))
		.await
		.unwrap()
	else {
		panic!("Expected a file");
	};
	assert_eq!(touched.uuid(), file.uuid());
	assert!(touched.last_modified().unwrap() > before_touch);
	assert_eq!(
		content_at(client, &format!("{test_dir}/a.txt")).await,
		b"content"
	);
}

#[shared_test_runtime]
async fn touch_creates_an_empty_file() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = format!("/{}", resources.dir.name().unwrap());
	let path = format!("{test_dir}/new.txt");

	authenticated_cli_with_args!("touch", &path)
		.success()
		.stdout(predicates::str::contains(format!(
			"Created empty file: {path}"
		)));

	assert_eq!(content_at(client, &path).await, b"");
}

#[test]
fn pwd_prints_the_working_path_without_logging_in() {
	let output = run_logged_out_without_a_terminal(&["pwd"]);

	assert_eq!(output.status.code(), Some(0));
	assert_eq!(String::from_utf8(output.stdout).unwrap(), "/\n");
}

#[shared_test_runtime]
async fn pwd_prints_the_working_path_it_is_given() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let test_dir = format!("/{}", resources.dir.name().unwrap());

	authenticated_cli_with_args!("--working-path", &test_dir, "pwd")
		.success()
		.stdout(format!("{test_dir}\n"));
}

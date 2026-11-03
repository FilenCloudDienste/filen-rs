#[cfg(unix)]
use std::{
	process::{Command, Stdio},
	time::{Duration, Instant},
};

use filen_macros::shared_test_runtime;
use filen_sdk_rs::{
	auth::Client,
	fs::{HasName, HasUUID, categories::NonRootFileType, dir::RemoteDirectory},
	io::client_impl::IoSharedClientExt,
};
use test_utils::authenticated_cli_with_args;

/// Uploads `content` as `name` into `parent`.
async fn upload(client: &Client, parent: &RemoteDirectory, name: &str, content: &[u8]) {
	let builder = client.make_file_builder(name, parent.uuid()).unwrap();
	client.upload_file(builder, content).await.unwrap();
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

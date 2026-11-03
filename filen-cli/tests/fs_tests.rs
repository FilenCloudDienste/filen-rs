use filen_macros::shared_test_runtime;
use filen_sdk_rs::fs::{HasName, categories::NonRootFileType};
use test_utils::authenticated_cli_with_args;

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

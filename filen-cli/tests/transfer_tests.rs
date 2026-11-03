use filen_macros::shared_test_runtime;
use filen_sdk_rs::fs::{HasName, categories::NonRootFileType};
use test_utils::authenticated_cli_with_args;

#[shared_test_runtime]
async fn uploading_a_directory_uploads_its_files() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir_name = resources.dir.name().unwrap();

	let local = tempfile::tempdir().unwrap();
	let source = local.path().join("uploaded");
	std::fs::create_dir_all(source.join("nested")).unwrap();
	std::fs::write(source.join("nested").join("a.txt"), b"contents").unwrap();

	authenticated_cli_with_args!("upload", source.to_str().unwrap(), test_dir_name).success();

	assert!(matches!(
		client
			.find_item_at_path(&format!("{test_dir_name}/uploaded/nested/a.txt"))
			.await
			.unwrap(),
		Some(NonRootFileType::File(_))
	));
}

#[cfg(unix)]
#[shared_test_runtime]
async fn uploading_a_symlinked_file_keeps_the_links_name() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir_name = resources.dir.name().unwrap();

	let local = tempfile::tempdir().unwrap();
	std::fs::write(local.path().join("target.txt"), b"contents").unwrap();
	std::os::unix::fs::symlink(
		local.path().join("target.txt"),
		local.path().join("link.txt"),
	)
	.unwrap();

	authenticated_cli_with_args!(
		"upload",
		local.path().join("link.txt").to_str().unwrap(),
		test_dir_name
	)
	.success();

	assert!(matches!(
		client
			.find_item_at_path(&format!("{test_dir_name}/link.txt"))
			.await
			.unwrap(),
		Some(NonRootFileType::File(_))
	));
	assert!(
		client
			.find_item_at_path(&format!("{test_dir_name}/target.txt"))
			.await
			.unwrap()
			.is_none()
	);
}

#[shared_test_runtime]
async fn uploading_to_a_new_file_path_renames_the_file() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir_name = resources.dir.name().unwrap();

	let local = tempfile::tempdir().unwrap();
	std::fs::create_dir(local.path().join("a")).unwrap();
	std::fs::write(local.path().join("a").join("b.txt"), b"contents").unwrap();
	authenticated_cli_with_args!("mkdir", &format!("{test_dir_name}/dir")).success();

	authenticated_cli_with_args!(
		"upload",
		local.path().join("a").join("b.txt").to_str().unwrap(),
		&format!("{test_dir_name}/dir/new.txt")
	)
	.success();

	assert!(matches!(
		client
			.find_item_at_path(&format!("{test_dir_name}/dir/new.txt"))
			.await
			.unwrap(),
		Some(NonRootFileType::File(_))
	));
	assert!(
		client
			.find_item_at_path(&format!("{test_dir_name}/dir/b.txt"))
			.await
			.unwrap()
			.is_none()
	);
}

#[shared_test_runtime]
async fn uploading_into_a_missing_directory_creates_it() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir_name = resources.dir.name().unwrap();

	let local = tempfile::tempdir().unwrap();
	std::fs::write(local.path().join("a.txt"), b"contents").unwrap();

	authenticated_cli_with_args!(
		"upload",
		local.path().join("a.txt").to_str().unwrap(),
		&format!("{test_dir_name}/x/y/z/")
	)
	.success();

	assert!(matches!(
		client
			.find_item_at_path(&format!("{test_dir_name}/x/y/z/a.txt"))
			.await
			.unwrap(),
		Some(NonRootFileType::File(_))
	));
}

#[shared_test_runtime]
async fn downloading_to_a_new_path_in_a_missing_directory_renames_the_file() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let test_dir_name = resources.dir.name().unwrap();

	let local = tempfile::tempdir().unwrap();
	std::fs::write(local.path().join("a.txt"), b"contents").unwrap();
	authenticated_cli_with_args!(
		"upload",
		local.path().join("a.txt").to_str().unwrap(),
		test_dir_name
	)
	.success();

	let target = local.path().join("missing").join("renamed.txt");
	authenticated_cli_with_args!(
		"download",
		&format!("{test_dir_name}/a.txt"),
		target.to_str().unwrap()
	)
	.success();

	assert_eq!(std::fs::read(&target).unwrap(), b"contents");
}

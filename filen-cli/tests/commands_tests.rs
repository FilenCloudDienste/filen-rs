use filen_macros::shared_test_runtime;
use filen_sdk_rs::{auth, fs::HasName};
use predicates::prelude::PredicateBooleanExt as _;
use rand::TryRngCore;
use test_utils::authenticated_cli_with_args;

#[shared_test_runtime]
async fn cmd_ls() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	// create test file to call ls on
	let file = client.make_file_builder("testfile.txt", test_dir).build();
	client.upload_file(file.into(), &[]).await.unwrap();

	// ls
	authenticated_cli_with_args!("ls", test_dir.name().unwrap())
		.success()
		.stdout(predicates::str::contains("testfile.txt"));
}

#[shared_test_runtime]
async fn cmd_cat() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	// create test file to call cat on
	let file = client.make_file_builder("testfile.txt", test_dir).build();
	let content = "Hello, Filen!";
	client
		.upload_file(file.into(), content.as_bytes())
		.await
		.unwrap();

	// cat
	authenticated_cli_with_args!("cat", &format!("{}/testfile.txt", test_dir.name().unwrap()))
		.success()
		.stdout(predicates::str::contains(content));
}

#[shared_test_runtime]
async fn cmd_head_tail() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	// create test file to call head/tail on
	let file = client.make_file_builder("testfile.txt", test_dir).build();
	let content = "Line 1\nLine 2\nLine 3\nLine 4\nLine 5\n";
	client
		.upload_file(file.into(), content.as_bytes())
		.await
		.unwrap();

	// head
	authenticated_cli_with_args!(
		"head",
		&format!("{}/testfile.txt", test_dir.name().unwrap()),
		"-n1"
	)
	.success()
	.stdout(predicates::str::contains("Line 1").and(predicates::str::contains("Line 2").not()));

	// tail
	authenticated_cli_with_args!(
		"tail",
		&format!("{}/testfile.txt", test_dir.name().unwrap()),
		"-n1"
	)
	.success()
	.stdout(predicates::str::contains("Line 5").and(predicates::str::contains("Line 4").not()));
}

#[shared_test_runtime]
async fn cmd_stat() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	// create test file to call stat on
	let file = client.make_file_builder("testfile.txt", test_dir).build();
	let mut contents = vec![0u8; 1024];
	rand::rng().try_fill_bytes(&mut contents).unwrap();
	client.upload_file(file.into(), &contents).await.unwrap();

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

#[shared_test_runtime]
async fn cmd_mkdir() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let new_dir_name = "new_test_dir";

	// mkdir
	authenticated_cli_with_args!(
		"mkdir",
		&format!("{}/{}", test_dir.name().unwrap(), new_dir_name)
	)
	.success()
	.stdout(predicates::str::contains("Directory created"));

	// verify dir was created
	let created_dir = client
		.find_item_at_path(&format!("{}/{}", test_dir.name().unwrap(), new_dir_name))
		.await
		.unwrap();
	assert!(created_dir.is_some());

	// mkdir -r
	let nested_dir_path = format!("{}/parent_dir/nested_dir", test_dir.name().unwrap());
	authenticated_cli_with_args!("mkdir", "-r", &nested_dir_path)
		.success()
		.stdout(predicates::str::contains("Directory created"));

	// verify nested dir was created
	let created_nested_dir = client.find_item_at_path(&nested_dir_path).await.unwrap();
	assert!(created_nested_dir.is_some());
}

#[shared_test_runtime]
async fn cmd_rm() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	// create test file to call rm on
	let file = client.make_file_builder("testfile.txt", test_dir).build();
	let content = "Hello, Filen!";
	client
		.upload_file(file.into(), content.as_bytes())
		.await
		.unwrap();

	// create test directory to call rm on
	client
		.create_dir(test_dir, String::from("testdir_to_delete"))
		.await
		.unwrap();

	// rm
	authenticated_cli_with_args!("rm", &format!("{}/testfile.txt", test_dir.name().unwrap()))
		.success()
		.stdout(predicates::str::contains("Trashed file"));
	authenticated_cli_with_args!(
		"rm",
		&format!("{}/testdir_to_delete", test_dir.name().unwrap())
	)
	.success()
	.stdout(predicates::str::contains("Trashed directory"));

	// verify file was deleted
	let deleted_file = client
		.find_item_at_path(&format!("{}/testfile.txt", test_dir.name().unwrap()))
		.await
		.unwrap();
	assert!(deleted_file.is_none());

	// verify directory was deleted
	let deleted_dir = client
		.find_item_at_path(&format!("{}/testdir_to_delete", test_dir.name().unwrap()))
		.await
		.unwrap();
	assert!(deleted_dir.is_none());
}

#[shared_test_runtime]
async fn cmd_mv_cp() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	// create test file to call mv on
	let file = client.make_file_builder("testfile.txt", test_dir).build();
	let content = "Hello, Filen!";
	client
		.upload_file(file.into(), content.as_bytes())
		.await
		.unwrap();

	// create destination directory
	let destination_dir = format!("{}/moved_dir", test_dir.name().unwrap());
	client
		.create_dir(test_dir, String::from("moved_dir"))
		.await
		.unwrap();

	// mv
	authenticated_cli_with_args!(
		"mv",
		&format!("{}/testfile.txt", test_dir.name().unwrap()),
		&destination_dir
	)
	.success()
	.stdout(predicates::str::contains("Moved"));

	// verify file was moved
	let old_file = client
		.find_item_at_path(&format!("{}/testfile.txt", test_dir.name().unwrap()))
		.await
		.unwrap();
	assert!(old_file.is_none());
	let new_file = client
		.find_item_at_path(&format!("{}/testfile.txt", &destination_dir))
		.await
		.unwrap();
	assert!(new_file.is_some());
}

#[shared_test_runtime]
async fn cmd_favorite_unfavorite() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	// create test file to call favorite on
	let file = client.make_file_builder("testfile.txt", test_dir).build();
	let content = "Hello, Filen!";
	client
		.upload_file(file.into(), content.as_bytes())
		.await
		.unwrap();

	let file_path = format!("{}/testfile.txt", test_dir.name().unwrap());

	// favorite
	authenticated_cli_with_args!("favorite", &file_path)
		.success()
		.stdout(predicates::str::contains("Favorited"));

	// verify file is favorited
	match client.find_item_at_path(&file_path).await.unwrap().unwrap() {
		filen_sdk_rs::fs::FSObject::File(file) => assert!(file.favorited),
		_ => panic!("Expected a file"),
	}

	// unfavorite
	authenticated_cli_with_args!("unfavorite", &file_path)
		.success()
		.stdout(predicates::str::contains("Unfavorited"));

	// verify file is unfavorited
	match client.find_item_at_path(&file_path).await.unwrap().unwrap() {
		filen_sdk_rs::fs::FSObject::File(file) => assert!(!file.favorited),
		_ => panic!("Expected a file"),
	}
}

#[shared_test_runtime]
async fn cmd_rclone() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	// create test file to call rclone on
	let file = client.make_file_builder("testfile.txt", test_dir).build();
	let content = "Hello, Filen!";
	client
		.upload_file(file.into(), content.as_bytes())
		.await
		.unwrap();

	// list file using rclone
	authenticated_cli_with_args!(
		"rclone",
		"lsf",
		&format!("filen:{}", test_dir.name().unwrap())
	)
	.success()
	.stdout(predicates::str::contains("testfile.txt"));
}

#[shared_test_runtime]
async fn cmd_list_trash_empty_trash() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;

	// create test file to trash
	let test_dir = &resources.dir;
	let file = client
		.make_file_builder("testfile_from_cli_test_list_trash.txt", test_dir)
		.build();
	let content = "Hello, Filen!";
	let mut file = client
		.upload_file(file.into(), content.as_bytes())
		.await
		.unwrap();

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

	// verify trash is listed as empty
	authenticated_cli_with_args!("list-trash")
		.success()
		.stdout(predicates::str::contains("Trash is empty"));
}

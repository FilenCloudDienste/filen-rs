use filen_mobile_native_cache::{
	CacheClient, FilenMobileDB,
	ffi::{FfiNonRootObject, FfiObject, FfiPathWithRoot},
	io,
};
use filen_sdk_rs::fs::{HasName, HasUUID};
use futures::AsyncWriteExt;
use test_log::test;
use test_utils::TestResources;

async fn get_db_resources() -> (FilenMobileDB, CacheClient, TestResources) {
	let path = std::env::temp_dir();
	let sqlite_path = path.join("sqlite");
	std::fs::create_dir_all(&sqlite_path).unwrap();
	let db = FilenMobileDB::initialize_in_memory().unwrap();
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = resources.client.to_stringified();
	db.add_root(&client.root_uuid).unwrap();
	let client = CacheClient::from_strings(
		client.email,
		&client.root_uuid,
		&client.auth_info,
		&client.private_key,
		client.api_key,
		client.auth_version,
	)
	.unwrap();
	(db, client, resources)
}

// Root query tests
#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_query_root_initial_state() {
	let (db, _, rss) = get_db_resources().await;

	let res = db
		.query_roots_info(rss.client.root().uuid().to_string())
		.unwrap()
		.unwrap();

	assert_eq!(res.max_storage, 0);
	assert_eq!(res.storage_used, 0);
	assert_eq!(res.last_updated, 0);
	assert_eq!(res.uuid, rss.client.root().uuid().to_string());
	assert_eq!(res.last_listed, 0);
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_query_root_after_update() {
	let (db, client, rss) = get_db_resources().await;

	db.update_roots_info(&client).await.unwrap();
	let root = db
		.query_roots_info(rss.client.root().uuid().to_string())
		.unwrap()
		.unwrap();

	assert_ne!(root.max_storage, 0);
	assert_ne!(root.storage_used, 0);
	assert_ne!(root.last_updated, 0);
	assert_eq!(root.uuid, client.root_uuid().to_string());
	assert_eq!(root.last_listed, 0);
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_query_root_nonexistent() {
	let (db, _client, _rss) = get_db_resources().await;

	let fake_uuid = uuid::Uuid::new_v4().to_string();
	let result = db.query_roots_info(fake_uuid).unwrap();
	assert!(result.is_none());
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_query_root_invalid_uuid() {
	let (db, _client, _rss) = get_db_resources().await;

	let result = db.query_roots_info("invalid-uuid".to_string());
	assert!(result.is_err());
}

// Directory children query tests
#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_query_children_empty_directory() {
	let (db, client, rss) = get_db_resources().await;
	let test_dir_path: FfiPathWithRoot =
		format!("{}/{}", client.root_uuid(), rss.dir.name()).into();

	// Before update - should return None
	let resp = db.query_dir_children(&test_dir_path, None).unwrap();
	assert!(resp.is_none());

	// After update - should return empty but valid response
	db.update_dir_children(&client, test_dir_path.clone())
		.await
		.unwrap();

	let resp = db
		.query_dir_children(&test_dir_path, None)
		.unwrap()
		.unwrap();
	assert_eq!(resp.objects.len(), 0);
	assert_eq!(resp.parent.uuid, rss.dir.uuid().to_string());
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_query_children_with_files_and_dirs() {
	let (db, client, rss) = get_db_resources().await;
	let test_dir_path: FfiPathWithRoot =
		format!("{}/{}", client.root_uuid(), rss.dir.name()).into();

	// Create test content
	let dir = rss
		.client
		.create_dir(&rss.dir, "test_subdir".to_string())
		.await
		.unwrap();

	let file = rss
		.client
		.make_file_builder("test_file.txt", &rss.dir)
		.build();
	let mut file = rss.client.get_file_writer(file).unwrap();
	file.write_all(b"Hello, world!").await.unwrap();
	file.close().await.unwrap();
	let file = file.into_remote_file().unwrap();

	// Update and verify
	db.update_dir_children(&client, test_dir_path.clone())
		.await
		.unwrap();
	let resp = db
		.query_dir_children(&test_dir_path, None)
		.unwrap()
		.unwrap();

	assert_eq!(resp.objects.len(), 2);
	assert_eq!(resp.parent.uuid, rss.dir.uuid().to_string());

	// Verify we have both file and directory
	let has_file = resp
		.objects
		.iter()
		.any(|obj| matches!(obj, FfiNonRootObject::File(f) if f.uuid == file.uuid().to_string()));
	let has_dir = resp
		.objects
		.iter()
		.any(|obj| matches!(obj, FfiNonRootObject::Dir(d) if d.uuid == dir.uuid().to_string()));
	assert!(has_file);
	assert!(has_dir);
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_query_children_sorting_by_size() {
	let (db, client, rss) = get_db_resources().await;
	let test_dir_path: FfiPathWithRoot =
		format!("{}/{}", client.root_uuid(), rss.dir.name()).into();

	// Create files with different sizes
	let large_file = rss.client.make_file_builder("large.txt", &rss.dir).build();
	let mut large_writer = rss.client.get_file_writer(large_file).unwrap();
	large_writer
		.write_all(b"This is a much larger file with more content")
		.await
		.unwrap();
	large_writer.close().await.unwrap();
	let large_file = large_writer.into_remote_file().unwrap();

	let small_file = rss.client.make_file_builder("small.txt", &rss.dir).build();
	let mut small_writer = rss.client.get_file_writer(small_file).unwrap();
	small_writer.write_all(b"small").await.unwrap();
	small_writer.close().await.unwrap();

	let empty_file = rss.client.make_file_builder("empty.txt", &rss.dir).build();
	let mut empty_writer = rss.client.get_file_writer(empty_file).unwrap();
	empty_writer.close().await.unwrap();
	let empty_file = empty_writer.into_remote_file().unwrap();

	db.update_dir_children(&client, test_dir_path.clone())
		.await
		.unwrap();

	// Test ascending size sort
	let resp = db
		.query_dir_children(&test_dir_path, Some("size ASC".to_string()))
		.unwrap()
		.unwrap();
	assert_eq!(resp.objects.len(), 3);
	// Empty file should be first
	assert!(matches!(
		&resp.objects[0],
		FfiNonRootObject::File(f) if f.uuid == empty_file.uuid().to_string()
	));
	// Large file should be last
	assert!(matches!(
		&resp.objects[2],
		FfiNonRootObject::File(f) if f.uuid == large_file.uuid().to_string()
	));

	// Test descending size sort
	let resp = db
		.query_dir_children(&test_dir_path, Some("size DESC".to_string()))
		.unwrap()
		.unwrap();
	assert_eq!(resp.objects.len(), 3);
	// Large file should be first
	assert!(matches!(
		&resp.objects[0],
		FfiNonRootObject::File(f) if f.uuid == large_file.uuid().to_string()
	));
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_query_children_sorting_by_name() {
	let (db, client, rss) = get_db_resources().await;
	let test_dir_path: FfiPathWithRoot =
		format!("{}/{}", client.root_uuid(), rss.dir.name()).into();

	// Create items with specific names for alphabetical testing
	rss.client
		.create_dir(&rss.dir, "zebra_dir".to_string())
		.await
		.unwrap();

	let alpha_file = rss.client.make_file_builder("alpha.txt", &rss.dir).build();
	let mut alpha_writer = rss.client.get_file_writer(alpha_file).unwrap();
	alpha_writer.close().await.unwrap();

	let beta_file = rss.client.make_file_builder("beta.txt", &rss.dir).build();
	let mut beta_writer = rss.client.get_file_writer(beta_file).unwrap();
	beta_writer.close().await.unwrap();

	db.update_dir_children(&client, test_dir_path.clone())
		.await
		.unwrap();

	let resp = db
		.query_dir_children(&test_dir_path, Some("display_name ASC".to_string()))
		.unwrap()
		.unwrap();
	assert_eq!(resp.objects.len(), 3);

	// Verify alphabetical order
	assert!(matches!(
		&resp.objects[0],
		FfiNonRootObject::File(f) if f.name == "alpha.txt"
	));
	assert!(matches!(
		&resp.objects[1],
		FfiNonRootObject::File(f) if f.name == "beta.txt"
	));
	assert!(matches!(
		&resp.objects[2],
		FfiNonRootObject::Dir(d) if d.name == "zebra_dir"
	));
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_query_children_after_deletion() {
	let (db, client, rss) = get_db_resources().await;
	let test_dir_path: FfiPathWithRoot =
		format!("{}/{}", client.root_uuid(), rss.dir.name()).into();

	// Create and then delete a directory
	let dir = rss
		.client
		.create_dir(&rss.dir, "temp_dir".to_string())
		.await
		.unwrap();

	let file = rss
		.client
		.make_file_builder("persistent.txt", &rss.dir)
		.build();
	let mut file_writer = rss.client.get_file_writer(file).unwrap();
	file_writer.close().await.unwrap();
	let file = file_writer.into_remote_file().unwrap();

	// Update to get both items
	db.update_dir_children(&client, test_dir_path.clone())
		.await
		.unwrap();
	let resp = db
		.query_dir_children(&test_dir_path, None)
		.unwrap()
		.unwrap();
	assert_eq!(resp.objects.len(), 2);

	// Delete the directory
	rss.client.trash_dir(&dir).await.unwrap();
	db.update_dir_children(&client, test_dir_path.clone())
		.await
		.unwrap();

	// Should now only have the file
	let resp = db
		.query_dir_children(&test_dir_path, None)
		.unwrap()
		.unwrap();
	assert_eq!(resp.objects.len(), 1);
	assert!(matches!(
		&resp.objects[0],
		FfiNonRootObject::File(f) if f.uuid == file.uuid().to_string()
	));
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_query_children_nonexistent_path() {
	let (db, client, _rss) = get_db_resources().await;
	let nonexistent_path: FfiPathWithRoot =
		format!("{}/nonexistent_dir", client.root_uuid()).into();

	let result = db.query_dir_children(&nonexistent_path, None).unwrap();
	assert!(result.is_none());
}

// Item query tests
#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_query_item_file() {
	let (db, client, rss) = get_db_resources().await;

	let file = rss
		.client
		.make_file_builder("query_test.txt", &rss.dir)
		.build();
	let mut file = rss.client.get_file_writer(file).unwrap();
	file.write_all(b"Test content").await.unwrap();
	file.close().await.unwrap();
	let file = file.into_remote_file().unwrap();

	let file_path: FfiPathWithRoot =
		format!("{}/{}/{}", client.root_uuid(), rss.dir.name(), file.name()).into();
	let dir_path: FfiPathWithRoot = format!("{}/{}", client.root_uuid(), rss.dir.name()).into();

	// Before update - should return None
	assert_eq!(db.query_item(&file_path).unwrap(), None);

	// After update - should return the file
	db.update_dir_children(&client, dir_path).await.unwrap();
	let result = db.query_item(&file_path).unwrap();

	match result {
		Some(FfiObject::File(retrieved_file)) => {
			assert_eq!(retrieved_file.uuid, file.uuid().to_string());
			assert_eq!(retrieved_file.name, file.name());
		}
		_ => panic!("Expected to find a file object"),
	}
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_query_item_directory() {
	let (db, client, rss) = get_db_resources().await;

	let dir = rss
		.client
		.create_dir(&rss.dir, "query_test_dir".to_string())
		.await
		.unwrap();

	let child_dir_path: FfiPathWithRoot =
		format!("{}/{}/{}", client.root_uuid(), rss.dir.name(), dir.name()).into();
	let parent_dir_path: FfiPathWithRoot =
		format!("{}/{}", client.root_uuid(), rss.dir.name()).into();

	// Before update - should return None
	assert_eq!(db.query_item(&child_dir_path).unwrap(), None);

	// After update - should return the directory
	db.update_dir_children(&client, parent_dir_path)
		.await
		.unwrap();
	let result = db.query_item(&child_dir_path).unwrap();

	match result {
		Some(FfiObject::Dir(retrieved_dir)) => {
			assert_eq!(retrieved_dir.uuid, dir.uuid().to_string());
			assert_eq!(retrieved_dir.name, dir.name());
		}
		_ => panic!("Expected to find a directory object"),
	}
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_query_item_root() {
	let (db, _, rss) = get_db_resources().await;

	let root_path: FfiPathWithRoot = rss.client.root().uuid().to_string().into();
	let result = db.query_item(&root_path).unwrap();

	match result {
		Some(FfiObject::Root(root)) => {
			assert_eq!(root.uuid, rss.client.root().uuid().to_string());
			assert_eq!(root.max_storage, 0); // Initial state
			assert_eq!(root.storage_used, 0); // Initial state
			assert_eq!(root.last_updated, 0); // Initial state
			assert_eq!(root.last_listed, 0); // Initial state
		}
		_ => panic!("Expected to find a root object"),
	}
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_query_item_nonexistent() {
	let (db, client, rss) = get_db_resources().await;

	let nonexistent_file_path: FfiPathWithRoot =
		format!("{}/{}/nonexistent.txt", client.root_uuid(), rss.dir.name()).into();

	let result = db.query_item(&nonexistent_file_path).unwrap();
	assert!(result.is_none());
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_query_item_invalid_path() {
	let (db, _client, _rss) = get_db_resources().await;

	let invalid_path: FfiPathWithRoot = "not-a-uuid/invalid/path".into();
	let result = db.query_item(&invalid_path);
	assert!(result.is_err());
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_query_item_deeply_nested() {
	let (db, client, rss) = get_db_resources().await;

	// Create nested structure: rss.dir/level1/level2/deep_file.txt
	let level1 = rss
		.client
		.create_dir(&rss.dir, "level1".to_string())
		.await
		.unwrap();

	let level2 = rss
		.client
		.create_dir(&level1, "level2".to_string())
		.await
		.unwrap();

	let deep_file = rss
		.client
		.make_file_builder("deep_file.txt", &level2)
		.build();
	let mut file_writer = rss.client.get_file_writer(deep_file).unwrap();
	file_writer.write_all(b"Deep content").await.unwrap();
	file_writer.close().await.unwrap();
	let deep_file = file_writer.into_remote_file().unwrap();

	// Update each level
	let dir_path: FfiPathWithRoot = format!("{}/{}", client.root_uuid(), rss.dir.name()).into();
	let level1_path: FfiPathWithRoot = format!("{}/level1", dir_path.0).into();
	let level2_path: FfiPathWithRoot = format!("{}/level2", level1_path.0).into();

	db.update_dir_children(&client, dir_path).await.unwrap();
	db.update_dir_children(&client, level1_path).await.unwrap();
	db.update_dir_children(&client, level2_path).await.unwrap();

	// Query the deep file
	let deep_file_path: FfiPathWithRoot = format!(
		"{}/{}/level1/level2/{}",
		client.root_uuid(),
		rss.dir.name(),
		deep_file.name()
	)
	.into();

	let result = db.query_item(&deep_file_path).unwrap();
	match result {
		Some(FfiObject::File(retrieved_file)) => {
			assert_eq!(retrieved_file.uuid, deep_file.uuid().to_string());
			assert_eq!(retrieved_file.name, deep_file.name());
		}
		_ => panic!("Expected to find the deeply nested file"),
	}

	// Also test querying intermediate directories
	let level1_query_path: FfiPathWithRoot =
		format!("{}/{}/level1", client.root_uuid(), rss.dir.name()).into();

	let result = db.query_item(&level1_query_path).unwrap();
	match result {
		Some(FfiObject::Dir(retrieved_dir)) => {
			assert_eq!(retrieved_dir.uuid, level1.uuid().to_string());
		}
		_ => panic!("Expected to find level1 directory"),
	}
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_download_file() {
	let (db, client, rss) = get_db_resources().await;

	// Create a test file with some content inside rss.dir
	let test_content = b"Hello, world! This is test content for download.";
	let file = rss
		.client
		.make_file_builder("test_download.txt", &rss.dir)
		.build();
	let mut file_writer = rss.client.get_file_writer(file).unwrap();
	file_writer.write_all(test_content).await.unwrap();
	file_writer.close().await.unwrap();
	let remote_file = file_writer.into_remote_file().unwrap();

	let file_path: FfiPathWithRoot = format!(
		"{}/{}/{}",
		client.root_uuid(),
		rss.dir.name(),
		remote_file.name()
	)
	.into();

	// Test downloading the file
	let downloaded_path = db.download_file(&client, file_path.clone()).await.unwrap();

	// Verify the file was downloaded and contains correct content
	assert!(std::path::Path::new(&downloaded_path).exists());
	let downloaded_content = std::fs::read(&downloaded_path).unwrap();
	assert_eq!(downloaded_content, test_content);

	// Clean up
	std::fs::remove_file(&downloaded_path).ok();
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_download_file_nonexistent() {
	let (db, client, rss) = get_db_resources().await;

	let nonexistent_path: FfiPathWithRoot = format!(
		"{}/{}/nonexistent_file.txt",
		client.root_uuid(),
		rss.dir.name()
	)
	.into();

	// Should fail when trying to download a non-existent file
	let result = db.download_file(&client, nonexistent_path).await;
	assert!(result.is_err());
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_download_file_invalid_path() {
	let (db, client, rss) = get_db_resources().await;

	// Create a directory first
	let dir = rss
		.client
		.create_dir(&rss.dir, "test_dir".to_string())
		.await
		.unwrap();
	let dir_path: FfiPathWithRoot =
		format!("{}/{}/{}", client.root_uuid(), rss.dir.name(), dir.name()).into();

	// Should fail when trying to download a directory path as a file
	let result = db.download_file(&client, dir_path).await;
	assert!(result.is_err());
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_upload_file_if_changed_new_file() {
	let (db, client, rss) = get_db_resources().await;

	// Create a temporary local file - the path parameter represents where the file should go in the cloud
	// but the actual file should exist locally at the filename part of that path
	let test_content = b"This is test content for upload.";
	let upload_path: FfiPathWithRoot =
		format!("{}/{}/test_upload.txt", client.root_uuid(), rss.dir.name()).into();
	let io_path = io::get_file_path(&upload_path).unwrap();
	std::fs::write(&io_path, test_content).unwrap();

	let upload_path: FfiPathWithRoot =
		format!("{}/{}/test_upload.txt", client.root_uuid(), rss.dir.name()).into();

	// Upload the file (should return true for new file)
	let was_uploaded = db
		.upload_file_if_changed(&client, upload_path.clone())
		.await
		.unwrap();
	assert!(was_uploaded);

	// Verify the file exists in the database by checking the parent directory
	let parent_path: FfiPathWithRoot = format!("{}/{}", client.root_uuid(), rss.dir.name()).into();
	db.update_dir_children(&client, parent_path.clone())
		.await
		.unwrap();
	let children = db.query_dir_children(&parent_path, None).unwrap().unwrap();

	let uploaded_file = children
		.objects
		.iter()
		.find(|obj| matches!(obj, FfiNonRootObject::File(f) if f.name == "test_upload.txt"));
	assert!(uploaded_file.is_some());

	// Clean up
	std::fs::remove_file(&io_path).ok();
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_upload_file_if_changed_unchanged_file() {
	let (db, client, rss) = get_db_resources().await;

	// Create and upload a file first
	let test_content = b"Unchanged content for hash test.";
	let file = rss
		.client
		.make_file_builder("hash_test.txt", &rss.dir)
		.build();
	let mut file_writer = rss.client.get_file_writer(file).unwrap();
	file_writer.write_all(test_content).await.unwrap();
	file_writer.close().await.unwrap();
	let remote_file = file_writer.into_remote_file().unwrap();

	// Update the database with this file info
	let dir_path: FfiPathWithRoot = format!("{}/{}", client.root_uuid(), rss.dir.name()).into();
	db.update_dir_children(&client, dir_path).await.unwrap();

	let upload_path: FfiPathWithRoot = format!(
		"{}/{}/{}",
		client.root_uuid(),
		rss.dir.name(),
		remote_file.name()
	)
	.into();
	let io_path = io::get_file_path(&upload_path).unwrap();

	std::fs::write(&io_path, test_content).unwrap();

	// Upload should return false (unchanged)
	let was_uploaded = db
		.upload_file_if_changed(&client, upload_path)
		.await
		.unwrap();
	assert!(!was_uploaded);

	// Clean up
	std::fs::remove_file(&io_path).ok();
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_upload_file_if_changed_modified_file() {
	let (db, client, rss) = get_db_resources().await;

	// Create and upload a file first using the SDK directly
	let original_content = b"Original content.";
	let file = rss
		.client
		.make_file_builder("modify_test.txt", &rss.dir)
		.build();
	let mut file_writer = rss.client.get_file_writer(file).unwrap();
	file_writer.write_all(original_content).await.unwrap();
	file_writer.close().await.unwrap();

	// Update the database with this file info
	let dir_path: FfiPathWithRoot = format!("{}/{}", client.root_uuid(), rss.dir.name()).into();
	db.update_dir_children(&client, dir_path).await.unwrap();

	// Create a local file with different content
	let modified_content = b"Modified content - completely different!";
	let upload_path: FfiPathWithRoot =
		format!("{}/{}/modify_test.txt", client.root_uuid(), rss.dir.name()).into();
	let io_path = io::get_file_path(&upload_path).unwrap();
	std::fs::write(&io_path, modified_content).unwrap();

	// Upload should return true (changed)
	let was_uploaded = db
		.upload_file_if_changed(&client, upload_path)
		.await
		.unwrap();
	assert!(was_uploaded);

	// Clean up
	std::fs::remove_file(&io_path).ok();
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_upload_file_if_changed_invalid_parent() {
	let (db, client, rss) = get_db_resources().await;

	let invalid_path: FfiPathWithRoot = format!(
		"{}/{}/nonexistent_subdir/invalid_parent.txt",
		client.root_uuid(),
		rss.dir.name()
	)
	.into();

	let io_path = io::get_file_path(&invalid_path).unwrap();

	// Create a temporary local file
	std::fs::write(&io_path, b"test").unwrap();

	// Should fail with invalid parent path
	let result = db.upload_file_if_changed(&client, invalid_path).await;
	assert!(result.is_err());

	// Clean up
	std::fs::remove_file(&io_path).ok();
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_create_empty_file() {
	let (db, client, rss) = get_db_resources().await;

	let parent_path: FfiPathWithRoot = format!("{}/{}", client.root_uuid(), rss.dir.name()).into();
	let file_name = "empty_test.txt".to_string();
	let mime_type = "text/plain".to_string();

	// Create an empty file
	let file_path = db
		.create_empty_file(
			&client,
			parent_path.clone(),
			file_name.clone(),
			mime_type.clone(),
		)
		.await
		.unwrap();

	// Verify the file exists in the database
	let queried_file = db.query_item(&file_path).unwrap();

	match queried_file {
		Some(FfiObject::File(file)) => {
			assert_eq!(file.name, file_name);
			assert_eq!(file.mime, mime_type);
			assert_eq!(file.size, 0); // Should be empty
		}
		_ => panic!("Expected to find a file object"),
	}
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_create_empty_file_different_mime_types() {
	let (db, client, rss) = get_db_resources().await;

	let parent_path: FfiPathWithRoot = format!("{}/{}", client.root_uuid(), rss.dir.name()).into();

	let test_cases = vec![
		("test.json", "application/json"),
		("test.xml", "application/xml"),
		("test.md", "text/markdown"),
		("test.csv", "text/csv"),
	];

	for (filename, mime_type) in test_cases {
		let file_path = db
			.create_empty_file(
				&client,
				parent_path.clone(),
				filename.to_string(),
				mime_type.to_string(),
			)
			.await
			.unwrap();

		// Verify each file was created with correct MIME type
		let queried_file = db.query_item(&file_path).unwrap();

		match queried_file {
			Some(FfiObject::File(file)) => {
				assert_eq!(file.name, filename);
				assert_eq!(file.mime, mime_type);
				assert_eq!(file.size, 0);
			}
			_ => panic!("Expected to find a file object for {}", filename),
		}
	}
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_create_empty_file_duplicate_name() {
	let (db, client, rss) = get_db_resources().await;

	let parent_path: FfiPathWithRoot = format!("{}/{}", client.root_uuid(), rss.dir.name()).into();
	let file_name = "duplicate.txt".to_string();
	let mime_type = "text/plain".to_string();

	// Create first file
	db.create_empty_file(
		&client,
		parent_path.clone(),
		file_name.clone(),
		mime_type.clone(),
	)
	.await
	.unwrap();

	assert!(
		db.create_empty_file(
			&client,
			parent_path.clone(),
			file_name.clone(),
			mime_type.clone(),
		)
		.await
		.is_ok()
	);
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_create_empty_file_invalid_parent() {
	let (db, client, rss) = get_db_resources().await;

	let invalid_parent_path: FfiPathWithRoot = format!(
		"{}/{}/nonexistent_subdir",
		client.root_uuid(),
		rss.dir.name()
	)
	.into();

	// Should fail with invalid parent path
	let result = db
		.create_empty_file(
			&client,
			invalid_parent_path,
			"test.txt".to_string(),
			"text/plain".to_string(),
		)
		.await;

	assert!(result.is_err());
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_create_empty_file_in_root() {
	let (db, client, rss) = get_db_resources().await;

	// Create file in the test directory (rss.dir), not the absolute root
	let parent_path: FfiPathWithRoot = format!("{}/{}", client.root_uuid(), rss.dir.name()).into();
	let file_name = "root_file.txt".to_string();
	let mime_type = "text/plain".to_string();

	// Create file in test directory
	let file_path = db
		.create_empty_file(
			&client,
			parent_path.clone(),
			file_name.clone(),
			mime_type.clone(),
		)
		.await
		.unwrap();

	// Verify the file exists
	let queried_file = db.query_item(&file_path).unwrap();

	match queried_file {
		Some(FfiObject::File(file)) => {
			assert_eq!(file.name, file_name);
			assert_eq!(file.mime, mime_type);
		}
		_ => panic!("Expected to find a file object in test directory"),
	}
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_trash_item_file_success() {
	let (db, client, rss) = get_db_resources().await;

	// Create a test file
	let file = rss
		.client
		.make_file_builder("trash_me.txt", &rss.dir)
		.build();
	let mut file_writer = rss.client.get_file_writer(file).unwrap();
	file_writer
		.write_all(b"This file will be trashed")
		.await
		.unwrap();
	file_writer.close().await.unwrap();
	let file = file_writer.into_remote_file().unwrap();

	let file_path: FfiPathWithRoot =
		format!("{}/{}/{}", client.root_uuid(), rss.dir.name(), file.name()).into();

	// Update the database to include the file
	let parent_path: FfiPathWithRoot = format!("{}/{}", client.root_uuid(), rss.dir.name()).into();
	db.update_dir_children(&client, parent_path.clone())
		.await
		.unwrap();

	// Verify file exists before trashing
	let result = db.query_item(&file_path).unwrap();
	assert!(result.is_some());

	// Trash the file
	db.trash_item(&client, file_path.clone()).await.unwrap();

	// Verify file is removed from database
	let result = db.query_item(&file_path).unwrap();
	assert!(result.is_none());

	// Verify file is no longer in parent directory listing
	db.update_dir_children(&client, parent_path.clone())
		.await
		.unwrap();
	let children = db.query_dir_children(&parent_path, None).unwrap().unwrap();
	let file_exists = children
		.objects
		.iter()
		.any(|obj| matches!(obj, FfiNonRootObject::File(f) if f.uuid == file.uuid().to_string()));
	assert!(!file_exists);
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_trash_item_directory_success() {
	let (db, client, rss) = get_db_resources().await;

	// Create a test directory
	let dir = rss
		.client
		.create_dir(&rss.dir, "trash_this_dir".to_string())
		.await
		.unwrap();

	let dir_path: FfiPathWithRoot =
		format!("{}/{}/{}", client.root_uuid(), rss.dir.name(), dir.name()).into();

	// Update the database to include the directory
	let parent_path: FfiPathWithRoot = format!("{}/{}", client.root_uuid(), rss.dir.name()).into();
	db.update_dir_children(&client, parent_path.clone())
		.await
		.unwrap();

	// Verify directory exists before trashing
	let result = db.query_item(&dir_path).unwrap();
	assert!(result.is_some());

	// Trash the directory
	db.trash_item(&client, dir_path.clone()).await.unwrap();

	// Verify directory is removed from database
	let result = db.query_item(&dir_path).unwrap();
	assert!(result.is_none());

	// Verify directory is no longer in parent directory listing
	db.update_dir_children(&client, parent_path.clone())
		.await
		.unwrap();
	let children = db.query_dir_children(&parent_path, None).unwrap().unwrap();
	let dir_exists = children
		.objects
		.iter()
		.any(|obj| matches!(obj, FfiNonRootObject::Dir(d) if d.uuid == dir.uuid().to_string()));
	assert!(!dir_exists);
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_trash_item_directory_with_contents() {
	let (db, client, rss) = get_db_resources().await;

	// Create a directory with nested content
	let parent_dir = rss
		.client
		.create_dir(&rss.dir, "parent_to_trash".to_string())
		.await
		.unwrap();

	// Add a subdirectory
	let sub_dir = rss
		.client
		.create_dir(&parent_dir, "subdirectory".to_string())
		.await
		.unwrap();

	// Add a file to the parent directory
	let file_in_parent = rss
		.client
		.make_file_builder("file_in_parent.txt", &parent_dir)
		.build();
	let mut file_writer = rss.client.get_file_writer(file_in_parent).unwrap();
	file_writer.write_all(b"Content in parent").await.unwrap();
	file_writer.close().await.unwrap();

	// Add a file to the subdirectory
	let file_in_sub = rss
		.client
		.make_file_builder("file_in_sub.txt", &sub_dir)
		.build();
	let mut file_writer = rss.client.get_file_writer(file_in_sub).unwrap();
	file_writer
		.write_all(b"Content in subdirectory")
		.await
		.unwrap();
	file_writer.close().await.unwrap();

	// Update database with all the content
	let base_path: FfiPathWithRoot = format!("{}/{}", client.root_uuid(), rss.dir.name()).into();
	let parent_dir_path: FfiPathWithRoot = format!("{}/{}", base_path.0, parent_dir.name()).into();
	let sub_dir_path: FfiPathWithRoot = format!("{}/{}", parent_dir_path.0, sub_dir.name()).into();

	db.update_dir_children(&client, base_path.clone())
		.await
		.unwrap();
	db.update_dir_children(&client, parent_dir_path.clone())
		.await
		.unwrap();
	db.update_dir_children(&client, sub_dir_path.clone())
		.await
		.unwrap();

	// Verify all content exists
	assert!(db.query_item(&parent_dir_path).unwrap().is_some());
	assert!(db.query_item(&sub_dir_path).unwrap().is_some());

	// Trash the parent directory (should remove everything)
	db.trash_item(&client, parent_dir_path.clone())
		.await
		.unwrap();

	// Verify parent directory is gone
	assert!(db.query_item(&parent_dir_path).unwrap().is_none());

	// Verify subdirectory is also gone (cascading delete)
	assert!(db.query_item(&sub_dir_path).unwrap().is_none());

	// Verify parent directory is no longer in base directory listing
	db.update_dir_children(&client, base_path.clone())
		.await
		.unwrap();
	let children = db.query_dir_children(&base_path, None).unwrap().unwrap();
	let parent_exists = children.objects.iter().any(
		|obj| matches!(obj, FfiNonRootObject::Dir(d) if d.uuid == parent_dir.uuid().to_string()),
	);
	assert!(!parent_exists);
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_trash_item_root_directory_error() {
	let (db, client, rss) = get_db_resources().await;

	// Attempt to trash the root directory
	let root_path: FfiPathWithRoot = client.root_uuid().into();
	let result = db.trash_item(&client, root_path).await;

	// Should fail with appropriate error
	assert!(result.is_err());
	let error_message = format!("{}", result.unwrap_err());
	assert!(error_message.contains("Cannot remove root directory"));
	assert!(error_message.contains(&rss.client.root().uuid().to_string()));
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_trash_item_nonexistent_file() {
	let (db, client, rss) = get_db_resources().await;

	let nonexistent_path: FfiPathWithRoot = format!(
		"{}/{}/nonexistent_file.txt",
		client.root_uuid(),
		rss.dir.name()
	)
	.into();

	// Should fail when trying to trash a non-existent file
	let result = db.trash_item(&client, nonexistent_path).await;
	assert!(result.is_err());
	let error_message = format!("{}", result.unwrap_err());
	assert!(error_message.contains("does not point to an item"));
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_trash_item_nonexistent_directory() {
	let (db, client, rss) = get_db_resources().await;

	let nonexistent_path: FfiPathWithRoot =
		format!("{}/{}/nonexistent_dir", client.root_uuid(), rss.dir.name()).into();

	// Should fail when trying to trash a non-existent directory
	let result = db.trash_item(&client, nonexistent_path).await;
	assert!(result.is_err());
	let error_message = format!("{}", result.unwrap_err());
	assert!(error_message.contains("does not point to an item"));
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_trash_item_invalid_path() {
	let (db, client, _rss) = get_db_resources().await;

	let invalid_path: FfiPathWithRoot = "not-a-uuid/invalid/path".into();
	let result = db.trash_item(&client, invalid_path).await;

	// Should fail with UUID parsing error
	assert!(result.is_err());
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_trash_item_partial_path() {
	let (db, client, rss) = get_db_resources().await;

	// Create a directory structure but don't update all levels
	let level1 = rss
		.client
		.create_dir(&rss.dir, "level1".to_string())
		.await
		.unwrap();

	rss.client
		.create_dir(&level1, "level2".to_string())
		.await
		.unwrap();

	// Only update the base directory, not the nested ones
	let base_path: FfiPathWithRoot = format!("{}/{}", client.root_uuid(), rss.dir.name()).into();
	db.update_dir_children(&client, base_path).await.unwrap();

	// Try to trash level2 without having updated level1's children
	let level2_path: FfiPathWithRoot =
		format!("{}/{}/level1/level2", client.root_uuid(), rss.dir.name()).into();

	db.trash_item(&client, level2_path).await.unwrap();
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_trash_item_file_then_query_parent() {
	let (db, client, rss) = get_db_resources().await;

	// Create multiple files in the same directory
	let file1 = rss
		.client
		.make_file_builder("keep_me.txt", &rss.dir)
		.build();
	let mut file1_writer = rss.client.get_file_writer(file1).unwrap();
	file1_writer.write_all(b"Keep this file").await.unwrap();
	file1_writer.close().await.unwrap();
	let file1 = file1_writer.into_remote_file().unwrap();

	let file2 = rss
		.client
		.make_file_builder("trash_me.txt", &rss.dir)
		.build();
	let mut file2_writer = rss.client.get_file_writer(file2).unwrap();
	file2_writer.write_all(b"Trash this file").await.unwrap();
	file2_writer.close().await.unwrap();
	let file2 = file2_writer.into_remote_file().unwrap();

	let parent_path: FfiPathWithRoot = format!("{}/{}", client.root_uuid(), rss.dir.name()).into();
	let file2_path: FfiPathWithRoot = format!("{}/{}", parent_path.0, file2.name()).into();

	// Update database
	db.update_dir_children(&client, parent_path.clone())
		.await
		.unwrap();

	// Verify both files exist
	let children = db.query_dir_children(&parent_path, None).unwrap().unwrap();
	assert_eq!(children.objects.len(), 2);

	// Trash one file
	db.trash_item(&client, file2_path).await.unwrap();

	// Update parent and verify only one file remains
	db.update_dir_children(&client, parent_path.clone())
		.await
		.unwrap();
	let children = db.query_dir_children(&parent_path, None).unwrap().unwrap();
	assert_eq!(children.objects.len(), 1);

	// Verify it's the correct remaining file
	assert!(matches!(
		&children.objects[0],
		FfiNonRootObject::File(f) if f.uuid == file1.uuid().to_string()
	));
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_trash_item_empty_directory() {
	let (db, client, rss) = get_db_resources().await;

	// Create an empty directory
	let empty_dir = rss
		.client
		.create_dir(&rss.dir, "empty_dir".to_string())
		.await
		.unwrap();

	let dir_path: FfiPathWithRoot = format!(
		"{}/{}/{}",
		client.root_uuid(),
		rss.dir.name(),
		empty_dir.name()
	)
	.into();

	let parent_path: FfiPathWithRoot = format!("{}/{}", client.root_uuid(), rss.dir.name()).into();

	// Update database
	db.update_dir_children(&client, parent_path.clone())
		.await
		.unwrap();

	// Verify directory exists and is empty
	assert!(db.query_item(&dir_path).unwrap().is_some());
	let empty_children = db.query_dir_children(&dir_path, None).unwrap().unwrap();
	assert_eq!(empty_children.objects.len(), 0);

	// Trash the empty directory
	db.trash_item(&client, dir_path.clone()).await.unwrap();

	// Verify it's gone
	assert!(db.query_item(&dir_path).unwrap().is_none());

	// Verify parent no longer contains it
	db.update_dir_children(&client, parent_path.clone())
		.await
		.unwrap();
	let parent_children = db.query_dir_children(&parent_path, None).unwrap().unwrap();
	let dir_exists = parent_children.objects.iter().any(
		|obj| matches!(obj, FfiNonRootObject::Dir(d) if d.uuid == empty_dir.uuid().to_string()),
	);
	assert!(!dir_exists);
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_trash_item_already_trashed_file() {
	let (db, client, rss) = get_db_resources().await;

	// Create and trash a file using the SDK directly first
	let file = rss
		.client
		.make_file_builder("already_trashed.txt", &rss.dir)
		.build();
	let mut file_writer = rss.client.get_file_writer(file).unwrap();
	file_writer
		.write_all(b"This will be trashed twice")
		.await
		.unwrap();
	file_writer.close().await.unwrap();
	let file = file_writer.into_remote_file().unwrap();

	// Trash it directly via SDK
	rss.client.trash_file(&file).await.unwrap();

	let file_path: FfiPathWithRoot =
		format!("{}/{}/{}", client.root_uuid(), rss.dir.name(), file.name()).into();

	// Now try to trash it via our method - should fail since it doesn't exist in our DB
	let result = db.trash_item(&client, file_path).await;
	assert!(result.is_err());
	let error_message = format!("{}", result.unwrap_err());
	assert!(error_message.contains("does not point to an item"));
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_move_item_file_success() {
	let (db, client, rss) = get_db_resources().await;

	// Create source and destination directories
	let source_dir = rss
		.client
		.create_dir(&rss.dir, "source_dir".to_string())
		.await
		.unwrap();

	let dest_dir = rss
		.client
		.create_dir(&rss.dir, "dest_dir".to_string())
		.await
		.unwrap();

	// Create a file in the source directory
	let file = rss
		.client
		.make_file_builder("move_me.txt", &source_dir)
		.build();
	let mut file_writer = rss.client.get_file_writer(file).unwrap();
	file_writer.write_all(b"Content to move").await.unwrap();
	file_writer.close().await.unwrap();
	let file = file_writer.into_remote_file().unwrap();

	// Update database with all directories
	let base_path: FfiPathWithRoot = format!("{}/{}", client.root_uuid(), rss.dir.name()).into();
	let source_path: FfiPathWithRoot = format!("{}/{}", base_path.0, source_dir.name()).into();
	let dest_path: FfiPathWithRoot = format!("{}/{}", base_path.0, dest_dir.name()).into();

	db.update_dir_children(&client, base_path).await.unwrap();
	db.update_dir_children(&client, source_path.clone())
		.await
		.unwrap();
	db.update_dir_children(&client, dest_path.clone())
		.await
		.unwrap();

	// Define paths for the move operation
	let file_path: FfiPathWithRoot = format!("{}/{}", source_path.0, file.name()).into();

	// Move the file
	let new_file_path = db
		.move_item(
			&client,
			file_path.clone(),
			source_path.clone(),
			dest_path.clone(),
		)
		.await
		.unwrap();

	// Verify the new path is correct
	let expected_new_path: FfiPathWithRoot = format!("{}/{}", dest_path.0, file.name()).into();
	assert_eq!(new_file_path.0, expected_new_path.0);

	// Verify file no longer exists at old location
	assert!(db.query_item(&file_path).unwrap().is_none());

	// Verify file exists at new location
	let moved_file = db.query_item(&new_file_path).unwrap();
	assert!(moved_file.is_some());
	match moved_file.unwrap() {
		FfiObject::File(f) => {
			assert_eq!(f.name, file.name());
			assert_eq!(f.uuid, file.uuid().to_string());
		}
		_ => panic!("Expected file object"),
	}

	// Verify source directory no longer contains the file
	db.update_dir_children(&client, source_path.clone())
		.await
		.unwrap();
	let source_children = db.query_dir_children(&source_path, None).unwrap().unwrap();
	let file_in_source = source_children
		.objects
		.iter()
		.any(|obj| matches!(obj, FfiNonRootObject::File(f) if f.uuid == file.uuid().to_string()));
	assert!(!file_in_source);

	// Verify destination directory contains the file
	db.update_dir_children(&client, dest_path.clone())
		.await
		.unwrap();
	let dest_children = db.query_dir_children(&dest_path, None).unwrap().unwrap();
	let file_in_dest = dest_children
		.objects
		.iter()
		.any(|obj| matches!(obj, FfiNonRootObject::File(f) if f.uuid == file.uuid().to_string()));
	assert!(file_in_dest);
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_move_item_directory_success() {
	let (db, client, rss) = get_db_resources().await;

	// Create source and destination directories
	let source_dir = rss
		.client
		.create_dir(&rss.dir, "source_dir".to_string())
		.await
		.unwrap();

	let dest_dir = rss
		.client
		.create_dir(&rss.dir, "dest_dir".to_string())
		.await
		.unwrap();

	// Create a directory to move
	let move_dir = rss
		.client
		.create_dir(&source_dir, "dir_to_move".to_string())
		.await
		.unwrap();

	// Add some content to the directory being moved
	let file_in_move_dir = rss
		.client
		.make_file_builder("content.txt", &move_dir)
		.build();
	let mut file_writer = rss.client.get_file_writer(file_in_move_dir).unwrap();
	file_writer
		.write_all(b"Content in moved dir")
		.await
		.unwrap();
	file_writer.close().await.unwrap();

	// Update database with all directories
	let base_path: FfiPathWithRoot = format!("{}/{}", client.root_uuid(), rss.dir.name()).into();
	let source_path: FfiPathWithRoot = format!("{}/{}", base_path.0, source_dir.name()).into();
	let dest_path: FfiPathWithRoot = format!("{}/{}", base_path.0, dest_dir.name()).into();
	let move_dir_path: FfiPathWithRoot = format!("{}/{}", source_path.0, move_dir.name()).into();

	db.update_dir_children(&client, base_path).await.unwrap();
	db.update_dir_children(&client, source_path.clone())
		.await
		.unwrap();
	db.update_dir_children(&client, dest_path.clone())
		.await
		.unwrap();
	db.update_dir_children(&client, move_dir_path.clone())
		.await
		.unwrap();

	// Move the directory
	let new_dir_path = db
		.move_item(
			&client,
			move_dir_path.clone(),
			source_path.clone(),
			dest_path.clone(),
		)
		.await
		.unwrap();

	// Verify the new path is correct
	let expected_new_path: FfiPathWithRoot = format!("{}/{}", dest_path.0, move_dir.name()).into();
	assert_eq!(new_dir_path.0, expected_new_path.0);

	// Verify directory no longer exists at old location
	assert!(db.query_item(&move_dir_path).unwrap().is_none());

	// Verify directory exists at new location
	let moved_dir = db.query_item(&new_dir_path).unwrap();
	assert!(moved_dir.is_some());
	match moved_dir.unwrap() {
		FfiObject::Dir(d) => {
			assert_eq!(d.name, move_dir.name());
			assert_eq!(d.uuid, move_dir.uuid().to_string());
		}
		_ => panic!("Expected directory object"),
	}

	// Verify source directory no longer contains the moved directory
	db.update_dir_children(&client, source_path.clone())
		.await
		.unwrap();
	let source_children = db.query_dir_children(&source_path, None).unwrap().unwrap();
	let dir_in_source = source_children.objects.iter().any(
		|obj| matches!(obj, FfiNonRootObject::Dir(d) if d.uuid == move_dir.uuid().to_string()),
	);
	assert!(!dir_in_source);

	// Verify destination directory contains the moved directory
	db.update_dir_children(&client, dest_path.clone())
		.await
		.unwrap();
	let dest_children = db.query_dir_children(&dest_path, None).unwrap().unwrap();
	let dir_in_dest = dest_children.objects.iter().any(
		|obj| matches!(obj, FfiNonRootObject::Dir(d) if d.uuid == move_dir.uuid().to_string()),
	);
	assert!(dir_in_dest);
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_move_item_file_to_root_directory() {
	let (db, client, rss) = get_db_resources().await;

	// Create a file in a subdirectory
	let file = rss
		.client
		.make_file_builder("move_to_root.txt", &rss.dir)
		.build();
	let mut file_writer = rss.client.get_file_writer(file).unwrap();
	file_writer.write_all(b"Moving to test root").await.unwrap();
	file_writer.close().await.unwrap();
	let file = file_writer.into_remote_file().unwrap();

	// Update database
	let base_path: FfiPathWithRoot = format!("{}/{}", client.root_uuid(), rss.dir.name()).into();
	let file_path: FfiPathWithRoot = format!("{}/{}", base_path.0, file.name()).into();
	let root_path: FfiPathWithRoot = client.root_uuid().into();

	db.update_dir_children(&client, base_path.clone())
		.await
		.unwrap();

	// Move file from subdirectory to root
	let new_file_path = db
		.move_item(
			&client,
			file_path.clone(),
			base_path.clone(),
			root_path.clone(),
		)
		.await
		.unwrap();

	// Verify the new path is at root level
	let expected_new_path: FfiPathWithRoot =
		format!("{}/{}", client.root_uuid(), file.name()).into();
	assert_eq!(new_file_path.0, expected_new_path.0);

	// Verify file no longer exists in subdirectory
	assert!(db.query_item(&file_path).unwrap().is_none());

	// Verify file exists at root
	let moved_file = db.query_item(&new_file_path).unwrap();
	assert!(moved_file.is_some());
	match moved_file.unwrap() {
		FfiObject::File(f) => {
			assert_eq!(f.name, file.name());
			assert_eq!(f.uuid, file.uuid().to_string());
		}
		_ => panic!("Expected file object"),
	}
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_move_item_nonexistent_item() {
	let (db, client, rss) = get_db_resources().await;

	// Create destination directory
	let dest_dir = rss
		.client
		.create_dir(&rss.dir, "dest_dir".to_string())
		.await
		.unwrap();

	let base_path: FfiPathWithRoot = format!("{}/{}", client.root_uuid(), rss.dir.name()).into();
	let dest_path: FfiPathWithRoot = format!("{}/{}", base_path.0, dest_dir.name()).into();
	let nonexistent_file_path: FfiPathWithRoot = format!("{}/nonexistent.txt", base_path.0).into();

	db.update_dir_children(&client, base_path.clone())
		.await
		.unwrap();

	// Try to move non-existent file
	let result = db
		.move_item(&client, nonexistent_file_path, base_path.clone(), dest_path)
		.await;

	assert!(result.is_err());
	let error_message = format!("{}", result.unwrap_err());
	assert!(error_message.contains("does not point to an item"));
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_move_item_nonexistent_destination() {
	let (db, client, rss) = get_db_resources().await;

	// Create a file to move
	let file = rss
		.client
		.make_file_builder("move_me.txt", &rss.dir)
		.build();
	let mut file_writer = rss.client.get_file_writer(file).unwrap();
	file_writer.write_all(b"Content").await.unwrap();
	file_writer.close().await.unwrap();
	let file = file_writer.into_remote_file().unwrap();

	let base_path: FfiPathWithRoot = format!("{}/{}", client.root_uuid(), rss.dir.name()).into();
	let file_path: FfiPathWithRoot = format!("{}/{}", base_path.0, file.name()).into();
	let nonexistent_dest: FfiPathWithRoot = format!("{}/nonexistent_dir", base_path.0).into();

	db.update_dir_children(&client, base_path.clone())
		.await
		.unwrap();

	// Try to move to non-existent destination
	let result = db
		.move_item(&client, file_path, base_path.clone(), nonexistent_dest)
		.await;

	assert!(result.is_err());
	let error_message = format!("{}", result.unwrap_err());
	assert!(error_message.contains("does not point to an item"));
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_move_item_invalid_parent_path() {
	let (db, client, rss) = get_db_resources().await;

	// Create source and destination directories
	let source_dir = rss
		.client
		.create_dir(&rss.dir, "source_dir".to_string())
		.await
		.unwrap();

	let dest_dir = rss
		.client
		.create_dir(&rss.dir, "dest_dir".to_string())
		.await
		.unwrap();

	// Create a file in source directory
	let file = rss
		.client
		.make_file_builder("test_file.txt", &source_dir)
		.build();
	let mut file_writer = rss.client.get_file_writer(file).unwrap();
	file_writer.write_all(b"Content").await.unwrap();
	file_writer.close().await.unwrap();
	let file = file_writer.into_remote_file().unwrap();

	let base_path: FfiPathWithRoot = format!("{}/{}", client.root_uuid(), rss.dir.name()).into();
	let source_path: FfiPathWithRoot = format!("{}/{}", base_path.0, source_dir.name()).into();
	let dest_path: FfiPathWithRoot = format!("{}/{}", base_path.0, dest_dir.name()).into();
	let file_path: FfiPathWithRoot = format!("{}/{}", source_path.0, file.name()).into();
	let wrong_parent_path: FfiPathWithRoot = format!("{}/{}", base_path.0, dest_dir.name()).into(); // Wrong parent

	db.update_dir_children(&client, base_path).await.unwrap();
	db.update_dir_children(&client, source_path.clone())
		.await
		.unwrap();

	// Try to move with wrong parent path
	let result = db
		.move_item(&client, file_path, wrong_parent_path, dest_path)
		.await;

	assert!(result.is_err());
	let error_message = format!("{}", result.unwrap_err());
	assert!(error_message.contains("does not point to the parent"));
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_move_item_destination_is_file() {
	let (db, client, rss) = get_db_resources().await;

	// Create a file to move
	let move_file = rss
		.client
		.make_file_builder("move_me.txt", &rss.dir)
		.build();
	let mut file_writer = rss.client.get_file_writer(move_file).unwrap();
	file_writer.write_all(b"Content to move").await.unwrap();
	file_writer.close().await.unwrap();
	let move_file = file_writer.into_remote_file().unwrap();

	// Create a file that will be used as invalid destination
	let dest_file = rss
		.client
		.make_file_builder("dest_file.txt", &rss.dir)
		.build();
	let mut dest_writer = rss.client.get_file_writer(dest_file).unwrap();
	dest_writer
		.write_all(b"This is not a directory")
		.await
		.unwrap();
	dest_writer.close().await.unwrap();
	let dest_file = dest_writer.into_remote_file().unwrap();

	let base_path: FfiPathWithRoot = format!("{}/{}", client.root_uuid(), rss.dir.name()).into();
	let move_file_path: FfiPathWithRoot = format!("{}/{}", base_path.0, move_file.name()).into();
	let dest_file_path: FfiPathWithRoot = format!("{}/{}", base_path.0, dest_file.name()).into();

	db.update_dir_children(&client, base_path.clone())
		.await
		.unwrap();

	// Try to move to a file instead of directory
	let result = db
		.move_item(&client, move_file_path, base_path, dest_file_path)
		.await;

	assert!(result.is_err());
	let error_message = format!("{}", result.unwrap_err());
	assert!(error_message.contains("does not point to a directory"));
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_move_item_root_directory_error() {
	let (db, client, rss) = get_db_resources().await;

	// Create destination directory
	let dest_dir = rss
		.client
		.create_dir(&rss.dir, "dest_dir".to_string())
		.await
		.unwrap();

	let root_path: FfiPathWithRoot = client.root_uuid().into();
	let base_path: FfiPathWithRoot = format!("{}/{}", client.root_uuid(), rss.dir.name()).into();
	let dest_path: FfiPathWithRoot = format!("{}/{}", base_path.0, dest_dir.name()).into();

	db.update_dir_children(&client, base_path).await.unwrap();

	// Try to move root directory (should fail at conversion to DBNonRootObject)
	let result = db
		.move_item(&client, root_path.clone(), root_path.clone(), dest_path)
		.await;

	assert!(result.is_err());
	let error_message = format!("{}", result.unwrap_err());
	assert!(error_message.contains("does not point to a non-root item"));
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_move_item_same_directory() {
	let (db, client, rss) = get_db_resources().await;

	// Create a file
	let file = rss
		.client
		.make_file_builder("stay_here.txt", &rss.dir)
		.build();
	let mut file_writer = rss.client.get_file_writer(file).unwrap();
	file_writer.write_all(b"Content").await.unwrap();
	file_writer.close().await.unwrap();
	let file = file_writer.into_remote_file().unwrap();

	let base_path: FfiPathWithRoot = format!("{}/{}", client.root_uuid(), rss.dir.name()).into();
	let file_path: FfiPathWithRoot = format!("{}/{}", base_path.0, file.name()).into();

	db.update_dir_children(&client, base_path.clone())
		.await
		.unwrap();

	// Move file to the same directory (should succeed)
	let new_file_path = db
		.move_item(
			&client,
			file_path.clone(),
			base_path.clone(),
			base_path.clone(),
		)
		.await
		.unwrap();

	// File should still be in the same location
	assert_eq!(new_file_path.0, file_path.0);

	// Verify file still exists
	let moved_file = db.query_item(&new_file_path).unwrap();
	assert!(moved_file.is_some());
	match moved_file.unwrap() {
		FfiObject::File(f) => {
			assert_eq!(f.name, file.name());
			assert_eq!(f.uuid, file.uuid().to_string());
		}
		_ => panic!("Expected file object"),
	}
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_move_item_nested_directory_structure() {
	let (db, client, rss) = get_db_resources().await;

	// Create nested structure: base/level1/level2/file.txt
	let level1 = rss
		.client
		.create_dir(&rss.dir, "level1".to_string())
		.await
		.unwrap();

	let level2 = rss
		.client
		.create_dir(&level1, "level2".to_string())
		.await
		.unwrap();

	let file = rss
		.client
		.make_file_builder("nested_file.txt", &level2)
		.build();
	let mut file_writer = rss.client.get_file_writer(file).unwrap();
	file_writer.write_all(b"Nested content").await.unwrap();
	file_writer.close().await.unwrap();
	let file = file_writer.into_remote_file().unwrap();

	// Create destination directory at root level
	let dest_dir = rss
		.client
		.create_dir(&rss.dir, "destination".to_string())
		.await
		.unwrap();

	// Set up paths
	let base_path: FfiPathWithRoot = format!("{}/{}", client.root_uuid(), rss.dir.name()).into();
	let level1_path: FfiPathWithRoot = format!("{}/{}", base_path.0, level1.name()).into();
	let level2_path: FfiPathWithRoot = format!("{}/{}", level1_path.0, level2.name()).into();
	let file_path: FfiPathWithRoot = format!("{}/{}", level2_path.0, file.name()).into();
	let dest_path: FfiPathWithRoot = format!("{}/{}", base_path.0, dest_dir.name()).into();

	// Update all levels
	db.update_dir_children(&client, base_path.clone())
		.await
		.unwrap();
	db.update_dir_children(&client, level1_path).await.unwrap();
	db.update_dir_children(&client, level2_path.clone())
		.await
		.unwrap();
	db.update_dir_children(&client, dest_path.clone())
		.await
		.unwrap();

	// Move file from deep nested location to destination
	let new_file_path = db
		.move_item(
			&client,
			file_path.clone(),
			level2_path.clone(),
			dest_path.clone(),
		)
		.await
		.unwrap();

	// Verify new path
	let expected_new_path: FfiPathWithRoot = format!("{}/{}", dest_path.0, file.name()).into();
	assert_eq!(new_file_path.0, expected_new_path.0);

	// Verify file no longer exists at original location
	assert!(db.query_item(&file_path).unwrap().is_none());

	// Verify file exists at new location
	let moved_file = db.query_item(&new_file_path).unwrap();
	assert!(moved_file.is_some());
	match moved_file.unwrap() {
		FfiObject::File(f) => {
			assert_eq!(f.name, file.name());
			assert_eq!(f.uuid, file.uuid().to_string());
		}
		_ => panic!("Expected file object"),
	}

	// Verify level2 directory is now empty
	db.update_dir_children(&client, level2_path.clone())
		.await
		.unwrap();
	let level2_children = db.query_dir_children(&level2_path, None).unwrap().unwrap();
	assert_eq!(level2_children.objects.len(), 0);

	// Verify destination directory contains the file
	db.update_dir_children(&client, dest_path.clone())
		.await
		.unwrap();
	let dest_children = db.query_dir_children(&dest_path, None).unwrap().unwrap();
	let file_in_dest = dest_children
		.objects
		.iter()
		.any(|obj| matches!(obj, FfiNonRootObject::File(f) if f.uuid == file.uuid().to_string()));
	assert!(file_in_dest);
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_move_item_directory_with_contents() {
	let (db, client, rss) = get_db_resources().await;

	// Create source directory with contents
	let source_dir = rss
		.client
		.create_dir(&rss.dir, "source_with_contents".to_string())
		.await
		.unwrap();

	// Create subdirectory and file in source
	let sub_dir = rss
		.client
		.create_dir(&source_dir, "subdirectory".to_string())
		.await
		.unwrap();

	let file_in_source = rss
		.client
		.make_file_builder("file_in_source.txt", &source_dir)
		.build();
	let mut file_writer = rss.client.get_file_writer(file_in_source).unwrap();
	file_writer.write_all(b"Source content").await.unwrap();
	file_writer.close().await.unwrap();
	let file_in_source = file_writer.into_remote_file().unwrap();

	let file_in_sub = rss
		.client
		.make_file_builder("file_in_sub.txt", &sub_dir)
		.build();
	let mut sub_file_writer = rss.client.get_file_writer(file_in_sub).unwrap();
	sub_file_writer.write_all(b"Sub content").await.unwrap();
	sub_file_writer.close().await.unwrap();

	// Create destination directory
	let dest_dir = rss
		.client
		.create_dir(&rss.dir, "destination".to_string())
		.await
		.unwrap();

	// Set up paths
	let base_path: FfiPathWithRoot = format!("{}/{}", client.root_uuid(), rss.dir.name()).into();
	let source_path: FfiPathWithRoot = format!("{}/{}", base_path.0, source_dir.name()).into();
	let dest_path: FfiPathWithRoot = format!("{}/{}", base_path.0, dest_dir.name()).into();

	// Update database
	db.update_dir_children(&client, base_path.clone())
		.await
		.unwrap();
	db.update_dir_children(&client, source_path.clone())
		.await
		.unwrap();
	db.update_dir_children(&client, dest_path.clone())
		.await
		.unwrap();

	// Move the entire source directory to destination
	let new_source_path = db
		.move_item(
			&client,
			source_path.clone(),
			base_path.clone(),
			dest_path.clone(),
		)
		.await
		.unwrap();

	// Verify new path
	let expected_new_path: FfiPathWithRoot =
		format!("{}/{}", dest_path.0, source_dir.name()).into();
	assert_eq!(new_source_path.0, expected_new_path.0);

	// Verify old source directory is gone
	assert!(db.query_item(&source_path).unwrap().is_none());

	// Verify new source directory exists
	let moved_dir = db.query_item(&new_source_path).unwrap();
	assert!(moved_dir.is_some());
	match moved_dir.unwrap() {
		FfiObject::Dir(d) => {
			assert_eq!(d.name, source_dir.name());
			assert_eq!(d.uuid, source_dir.uuid().to_string());
		}
		_ => panic!("Expected directory object"),
	}

	// Verify base directory no longer contains old source
	db.update_dir_children(&client, base_path.clone())
		.await
		.unwrap();
	let base_children = db.query_dir_children(&base_path, None).unwrap().unwrap();
	let old_source_in_base = base_children.objects.iter().any(
		|obj| matches!(obj, FfiNonRootObject::Dir(d) if d.uuid == source_dir.uuid().to_string()),
	);
	assert!(!old_source_in_base);

	// Verify destination contains the moved directory
	db.update_dir_children(&client, dest_path.clone())
		.await
		.unwrap();
	let dest_children = db.query_dir_children(&dest_path, None).unwrap().unwrap();
	let source_in_dest = dest_children.objects.iter().any(
		|obj| matches!(obj, FfiNonRootObject::Dir(d) if d.uuid == source_dir.uuid().to_string()),
	);
	assert!(source_in_dest);

	// Verify contents are preserved (this tests that the move operation preserves the directory structure)
	db.update_dir_children(&client, new_source_path.clone())
		.await
		.unwrap();
	let moved_source_children = db
		.query_dir_children(&new_source_path, None)
		.unwrap()
		.unwrap();
	assert_eq!(moved_source_children.objects.len(), 2); // subdirectory + file

	let has_sub_dir = moved_source_children
		.objects
		.iter()
		.any(|obj| matches!(obj, FfiNonRootObject::Dir(d) if d.uuid == sub_dir.uuid().to_string()));
	let has_file = moved_source_children.objects.iter().any(
		|obj| matches!(obj, FfiNonRootObject::File(f) if f.uuid == file_in_source.uuid().to_string()),
	);
	assert!(has_sub_dir);
	assert!(has_file);
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_move_item_invalid_uuid_in_path() {
	let (db, client, rss) = get_db_resources().await;

	let dest_dir = rss
		.client
		.create_dir(&rss.dir, "dest_dir".to_string())
		.await
		.unwrap();

	let base_path: FfiPathWithRoot = format!("{}/{}", client.root_uuid(), rss.dir.name()).into();
	let dest_path: FfiPathWithRoot = format!("{}/{}", base_path.0, dest_dir.name()).into();
	let invalid_item_path: FfiPathWithRoot = "invalid-uuid/some/path".into();
	let invalid_parent_path: FfiPathWithRoot = "invalid-uuid/parent".into();

	// Try to move with invalid UUID in item path
	let result = db
		.move_item(&client, invalid_item_path, invalid_parent_path, dest_path)
		.await;

	assert!(result.is_err());
	// Should fail with UUID parsing error
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_move_item_partial_path_resolution() {
	let (db, client, rss) = get_db_resources().await;

	// Create nested structure but only update some levels
	let level1 = rss
		.client
		.create_dir(&rss.dir, "level1".to_string())
		.await
		.unwrap();

	let level2 = rss
		.client
		.create_dir(&level1, "level2".to_string())
		.await
		.unwrap();

	let file = rss
		.client
		.make_file_builder("deep_file.txt", &level2)
		.build();
	let mut file_writer = rss.client.get_file_writer(file).unwrap();
	file_writer.write_all(b"Deep content").await.unwrap();
	file_writer.close().await.unwrap();
	let file = file_writer.into_remote_file().unwrap();

	let dest_dir = rss
		.client
		.create_dir(&rss.dir, "destination".to_string())
		.await
		.unwrap();

	// Only update base level, not the nested levels
	let base_path: FfiPathWithRoot = format!("{}/{}", client.root_uuid(), rss.dir.name()).into();
	let level2_path: FfiPathWithRoot = format!("{}/level1/level2", base_path.0).into();
	let file_path: FfiPathWithRoot = format!("{}/deep_file.txt", level2_path.0).into();
	let dest_path: FfiPathWithRoot = format!("{}/{}", base_path.0, dest_dir.name()).into();

	db.update_dir_children(&client, base_path.clone())
		.await
		.unwrap();

	// Move should work with partial path resolution (sync::update_items_in_path should handle this)
	let new_file_path = db
		.move_item(&client, file_path.clone(), level2_path, dest_path.clone())
		.await
		.unwrap();

	// Verify file was moved successfully
	let expected_new_path: FfiPathWithRoot = format!("{}/{}", dest_path.0, file.name()).into();
	assert_eq!(new_file_path.0, expected_new_path.0);

	// Verify file exists at new location
	let moved_file = db.query_item(&new_file_path).unwrap();
	assert!(moved_file.is_some());
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_move_item_name_collision_handling() {
	let (db, client, rss) = get_db_resources().await;

	// Create source directory with a file
	let source_dir = rss
		.client
		.create_dir(&rss.dir, "source".to_string())
		.await
		.unwrap();

	let file_to_move = rss
		.client
		.make_file_builder("duplicate_name.txt", &source_dir)
		.build();
	let mut file_writer = rss.client.get_file_writer(file_to_move).unwrap();
	file_writer.write_all(b"Content to move").await.unwrap();
	file_writer.close().await.unwrap();
	let file_to_move = file_writer.into_remote_file().unwrap();

	// Create destination directory with a file of the same name
	let dest_dir = rss
		.client
		.create_dir(&rss.dir, "destination".to_string())
		.await
		.unwrap();

	let existing_file = rss
		.client
		.make_file_builder("duplicate_name.txt", &dest_dir)
		.build();
	let mut existing_writer = rss.client.get_file_writer(existing_file).unwrap();
	existing_writer
		.write_all(b"Existing content")
		.await
		.unwrap();
	existing_writer.close().await.unwrap();

	// Set up paths
	let base_path: FfiPathWithRoot = format!("{}/{}", client.root_uuid(), rss.dir.name()).into();
	let source_path: FfiPathWithRoot = format!("{}/{}", base_path.0, source_dir.name()).into();
	let dest_path: FfiPathWithRoot = format!("{}/{}", base_path.0, dest_dir.name()).into();
	let file_path: FfiPathWithRoot = format!("{}/{}", source_path.0, file_to_move.name()).into();

	// Update database
	db.update_dir_children(&client, base_path).await.unwrap();
	db.update_dir_children(&client, source_path.clone())
		.await
		.unwrap();
	db.update_dir_children(&client, dest_path.clone())
		.await
		.unwrap();

	// Move should succeed (the SDK should handle name conflicts)
	let new_file_path = db
		.move_item(
			&client,
			file_path.clone(),
			source_path.clone(),
			dest_path.clone(),
		)
		.await
		.unwrap();

	// The move operation should succeed - the SDK typically handles name conflicts
	// by either overwriting or creating a new name variant
	println!(
		"New file path: {} old file path {}",
		new_file_path.0, file_path.0
	);
	assert!(new_file_path.0.contains(&dest_path.0));

	// Verify file no longer exists in source
	assert!(db.query_item(&file_path).unwrap().is_none());

	// Verify some file exists at the new location (name might be modified by SDK)
	let moved_file = db.query_item(&new_file_path).unwrap();
	assert!(moved_file.is_some());
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_move_item_multiple_files_same_operation() {
	let (db, client, rss) = get_db_resources().await;

	// Create source directory with multiple files
	let source_dir = rss
		.client
		.create_dir(&rss.dir, "multi_source".to_string())
		.await
		.unwrap();

	let file1 = rss
		.client
		.make_file_builder("file1.txt", &source_dir)
		.build();
	let mut writer1 = rss.client.get_file_writer(file1).unwrap();
	writer1.write_all(b"Content 1").await.unwrap();
	writer1.close().await.unwrap();
	let file1 = writer1.into_remote_file().unwrap();

	let file2 = rss
		.client
		.make_file_builder("file2.txt", &source_dir)
		.build();
	let mut writer2 = rss.client.get_file_writer(file2).unwrap();
	writer2.write_all(b"Content 2").await.unwrap();
	writer2.close().await.unwrap();
	let file2 = writer2.into_remote_file().unwrap();

	// Create destination directory
	let dest_dir = rss
		.client
		.create_dir(&rss.dir, "multi_dest".to_string())
		.await
		.unwrap();

	// Set up paths
	let base_path: FfiPathWithRoot = format!("{}/{}", client.root_uuid(), rss.dir.name()).into();
	let source_path: FfiPathWithRoot = format!("{}/{}", base_path.0, source_dir.name()).into();
	let dest_path: FfiPathWithRoot = format!("{}/{}", base_path.0, dest_dir.name()).into();
	let file1_path: FfiPathWithRoot = format!("{}/{}", source_path.0, file1.name()).into();
	let file2_path: FfiPathWithRoot = format!("{}/{}", source_path.0, file2.name()).into();

	// Update database
	db.update_dir_children(&client, base_path).await.unwrap();
	db.update_dir_children(&client, source_path.clone())
		.await
		.unwrap();
	db.update_dir_children(&client, dest_path.clone())
		.await
		.unwrap();

	// Move both files
	let new_file1_path = db
		.move_item(
			&client,
			file1_path.clone(),
			source_path.clone(),
			dest_path.clone(),
		)
		.await
		.unwrap();

	let new_file2_path = db
		.move_item(
			&client,
			file2_path.clone(),
			source_path.clone(),
			dest_path.clone(),
		)
		.await
		.unwrap();

	// Verify both files were moved
	assert!(db.query_item(&file1_path).unwrap().is_none());
	assert!(db.query_item(&file2_path).unwrap().is_none());

	assert!(db.query_item(&new_file1_path).unwrap().is_some());
	assert!(db.query_item(&new_file2_path).unwrap().is_some());

	// Verify source directory is now empty
	db.update_dir_children(&client, source_path.clone())
		.await
		.unwrap();
	let source_children = db.query_dir_children(&source_path, None).unwrap().unwrap();
	assert_eq!(source_children.objects.len(), 0);

	// Verify destination directory contains both files
	db.update_dir_children(&client, dest_path.clone())
		.await
		.unwrap();
	let dest_children = db.query_dir_children(&dest_path, None).unwrap().unwrap();
	assert_eq!(dest_children.objects.len(), 2);

	let has_file1 = dest_children
		.objects
		.iter()
		.any(|obj| matches!(obj, FfiNonRootObject::File(f) if f.uuid == file1.uuid().to_string()));
	let has_file2 = dest_children
		.objects
		.iter()
		.any(|obj| matches!(obj, FfiNonRootObject::File(f) if f.uuid == file2.uuid().to_string()));
	assert!(has_file1);
	assert!(has_file2);
}

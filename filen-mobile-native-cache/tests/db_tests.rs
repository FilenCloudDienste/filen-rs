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
	let mut file = rss.client.get_file_writer(file);
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
	let mut large_writer = rss.client.get_file_writer(large_file);
	large_writer
		.write_all(b"This is a much larger file with more content")
		.await
		.unwrap();
	large_writer.close().await.unwrap();
	let large_file = large_writer.into_remote_file().unwrap();

	let small_file = rss.client.make_file_builder("small.txt", &rss.dir).build();
	let mut small_writer = rss.client.get_file_writer(small_file);
	small_writer.write_all(b"small").await.unwrap();
	small_writer.close().await.unwrap();

	let empty_file = rss.client.make_file_builder("empty.txt", &rss.dir).build();
	let mut empty_writer = rss.client.get_file_writer(empty_file);
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
	let mut alpha_writer = rss.client.get_file_writer(alpha_file);
	alpha_writer.close().await.unwrap();

	let beta_file = rss.client.make_file_builder("beta.txt", &rss.dir).build();
	let mut beta_writer = rss.client.get_file_writer(beta_file);
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
	let mut file_writer = rss.client.get_file_writer(file);
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
	let mut file = rss.client.get_file_writer(file);
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
	let mut file_writer = rss.client.get_file_writer(deep_file);
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
	let mut file_writer = rss.client.get_file_writer(file);
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
	let mut file_writer = rss.client.get_file_writer(file);
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
	let mut file_writer = rss.client.get_file_writer(file);
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

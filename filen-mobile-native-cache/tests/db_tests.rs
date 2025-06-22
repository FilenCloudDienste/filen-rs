use std::sync::Arc;

use filen_mobile_native_cache::{
	FilenMobileCacheState,
	ffi::{FfiNonRootObject, FfiObject, FfiPathWithRoot},
	traits::ProgressCallback,
};
use filen_sdk_rs::fs::{HasName, HasUUID};
use rand::TryRngCore;
use test_log::test;
use test_utils::TestResources;

pub struct NoOpProgressCallback;
impl ProgressCallback for NoOpProgressCallback {
	fn set_total(&self, _size: u64) {}
	fn on_progress(&self, _bytes_processed: u64) {}
}

#[derive(Debug, Default)]
pub struct SumProgressCallback {
	pub max: std::sync::atomic::AtomicU64,
	pub count: std::sync::atomic::AtomicU64,
}

impl ProgressCallback for SumProgressCallback {
	fn set_total(&self, size: u64) {
		self.max.store(size, std::sync::atomic::Ordering::Relaxed);
		self.count.store(0, std::sync::atomic::Ordering::Relaxed);
	}

	fn on_progress(&self, bytes_processed: u64) {
		self.count
			.fetch_add(bytes_processed, std::sync::atomic::Ordering::Relaxed);
	}
}

async fn get_db_resources() -> (FilenMobileCacheState, TestResources) {
	let path = std::env::temp_dir();
	let files_path = path.join("test_files");
	std::fs::create_dir_all(&files_path).unwrap();
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = resources.client.to_stringified();
	let state = FilenMobileCacheState::from_strings_in_memory(
		client.email,
		&client.root_uuid,
		&client.auth_info,
		&client.private_key,
		client.api_key,
		client.auth_version,
		files_path.to_string_lossy().as_ref(),
	)
	.unwrap();
	state.add_root(&client.root_uuid).unwrap();
	(state, resources)
}

// Root query tests
#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_query_root_initial_state() {
	let (db, rss) = get_db_resources().await;

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
	let (state, rss) = get_db_resources().await;

	state.update_roots_info().await.unwrap();
	let root = state
		.query_roots_info(rss.client.root().uuid().to_string())
		.unwrap()
		.unwrap();

	assert_ne!(root.max_storage, 0);
	assert_ne!(root.storage_used, 0);
	assert_ne!(root.last_updated, 0);
	assert_eq!(root.uuid, state.root_uuid().to_string());
	assert_eq!(root.last_listed, 0);
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_query_root_nonexistent() {
	let (db, _rss) = get_db_resources().await;

	let fake_uuid = uuid::Uuid::new_v4().to_string();
	let result = db.query_roots_info(fake_uuid).unwrap();
	assert!(result.is_none());
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_query_root_invalid_uuid() {
	let (db, _rss) = get_db_resources().await;

	let result = db.query_roots_info("invalid-uuid".to_string());
	assert!(result.is_err());
}

// Directory children query tests
#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_query_children_empty_directory() {
	let (db, rss) = get_db_resources().await;
	let test_dir_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();

	// Before update - should return None
	let resp = db.query_dir_children(&test_dir_path, None).unwrap();
	assert!(resp.is_none());

	// After update - should return empty but valid response
	db.update_dir_children(test_dir_path.clone()).await.unwrap();

	let resp = db
		.query_dir_children(&test_dir_path, None)
		.unwrap()
		.unwrap();
	assert_eq!(resp.objects.len(), 0);
	assert_eq!(resp.parent.uuid, rss.dir.uuid().to_string());
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_query_children_with_files_and_dirs() {
	let (db, rss) = get_db_resources().await;
	let test_dir_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();

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
	let file = rss
		.client
		.upload_file(file.into(), b"Hello, world!")
		.await
		.unwrap();

	// Update and verify
	db.update_dir_children(test_dir_path.clone()).await.unwrap();
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
	let (db, rss) = get_db_resources().await;
	let test_dir_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();

	// Create files with different sizes
	let large_file = rss.client.make_file_builder("large.txt", &rss.dir).build();
	let large_file = rss
		.client
		.upload_file(
			large_file.into(),
			b"This is a much larger file with more content",
		)
		.await
		.unwrap();

	let small_file = rss.client.make_file_builder("small.txt", &rss.dir).build();
	rss.client
		.upload_file(small_file.into(), b"small")
		.await
		.unwrap();

	let empty_file = rss.client.make_file_builder("empty.txt", &rss.dir).build();
	let empty_file = rss
		.client
		.upload_file(empty_file.into(), b"")
		.await
		.unwrap();

	db.update_dir_children(test_dir_path.clone()).await.unwrap();

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
	let (db, rss) = get_db_resources().await;
	let test_dir_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();

	// Create items with specific names for alphabetical testing
	rss.client
		.create_dir(&rss.dir, "zebra_dir".to_string())
		.await
		.unwrap();

	let alpha_file = rss.client.make_file_builder("alpha.txt", &rss.dir).build();
	rss.client
		.upload_file(alpha_file.into(), b"")
		.await
		.unwrap();

	let beta_file = rss.client.make_file_builder("beta.txt", &rss.dir).build();
	rss.client.upload_file(beta_file.into(), b"").await.unwrap();

	db.update_dir_children(test_dir_path.clone()).await.unwrap();

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
	let (db, rss) = get_db_resources().await;
	let test_dir_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();

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
	let file = rss.client.upload_file(file.into(), b"").await.unwrap();

	// Update to get both items
	db.update_dir_children(test_dir_path.clone()).await.unwrap();
	let resp = db
		.query_dir_children(&test_dir_path, None)
		.unwrap()
		.unwrap();
	assert_eq!(resp.objects.len(), 2);

	// Delete the directory
	rss.client.trash_dir(&dir).await.unwrap();
	db.update_dir_children(test_dir_path.clone()).await.unwrap();

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
	let (db, _rss) = get_db_resources().await;
	let nonexistent_path: FfiPathWithRoot = format!("{}/nonexistent_dir", db.root_uuid()).into();

	let result = db.query_dir_children(&nonexistent_path, None).unwrap();
	assert!(result.is_none());
}

// Item query tests
#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_query_item_file() {
	let (db, rss) = get_db_resources().await;

	let file = rss
		.client
		.make_file_builder("query_test.txt", &rss.dir)
		.build();
	let file = rss
		.client
		.upload_file(file.into(), b"Test content")
		.await
		.unwrap();

	let file_path: FfiPathWithRoot =
		format!("{}/{}/{}", db.root_uuid(), rss.dir.name(), file.name()).into();
	let dir_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();

	// Before update - should return None
	assert_eq!(db.query_item(&file_path).unwrap(), None);

	// After update - should return the file
	db.update_dir_children(dir_path).await.unwrap();
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
	let (db, rss) = get_db_resources().await;

	let dir = rss
		.client
		.create_dir(&rss.dir, "query_test_dir".to_string())
		.await
		.unwrap();

	let child_dir_path: FfiPathWithRoot =
		format!("{}/{}/{}", db.root_uuid(), rss.dir.name(), dir.name()).into();
	let parent_dir_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();

	// Before update - should return None
	assert_eq!(db.query_item(&child_dir_path).unwrap(), None);

	// After update - should return the directory
	db.update_dir_children(parent_dir_path).await.unwrap();
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
	let (db, rss) = get_db_resources().await;

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
	let (db, rss) = get_db_resources().await;

	let nonexistent_file_path: FfiPathWithRoot =
		format!("{}/{}/nonexistent.txt", db.root_uuid(), rss.dir.name()).into();

	let result = db.query_item(&nonexistent_file_path).unwrap();
	assert!(result.is_none());
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_query_item_invalid_path() {
	let (db, _rss) = get_db_resources().await;

	let invalid_path: FfiPathWithRoot = "not-a-uuid/invalid/path".into();
	let result = db.query_item(&invalid_path);
	assert!(result.is_err());
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_query_item_deeply_nested() {
	let (db, rss) = get_db_resources().await;

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
	let deep_file = rss
		.client
		.upload_file(deep_file.into(), b"Deep content")
		.await
		.unwrap();

	// Update each level
	let dir_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	let level1_path: FfiPathWithRoot = format!("{}/level1", dir_path.0).into();
	let level2_path: FfiPathWithRoot = format!("{}/level2", level1_path.0).into();

	db.update_dir_children(dir_path).await.unwrap();
	db.update_dir_children(level1_path).await.unwrap();
	db.update_dir_children(level2_path).await.unwrap();

	// Query the deep file
	let deep_file_path: FfiPathWithRoot = format!(
		"{}/{}/level1/level2/{}",
		db.root_uuid(),
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
		format!("{}/{}/level1", db.root_uuid(), rss.dir.name()).into();

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
	let (db, rss) = get_db_resources().await;

	// Create a test file with some content inside rss.dir
	let test_content = b"Hello, world! This is test content for download.";
	let file = rss
		.client
		.make_file_builder("test_download.txt", &rss.dir)
		.build();
	let remote_file = rss
		.client
		.upload_file(file.into(), test_content)
		.await
		.unwrap();

	let file_path: FfiPathWithRoot = format!(
		"{}/{}/{}",
		db.root_uuid(),
		rss.dir.name(),
		remote_file.name()
	)
	.into();

	// Test downloading the file
	let downloaded_path = db
		.download_file_if_changed(file_path.clone(), Arc::new(NoOpProgressCallback))
		.await
		.unwrap();

	// Verify the file was downloaded and contains correct content
	assert!(std::path::Path::new(&downloaded_path).exists());
	let downloaded_content = std::fs::read(&downloaded_path).unwrap();
	assert_eq!(downloaded_content, test_content);

	// Clean up
	std::fs::remove_file(&downloaded_path).ok();
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_download_file_nonexistent() {
	let (db, rss) = get_db_resources().await;

	let nonexistent_path: FfiPathWithRoot =
		format!("{}/{}/nonexistent_file.txt", db.root_uuid(), rss.dir.name()).into();

	// Should fail when trying to download a non-existent file
	let result = db
		.download_file_if_changed(nonexistent_path, Arc::new(NoOpProgressCallback))
		.await;
	assert!(result.is_err());
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_download_file_invalid_path() {
	let (db, rss) = get_db_resources().await;

	// Create a directory first
	let dir = rss
		.client
		.create_dir(&rss.dir, "test_dir".to_string())
		.await
		.unwrap();
	let dir_path: FfiPathWithRoot =
		format!("{}/{}/{}", db.root_uuid(), rss.dir.name(), dir.name()).into();

	// Should fail when trying to download a directory path as a file
	let result = db
		.download_file_if_changed(dir_path, Arc::new(NoOpProgressCallback))
		.await;
	assert!(result.is_err());
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_progress_callback() {
	let (db, rss) = get_db_resources().await;
	let mut contents = vec![0u8; 10 * 1024 * 1024];
	rand::rng().try_fill_bytes(&mut contents).unwrap();
	let parent_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name(),).into();
	let name = "test_progress.txt".to_string();
	let create_resp = db
		.create_empty_file(parent_path, name.clone(), "".into())
		.await
		.unwrap();
	std::fs::write(create_resp.path, &contents).unwrap();

	let upload_progress_callback = Arc::new(SumProgressCallback::default());
	db.upload_file_if_changed(create_resp.id.clone(), upload_progress_callback.clone())
		.await
		.unwrap();
	let count = upload_progress_callback
		.count
		.load(std::sync::atomic::Ordering::Relaxed);
	let max = upload_progress_callback
		.max
		.load(std::sync::atomic::Ordering::Relaxed);
	assert_eq!(count, max);
	assert_eq!(max, contents.len() as u64);

	db.clear_local_cache(create_resp.id.clone()).await.unwrap();

	let progress_callback = Arc::new(SumProgressCallback::default());

	let downloaded_path = db
		.download_file_if_changed(create_resp.id.clone(), progress_callback.clone())
		.await
		.unwrap();

	assert!(std::path::Path::new(&downloaded_path).exists());
	let downloaded_content = std::fs::read(&downloaded_path).unwrap();
	let count = progress_callback
		.count
		.load(std::sync::atomic::Ordering::Relaxed);
	let max = progress_callback
		.max
		.load(std::sync::atomic::Ordering::Relaxed);
	assert_eq!(count, max);
	assert_eq!(max, contents.len() as u64);
	assert_eq!(downloaded_content, contents);

	std::fs::remove_file(&downloaded_path).ok();
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_create_empty_file() {
	let (db, rss) = get_db_resources().await;

	let parent_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	let file_name = "empty_test.txt".to_string();
	let mime_type = "text/plain".to_string();

	// Create an empty file
	let create_resp = db
		.create_empty_file(parent_path.clone(), file_name.clone(), mime_type.clone())
		.await
		.unwrap();

	// Verify the file exists in the database
	let queried_file = db.query_item(&create_resp.id).unwrap();

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
	let (db, rss) = get_db_resources().await;

	let parent_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();

	let test_cases = vec![
		("test.json", "application/json"),
		("test.xml", "application/xml"),
		("test.md", "text/markdown"),
		("test.csv", "text/csv"),
	];

	for (filename, mime_type) in test_cases {
		let create_resp = db
			.create_empty_file(
				parent_path.clone(),
				filename.to_string(),
				mime_type.to_string(),
			)
			.await
			.unwrap();

		// Verify each file was created with correct MIME type
		let queried_file = db.query_item(&create_resp.id).unwrap();

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
	let (db, rss) = get_db_resources().await;

	let parent_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	let file_name = "duplicate.txt".to_string();
	let mime_type = "text/plain".to_string();

	// Create first file
	db.create_empty_file(parent_path.clone(), file_name.clone(), mime_type.clone())
		.await
		.unwrap();

	assert!(
		db.create_empty_file(parent_path.clone(), file_name.clone(), mime_type.clone(),)
			.await
			.is_ok()
	);
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_create_empty_file_invalid_parent() {
	let (db, rss) = get_db_resources().await;

	let invalid_parent_path: FfiPathWithRoot =
		format!("{}/{}/nonexistent_subdir", db.root_uuid(), rss.dir.name()).into();

	// Should fail with invalid parent path
	let result = db
		.create_empty_file(
			invalid_parent_path,
			"test.txt".to_string(),
			"text/plain".to_string(),
		)
		.await;

	assert!(result.is_err());
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_create_empty_file_in_root() {
	let (db, rss) = get_db_resources().await;

	// Create file in the test directory (rss.dir), not the absolute root
	let parent_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	let file_name = "root_file.txt".to_string();
	let mime_type = "text/plain".to_string();

	// Create file in test directory
	let create_resp = db
		.create_empty_file(parent_path.clone(), file_name.clone(), mime_type.clone())
		.await
		.unwrap();

	// Verify the file exists
	let queried_file = db.query_item(&create_resp.id).unwrap();

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
	let (db, rss) = get_db_resources().await;

	// Create a test file
	let file = rss
		.client
		.make_file_builder("trash_me.txt", &rss.dir)
		.build();
	let file = rss
		.client
		.upload_file(file.into(), b"This file will be trashed")
		.await
		.unwrap();

	let file_path: FfiPathWithRoot =
		format!("{}/{}/{}", db.root_uuid(), rss.dir.name(), file.name()).into();

	// Update the database to include the file
	let parent_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	db.update_dir_children(parent_path.clone()).await.unwrap();

	// Verify file exists before trashing
	let result = db.query_item(&file_path).unwrap();
	assert!(result.is_some());

	// Trash the file
	db.trash_item(file_path.clone()).await.unwrap();

	// Verify file is removed from database
	let result = db.query_item(&file_path).unwrap();
	assert!(result.is_none());

	// Verify file is no longer in parent directory listing
	db.update_dir_children(parent_path.clone()).await.unwrap();
	let children = db.query_dir_children(&parent_path, None).unwrap().unwrap();
	let file_exists = children
		.objects
		.iter()
		.any(|obj| matches!(obj, FfiNonRootObject::File(f) if f.uuid == file.uuid().to_string()));
	assert!(!file_exists);
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_trash_item_directory_success() {
	let (db, rss) = get_db_resources().await;

	// Create a test directory
	let dir = rss
		.client
		.create_dir(&rss.dir, "trash_this_dir".to_string())
		.await
		.unwrap();

	let dir_path: FfiPathWithRoot =
		format!("{}/{}/{}", db.root_uuid(), rss.dir.name(), dir.name()).into();

	// Update the database to include the directory
	let parent_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	db.update_dir_children(parent_path.clone()).await.unwrap();

	// Verify directory exists before trashing
	let result = db.query_item(&dir_path).unwrap();
	assert!(result.is_some());

	// Trash the directory
	db.trash_item(dir_path.clone()).await.unwrap();

	// Verify directory is removed from database
	let result = db.query_item(&dir_path).unwrap();
	assert!(result.is_none());

	// Verify directory is no longer in parent directory listing
	db.update_dir_children(parent_path.clone()).await.unwrap();
	let children = db.query_dir_children(&parent_path, None).unwrap().unwrap();
	let dir_exists = children
		.objects
		.iter()
		.any(|obj| matches!(obj, FfiNonRootObject::Dir(d) if d.uuid == dir.uuid().to_string()));
	assert!(!dir_exists);
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_trash_item_directory_with_contents() {
	let (db, rss) = get_db_resources().await;

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
	rss.client
		.upload_file(file_in_parent.into(), b"Content in parent")
		.await
		.unwrap();

	// Add a file to the subdirectory
	let file_in_sub = rss
		.client
		.make_file_builder("file_in_sub.txt", &sub_dir)
		.build();
	rss.client
		.upload_file(file_in_sub.into(), b"Content in subdirectory")
		.await
		.unwrap();

	// Update database with all the content
	let base_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	let parent_dir_path: FfiPathWithRoot = format!("{}/{}", base_path.0, parent_dir.name()).into();
	let sub_dir_path: FfiPathWithRoot = format!("{}/{}", parent_dir_path.0, sub_dir.name()).into();

	db.update_dir_children(base_path.clone()).await.unwrap();
	db.update_dir_children(parent_dir_path.clone())
		.await
		.unwrap();
	db.update_dir_children(sub_dir_path.clone()).await.unwrap();

	// Verify all content exists
	assert!(db.query_item(&parent_dir_path).unwrap().is_some());
	assert!(db.query_item(&sub_dir_path).unwrap().is_some());

	// Trash the parent directory (should remove everything)
	db.trash_item(parent_dir_path.clone()).await.unwrap();

	// Verify parent directory is gone
	assert!(db.query_item(&parent_dir_path).unwrap().is_none());

	// Verify subdirectory is also gone (cascading delete)
	assert!(db.query_item(&sub_dir_path).unwrap().is_none());

	// Verify parent directory is no longer in base directory listing
	db.update_dir_children(base_path.clone()).await.unwrap();
	let children = db.query_dir_children(&base_path, None).unwrap().unwrap();
	let parent_exists = children.objects.iter().any(
		|obj| matches!(obj, FfiNonRootObject::Dir(d) if d.uuid == parent_dir.uuid().to_string()),
	);
	assert!(!parent_exists);
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_trash_item_root_directory_error() {
	let (db, rss) = get_db_resources().await;

	// Attempt to trash the root directory
	let root_path: FfiPathWithRoot = db.root_uuid().into();
	let result = db.trash_item(root_path).await;

	// Should fail with appropriate error
	assert!(result.is_err());
	let error_message = format!("{}", result.unwrap_err());
	assert!(error_message.contains("Cannot remove root directory"));
	assert!(error_message.contains(&rss.client.root().uuid().to_string()));
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_trash_item_nonexistent_file() {
	let (db, rss) = get_db_resources().await;

	let nonexistent_path: FfiPathWithRoot =
		format!("{}/{}/nonexistent_file.txt", db.root_uuid(), rss.dir.name()).into();

	// Should fail when trying to trash a non-existent file
	let result = db.trash_item(nonexistent_path).await;
	assert!(result.is_err());
	let error_message = format!("{}", result.unwrap_err());
	assert!(error_message.contains("does not point to an item"));
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_trash_item_nonexistent_directory() {
	let (db, rss) = get_db_resources().await;

	let nonexistent_path: FfiPathWithRoot =
		format!("{}/{}/nonexistent_dir", db.root_uuid(), rss.dir.name()).into();

	// Should fail when trying to trash a non-existent directory
	let result = db.trash_item(nonexistent_path).await;
	assert!(result.is_err());
	let error_message = format!("{}", result.unwrap_err());
	assert!(error_message.contains("does not point to an item"));
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_trash_item_invalid_path() {
	let (db, _rss) = get_db_resources().await;

	let invalid_path: FfiPathWithRoot = "not-a-uuid/invalid/path".into();
	let result = db.trash_item(invalid_path).await;

	// Should fail with UUID parsing error
	assert!(result.is_err());
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_trash_item_partial_path() {
	let (db, rss) = get_db_resources().await;

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
	let base_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	db.update_dir_children(base_path).await.unwrap();

	// Try to trash level2 without having updated level1's children
	let level2_path: FfiPathWithRoot =
		format!("{}/{}/level1/level2", db.root_uuid(), rss.dir.name()).into();

	db.trash_item(level2_path).await.unwrap();
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_trash_item_file_then_query_parent() {
	let (db, rss) = get_db_resources().await;

	// Create multiple files in the same directory
	let file1 = rss
		.client
		.make_file_builder("keep_me.txt", &rss.dir)
		.build();
	let file1 = rss
		.client
		.upload_file(file1.into(), b"Keep this file")
		.await
		.unwrap();

	let file2 = rss
		.client
		.make_file_builder("trash_me.txt", &rss.dir)
		.build();
	let file2 = rss
		.client
		.upload_file(file2.into(), b"Trash this file")
		.await
		.unwrap();

	let parent_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	let file2_path: FfiPathWithRoot = format!("{}/{}", parent_path.0, file2.name()).into();

	// Update database
	db.update_dir_children(parent_path.clone()).await.unwrap();

	// Verify both files exist
	let children = db.query_dir_children(&parent_path, None).unwrap().unwrap();
	assert_eq!(children.objects.len(), 2);

	// Trash one file
	db.trash_item(file2_path).await.unwrap();

	// Update parent and verify only one file remains
	db.update_dir_children(parent_path.clone()).await.unwrap();
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
	let (db, rss) = get_db_resources().await;

	// Create an empty directory
	let empty_dir = rss
		.client
		.create_dir(&rss.dir, "empty_dir".to_string())
		.await
		.unwrap();

	let dir_path: FfiPathWithRoot =
		format!("{}/{}/{}", db.root_uuid(), rss.dir.name(), empty_dir.name()).into();

	let parent_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();

	// Update database
	db.update_dir_children(parent_path.clone()).await.unwrap();

	// Verify directory exists and is empty
	assert!(db.query_item(&dir_path).unwrap().is_some());
	let empty_children = db.query_dir_children(&dir_path, None).unwrap().unwrap();
	assert_eq!(empty_children.objects.len(), 0);

	// Trash the empty directory
	db.trash_item(dir_path.clone()).await.unwrap();

	// Verify it's gone
	assert!(db.query_item(&dir_path).unwrap().is_none());

	// Verify parent no longer contains it
	db.update_dir_children(parent_path.clone()).await.unwrap();
	let parent_children = db.query_dir_children(&parent_path, None).unwrap().unwrap();
	let dir_exists = parent_children.objects.iter().any(
		|obj| matches!(obj, FfiNonRootObject::Dir(d) if d.uuid == empty_dir.uuid().to_string()),
	);
	assert!(!dir_exists);
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_trash_item_already_trashed_file() {
	let (db, rss) = get_db_resources().await;

	// Create and trash a file using the SDK directly first
	let file = rss
		.client
		.make_file_builder("already_trashed.txt", &rss.dir)
		.build();
	let file = rss
		.client
		.upload_file(file.into(), b"This will be trashed twice")
		.await
		.unwrap();

	// Trash it directly via SDK
	rss.client.trash_file(&file).await.unwrap();

	let file_path: FfiPathWithRoot =
		format!("{}/{}/{}", db.root_uuid(), rss.dir.name(), file.name()).into();

	// Now try to trash it via our method - should fail since it doesn't exist in our DB
	let result = db.trash_item(file_path).await;
	assert!(result.is_err());
	let error_message = format!("{}", result.unwrap_err());
	assert!(error_message.contains("does not point to an item"));
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_move_item_file_success() {
	let (db, rss) = get_db_resources().await;

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
	let file = rss
		.client
		.upload_file(file.into(), b"Content to move")
		.await
		.unwrap();

	// Update database with all directories
	let base_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	let source_path: FfiPathWithRoot = format!("{}/{}", base_path.0, source_dir.name()).into();
	let dest_path: FfiPathWithRoot = format!("{}/{}", base_path.0, dest_dir.name()).into();

	db.update_dir_children(base_path).await.unwrap();
	db.update_dir_children(source_path.clone()).await.unwrap();
	db.update_dir_children(dest_path.clone()).await.unwrap();

	// Define paths for the move operation
	let file_path: FfiPathWithRoot = format!("{}/{}", source_path.0, file.name()).into();

	// Move the file
	let new_file_path = db
		.move_item(file_path.clone(), source_path.clone(), dest_path.clone())
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
	db.update_dir_children(source_path.clone()).await.unwrap();
	let source_children = db.query_dir_children(&source_path, None).unwrap().unwrap();
	let file_in_source = source_children
		.objects
		.iter()
		.any(|obj| matches!(obj, FfiNonRootObject::File(f) if f.uuid == file.uuid().to_string()));
	assert!(!file_in_source);

	// Verify destination directory contains the file
	db.update_dir_children(dest_path.clone()).await.unwrap();
	let dest_children = db.query_dir_children(&dest_path, None).unwrap().unwrap();
	let file_in_dest = dest_children
		.objects
		.iter()
		.any(|obj| matches!(obj, FfiNonRootObject::File(f) if f.uuid == file.uuid().to_string()));
	assert!(file_in_dest);
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_move_item_directory_success() {
	let (db, rss) = get_db_resources().await;

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
	rss.client
		.upload_file(file_in_move_dir.into(), b"Content in moved dir")
		.await
		.unwrap();

	// Update database with all directories
	let base_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	let source_path: FfiPathWithRoot = format!("{}/{}", base_path.0, source_dir.name()).into();
	let dest_path: FfiPathWithRoot = format!("{}/{}", base_path.0, dest_dir.name()).into();
	let move_dir_path: FfiPathWithRoot = format!("{}/{}", source_path.0, move_dir.name()).into();

	db.update_dir_children(base_path).await.unwrap();
	db.update_dir_children(source_path.clone()).await.unwrap();
	db.update_dir_children(dest_path.clone()).await.unwrap();
	db.update_dir_children(move_dir_path.clone()).await.unwrap();

	// Move the directory
	let new_dir_path = db
		.move_item(
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
	db.update_dir_children(source_path.clone()).await.unwrap();
	let source_children = db.query_dir_children(&source_path, None).unwrap().unwrap();
	let dir_in_source = source_children.objects.iter().any(
		|obj| matches!(obj, FfiNonRootObject::Dir(d) if d.uuid == move_dir.uuid().to_string()),
	);
	assert!(!dir_in_source);

	// Verify destination directory contains the moved directory
	db.update_dir_children(dest_path.clone()).await.unwrap();
	let dest_children = db.query_dir_children(&dest_path, None).unwrap().unwrap();
	let dir_in_dest = dest_children.objects.iter().any(
		|obj| matches!(obj, FfiNonRootObject::Dir(d) if d.uuid == move_dir.uuid().to_string()),
	);
	assert!(dir_in_dest);
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_move_item_nonexistent_item() {
	let (db, rss) = get_db_resources().await;

	// Create destination directory
	let dest_dir = rss
		.client
		.create_dir(&rss.dir, "dest_dir".to_string())
		.await
		.unwrap();

	let base_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	let dest_path: FfiPathWithRoot = format!("{}/{}", base_path.0, dest_dir.name()).into();
	let nonexistent_file_path: FfiPathWithRoot = format!("{}/nonexistent.txt", base_path.0).into();

	db.update_dir_children(base_path.clone()).await.unwrap();

	// Try to move non-existent file
	let result = db
		.move_item(nonexistent_file_path, base_path.clone(), dest_path)
		.await;

	assert!(result.is_err());
	let error_message = format!("{}", result.unwrap_err());
	assert!(error_message.contains("does not point to an item"));
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_move_item_nonexistent_destination() {
	let (db, rss) = get_db_resources().await;

	// Create a file to move
	let file = rss
		.client
		.make_file_builder("move_me.txt", &rss.dir)
		.build();
	let file = rss
		.client
		.upload_file(file.into(), b"Content")
		.await
		.unwrap();

	let base_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	let file_path: FfiPathWithRoot = format!("{}/{}", base_path.0, file.name()).into();
	let nonexistent_dest: FfiPathWithRoot = format!("{}/nonexistent_dir", base_path.0).into();

	db.update_dir_children(base_path.clone()).await.unwrap();

	// Try to move to non-existent destination
	let result = db
		.move_item(file_path, base_path.clone(), nonexistent_dest)
		.await;

	assert!(result.is_err());
	let error_message = format!("{}", result.unwrap_err());
	assert!(error_message.contains("does not point to an item"));
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_move_item_invalid_parent_path() {
	let (db, rss) = get_db_resources().await;

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
	let file = rss
		.client
		.upload_file(file.into(), b"Content")
		.await
		.unwrap();

	let base_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	let source_path: FfiPathWithRoot = format!("{}/{}", base_path.0, source_dir.name()).into();
	let dest_path: FfiPathWithRoot = format!("{}/{}", base_path.0, dest_dir.name()).into();
	let file_path: FfiPathWithRoot = format!("{}/{}", source_path.0, file.name()).into();
	let wrong_parent_path: FfiPathWithRoot = format!("{}/{}", base_path.0, dest_dir.name()).into(); // Wrong parent

	db.update_dir_children(base_path).await.unwrap();
	db.update_dir_children(source_path.clone()).await.unwrap();

	// Try to move with wrong parent path
	let result = db.move_item(file_path, wrong_parent_path, dest_path).await;

	assert!(result.is_err());
	let error_message = format!("{}", result.unwrap_err());
	assert!(error_message.contains("does not point to the parent"));
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_move_item_destination_is_file() {
	let (db, rss) = get_db_resources().await;

	// Create a file to move
	let move_file = rss
		.client
		.make_file_builder("move_me.txt", &rss.dir)
		.build();
	let move_file = rss
		.client
		.upload_file(move_file.into(), b"Content to move")
		.await
		.unwrap();

	// Create a file that will be used as invalid destination
	let dest_file = rss
		.client
		.make_file_builder("dest_file.txt", &rss.dir)
		.build();
	let dest_file = rss
		.client
		.upload_file(dest_file.into(), b"This is not a directory")
		.await
		.unwrap();

	let base_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	let move_file_path: FfiPathWithRoot = format!("{}/{}", base_path.0, move_file.name()).into();
	let dest_file_path: FfiPathWithRoot = format!("{}/{}", base_path.0, dest_file.name()).into();

	db.update_dir_children(base_path.clone()).await.unwrap();

	// Try to move to a file instead of directory
	let result = db
		.move_item(move_file_path, base_path, dest_file_path)
		.await;

	assert!(result.is_err());
	let error_message = format!("{}", result.unwrap_err());
	assert!(error_message.contains("does not point to a directory"));
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_move_item_root_directory_error() {
	let (db, rss) = get_db_resources().await;

	// Create destination directory
	let dest_dir = rss
		.client
		.create_dir(&rss.dir, "dest_dir".to_string())
		.await
		.unwrap();

	let root_path: FfiPathWithRoot = db.root_uuid().into();
	let base_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	let dest_path: FfiPathWithRoot = format!("{}/{}", base_path.0, dest_dir.name()).into();

	db.update_dir_children(base_path).await.unwrap();

	// Try to move root directory (should fail at conversion to DBNonRootObject)
	let result = db
		.move_item(root_path.clone(), root_path.clone(), dest_path)
		.await;

	assert!(result.is_err());
	let error_message = format!("{}", result.unwrap_err());
	assert!(error_message.contains("does not point to a non-root item"));
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_move_item_same_directory() {
	let (db, rss) = get_db_resources().await;

	// Create a file
	let file = rss
		.client
		.make_file_builder("stay_here.txt", &rss.dir)
		.build();
	let file = rss
		.client
		.upload_file(file.into(), b"Content")
		.await
		.unwrap();

	let base_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	let file_path: FfiPathWithRoot = format!("{}/{}", base_path.0, file.name()).into();

	db.update_dir_children(base_path.clone()).await.unwrap();

	// Move file to the same directory (should succeed)
	let new_file_path = db
		.move_item(file_path.clone(), base_path.clone(), base_path.clone())
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
	let (db, rss) = get_db_resources().await;

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
	let file = rss
		.client
		.upload_file(file.into(), b"Nested content")
		.await
		.unwrap();

	// Create destination directory at root level
	let dest_dir = rss
		.client
		.create_dir(&rss.dir, "destination".to_string())
		.await
		.unwrap();

	// Set up paths
	let base_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	let level1_path: FfiPathWithRoot = format!("{}/{}", base_path.0, level1.name()).into();
	let level2_path: FfiPathWithRoot = format!("{}/{}", level1_path.0, level2.name()).into();
	let file_path: FfiPathWithRoot = format!("{}/{}", level2_path.0, file.name()).into();
	let dest_path: FfiPathWithRoot = format!("{}/{}", base_path.0, dest_dir.name()).into();

	// Update all levels
	db.update_dir_children(base_path.clone()).await.unwrap();
	db.update_dir_children(level1_path).await.unwrap();
	db.update_dir_children(level2_path.clone()).await.unwrap();
	db.update_dir_children(dest_path.clone()).await.unwrap();

	// Move file from deep nested location to destination
	let new_file_path = db
		.move_item(file_path.clone(), level2_path.clone(), dest_path.clone())
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
	db.update_dir_children(level2_path.clone()).await.unwrap();
	let level2_children = db.query_dir_children(&level2_path, None).unwrap().unwrap();
	assert_eq!(level2_children.objects.len(), 0);

	// Verify destination directory contains the file
	db.update_dir_children(dest_path.clone()).await.unwrap();
	let dest_children = db.query_dir_children(&dest_path, None).unwrap().unwrap();
	let file_in_dest = dest_children
		.objects
		.iter()
		.any(|obj| matches!(obj, FfiNonRootObject::File(f) if f.uuid == file.uuid().to_string()));
	assert!(file_in_dest);
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_move_item_directory_with_contents() {
	let (db, rss) = get_db_resources().await;

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
	let file_in_source = rss
		.client
		.upload_file(file_in_source.into(), b"Source content")
		.await
		.unwrap();

	let file_in_sub = rss
		.client
		.make_file_builder("file_in_sub.txt", &sub_dir)
		.build();
	rss.client
		.upload_file(file_in_sub.into(), b"Sub content")
		.await
		.unwrap();

	// Create destination directory
	let dest_dir = rss
		.client
		.create_dir(&rss.dir, "destination".to_string())
		.await
		.unwrap();

	// Set up paths
	let base_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	let source_path: FfiPathWithRoot = format!("{}/{}", base_path.0, source_dir.name()).into();
	let dest_path: FfiPathWithRoot = format!("{}/{}", base_path.0, dest_dir.name()).into();

	// Update database
	db.update_dir_children(base_path.clone()).await.unwrap();
	db.update_dir_children(source_path.clone()).await.unwrap();
	db.update_dir_children(dest_path.clone()).await.unwrap();

	// Move the entire source directory to destination
	let new_source_path = db
		.move_item(source_path.clone(), base_path.clone(), dest_path.clone())
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
	db.update_dir_children(base_path.clone()).await.unwrap();
	let base_children = db.query_dir_children(&base_path, None).unwrap().unwrap();
	let old_source_in_base = base_children.objects.iter().any(
		|obj| matches!(obj, FfiNonRootObject::Dir(d) if d.uuid == source_dir.uuid().to_string()),
	);
	assert!(!old_source_in_base);

	// Verify destination contains the moved directory
	db.update_dir_children(dest_path.clone()).await.unwrap();
	let dest_children = db.query_dir_children(&dest_path, None).unwrap().unwrap();
	let source_in_dest = dest_children.objects.iter().any(
		|obj| matches!(obj, FfiNonRootObject::Dir(d) if d.uuid == source_dir.uuid().to_string()),
	);
	assert!(source_in_dest);

	// Verify contents are preserved (this tests that the move operation preserves the directory structure)
	db.update_dir_children(new_source_path.clone())
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
	let (db, rss) = get_db_resources().await;

	let dest_dir = rss
		.client
		.create_dir(&rss.dir, "dest_dir".to_string())
		.await
		.unwrap();

	let base_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	let dest_path: FfiPathWithRoot = format!("{}/{}", base_path.0, dest_dir.name()).into();
	let invalid_item_path: FfiPathWithRoot = "invalid-uuid/some/path".into();
	let invalid_parent_path: FfiPathWithRoot = "invalid-uuid/parent".into();

	// Try to move with invalid UUID in item path
	let result = db
		.move_item(invalid_item_path, invalid_parent_path, dest_path)
		.await;

	assert!(result.is_err());
	// Should fail with UUID parsing error
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_move_item_partial_path_resolution() {
	let (db, rss) = get_db_resources().await;

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
	let file = rss
		.client
		.upload_file(file.into(), b"Deep content")
		.await
		.unwrap();

	let dest_dir = rss
		.client
		.create_dir(&rss.dir, "destination".to_string())
		.await
		.unwrap();

	// Only update base level, not the nested levels
	let base_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	let level2_path: FfiPathWithRoot = format!("{}/level1/level2", base_path.0).into();
	let file_path: FfiPathWithRoot = format!("{}/deep_file.txt", level2_path.0).into();
	let dest_path: FfiPathWithRoot = format!("{}/{}", base_path.0, dest_dir.name()).into();

	db.update_dir_children(base_path.clone()).await.unwrap();

	// Move should work with partial path resolution (sync::update_items_in_path should handle this)
	let new_file_path = db
		.move_item(file_path.clone(), level2_path, dest_path.clone())
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
	let (db, rss) = get_db_resources().await;

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
	let file_to_move = rss
		.client
		.upload_file(file_to_move.into(), b"Content to move")
		.await
		.unwrap();

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
	rss.client
		.upload_file(existing_file.into(), b"Existing content")
		.await
		.unwrap();

	// Set up paths
	let base_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	let source_path: FfiPathWithRoot = format!("{}/{}", base_path.0, source_dir.name()).into();
	let dest_path: FfiPathWithRoot = format!("{}/{}", base_path.0, dest_dir.name()).into();
	let file_path: FfiPathWithRoot = format!("{}/{}", source_path.0, file_to_move.name()).into();

	// Update database
	db.update_dir_children(base_path).await.unwrap();
	db.update_dir_children(source_path.clone()).await.unwrap();
	db.update_dir_children(dest_path.clone()).await.unwrap();

	// Move should succeed (the SDK should handle name conflicts)
	let new_file_path = db
		.move_item(file_path.clone(), source_path.clone(), dest_path.clone())
		.await
		.unwrap();

	// The move operation should succeed - the SDK typically handles name conflicts
	// by either overwriting or creating a new name variant
	assert!(new_file_path.0.contains(&dest_path.0));

	// Verify file no longer exists in source
	assert!(db.query_item(&file_path).unwrap().is_none());

	// Verify some file exists at the new location (name might be modified by SDK)
	let moved_file = db.query_item(&new_file_path).unwrap();
	assert!(moved_file.is_some());
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_move_item_multiple_files_same_operation() {
	let (db, rss) = get_db_resources().await;

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
	let file1 = rss
		.client
		.upload_file(file1.into(), b"Content 1")
		.await
		.unwrap();

	let file2 = rss
		.client
		.make_file_builder("file2.txt", &source_dir)
		.build();
	let file2 = rss
		.client
		.upload_file(file2.into(), b"Content 2")
		.await
		.unwrap();

	// Create destination directory
	let dest_dir = rss
		.client
		.create_dir(&rss.dir, "multi_dest".to_string())
		.await
		.unwrap();

	// Set up paths
	let base_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	let source_path: FfiPathWithRoot = format!("{}/{}", base_path.0, source_dir.name()).into();
	let dest_path: FfiPathWithRoot = format!("{}/{}", base_path.0, dest_dir.name()).into();
	let file1_path: FfiPathWithRoot = format!("{}/{}", source_path.0, file1.name()).into();
	let file2_path: FfiPathWithRoot = format!("{}/{}", source_path.0, file2.name()).into();

	// Update database
	db.update_dir_children(base_path).await.unwrap();
	db.update_dir_children(source_path.clone()).await.unwrap();
	db.update_dir_children(dest_path.clone()).await.unwrap();

	// Move both files
	let new_file1_path = db
		.move_item(file1_path.clone(), source_path.clone(), dest_path.clone())
		.await
		.unwrap();

	let new_file2_path = db
		.move_item(file2_path.clone(), source_path.clone(), dest_path.clone())
		.await
		.unwrap();

	// Verify both files were moved
	assert!(db.query_item(&file1_path).unwrap().is_none());
	assert!(db.query_item(&file2_path).unwrap().is_none());

	assert!(db.query_item(&new_file1_path).unwrap().is_some());
	assert!(db.query_item(&new_file2_path).unwrap().is_some());

	// Verify source directory is now empty
	db.update_dir_children(source_path.clone()).await.unwrap();
	let source_children = db.query_dir_children(&source_path, None).unwrap().unwrap();
	assert_eq!(source_children.objects.len(), 0);

	// Verify destination directory contains both files
	db.update_dir_children(dest_path.clone()).await.unwrap();
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

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_rename_item_file_success() {
	let (db, rss) = get_db_resources().await;

	// Create a test file
	let file = rss
		.client
		.make_file_builder("old_name.txt", &rss.dir)
		.build();
	let file = rss
		.client
		.upload_file(file.into(), b"Content to rename")
		.await
		.unwrap();

	let file_path: FfiPathWithRoot =
		format!("{}/{}/{}", db.root_uuid(), rss.dir.name(), file.name()).into();

	// Update database
	let parent_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	db.update_dir_children(parent_path.clone()).await.unwrap();

	// Rename the file
	let new_name = "new_name.txt".to_string();
	let new_file_path = db
		.rename_item(file_path.clone(), new_name.clone())
		.await
		.unwrap()
		.unwrap();

	// Verify the new path is correct
	let expected_new_path: FfiPathWithRoot = format!("{}/{}", parent_path.0, new_name).into();
	assert_eq!(new_file_path.0, expected_new_path.0);

	// Verify old file path no longer exists
	assert!(db.query_item(&file_path).unwrap().is_none());

	// Verify file exists at new path with new name
	let renamed_file = db.query_item(&new_file_path).unwrap();
	assert!(renamed_file.is_some());
	match renamed_file.unwrap() {
		FfiObject::File(f) => {
			assert_eq!(f.name, new_name);
			assert_eq!(f.uuid, file.uuid().to_string());
		}
		_ => panic!("Expected file object"),
	}

	// Verify parent directory listing reflects the rename
	db.update_dir_children(parent_path.clone()).await.unwrap();
	let children = db.query_dir_children(&parent_path, None).unwrap().unwrap();

	let renamed_file_in_listing = children
		.objects
		.iter()
		.find(|obj| matches!(obj, FfiNonRootObject::File(f) if f.uuid == file.uuid().to_string()));
	assert!(renamed_file_in_listing.is_some());

	if let Some(FfiNonRootObject::File(f)) = renamed_file_in_listing {
		assert_eq!(f.name, new_name);
	}
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_rename_item_directory_success() {
	let (db, rss) = get_db_resources().await;

	// Create a test directory with some content
	let dir = rss
		.client
		.create_dir(&rss.dir, "old_dir_name".to_string())
		.await
		.unwrap();

	// Add a file to the directory to verify contents are preserved
	let file_in_dir = rss.client.make_file_builder("content.txt", &dir).build();
	let file_in_dir = rss
		.client
		.upload_file(file_in_dir.into(), b"Directory content")
		.await
		.unwrap();

	let dir_path: FfiPathWithRoot =
		format!("{}/{}/{}", db.root_uuid(), rss.dir.name(), dir.name()).into();

	// Update database
	let parent_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	db.update_dir_children(parent_path.clone()).await.unwrap();
	db.update_dir_children(dir_path.clone()).await.unwrap();

	// Rename the directory
	let new_name = "new_dir_name".to_string();
	let new_dir_path = db
		.rename_item(dir_path.clone(), new_name.clone())
		.await
		.unwrap()
		.unwrap();

	// Verify the new path is correct
	let expected_new_path: FfiPathWithRoot = format!("{}/{}", parent_path.0, new_name).into();
	assert_eq!(new_dir_path.0, expected_new_path.0);

	// Verify old directory path no longer exists
	assert!(db.query_item(&dir_path).unwrap().is_none());

	// Verify directory exists at new path with new name
	let renamed_dir = db.query_item(&new_dir_path).unwrap();
	assert!(renamed_dir.is_some());
	match renamed_dir.unwrap() {
		FfiObject::Dir(d) => {
			assert_eq!(d.name, new_name);
			assert_eq!(d.uuid, dir.uuid().to_string());
		}
		_ => panic!("Expected directory object"),
	}

	// Verify parent directory listing reflects the rename
	db.update_dir_children(parent_path.clone()).await.unwrap();
	let children = db.query_dir_children(&parent_path, None).unwrap().unwrap();

	let renamed_dir_in_listing = children
		.objects
		.iter()
		.find(|obj| matches!(obj, FfiNonRootObject::Dir(d) if d.uuid == dir.uuid().to_string()));
	assert!(renamed_dir_in_listing.is_some());

	if let Some(FfiNonRootObject::Dir(d)) = renamed_dir_in_listing {
		assert_eq!(d.name, new_name);
	}

	// Verify directory contents are preserved
	db.update_dir_children(new_dir_path.clone()).await.unwrap();
	let dir_contents = db.query_dir_children(&new_dir_path, None).unwrap().unwrap();
	assert_eq!(dir_contents.objects.len(), 1);

	let file_in_renamed_dir = dir_contents.objects.iter().find(
		|obj| matches!(obj, FfiNonRootObject::File(f) if f.uuid == file_in_dir.uuid().to_string()),
	);
	assert!(file_in_renamed_dir.is_some());
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_rename_item_file_extension_change() {
	let (db, rss) = get_db_resources().await;

	// Create a text file
	let file = rss
		.client
		.make_file_builder("document.txt", &rss.dir)
		.build();
	let file = rss
		.client
		.upload_file(file.into(), b"Text content")
		.await
		.unwrap();

	let file_path: FfiPathWithRoot =
		format!("{}/{}/{}", db.root_uuid(), rss.dir.name(), file.name()).into();

	let parent_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	db.update_dir_children(parent_path.clone()).await.unwrap();

	// Rename to change extension
	let new_name = "document.md".to_string();
	let new_file_path = db
		.rename_item(file_path.clone(), new_name.clone())
		.await
		.unwrap()
		.unwrap();

	// Verify rename worked
	let renamed_file = db.query_item(&new_file_path).unwrap();
	assert!(renamed_file.is_some());
	match renamed_file.unwrap() {
		FfiObject::File(f) => {
			assert_eq!(f.name, new_name);
			assert_eq!(f.uuid, file.uuid().to_string());
		}
		_ => panic!("Expected file object"),
	}
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_rename_item_same_name() {
	let (db, rss) = get_db_resources().await;

	// Create a test file
	let file = rss
		.client
		.make_file_builder("same_name.txt", &rss.dir)
		.build();
	let file = rss
		.client
		.upload_file(file.into(), b"Content")
		.await
		.unwrap();

	let file_path: FfiPathWithRoot =
		format!("{}/{}/{}", db.root_uuid(), rss.dir.name(), file.name()).into();

	let parent_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	db.update_dir_children(parent_path.clone()).await.unwrap();

	// Rename to the same name
	let same_name = file.name().to_string();
	let new_file_path = db
		.rename_item(file_path.clone(), same_name.clone())
		.await
		.unwrap();

	// Path should be the same
	assert_eq!(new_file_path, None);

	// File should still exist and be queryable
	let file_result = db.query_item(&file_path).unwrap();
	assert!(file_result.is_some());
	match file_result.unwrap() {
		FfiObject::File(f) => {
			assert_eq!(f.name, same_name);
			assert_eq!(f.uuid, file.uuid().to_string());
		}
		_ => panic!("Expected file object"),
	}
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_rename_item_nonexistent_file() {
	let (db, rss) = get_db_resources().await;

	let nonexistent_path: FfiPathWithRoot =
		format!("{}/{}/nonexistent.txt", db.root_uuid(), rss.dir.name()).into();

	let parent_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	db.update_dir_children(parent_path).await.unwrap();

	// Try to rename non-existent file
	let result = db
		.rename_item(nonexistent_path, "new_name.txt".to_string())
		.await;

	assert!(result.is_err());
	let error_message = format!("{}", result.unwrap_err());
	assert!(error_message.contains("does not point to an item"));
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_rename_item_root_directory_error() {
	let (db, _rss) = get_db_resources().await;

	let root_path: FfiPathWithRoot = db.root_uuid().into();

	// Try to rename root directory
	let result = db.rename_item(root_path, "new_root_name".to_string()).await;

	assert!(result.is_err());
	let error_message = format!("{}", result.unwrap_err());
	assert!(error_message.contains("Cannot rename item"));
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_rename_item_invalid_path() {
	let (db, _rss) = get_db_resources().await;

	let invalid_path: FfiPathWithRoot = "not-a-uuid/invalid/path".into();

	// Try to rename with invalid path
	let result = db
		.rename_item(invalid_path, "new_name.txt".to_string())
		.await;

	assert!(result.is_err());
	// Should fail with UUID parsing error
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_rename_item_empty_name() {
	let (db, rss) = get_db_resources().await;

	// Create a test file
	let file = rss
		.client
		.make_file_builder("test_file.txt", &rss.dir)
		.build();
	let file = rss
		.client
		.upload_file(file.into(), b"Content")
		.await
		.unwrap();

	let file_path: FfiPathWithRoot =
		format!("{}/{}/{}", db.root_uuid(), rss.dir.name(), file.name()).into();

	let parent_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	db.update_dir_children(parent_path).await.unwrap();

	// Try to rename to empty string
	let result = db.rename_item(file_path, "".to_string()).await;

	let err = result.unwrap_err();
	assert!(err.to_string().contains("Invalid Name ''"));
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_rename_item_special_characters() {
	let (db, rss) = get_db_resources().await;

	// Create a test file
	let file = rss
		.client
		.make_file_builder("normal_name.txt", &rss.dir)
		.build();
	let file = rss
		.client
		.upload_file(file.into(), b"Content")
		.await
		.unwrap();

	let file_path: FfiPathWithRoot =
		format!("{}/{}/{}", db.root_uuid(), rss.dir.name(), file.name()).into();

	let parent_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	db.update_dir_children(parent_path.clone()).await.unwrap();

	// Test various special characters
	let special_names = vec![
		"file with spaces.txt",
		"file-with-dashes.txt",
		"file_with_underscores.txt",
		"file.with.dots.txt",
		"file(with)parentheses.txt",
		"file[with]brackets.txt",
		"файл.txt", // Unicode characters
		"文件.txt", // Chinese characters
	];

	for special_name in special_names {
		// Try to rename to special name
		let result = db
			.rename_item(file_path.clone(), special_name.to_string())
			.await;

		if result.is_ok() {
			let new_path = result.unwrap().unwrap();
			let renamed_file = db.query_item(&new_path).unwrap();
			assert!(renamed_file.is_some());

			match renamed_file.unwrap() {
				FfiObject::File(f) => {
					assert_eq!(f.name, special_name);
					assert_eq!(f.uuid, file.uuid().to_string());
				}
				_ => panic!("Expected file object"),
			}

			// Reset for next test by renaming back
			let _ = db.rename_item(new_path, file.name().to_string()).await;
		} else {
			// Document which special characters are rejected
			panic!(
				"Special name '{}' rejected: {}",
				special_name,
				result.unwrap_err()
			);
		}
	}
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_rename_item_name_collision() {
	let (db, rss) = get_db_resources().await;

	// Create two files in the same directory
	let file1 = rss.client.make_file_builder("file1.txt", &rss.dir).build();
	let file1 = rss
		.client
		.upload_file(file1.into(), b"Content 1")
		.await
		.unwrap();

	let file2 = rss.client.make_file_builder("file2.txt", &rss.dir).build();
	let file2 = rss
		.client
		.upload_file(file2.into(), b"Content 2")
		.await
		.unwrap();

	let file1_path: FfiPathWithRoot =
		format!("{}/{}/{}", db.root_uuid(), rss.dir.name(), file1.name()).into();

	let parent_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	db.update_dir_children(parent_path.clone()).await.unwrap();

	// Try to rename file1 to file2's name (collision)
	let result = db.rename_item(file1_path, file2.name().to_string()).await;

	assert!(result.is_err());
	assert!(
		result
			.unwrap_err()
			.to_string()
			.contains("File with the same name already exists at destination")
	);
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_rename_item_nested_file() {
	let (db, rss) = get_db_resources().await;

	// Create nested directory structure
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

	let nested_file = rss
		.client
		.make_file_builder("nested_file.txt", &level2)
		.build();
	let nested_file = rss
		.client
		.upload_file(nested_file.into(), b"Nested content")
		.await
		.unwrap();

	// Set up paths
	let base_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	let level1_path: FfiPathWithRoot = format!("{}/level1", base_path.0).into();
	let level2_path: FfiPathWithRoot = format!("{}/level2", level1_path.0).into();
	let file_path: FfiPathWithRoot = format!("{}/{}", level2_path.0, nested_file.name()).into();

	// Update all levels
	db.update_dir_children(base_path).await.unwrap();
	db.update_dir_children(level1_path).await.unwrap();
	db.update_dir_children(level2_path.clone()).await.unwrap();

	// Rename the nested file
	let new_name = "renamed_nested_file.txt".to_string();
	let new_file_path = db
		.rename_item(file_path.clone(), new_name.clone())
		.await
		.unwrap()
		.unwrap();

	// Verify the new path is correct
	let expected_new_path: FfiPathWithRoot = format!("{}/{}", level2_path.0, new_name).into();
	assert_eq!(new_file_path.0, expected_new_path.0);

	// Verify old path no longer exists
	assert!(db.query_item(&file_path).unwrap().is_none());

	// Verify file exists at new path
	let renamed_file = db.query_item(&new_file_path).unwrap();
	assert!(renamed_file.is_some());
	match renamed_file.unwrap() {
		FfiObject::File(f) => {
			assert_eq!(f.name, new_name);
			assert_eq!(f.uuid, nested_file.uuid().to_string());
		}
		_ => panic!("Expected file object"),
	}
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_rename_item_long_name() {
	let (db, rss) = get_db_resources().await;

	// Create a test file
	let file = rss.client.make_file_builder("short.txt", &rss.dir).build();
	let file = rss
		.client
		.upload_file(file.into(), b"Content")
		.await
		.unwrap();

	let file_path: FfiPathWithRoot =
		format!("{}/{}/{}", db.root_uuid(), rss.dir.name(), file.name()).into();

	let parent_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	db.update_dir_children(parent_path).await.unwrap();

	// Try to rename to a very long name
	let long_name = "a".repeat(255) + ".txt"; // 255 'a' characters plus extension
	let result = db.rename_item(file_path, long_name.clone()).await;

	let new_path = result.unwrap().unwrap();
	let renamed_file = db.query_item(&new_path).unwrap();
	assert!(renamed_file.is_some());

	match renamed_file.unwrap() {
		FfiObject::File(f) => {
			// Name might be truncated by the system
			assert!(!f.name.is_empty());
			assert_eq!(f.uuid, file.uuid().to_string());
		}
		_ => panic!("Expected file object"),
	}
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_rename_item_multiple_renames() {
	let (db, rss) = get_db_resources().await;

	// Create a test file
	let file = rss
		.client
		.make_file_builder("original.txt", &rss.dir)
		.build();
	let file = rss
		.client
		.upload_file(file.into(), b"Content")
		.await
		.unwrap();

	let mut current_path: FfiPathWithRoot =
		format!("{}/{}/{}", db.root_uuid(), rss.dir.name(), file.name()).into();

	let parent_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	db.update_dir_children(parent_path.clone()).await.unwrap();

	// Perform multiple renames in sequence
	let names = vec!["first_rename.txt", "second_rename.txt", "final_name.txt"];

	for name in names {
		let new_path = db
			.rename_item(current_path.clone(), name.to_string())
			.await
			.unwrap()
			.unwrap();

		// Verify old path no longer exists
		assert!(db.query_item(&current_path).unwrap().is_none());

		// Verify new path exists
		let renamed_file = db.query_item(&new_path).unwrap();
		assert!(renamed_file.is_some());
		match renamed_file.unwrap() {
			FfiObject::File(f) => {
				assert_eq!(f.name, name);
				assert_eq!(f.uuid, file.uuid().to_string());
			}
			_ => panic!("Expected file object"),
		}

		// Update current path for next iteration
		current_path = new_path;
	}

	// Verify final state in parent directory
	db.update_dir_children(parent_path.clone()).await.unwrap();
	let children = db.query_dir_children(&parent_path, None).unwrap().unwrap();

	let final_file = children
		.objects
		.iter()
		.find(|obj| matches!(obj, FfiNonRootObject::File(f) if f.uuid == file.uuid().to_string()));
	assert!(final_file.is_some());

	if let Some(FfiNonRootObject::File(f)) = final_file {
		assert_eq!(f.name, "final_name.txt");
	}
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_get_all_descendant_paths_empty_directory() {
	let (db, rss) = get_db_resources().await;

	// Create an empty directory
	let empty_dir = rss
		.client
		.create_dir(&rss.dir, "empty_dir".to_string())
		.await
		.unwrap();

	let dir_path: FfiPathWithRoot =
		format!("{}/{}/{}", db.root_uuid(), rss.dir.name(), empty_dir.name()).into();

	// Update database to include the directory
	let parent_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	db.update_dir_children(parent_path).await.unwrap();
	db.update_dir_children(dir_path.clone()).await.unwrap();

	// Get descendant paths - should be empty
	let descendant_paths = db.get_all_descendant_paths(&dir_path).unwrap();
	assert_eq!(descendant_paths.len(), 0);
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_get_all_descendant_paths_files_only() {
	let (db, rss) = get_db_resources().await;

	// Create a directory with multiple files
	let test_dir = rss
		.client
		.create_dir(&rss.dir, "files_dir".to_string())
		.await
		.unwrap();

	// Create several files
	let file1 = rss.client.make_file_builder("file1.txt", &test_dir).build();
	let file1 = rss
		.client
		.upload_file(file1.into(), b"Content 1")
		.await
		.unwrap();

	let file2 = rss.client.make_file_builder("file2.txt", &test_dir).build();
	let file2 = rss
		.client
		.upload_file(file2.into(), b"Content 2")
		.await
		.unwrap();

	let file3 = rss.client.make_file_builder("file3.md", &test_dir).build();
	let file3 = rss
		.client
		.upload_file(file3.into(), b"Markdown content")
		.await
		.unwrap();

	let dir_path: FfiPathWithRoot =
		format!("{}/{}/{}", db.root_uuid(), rss.dir.name(), test_dir.name()).into();

	// Update database
	let parent_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	db.update_dir_children(parent_path).await.unwrap();
	db.update_dir_children(dir_path.clone()).await.unwrap();

	// Get descendant paths
	let descendant_paths = db.get_all_descendant_paths(&dir_path).unwrap();
	assert_eq!(descendant_paths.len(), 3);

	// Verify all file paths are present
	let expected_paths = vec![
		format!("{}/{}", dir_path.0, file1.name()),
		format!("{}/{}", dir_path.0, file2.name()),
		format!("{}/{}", dir_path.0, file3.name()),
	];

	for expected_path in expected_paths {
		let found = descendant_paths.iter().any(|p| p.0 == expected_path);
		assert!(
			found,
			"Expected path {} not found in descendant paths",
			expected_path
		);
	}
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_get_all_descendant_paths_directories_only() {
	let (db, rss) = get_db_resources().await;

	// Create a directory with subdirectories
	let test_dir = rss
		.client
		.create_dir(&rss.dir, "dirs_dir".to_string())
		.await
		.unwrap();

	let subdir1 = rss
		.client
		.create_dir(&test_dir, "subdir1".to_string())
		.await
		.unwrap();

	let subdir2 = rss
		.client
		.create_dir(&test_dir, "subdir2".to_string())
		.await
		.unwrap();

	let subdir3 = rss
		.client
		.create_dir(&test_dir, "subdir3".to_string())
		.await
		.unwrap();

	let dir_path: FfiPathWithRoot =
		format!("{}/{}/{}", db.root_uuid(), rss.dir.name(), test_dir.name()).into();

	// Update database
	let parent_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	db.update_dir_children(parent_path).await.unwrap();
	db.update_dir_children(dir_path.clone()).await.unwrap();

	// Get descendant paths
	let descendant_paths = db.get_all_descendant_paths(&dir_path).unwrap();
	assert_eq!(descendant_paths.len(), 3);

	// Verify all directory paths are present
	let expected_paths = vec![
		format!("{}/{}", dir_path.0, subdir1.name()),
		format!("{}/{}", dir_path.0, subdir2.name()),
		format!("{}/{}", dir_path.0, subdir3.name()),
	];

	for expected_path in expected_paths {
		let found = descendant_paths.iter().any(|p| p.0 == expected_path);
		assert!(
			found,
			"Expected path {} not found in descendant paths",
			expected_path
		);
	}
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_get_all_descendant_paths_mixed_content() {
	let (db, rss) = get_db_resources().await;

	// Create a directory with mixed files and subdirectories
	let test_dir = rss
		.client
		.create_dir(&rss.dir, "mixed_dir".to_string())
		.await
		.unwrap();

	// Create files
	let file1 = rss
		.client
		.make_file_builder("readme.txt", &test_dir)
		.build();
	let file1 = rss
		.client
		.upload_file(file1.into(), b"Readme content")
		.await
		.unwrap();

	// Create subdirectory
	let subdir = rss
		.client
		.create_dir(&test_dir, "subfolder".to_string())
		.await
		.unwrap();

	// Create another file
	let file2 = rss
		.client
		.make_file_builder("config.json", &test_dir)
		.build();
	let file2 = rss.client.upload_file(file2.into(), b"{}").await.unwrap();

	let dir_path: FfiPathWithRoot =
		format!("{}/{}/{}", db.root_uuid(), rss.dir.name(), test_dir.name()).into();

	// Update database
	let parent_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	db.update_dir_children(parent_path).await.unwrap();
	db.update_dir_children(dir_path.clone()).await.unwrap();

	// Get descendant paths
	let descendant_paths = db.get_all_descendant_paths(&dir_path).unwrap();
	assert_eq!(descendant_paths.len(), 3);

	// Verify all paths are present
	let expected_paths = vec![
		format!("{}/{}", dir_path.0, file1.name()),
		format!("{}/{}", dir_path.0, subdir.name()),
		format!("{}/{}", dir_path.0, file2.name()),
	];

	for expected_path in expected_paths {
		let found = descendant_paths.iter().any(|p| p.0 == expected_path);
		assert!(
			found,
			"Expected path {} not found in descendant paths",
			expected_path
		);
	}
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_get_all_descendant_paths_nested_structure() {
	let (db, rss) = get_db_resources().await;

	// Create a deeply nested structure
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

	let level3 = rss
		.client
		.create_dir(&level2, "level3".to_string())
		.await
		.unwrap();

	// Add files at different levels
	let file_l1 = rss
		.client
		.make_file_builder("file_level1.txt", &level1)
		.build();
	let file_l1 = rss
		.client
		.upload_file(file_l1.into(), b"Level 1 content")
		.await
		.unwrap();

	let file_l2 = rss
		.client
		.make_file_builder("file_level2.txt", &level2)
		.build();
	let file_l2 = rss
		.client
		.upload_file(file_l2.into(), b"Level 2 content")
		.await
		.unwrap();

	let file_l3 = rss
		.client
		.make_file_builder("file_level3.txt", &level3)
		.build();
	let file_l3 = rss
		.client
		.upload_file(file_l3.into(), b"Level 3 content")
		.await
		.unwrap();

	// Set up paths
	let base_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	let level1_path: FfiPathWithRoot = format!("{}/{}", base_path.0, level1.name()).into();
	let level2_path: FfiPathWithRoot = format!("{}/{}", level1_path.0, level2.name()).into();
	let level3_path: FfiPathWithRoot = format!("{}/{}", level2_path.0, level3.name()).into();

	// Update all levels in database
	db.update_dir_children(base_path).await.unwrap();
	db.update_dir_children(level1_path.clone()).await.unwrap();
	db.update_dir_children(level2_path.clone()).await.unwrap();
	db.update_dir_children(level3_path).await.unwrap();

	// Get descendant paths from level1
	let descendant_paths = db.get_all_descendant_paths(&level1_path).unwrap();

	// Should include: level2 dir, file_l1, level3 dir, file_l2, file_l3
	assert_eq!(descendant_paths.len(), 5);

	// Verify all expected paths are present
	let expected_paths = vec![
		format!("{}/{}", level1_path.0, file_l1.name()), // Direct file in level1
		format!("{}/{}", level1_path.0, level2.name()),  // level2 directory
		format!("{}/{}/{}", level1_path.0, level2.name(), file_l2.name()), // File in level2
		format!("{}/{}/{}", level1_path.0, level2.name(), level3.name()), // level3 directory
		format!(
			"{}/{}/{}/{}",
			level1_path.0,
			level2.name(),
			level3.name(),
			file_l3.name()
		), // File in level3
	];

	for expected_path in &expected_paths {
		let found = descendant_paths.iter().any(|p| &p.0 == expected_path);
		assert!(
			found,
			"Expected path {} not found in descendant paths.\nActual paths: {:#?}",
			expected_path,
			descendant_paths.iter().map(|p| &p.0).collect::<Vec<_>>()
		);
	}
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_get_all_descendant_paths_complex_nested_structure() {
	let (db, rss) = get_db_resources().await;

	// Create a complex structure with multiple branches
	let root_dir = rss
		.client
		.create_dir(&rss.dir, "complex_root".to_string())
		.await
		.unwrap();

	// Branch 1: documents
	let docs_dir = rss
		.client
		.create_dir(&root_dir, "documents".to_string())
		.await
		.unwrap();

	let doc_file = rss.client.make_file_builder("readme.md", &docs_dir).build();
	let doc_file = rss
		.client
		.upload_file(doc_file.into(), b"Documentation")
		.await
		.unwrap();

	// Branch 2: images with subdirectories
	let images_dir = rss
		.client
		.create_dir(&root_dir, "images".to_string())
		.await
		.unwrap();

	let thumbnails_dir = rss
		.client
		.create_dir(&images_dir, "thumbnails".to_string())
		.await
		.unwrap();

	let thumb_file = rss
		.client
		.make_file_builder("thumb1.jpg", &thumbnails_dir)
		.build();
	let thumb_file = rss
		.client
		.upload_file(thumb_file.into(), b"thumbnail data")
		.await
		.unwrap();

	let full_image = rss
		.client
		.make_file_builder("photo.png", &images_dir)
		.build();
	let full_image = rss
		.client
		.upload_file(full_image.into(), b"image data")
		.await
		.unwrap();

	// Branch 3: config file at root level
	let config_file = rss
		.client
		.make_file_builder("config.json", &root_dir)
		.build();
	let config_file = rss
		.client
		.upload_file(config_file.into(), b"{}")
		.await
		.unwrap();

	// Set up paths
	let base_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	let root_path: FfiPathWithRoot = format!("{}/{}", base_path.0, root_dir.name()).into();
	let docs_path: FfiPathWithRoot = format!("{}/{}", root_path.0, docs_dir.name()).into();
	let images_path: FfiPathWithRoot = format!("{}/{}", root_path.0, images_dir.name()).into();
	let thumbnails_path: FfiPathWithRoot =
		format!("{}/{}", images_path.0, thumbnails_dir.name()).into();

	// Update all directories in database
	db.update_dir_children(base_path).await.unwrap();
	db.update_dir_children(root_path.clone()).await.unwrap();
	db.update_dir_children(docs_path).await.unwrap();
	db.update_dir_children(images_path).await.unwrap();
	db.update_dir_children(thumbnails_path).await.unwrap();

	// Get all descendant paths from root
	let descendant_paths = db.get_all_descendant_paths(&root_path).unwrap();

	// Should include: config.json, documents/, readme.md, images/, photo.png, thumbnails/, thumb1.jpg
	assert_eq!(descendant_paths.len(), 7);

	// Build expected paths
	let expected_paths = vec![
		format!("{}/{}", root_path.0, config_file.name()),
		format!("{}/{}", root_path.0, docs_dir.name()),
		format!("{}/{}/{}", root_path.0, docs_dir.name(), doc_file.name()),
		format!("{}/{}", root_path.0, images_dir.name()),
		format!(
			"{}/{}/{}",
			root_path.0,
			images_dir.name(),
			full_image.name()
		),
		format!(
			"{}/{}/{}",
			root_path.0,
			images_dir.name(),
			thumbnails_dir.name()
		),
		format!(
			"{}/{}/{}/{}",
			root_path.0,
			images_dir.name(),
			thumbnails_dir.name(),
			thumb_file.name()
		),
	];

	for expected_path in &expected_paths {
		let found = descendant_paths.iter().any(|p| &p.0 == expected_path);
		assert!(
			found,
			"Expected path {} not found in descendant paths.\nActual paths: {:#?}",
			expected_path,
			descendant_paths.iter().map(|p| &p.0).collect::<Vec<_>>()
		);
	}
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_get_all_descendant_paths_nonexistent_path() {
	let (db, rss) = get_db_resources().await;

	let nonexistent_path: FfiPathWithRoot =
		format!("{}/{}/nonexistent_dir", db.root_uuid(), rss.dir.name()).into();

	// Get descendant paths for non-existent directory
	let descendant_paths = db.get_all_descendant_paths(&nonexistent_path).unwrap();

	// Should return empty vector for non-existent path
	assert_eq!(descendant_paths.len(), 0);
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_get_all_descendant_paths_invalid_path() {
	let (db, _rss) = get_db_resources().await;

	let invalid_path: FfiPathWithRoot = "not-a-uuid/invalid/path".into();

	// Should fail with UUID parsing error
	let result = db.get_all_descendant_paths(&invalid_path);
	assert!(result.is_err());
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_get_all_descendant_paths_file_path() {
	let (db, rss) = get_db_resources().await;

	// Create a file
	let file = rss
		.client
		.make_file_builder("test_file.txt", &rss.dir)
		.build();
	let file = rss
		.client
		.upload_file(file.into(), b"Test content")
		.await
		.unwrap();

	let file_path: FfiPathWithRoot =
		format!("{}/{}/{}", db.root_uuid(), rss.dir.name(), file.name()).into();

	// Update database
	let parent_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	db.update_dir_children(parent_path).await.unwrap();

	// Get descendant paths for a file (files have no descendants)
	let descendant_paths = db.get_all_descendant_paths(&file_path).unwrap();

	// Should return empty vector for file paths
	assert_eq!(descendant_paths.len(), 0);
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_get_all_descendant_paths_root_directory() {
	let (db, rss) = get_db_resources().await;

	// Create some content in the test root directory
	let file_in_root = rss
		.client
		.make_file_builder("root_file.txt", &rss.dir)
		.build();
	let file_in_root = rss
		.client
		.upload_file(file_in_root.into(), b"Root content")
		.await
		.unwrap();

	let subdir_in_root = rss
		.client
		.create_dir(&rss.dir, "root_subdir".to_string())
		.await
		.unwrap();

	// Use the test directory as root (since we can't access the absolute root easily)
	let root_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();

	// Update database
	let parent_path: FfiPathWithRoot = db.root_uuid().into();
	db.update_dir_children(parent_path).await.unwrap();
	db.update_dir_children(root_path.clone()).await.unwrap();

	// Get descendant paths from our test "root"
	let descendant_paths = db.get_all_descendant_paths(&root_path).unwrap();

	// Should include both the file and directory
	assert_eq!(descendant_paths.len(), 2);

	println!("Descendant paths: {:?}", descendant_paths);

	let expected_paths = vec![
		format!("{}/{}", root_path.0, file_in_root.name()),
		format!("{}/{}", root_path.0, subdir_in_root.name()),
	];

	for expected_path in expected_paths {
		let found = descendant_paths.iter().any(|p| p.0 == expected_path);
		assert!(
			found,
			"Expected path {} not found in descendant paths",
			expected_path
		);
	}
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_get_all_descendant_paths_partial_database_state() {
	let (db, rss) = get_db_resources().await;

	// Create nested structure but only update some levels in database
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

	let file_l2 = rss
		.client
		.make_file_builder("file_level2.txt", &level2)
		.build();
	rss.client
		.upload_file(file_l2.into(), b"Level 2 content")
		.await
		.unwrap();

	// Only update the base and level1, not level2
	let base_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	let level1_path: FfiPathWithRoot = format!("{}/{}", base_path.0, level1.name()).into();

	db.update_dir_children(base_path).await.unwrap();
	db.update_dir_children(level1_path.clone()).await.unwrap();
	// Note: NOT updating level2 contents

	// Get descendant paths from level1
	let descendant_paths = db.get_all_descendant_paths(&level1_path).unwrap();

	// Should only include level2 directory, not its contents since they're not in database
	assert_eq!(descendant_paths.len(), 1);

	let expected_level2_path = format!("{}/{}", level1_path.0, level2.name());
	assert_eq!(descendant_paths[0].0, expected_level2_path);
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_get_all_descendant_paths_special_characters_in_names() {
	let (db, rss) = get_db_resources().await;

	// Create items with special characters in names
	let special_dir = rss
		.client
		.create_dir(&rss.dir, "dir with spaces".to_string())
		.await
		.unwrap();

	let special_file1 = rss
		.client
		.make_file_builder("file-with-dashes.txt", &special_dir)
		.build();
	let special_file1 = rss
		.client
		.upload_file(special_file1.into(), b"Content 1")
		.await
		.unwrap();

	let special_file2 = rss
		.client
		.make_file_builder("file_with_underscores.txt", &special_dir)
		.build();
	let special_file2 = rss
		.client
		.upload_file(special_file2.into(), b"Content 2")
		.await
		.unwrap();

	let unicode_file = rss
		.client
		.make_file_builder("файл.txt", &special_dir)
		.build();
	let unicode_file = rss
		.client
		.upload_file(unicode_file.into(), b"Unicode content")
		.await
		.unwrap();

	let dir_path: FfiPathWithRoot = format!(
		"{}/{}/{}",
		db.root_uuid(),
		rss.dir.name(),
		special_dir.name()
	)
	.into();

	// Update database
	let parent_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	db.update_dir_children(parent_path).await.unwrap();
	db.update_dir_children(dir_path.clone()).await.unwrap();

	// Get descendant paths
	let descendant_paths = db.get_all_descendant_paths(&dir_path).unwrap();
	assert_eq!(descendant_paths.len(), 3);

	// Verify all special-named files are present
	let expected_paths = vec![
		format!("{}/{}", dir_path.0, special_file1.name()),
		format!("{}/{}", dir_path.0, special_file2.name()),
		format!("{}/{}", dir_path.0, unicode_file.name()),
	];

	for expected_path in expected_paths {
		let found = descendant_paths.iter().any(|p| p.0 == expected_path);
		assert!(
			found,
			"Expected path {} not found in descendant paths",
			expected_path
		);
	}
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_get_all_descendant_paths_path_ordering() {
	let (db, rss) = get_db_resources().await;

	// Create structure to test path ordering
	let test_dir = rss
		.client
		.create_dir(&rss.dir, "ordered_test".to_string())
		.await
		.unwrap();

	// Create items in a specific order to see how paths are returned
	let b_file = rss
		.client
		.make_file_builder("b_file.txt", &test_dir)
		.build();
	let b_file = rss
		.client
		.upload_file(b_file.into(), b"B content")
		.await
		.unwrap();

	let a_dir = rss
		.client
		.create_dir(&test_dir, "a_directory".to_string())
		.await
		.unwrap();

	let c_file = rss
		.client
		.make_file_builder("c_file.txt", &test_dir)
		.build();
	let c_file = rss
		.client
		.upload_file(c_file.into(), b"C content")
		.await
		.unwrap();

	// Add file to subdirectory
	let nested_file = rss.client.make_file_builder("nested.txt", &a_dir).build();
	let nested_file = rss
		.client
		.upload_file(nested_file.into(), b"Nested content")
		.await
		.unwrap();

	let dir_path: FfiPathWithRoot =
		format!("{}/{}/{}", db.root_uuid(), rss.dir.name(), test_dir.name()).into();

	// Update database
	let parent_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	let a_dir_path: FfiPathWithRoot = format!("{}/{}", dir_path.0, a_dir.name()).into();

	db.update_dir_children(parent_path).await.unwrap();
	db.update_dir_children(dir_path.clone()).await.unwrap();
	db.update_dir_children(a_dir_path).await.unwrap();

	// Get descendant paths
	let descendant_paths = db.get_all_descendant_paths(&dir_path).unwrap();
	assert_eq!(descendant_paths.len(), 4);

	// Verify all items are present (order may vary based on SQL query order)
	let expected_paths = vec![
		format!("{}/{}", dir_path.0, b_file.name()),
		format!("{}/{}", dir_path.0, a_dir.name()),
		format!("{}/{}", dir_path.0, c_file.name()),
		format!("{}/{}/{}", dir_path.0, a_dir.name(), nested_file.name()),
	];

	for expected_path in expected_paths {
		let found = descendant_paths.iter().any(|p| p.0 == expected_path);
		assert!(
			found,
			"Expected path {} not found in descendant paths",
			expected_path
		);
	}

	// Verify that all paths start with the correct base path
	for path in &descendant_paths {
		assert!(
			path.0.starts_with(&dir_path.0),
			"Path {} should start with base path {}",
			path.0,
			dir_path.0
		);
	}
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_get_all_descendant_paths_large_directory() {
	let (db, rss) = get_db_resources().await;

	// Create a directory with many files to test performance and correctness
	let large_dir = rss
		.client
		.create_dir(&rss.dir, "large_directory".to_string())
		.await
		.unwrap();

	let mut created_files = Vec::new();

	// Create 20 files
	for i in 0..20 {
		let file = rss
			.client
			.make_file_builder(format!("file_{:02}.txt", i), &large_dir)
			.build();
		let file = rss
			.client
			.upload_file(file.into(), format!("Content {}", i).as_bytes())
			.await
			.unwrap();
		created_files.push(file);
	}

	// Create a few subdirectories
	let subdir1 = rss
		.client
		.create_dir(&large_dir, "subdir_01".to_string())
		.await
		.unwrap();

	let subdir2 = rss
		.client
		.create_dir(&large_dir, "subdir_02".to_string())
		.await
		.unwrap();

	// Add files to subdirectories
	let nested_file1 = rss
		.client
		.make_file_builder("nested_1.txt", &subdir1)
		.build();
	let nested_file1 = rss
		.client
		.upload_file(nested_file1.into(), b"Nested content 1")
		.await
		.unwrap();

	let nested_file2 = rss
		.client
		.make_file_builder("nested_2.txt", &subdir2)
		.build();
	let nested_file2 = rss
		.client
		.upload_file(nested_file2.into(), b"Nested content 2")
		.await
		.unwrap();

	let dir_path: FfiPathWithRoot =
		format!("{}/{}/{}", db.root_uuid(), rss.dir.name(), large_dir.name()).into();

	// Update database
	let parent_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	let subdir1_path: FfiPathWithRoot = format!("{}/{}", dir_path.0, subdir1.name()).into();
	let subdir2_path: FfiPathWithRoot = format!("{}/{}", dir_path.0, subdir2.name()).into();

	db.update_dir_children(parent_path).await.unwrap();
	db.update_dir_children(dir_path.clone()).await.unwrap();
	db.update_dir_children(subdir1_path).await.unwrap();
	db.update_dir_children(subdir2_path).await.unwrap();

	// Get descendant paths
	let descendant_paths = db.get_all_descendant_paths(&dir_path).unwrap();

	// Should have: 20 files + 2 subdirs + 2 nested files = 24 total
	assert_eq!(descendant_paths.len(), 24);

	// Verify all created files are present
	for file in &created_files {
		let expected_path = format!("{}/{}", dir_path.0, file.name());
		let found = descendant_paths.iter().any(|p| p.0 == expected_path);
		assert!(found, "Expected file path {} not found", expected_path);
	}

	// Verify subdirectories are present
	let subdir1_expected = format!("{}/{}", dir_path.0, subdir1.name());
	let subdir2_expected = format!("{}/{}", dir_path.0, subdir2.name());
	assert!(descendant_paths.iter().any(|p| p.0 == subdir1_expected));
	assert!(descendant_paths.iter().any(|p| p.0 == subdir2_expected));

	// Verify nested files are present
	let nested1_expected = format!("{}/{}/{}", dir_path.0, subdir1.name(), nested_file1.name());
	let nested2_expected = format!("{}/{}/{}", dir_path.0, subdir2.name(), nested_file2.name());
	assert!(descendant_paths.iter().any(|p| p.0 == nested1_expected));
	assert!(descendant_paths.iter().any(|p| p.0 == nested2_expected));
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_get_all_descendant_paths_empty_names() {
	let (db, rss) = get_db_resources().await;

	// Test edge case with empty or unusual names (if the SDK allows them)
	let test_dir = rss
		.client
		.create_dir(&rss.dir, "edge_case_dir".to_string())
		.await
		.unwrap();

	// Try to create files with edge case names
	let normal_file = rss
		.client
		.make_file_builder("normal.txt", &test_dir)
		.build();
	let normal_file = rss
		.client
		.upload_file(normal_file.into(), b"Normal content")
		.await
		.unwrap();

	// Test with just extension
	let dot_file = rss.client.make_file_builder(".hidden", &test_dir).build();
	let dot_file = rss
		.client
		.upload_file(dot_file.into(), b"Hidden file content")
		.await
		.unwrap();

	let dir_path: FfiPathWithRoot =
		format!("{}/{}/{}", db.root_uuid(), rss.dir.name(), test_dir.name()).into();

	// Update database
	let parent_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	db.update_dir_children(parent_path).await.unwrap();
	db.update_dir_children(dir_path.clone()).await.unwrap();

	// Get descendant paths
	let descendant_paths = db.get_all_descendant_paths(&dir_path).unwrap();
	assert_eq!(descendant_paths.len(), 2);

	// Verify both files are present
	let expected_paths = vec![
		format!("{}/{}", dir_path.0, normal_file.name()),
		format!("{}/{}", dir_path.0, dot_file.name()),
	];

	for expected_path in expected_paths {
		let found = descendant_paths.iter().any(|p| p.0 == expected_path);
		assert!(
			found,
			"Expected path {} not found in descendant paths",
			expected_path
		);
	}
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_get_all_descendant_paths_concurrent_modifications() {
	let (db, rss) = get_db_resources().await;

	// Create initial structure
	let test_dir = rss
		.client
		.create_dir(&rss.dir, "concurrent_test".to_string())
		.await
		.unwrap();

	let initial_file = rss
		.client
		.make_file_builder("initial.txt", &test_dir)
		.build();
	let initial_file = rss
		.client
		.upload_file(initial_file.into(), b"Initial content")
		.await
		.unwrap();

	let dir_path: FfiPathWithRoot =
		format!("{}/{}/{}", db.root_uuid(), rss.dir.name(), test_dir.name()).into();

	// Update database
	let parent_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	db.update_dir_children(parent_path).await.unwrap();
	db.update_dir_children(dir_path.clone()).await.unwrap();

	// Get initial descendant paths
	let initial_paths = db.get_all_descendant_paths(&dir_path).unwrap();
	assert_eq!(initial_paths.len(), 1);

	// Add more files after initial query
	let additional_file = rss
		.client
		.make_file_builder("additional.txt", &test_dir)
		.build();
	let additional_file = rss
		.client
		.upload_file(additional_file.into(), b"Additional content")
		.await
		.unwrap();

	// Update database again
	db.update_dir_children(dir_path.clone()).await.unwrap();

	// Get updated descendant paths
	let updated_paths = db.get_all_descendant_paths(&dir_path).unwrap();
	assert_eq!(updated_paths.len(), 2);

	// Verify both files are present
	let expected_paths = vec![
		format!("{}/{}", dir_path.0, initial_file.name()),
		format!("{}/{}", dir_path.0, additional_file.name()),
	];

	for expected_path in expected_paths {
		let found = updated_paths.iter().any(|p| p.0 == expected_path);
		assert!(
			found,
			"Expected path {} not found in updated descendant paths",
			expected_path
		);
	}
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_get_all_descendant_paths_path_format_consistency() {
	let (db, rss) = get_db_resources().await;

	// Create nested structure to test path format consistency
	let root_dir = rss
		.client
		.create_dir(&rss.dir, "path_test".to_string())
		.await
		.unwrap();

	let sub_dir = rss
		.client
		.create_dir(&root_dir, "subdir".to_string())
		.await
		.unwrap();

	let file_in_sub = rss.client.make_file_builder("file.txt", &sub_dir).build();
	let file_in_sub = rss
		.client
		.upload_file(file_in_sub.into(), b"File content")
		.await
		.unwrap();

	let dir_path: FfiPathWithRoot =
		format!("{}/{}/{}", db.root_uuid(), rss.dir.name(), root_dir.name()).into();

	// Update database
	let parent_path: FfiPathWithRoot = format!("{}/{}", db.root_uuid(), rss.dir.name()).into();
	let sub_dir_path: FfiPathWithRoot = format!("{}/{}", dir_path.0, sub_dir.name()).into();

	db.update_dir_children(parent_path).await.unwrap();
	db.update_dir_children(dir_path.clone()).await.unwrap();
	db.update_dir_children(sub_dir_path).await.unwrap();

	// Get descendant paths
	let descendant_paths = db.get_all_descendant_paths(&dir_path).unwrap();
	assert_eq!(descendant_paths.len(), 2);

	// Verify path format consistency
	for path in &descendant_paths {
		// Paths should not have double slashes
		assert!(
			!path.0.contains("//"),
			"Path should not contain double slashes: {}",
			path.0
		);

		// Paths should start with the base path
		assert!(
			path.0.starts_with(&dir_path.0),
			"Path {} should start with base path {}",
			path.0,
			dir_path.0
		);

		// Paths should not end with slash (unless it's just the root)
		if path.0.len() > 1 {
			assert!(
				!path.0.ends_with('/'),
				"Path should not end with slash: {}",
				path.0
			);
		}
	}

	// Check specific expected paths
	let expected_subdir = format!("{}/{}", dir_path.0, sub_dir.name());
	let expected_file = format!("{}/{}/{}", dir_path.0, sub_dir.name(), file_in_sub.name());

	assert!(
		descendant_paths.iter().any(|p| p.0 == expected_subdir),
		"Expected subdirectory path not found"
	);
	assert!(
		descendant_paths.iter().any(|p| p.0 == expected_file),
		"Expected file path not found"
	);
}

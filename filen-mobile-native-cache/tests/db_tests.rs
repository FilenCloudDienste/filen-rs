use std::sync::Arc;

use base64::{Engine, prelude::BASE64_URL_SAFE_NO_PAD};
use filen_macros::shared_test_runtime;
use filen_mobile_native_cache::{
	CacheError,
	abort::FfiAbortController,
	auth::{AuthFile, DB_FILE_NAME, FilenMobileCacheState},
	ffi::{
		FfiChanges, FfiDir, FfiFile, FfiId, FfiNonRootObject, FfiObject, ItemType, SearchQueryArgs,
		SearchQueryResponseEntry, UploadFileInfo,
	},
	traits::{ProgressCallback, SearchUpdateCallback, WorkingSetUpdateListener},
};
use filen_sdk_rs::{
	crypto::{shared::DataCrypter, v3::EncryptionKey},
	fs::{
		HasName, HasUUID,
		dir::meta::DirectoryMetaChanges,
		file::{meta::FileMetaChanges, traits::HasFileInfo},
	},
};
use filen_types::fs::Uuid;
use futures::{StreamExt, future::BoxFuture, stream::FuturesUnordered};
use rand::TryRngCore;
use test_utils::TestResources;

// Mirrors the app-side auth.json encryption (fileProvider.ts) so the file-init test exercises the
// real decrypt path in FilenMobileCacheState::new: version(0x01) ++ nonce(12) ++ ciphertext ++ tag(16).
fn encrypt_auth_json(plaintext: &[u8], dek: &EncryptionKey) -> Vec<u8> {
	let mut data = plaintext.to_vec();
	dek.blocking_encrypt_data(&mut data).unwrap();

	let mut out = Vec::with_capacity(1 + data.len());
	out.push(0x01);
	out.extend_from_slice(&data);
	out
}

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

static MOBILE_CACHE_STATE_INIT: std::sync::OnceLock<Arc<FilenMobileCacheState>> =
	std::sync::OnceLock::new();

async fn get_db_resources() -> (Arc<FilenMobileCacheState>, TestResources) {
	let path = std::env::temp_dir();
	let files_path = path.join("test_files");
	std::fs::create_dir_all(&files_path).unwrap();
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = resources.client.to_stringified();
	let state = MOBILE_CACHE_STATE_INIT.get_or_init(|| {
		Arc::new(
			FilenMobileCacheState::from_stringified_in_memory(
				client,
				files_path.to_string_lossy().as_ref(),
			)
			.unwrap(),
		)
	});
	(state.clone(), resources)
}

// Root query tests
#[shared_test_runtime]
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

	let root_path: FfiId = rss.client.root().uuid().to_string().into();
	let result = db.query_item(&root_path).unwrap();

	match result {
		Some(FfiObject::Root(root)) => {
			assert_eq!(root.uuid, rss.client.root().uuid().to_string());
			assert_eq!(root.max_storage, 0); // Initial state
			assert_eq!(root.storage_used, 0); // Initial state
		}
		_ => panic!("Expected to find a root object"),
	}

	db.update_roots_info().await.unwrap();
	let root = db
		.query_roots_info(rss.client.root().uuid().to_string())
		.unwrap()
		.unwrap();

	assert_ne!(root.max_storage, 0);
	assert_ne!(root.storage_used, 0);
	assert_ne!(root.last_updated, 0);
	assert_eq!(root.uuid, db.root_uuid().unwrap().to_string());
}

#[shared_test_runtime]
pub async fn test_query_root_nonexistent() {
	let (db, _rss) = get_db_resources().await;

	let fake_uuid = Uuid::new_v4().to_string();
	let result = db.query_roots_info(fake_uuid).unwrap();
	assert!(result.is_none());
}

#[shared_test_runtime]
pub async fn test_query_root_invalid_uuid() {
	let (db, _rss) = get_db_resources().await;

	let result = db.query_roots_info("invalid-uuid".to_string());
	assert!(result.is_err());
}

// Directory children query tests
#[shared_test_runtime]
pub async fn test_query_children_empty_directory() {
	let (db, rss) = get_db_resources().await;
	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();

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

#[shared_test_runtime]
pub async fn test_query_children_with_files_and_dirs() {
	let (db, rss) = get_db_resources().await;
	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();

	// Create test content
	let dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "test_subdir")
		.await
		.unwrap();

	let file = rss
		.client
		.make_file_builder("test_file.txt", rss.dir.uuid())
		.unwrap();
	let file = rss
		.client
		.upload_file(file, b"Hello, world!")
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

#[shared_test_runtime]
pub async fn test_query_children_sorting_by_size() {
	let (db, rss) = get_db_resources().await;
	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();

	// Create files with different sizes
	let large_file = rss
		.client
		.make_file_builder("large.txt", rss.dir.uuid())
		.unwrap();
	let large_file = rss
		.client
		.upload_file(large_file, b"This is a much larger file with more content")
		.await
		.unwrap();

	let small_file = rss
		.client
		.make_file_builder("small.txt", rss.dir.uuid())
		.unwrap();
	rss.client.upload_file(small_file, b"small").await.unwrap();

	let empty_file = rss
		.client
		.make_file_builder("empty.txt", rss.dir.uuid())
		.unwrap();
	let empty_file = rss.client.upload_file(empty_file, b"").await.unwrap();

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

#[shared_test_runtime]
pub async fn test_query_children_sorting_by_name() {
	let (db, rss) = get_db_resources().await;
	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();

	// Create items with specific names for alphabetical testing
	rss.client
		.create_dir(&(&rss.dir).into(), "zebra_dir")
		.await
		.unwrap();

	let alpha_file = rss
		.client
		.make_file_builder("alpha.txt", rss.dir.uuid())
		.unwrap();
	rss.client.upload_file(alpha_file, b"").await.unwrap();

	let beta_file = rss
		.client
		.make_file_builder("beta.txt", rss.dir.uuid())
		.unwrap();
	rss.client.upload_file(beta_file, b"").await.unwrap();

	db.update_dir_children(test_dir_path.clone()).await.unwrap();

	let resp = db
		.query_dir_children(&test_dir_path, Some("display_name ASC".to_string()))
		.unwrap()
		.unwrap();
	assert_eq!(resp.objects.len(), 3);

	// Verify alphabetical order
	assert!(matches!(
		&resp.objects[0],
		FfiNonRootObject::File(f) if f.meta.clone().unwrap().name == "alpha.txt"
	));
	assert!(matches!(
		&resp.objects[1],
		FfiNonRootObject::File(f) if f.meta.clone().unwrap().name == "beta.txt"
	));
	assert!(matches!(
		&resp.objects[2],
		FfiNonRootObject::Dir(d) if d.meta.clone().unwrap().name == "zebra_dir"
	));
}

#[shared_test_runtime]
pub async fn test_query_children_after_deletion() {
	let (db, rss) = get_db_resources().await;
	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();

	// Create and then delete a directory
	let mut dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "temp_dir")
		.await
		.unwrap();

	let file = rss
		.client
		.make_file_builder("persistent.txt", rss.dir.uuid())
		.unwrap();
	let file = rss.client.upload_file(file, b"").await.unwrap();

	// Update to get both items
	db.update_dir_children(test_dir_path.clone()).await.unwrap();
	let resp = db
		.query_dir_children(&test_dir_path, None)
		.unwrap()
		.unwrap();
	assert_eq!(resp.objects.len(), 2);

	// Delete the directory
	rss.client.trash_dir(&mut dir).await.unwrap();
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

#[shared_test_runtime]
pub async fn test_query_children_nonexistent_path() {
	let (db, _rss) = get_db_resources().await;
	let nonexistent_path: FfiId = format!("{}/nonexistent_dir", db.root_uuid().unwrap()).into();

	let result = db.query_dir_children(&nonexistent_path, None).unwrap();
	assert!(result.is_none());
}

// Item query tests
#[shared_test_runtime]
pub async fn test_query_item_file() {
	let (db, rss) = get_db_resources().await;

	let file = rss
		.client
		.make_file_builder("query_test.txt", rss.dir.uuid())
		.unwrap();
	let file = rss.client.upload_file(file, b"Test content").await.unwrap();

	let file_path: FfiId = format!(
		"{}/{}/{}",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap(),
		file.name().unwrap()
	)
	.into();
	let dir_path: FfiId = format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();

	// Before update - should return None
	assert_eq!(db.query_item(&file_path).unwrap(), None);

	// After update - should return the file
	db.update_dir_children(dir_path).await.unwrap();
	let result = db.query_item(&file_path).unwrap();

	match result {
		Some(FfiObject::File(retrieved_file)) => {
			assert_eq!(retrieved_file.uuid, file.uuid().to_string());
			assert_eq!(retrieved_file.meta.unwrap().name, file.name().unwrap());
		}
		_ => panic!("Expected to find a file object"),
	}
}

#[shared_test_runtime]
pub async fn test_query_item_directory() {
	let (db, rss) = get_db_resources().await;

	let dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "query_test_dir")
		.await
		.unwrap();

	let child_dir_path: FfiId = format!(
		"{}/{}/{}",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap(),
		dir.name().unwrap()
	)
	.into();
	let parent_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();

	// Before update - should return None
	assert_eq!(db.query_item(&child_dir_path).unwrap(), None);

	// After update - should return the directory
	db.update_dir_children(parent_dir_path).await.unwrap();
	let result = db.query_item(&child_dir_path).unwrap();

	match result {
		Some(FfiObject::Dir(retrieved_dir)) => {
			assert_eq!(retrieved_dir.uuid, dir.uuid().to_string());
			assert_eq!(retrieved_dir.meta.unwrap().name, dir.name().unwrap());
		}
		_ => panic!("Expected to find a directory object"),
	}
}

#[shared_test_runtime]
pub async fn test_query_item_nonexistent() {
	let (db, rss) = get_db_resources().await;

	let nonexistent_file_path: FfiId = format!(
		"{}/{}/nonexistent.txt",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap()
	)
	.into();

	let result = db.query_item(&nonexistent_file_path).unwrap();
	assert!(result.is_none());
}

#[shared_test_runtime]
pub async fn test_query_item_invalid_path() {
	let (db, _rss) = get_db_resources().await;

	let invalid_path: FfiId = "not-a-uuid/invalid/path".into();
	let result = db.query_item(&invalid_path);
	assert!(result.is_err());
}

#[shared_test_runtime]
pub async fn test_query_item_deeply_nested() {
	let (db, rss) = get_db_resources().await;

	// Create nested structure: rss.dir/level1/level2/deep_file.txt
	let level1 = rss
		.client
		.create_dir(&(&rss.dir).into(), "level1")
		.await
		.unwrap();

	let level2 = rss
		.client
		.create_dir(&(&level1).into(), "level2")
		.await
		.unwrap();

	let deep_file = rss
		.client
		.make_file_builder("deep_file.txt", level2.uuid())
		.unwrap();
	let deep_file = rss
		.client
		.upload_file(deep_file, b"Deep content")
		.await
		.unwrap();

	// Update each level
	let dir_path: FfiId = format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let level1_path: FfiId = format!("{}/level1", dir_path.0).into();
	let level2_path: FfiId = format!("{}/level2", level1_path.0).into();

	db.update_dir_children(dir_path).await.unwrap();
	db.update_dir_children(level1_path).await.unwrap();
	db.update_dir_children(level2_path).await.unwrap();

	// Query the deep file
	let deep_file_path: FfiId = format!(
		"{}/{}/level1/level2/{}",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap(),
		deep_file.name().unwrap()
	)
	.into();

	let result = db.query_item(&deep_file_path).unwrap();
	match result {
		Some(FfiObject::File(retrieved_file)) => {
			assert_eq!(retrieved_file.uuid, deep_file.uuid().to_string());
			assert_eq!(retrieved_file.meta.unwrap().name, deep_file.name().unwrap());
		}
		_ => panic!("Expected to find the deeply nested file"),
	}

	// Also test querying intermediate directories
	let level1_query_path: FfiId = format!(
		"{}/{}/level1",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap()
	)
	.into();

	let result = db.query_item(&level1_query_path).unwrap();
	match result {
		Some(FfiObject::Dir(retrieved_dir)) => {
			assert_eq!(retrieved_dir.uuid, level1.uuid().to_string());
		}
		_ => panic!("Expected to find level1 directory"),
	}
}

#[shared_test_runtime]
pub async fn test_download_file() {
	let (db, rss) = get_db_resources().await;

	// Create a test file with some content inside rss.dir
	let test_content = b"Hello, world! This is test content for download.";
	let file = rss
		.client
		.make_file_builder("test_download.txt", rss.dir.uuid())
		.unwrap();
	let remote_file = rss.client.upload_file(file, test_content).await.unwrap();

	let file_path: FfiId = format!(
		"{}/{}/{}",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap(),
		remote_file.name().unwrap()
	)
	.into();

	// Test downloading the file
	let downloaded_path = db
		.download_file_if_changed_by_path(file_path.clone(), None)
		.await
		.unwrap();

	// Verify the file was downloaded and contains correct content
	assert!(std::path::Path::new(&downloaded_path).exists());
	let downloaded_content = std::fs::read(&downloaded_path).unwrap();
	assert_eq!(downloaded_content, test_content);

	// Clean up
	std::fs::remove_file(&downloaded_path).ok();
}

// The stale-read regression (KeePassDX reload): a remote same-name re-upload retires the cached
// uuid, but the retired uuid keeps resolving byte-identical as an archived version, so a
// uuid-keyed freshness check alone serves the stale cached bytes until an unrelated parent
// re-list. The v3/file `versioned` flag must force name-based re-resolution and a fresh
// download on the very next open.
#[shared_test_runtime]
pub async fn test_download_file_after_remote_replace() {
	let (db, rss) = get_db_resources().await;

	let old_content = b"old kdbx bytes";
	let file = rss
		.client
		.make_file_builder("replaced_download.txt", rss.dir.uuid())
		.unwrap();
	let old_file = rss.client.upload_file(file, old_content).await.unwrap();

	let file_path: FfiId = format!(
		"{}/{}/{}",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap(),
		old_file.name().unwrap()
	)
	.into();

	// Prime the cache: first open serves + caches the original bytes.
	let downloaded_path = db
		.download_file_if_changed_by_path(file_path.clone(), None)
		.await
		.unwrap();
	assert_eq!(std::fs::read(&downloaded_path).unwrap(), old_content);

	// Replace remotely (same name+parent, new uuid; old uuid archived as a version).
	let new_content = b"new kdbx bytes!";
	let file = rss
		.client
		.make_file_builder("replaced_download.txt", rss.dir.uuid())
		.unwrap();
	let new_file = rss.client.upload_file(file, new_content).await.unwrap();
	assert_ne!(old_file.uuid(), new_file.uuid());

	// The very next open must serve the new bytes — no parent listing in between.
	let downloaded_path = db
		.download_file_if_changed_by_path(file_path.clone(), None)
		.await
		.unwrap();
	assert_eq!(std::fs::read(&downloaded_path).unwrap(), new_content);

	// The recovery must also self-heal the cached row to the new uuid, so later opens take
	// the clean SameFile path instead of re-running the NotFound fallback every time.
	match db.query_item(&file_path).unwrap() {
		Some(FfiObject::File(f)) => assert_eq!(f.uuid, new_file.uuid().to_string()),
		other => panic!("expected cached file row after replace recovery, got {other:?}"),
	}

	std::fs::remove_file(&downloaded_path).ok();
}

// A remote rename + content edit re-mints the uuid AND changes the name, so neither of the
// legacy row-resolution keys (uuid, (parent, name)) can locate the cached row — only the
// server-minted stable id can. The row must be updated in place, surviving with its
// local_data, instead of being dropped and re-created as a new identity.
#[shared_test_runtime]
pub async fn test_remote_rename_and_edit_preserves_row_identity() {
	let (db, rss) = get_db_resources().await;

	let mut file = rss
		.client
		.upload_file(
			rss.client
				.make_file_builder("stable_identity_a.txt", rss.dir.uuid())
				.unwrap(),
			b"original bytes",
		)
		.await
		.unwrap();

	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let old_path: FfiId = test_dir_path.join("stable_identity_a.txt");

	db.update_dir_children(test_dir_path.clone()).await.unwrap();
	let mut local_data = std::collections::HashMap::new();
	local_data.insert("pinned".to_string(), "true".to_string());
	db.update_local_data(&file.uuid().to_string(), local_data.clone())
		.unwrap();

	// rename + edit remotely, with no cache sync in between
	rss.client
		.update_file_metadata(
			&mut file,
			FileMetaChanges::default()
				.name("stable_identity_b.txt")
				.unwrap(),
		)
		.await
		.unwrap();
	let edited = rss
		.client
		.upload_file(
			rss.client
				.make_file_builder("stable_identity_b.txt", rss.dir.uuid())
				.unwrap(),
			b"edited bytes",
		)
		.await
		.unwrap();
	assert_ne!(edited.uuid(), file.uuid());
	assert_eq!(edited.stable_uuid(), file.stable_uuid());

	db.update_dir_children(test_dir_path.clone()).await.unwrap();

	let new_path: FfiId = test_dir_path.join("stable_identity_b.txt");
	let updated = match db.query_item(&new_path).unwrap() {
		Some(FfiObject::File(f)) => f,
		other => panic!("expected the edited file under its new name, got {other:?}"),
	};
	assert_eq!(updated.uuid, edited.uuid().to_string());
	// the row survived the re-mint: provider-side state is still attached
	assert_eq!(updated.local_data, Some(local_data));
	// and the old name no longer resolves to anything
	assert!(db.query_item(&old_path).unwrap().is_none());
}

// Editing a file on a versioning-disabled account replaces it in place: the old uuid is
// trashed AND superseded, and for ~60s it stays readable, re-stamped with a fresh stable id
// that belongs to no lineage. That ghost must never be adopted onto the cached row (it would
// falsely trash the row or corrupt its identity) — the next open must re-resolve the path
// and serve the new head, which still carries the original stable id.
#[shared_test_runtime]
pub async fn test_versioning_disabled_edit_ghost_is_not_adopted() {
	let (db, rss) = get_db_resources().await;

	let _version_lock = rss
		.client
		.acquire_lock_with_default("test:versions")
		.await
		.unwrap();
	rss.client.set_versioning_enabled(false).await.unwrap();

	let old_content = b"versioning off v1";
	let old_file = rss
		.client
		.upload_file(
			rss.client
				.make_file_builder("versioning_off_ghost.txt", rss.dir.uuid())
				.unwrap(),
			old_content,
		)
		.await
		.unwrap();

	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let file_path: FfiId = test_dir_path.join("versioning_off_ghost.txt");

	// prime the cache with the original row
	let downloaded_path = db
		.download_file_if_changed_by_path(file_path.clone(), None)
		.await
		.unwrap();
	assert_eq!(std::fs::read(&downloaded_path).unwrap(), old_content);

	// edit remotely: in-place replace; the old uuid becomes the short-lived trashed ghost
	let new_content = b"versioning off v2";
	let new_file = rss
		.client
		.upload_file(
			rss.client
				.make_file_builder("versioning_off_ghost.txt", rss.dir.uuid())
				.unwrap(),
			new_content,
		)
		.await
		.unwrap();
	assert_ne!(new_file.uuid(), old_file.uuid());
	assert_eq!(new_file.stable_uuid(), old_file.stable_uuid());

	// the very next open revalidates the dead uuid against the ghost and must recover
	let downloaded_path = db
		.download_file_if_changed_by_path(file_path.clone(), None)
		.await
		.unwrap();
	assert_eq!(std::fs::read(&downloaded_path).unwrap(), new_content);

	// the cached row self-healed to the new head and was never marked trashed
	match db.query_item(&file_path).unwrap() {
		Some(FfiObject::File(f)) => assert_eq!(f.uuid, new_file.uuid().to_string()),
		other => panic!("expected the healed file row, got {other:?}"),
	}

	rss.client.set_versioning_enabled(true).await.unwrap();
	std::fs::remove_file(&downloaded_path).ok();
}

#[shared_test_runtime]
pub async fn test_download_file_nonexistent() {
	let (db, rss) = get_db_resources().await;

	let nonexistent_path: FfiId = format!(
		"{}/{}/nonexistent_file.txt",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap()
	)
	.into();

	// Should fail when trying to download a non-existent file
	let result = db
		.download_file_if_changed_by_path(nonexistent_path, None)
		.await;
	assert!(result.is_err());
}

#[shared_test_runtime]
pub async fn test_download_file_invalid_path() {
	let (db, rss) = get_db_resources().await;

	// Create a directory first
	let dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "test_dir")
		.await
		.unwrap();
	let dir_path: FfiId = format!(
		"{}/{}/{}",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap(),
		dir.name().unwrap()
	)
	.into();

	// Should fail when trying to download a directory path as a file
	let result = db.download_file_if_changed_by_path(dir_path, None).await;
	assert!(result.is_err());
}

#[shared_test_runtime]
pub async fn test_progress_callback() {
	let (db, rss) = get_db_resources().await;
	let mut contents = vec![0u8; 10 * 1024 * 1024];
	rand::rng().try_fill_bytes(&mut contents).unwrap();
	let parent_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap(),).into();
	let name = "test_progress.txt".to_string();
	let create_resp = db
		.create_empty_file(parent_path, name.clone(), None)
		.await
		.unwrap();
	std::fs::write(create_resp.path, &contents).unwrap();

	let upload_progress_callback = Arc::new(SumProgressCallback::default());
	db.upload_file_if_changed(
		create_resp.id.clone(),
		Some(upload_progress_callback.clone()),
	)
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
		.download_file_if_changed_by_path(create_resp.id.clone(), Some(progress_callback.clone()))
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

#[shared_test_runtime]
pub async fn test_create_empty_file() {
	let (db, rss) = get_db_resources().await;

	let parent_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let file_name = "empty_test.txt".to_string();
	let mime_type = "text/plain".to_string();

	// Create an empty file
	let create_resp = db
		.create_empty_file(
			parent_path.clone(),
			file_name.clone(),
			Some(mime_type.clone()),
		)
		.await
		.unwrap();

	// Verify the file exists in the database
	let queried_file = db.query_item(&create_resp.id).unwrap();

	match queried_file {
		Some(FfiObject::File(file)) => {
			let meta = file.meta.unwrap();
			assert_eq!(meta.name, file_name);
			assert_eq!(meta.mime, mime_type);
			assert_eq!(file.size, 0); // Should be empty
		}
		_ => panic!("Expected to find a file object"),
	}
}

#[shared_test_runtime]
pub async fn test_create_empty_file_different_mime_types() {
	let (db, rss) = get_db_resources().await;

	let parent_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();

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
				Some(mime_type.to_string()),
			)
			.await
			.unwrap();

		// Verify each file was created with correct MIME type
		let queried_file = db.query_item(&create_resp.id).unwrap();

		match queried_file {
			Some(FfiObject::File(file)) => {
				let meta = file.meta.unwrap();
				assert_eq!(meta.name, filename);
				assert_eq!(meta.mime, mime_type);
				assert_eq!(file.size, 0);
			}
			_ => panic!("Expected to find a file object for {filename}"),
		}
	}
}

#[shared_test_runtime]
pub async fn test_create_empty_file_duplicate_name() {
	let (db, rss) = get_db_resources().await;

	let parent_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let file_name = "duplicate.txt".to_string();
	let mime_type = "text/plain".to_string();

	// Create first file
	db.create_empty_file(
		parent_path.clone(),
		file_name.clone(),
		Some(mime_type.clone()),
	)
	.await
	.unwrap();

	assert!(
		db.create_empty_file(
			parent_path.clone(),
			file_name.clone(),
			Some(mime_type.clone()),
		)
		.await
		.is_ok()
	);
}

#[shared_test_runtime]
pub async fn test_create_empty_file_invalid_parent() {
	let (db, rss) = get_db_resources().await;

	let invalid_parent_path: FfiId = format!(
		"{}/{}/nonexistent_subdir",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap()
	)
	.into();

	// Should fail with invalid parent path
	let result = db
		.create_empty_file(
			invalid_parent_path,
			"test.txt".to_string(),
			Some("text/plain".to_string()),
		)
		.await;

	assert!(result.is_err());
}

#[shared_test_runtime]
pub async fn test_create_empty_file_in_root() {
	let (db, rss) = get_db_resources().await;

	// Create file in the test directory (rss.dir), not the absolute root
	let parent_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let file_name = "root_file.txt".to_string();
	let mime_type = "text/plain".to_string();

	// Create file in test directory
	let create_resp = db
		.create_empty_file(
			parent_path.clone(),
			file_name.clone(),
			Some(mime_type.clone()),
		)
		.await
		.unwrap();

	// Verify the file exists
	let queried_file = db.query_item(&create_resp.id).unwrap();

	match queried_file {
		Some(FfiObject::File(file)) => {
			let meta = file.meta.unwrap();
			assert_eq!(meta.name, file_name);
			assert_eq!(meta.mime, mime_type);
		}
		_ => panic!("Expected to find a file object in test directory"),
	}
}

#[shared_test_runtime]
pub async fn test_trash_item_file_restore() {
	let (db, rss) = get_db_resources().await;

	// Create a test file
	let file = rss
		.client
		.make_file_builder("restore_me.txt", rss.dir.uuid())
		.unwrap();
	let file = rss
		.client
		.upload_file(file, b"This file will be restored")
		.await
		.unwrap();

	let file_path: FfiId = format!(
		"{}/{}/{}",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap(),
		file.name().unwrap()
	)
	.into();

	// Update the database to include the file
	let parent_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	db.update_dir_children(parent_path.clone()).await.unwrap();

	// Verify file exists before trashing
	let db_obj = db.query_item(&file_path).unwrap().unwrap();

	// Trash the file
	db.trash_item(file_path.clone()).await.unwrap();

	// Verify file is removed from database
	let result = db.query_item(&file_path).unwrap();
	assert!(result.is_none());

	let trashed_db_obj = db
		.query_item(&format!("trash/{}", file.uuid()).into())
		.unwrap()
		.unwrap();

	match (db_obj, trashed_db_obj) {
		(FfiObject::File(original_file), FfiObject::File(trashed_file)) => {
			assert_eq!(original_file.uuid, trashed_file.uuid);
			assert_eq!(
				original_file.meta.unwrap().name,
				trashed_file.meta.unwrap().name
			);
			assert_eq!(original_file.size, trashed_file.size);
			assert_eq!(trashed_file.parent, "trash");
		}
		(db_obj, trashed_db_obj) => panic!(
			"Expected to find a file object in both original and trashed state {db_obj:?} {trashed_db_obj:?}"
		),
	}

	// Restore the file
	db.restore_item(&file.uuid().to_string(), None)
		.await
		.unwrap();

	// Verify the file is back in the database
	let restored_result = db.query_item(&file_path).unwrap();
	let restored_file = match restored_result.unwrap() {
		FfiObject::File(restored_file) => {
			assert_eq!(restored_file.uuid, file.uuid().to_string());
			assert_eq!(
				restored_file.meta.clone().unwrap().name,
				file.name().unwrap()
			);
			assert_eq!(restored_file.size, file.size() as i64); // Size of "This file will be restored"
			restored_file
		}
		_ => panic!("Expected to find a restored file object"),
	};

	// Verify the restored file is in the parent directory listing
	db.update_dir_children(parent_path.clone()).await.unwrap();
	let children = db.query_dir_children(&parent_path, None).unwrap().unwrap();
	let file_exists = children
		.objects
		.iter()
		.any(|obj| matches!(obj, FfiNonRootObject::File(f) if f.uuid == restored_file.uuid));
	assert!(file_exists);
}

#[shared_test_runtime]
pub async fn test_trash_item_file_success() {
	let (db, rss) = get_db_resources().await;

	// Create a test file
	let file = rss
		.client
		.make_file_builder("trash_me.txt", rss.dir.uuid())
		.unwrap();
	let file = rss
		.client
		.upload_file(file, b"This file will be trashed")
		.await
		.unwrap();

	let file_path: FfiId = format!(
		"{}/{}/{}",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap(),
		file.name().unwrap()
	)
	.into();

	// Update the database to include the file
	let parent_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
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

#[shared_test_runtime]
pub async fn test_trash_item_directory_success() {
	let (db, rss) = get_db_resources().await;

	// Create a test directory
	let dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "trash_this_dir")
		.await
		.unwrap();

	let dir_path: FfiId = format!(
		"{}/{}/{}",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap(),
		dir.name().unwrap()
	)
	.into();

	// Update the database to include the directory
	let parent_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
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

#[shared_test_runtime]
pub async fn test_trash_item_directory_with_contents() {
	let (db, rss) = get_db_resources().await;

	// Create a directory with nested content
	let parent_dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "parent_to_trash")
		.await
		.unwrap();

	// Add a subdirectory
	let sub_dir = rss
		.client
		.create_dir(&(&parent_dir).into(), "subdirectory")
		.await
		.unwrap();

	// Add a file to the parent directory
	let file_in_parent = rss
		.client
		.make_file_builder("file_in_parent.txt", parent_dir.uuid())
		.unwrap();
	rss.client
		.upload_file(file_in_parent, b"Content in parent")
		.await
		.unwrap();

	// Add a file to the subdirectory
	let file_in_sub = rss
		.client
		.make_file_builder("file_in_sub.txt", sub_dir.uuid())
		.unwrap();
	rss.client
		.upload_file(file_in_sub, b"Content in subdirectory")
		.await
		.unwrap();

	// Update database with all the content
	let base_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let parent_dir_path: FfiId = format!("{}/{}", base_path.0, parent_dir.name().unwrap()).into();
	let sub_dir_path: FfiId = format!("{}/{}", parent_dir_path.0, sub_dir.name().unwrap()).into();

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

#[shared_test_runtime]
pub async fn test_trash_item_root_directory_error() {
	let (db, rss) = get_db_resources().await;

	// Attempt to trash the root directory
	let root_path: FfiId = db.root_uuid().unwrap().into();
	let result = db.trash_item(root_path).await;

	// Should fail with appropriate error
	assert!(result.is_err());
	let error_message = format!("{}", result.unwrap_err());
	assert!(error_message.contains("Cannot remove root directory"));
	assert!(error_message.contains(&rss.client.root().uuid().to_string()));
}

#[shared_test_runtime]
pub async fn test_trash_item_nonexistent_file() {
	let (db, rss) = get_db_resources().await;

	let nonexistent_path: FfiId = format!(
		"{}/{}/nonexistent_file.txt",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap()
	)
	.into();

	// Should fail when trying to trash a non-existent file
	let result = db.trash_item(nonexistent_path).await;
	assert!(result.is_err());
	let error_message = format!("{}", result.unwrap_err());
	assert!(error_message.contains("does not point to an item"));
}

#[shared_test_runtime]
pub async fn test_trash_item_nonexistent_directory() {
	let (db, rss) = get_db_resources().await;

	let nonexistent_path: FfiId = format!(
		"{}/{}/nonexistent_dir",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap()
	)
	.into();

	// Should fail when trying to trash a non-existent directory
	let result = db.trash_item(nonexistent_path).await;
	assert!(result.is_err());
	let error_message = format!("{}", result.unwrap_err());
	assert!(error_message.contains("does not point to an item"));
}

#[shared_test_runtime]
pub async fn test_trash_item_invalid_path() {
	let (db, _rss) = get_db_resources().await;

	let invalid_path: FfiId = "not-a-uuid/invalid/path".into();
	let result = db.trash_item(invalid_path).await;

	// Should fail with UUID parsing error
	assert!(result.is_err());
}

#[shared_test_runtime]
pub async fn test_trash_item_partial_path() {
	let (db, rss) = get_db_resources().await;

	// Create a directory structure but don't update all levels
	let level1 = rss
		.client
		.create_dir(&(&rss.dir).into(), "level1")
		.await
		.unwrap();

	rss.client
		.create_dir(&(&level1).into(), "level2")
		.await
		.unwrap();

	// Only update the base directory, not the nested ones
	let base_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	db.update_dir_children(base_path).await.unwrap();

	// Try to trash level2 without having updated level1's children
	let level2_path: FfiId = format!(
		"{}/{}/level1/level2",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap()
	)
	.into();

	db.trash_item(level2_path).await.unwrap();
}

#[shared_test_runtime]
pub async fn test_trash_item_file_then_query_parent() {
	let (db, rss) = get_db_resources().await;

	// Create multiple files in the same directory
	let file1 = rss
		.client
		.make_file_builder("keep_me.txt", rss.dir.uuid())
		.unwrap();
	let file1 = rss
		.client
		.upload_file(file1, b"Keep this file")
		.await
		.unwrap();

	let file2 = rss
		.client
		.make_file_builder("trash_me.txt", rss.dir.uuid())
		.unwrap();
	let file2 = rss
		.client
		.upload_file(file2, b"Trash this file")
		.await
		.unwrap();

	let parent_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let file2_path: FfiId = format!("{}/{}", parent_path.0, file2.name().unwrap()).into();

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

#[shared_test_runtime]
pub async fn test_trash_item_empty_directory() {
	let (db, rss) = get_db_resources().await;

	// Create an empty directory
	let empty_dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "empty_dir")
		.await
		.unwrap();

	let dir_path: FfiId = format!(
		"{}/{}/{}",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap(),
		empty_dir.name().unwrap()
	)
	.into();

	let parent_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();

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

#[shared_test_runtime]
pub async fn test_trash_item_already_trashed_file() {
	let (db, rss) = get_db_resources().await;

	// Create and trash a file using the SDK directly first
	let file = rss
		.client
		.make_file_builder("already_trashed.txt", rss.dir.uuid())
		.unwrap();
	let mut file = rss
		.client
		.upload_file(file, b"This will be trashed twice")
		.await
		.unwrap();

	// Trash it directly via SDK
	rss.client.trash_file(&mut file).await.unwrap();

	let file_path: FfiId = format!(
		"{}/{}/{}",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap(),
		file.name().unwrap()
	)
	.into();

	// Now try to trash it via our method - should fail since it doesn't exist in our DB
	let result = db.trash_item(file_path).await;
	assert!(result.is_err());
	let error_message = format!("{}", result.unwrap_err());
	assert!(error_message.contains("does not point to an item"));
}

#[shared_test_runtime]
pub async fn test_move_item_file_success() {
	let (db, rss) = get_db_resources().await;

	// Create source and destination directories
	let source_dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "source_dir")
		.await
		.unwrap();

	let dest_dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "dest_dir")
		.await
		.unwrap();

	// Create a file in the source directory
	let file = rss
		.client
		.make_file_builder("move_me.txt", source_dir.uuid())
		.unwrap();
	let file = rss
		.client
		.upload_file(file, b"Content to move")
		.await
		.unwrap();

	// Update database with all directories
	let base_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let source_path: FfiId = format!("{}/{}", base_path.0, source_dir.name().unwrap()).into();
	let dest_path: FfiId = format!("{}/{}", base_path.0, dest_dir.name().unwrap()).into();

	db.update_dir_children(base_path).await.unwrap();
	db.update_dir_children(source_path.clone()).await.unwrap();
	db.update_dir_children(dest_path.clone()).await.unwrap();

	// Define paths for the move operation
	let file_path: FfiId = format!("{}/{}", source_path.0, file.name().unwrap()).into();

	// Move the file
	let new_file_path = db
		.move_item(file_path.clone(), dest_path.clone())
		.await
		.unwrap();

	// Verify the new path is correct
	let expected_new_path: FfiId = format!("{}/{}", dest_path.0, file.name().unwrap()).into();
	assert_eq!(new_file_path.id, expected_new_path);

	// Verify file no longer exists at old location
	assert!(db.query_item(&file_path).unwrap().is_none());

	// Verify file exists at new location
	let moved_file = db.query_item(&new_file_path.id).unwrap();
	assert!(moved_file.is_some());
	match moved_file.unwrap() {
		FfiObject::File(f) => {
			assert_eq!(f.meta.unwrap().name, file.name().unwrap());
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

#[shared_test_runtime]
pub async fn test_move_item_directory_success() {
	let (db, rss) = get_db_resources().await;

	// Create source and destination directories
	let source_dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "source_dir")
		.await
		.unwrap();

	let dest_dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "dest_dir")
		.await
		.unwrap();

	// Create a directory to move
	let move_dir = rss
		.client
		.create_dir(&(&source_dir).into(), "dir_to_move")
		.await
		.unwrap();

	// Add some content to the directory being moved
	let file_in_move_dir = rss
		.client
		.make_file_builder("content.txt", move_dir.uuid())
		.unwrap();
	rss.client
		.upload_file(file_in_move_dir, b"Content in moved dir")
		.await
		.unwrap();

	// Update database with all directories
	let base_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let source_path: FfiId = format!("{}/{}", base_path.0, source_dir.name().unwrap()).into();
	let dest_path: FfiId = format!("{}/{}", base_path.0, dest_dir.name().unwrap()).into();
	let move_dir_path: FfiId = format!("{}/{}", source_path.0, move_dir.name().unwrap()).into();

	db.update_dir_children(base_path).await.unwrap();
	db.update_dir_children(source_path.clone()).await.unwrap();
	db.update_dir_children(dest_path.clone()).await.unwrap();
	db.update_dir_children(move_dir_path.clone()).await.unwrap();

	// Move the directory
	let new_dir_path = db
		.move_item(move_dir_path.clone(), dest_path.clone())
		.await
		.unwrap();

	// Verify the new path is correct
	let expected_new_path: FfiId = format!("{}/{}", dest_path.0, move_dir.name().unwrap()).into();
	assert_eq!(new_dir_path.id, expected_new_path);

	// Verify directory no longer exists at old location
	assert!(db.query_item(&move_dir_path).unwrap().is_none());

	// Verify directory exists at new location
	let moved_dir = db.query_item(&new_dir_path.id).unwrap();
	assert!(moved_dir.is_some());
	match moved_dir.unwrap() {
		FfiObject::Dir(d) => {
			assert_eq!(d.meta.unwrap().name, move_dir.name().unwrap());
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

#[shared_test_runtime]
pub async fn test_move_item_nonexistent_item() {
	let (db, rss) = get_db_resources().await;

	// Create destination directory
	let dest_dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "dest_dir")
		.await
		.unwrap();

	let base_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let dest_path: FfiId = format!("{}/{}", base_path.0, dest_dir.name().unwrap()).into();
	let nonexistent_file_path: FfiId = format!("{}/nonexistent.txt", base_path.0).into();

	db.update_dir_children(base_path.clone()).await.unwrap();

	// Try to move non-existent file
	let result = db.move_item(nonexistent_file_path, dest_path).await;

	assert!(result.is_err());
	let error_message = format!("{}", result.unwrap_err());
	assert!(error_message.contains("does not point to an item"));
}

#[shared_test_runtime]
pub async fn test_move_item_nonexistent_destination() {
	let (db, rss) = get_db_resources().await;

	// Create a file to move
	let file = rss
		.client
		.make_file_builder("move_me.txt", rss.dir.uuid())
		.unwrap();
	let file = rss.client.upload_file(file, b"Content").await.unwrap();

	let base_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let file_path: FfiId = format!("{}/{}", base_path.0, file.name().unwrap()).into();
	let nonexistent_dest: FfiId = format!("{}/nonexistent_dir", base_path.0).into();

	db.update_dir_children(base_path.clone()).await.unwrap();

	// Try to move to non-existent destination
	let result = db.move_item(file_path, nonexistent_dest).await;

	assert!(result.is_err());
	let error_message = format!("{}", result.unwrap_err());
	assert!(error_message.contains("does not point to an item"));
}

#[shared_test_runtime]
pub async fn test_move_item_destination_is_file() {
	let (db, rss) = get_db_resources().await;

	// Create a file to move
	let move_file = rss
		.client
		.make_file_builder("move_me.txt", rss.dir.uuid())
		.unwrap();
	let move_file = rss
		.client
		.upload_file(move_file, b"Content to move")
		.await
		.unwrap();

	// Create a file that will be used as invalid destination
	let dest_file = rss
		.client
		.make_file_builder("dest_file.txt", rss.dir.uuid())
		.unwrap();
	let dest_file = rss
		.client
		.upload_file(dest_file, b"This is not a directory")
		.await
		.unwrap();

	let base_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let move_file_path: FfiId = format!("{}/{}", base_path.0, move_file.name().unwrap()).into();
	let dest_file_path: FfiId = format!("{}/{}", base_path.0, dest_file.name().unwrap()).into();

	db.update_dir_children(base_path.clone()).await.unwrap();

	// Try to move to a file instead of directory
	let result = db.move_item(move_file_path, dest_file_path).await;

	assert!(result.is_err());
	let error_message = format!("{}", result.unwrap_err());
	assert!(error_message.contains("does not point to a directory"));
}

#[shared_test_runtime]
pub async fn test_move_item_root_directory_error() {
	let (db, rss) = get_db_resources().await;

	// Create destination directory
	let dest_dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "dest_dir")
		.await
		.unwrap();

	let root_path: FfiId = db.root_uuid().unwrap().into();
	let base_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let dest_path: FfiId = format!("{}/{}", base_path.0, dest_dir.name().unwrap()).into();

	db.update_dir_children(base_path).await.unwrap();

	// Try to move root directory (should fail at conversion to DBNonRootObject)
	let result = db.move_item(root_path.clone(), dest_path).await;

	assert!(result.is_err());
	let error_message = format!("{}", result.unwrap_err());
	assert!(error_message.contains("does not point to a non-root item"));
}

#[shared_test_runtime]
pub async fn test_move_item_same_directory() {
	let (db, rss) = get_db_resources().await;

	// Create a file
	let file = rss
		.client
		.make_file_builder("stay_here.txt", rss.dir.uuid())
		.unwrap();
	let file = rss.client.upload_file(file, b"Content").await.unwrap();

	let base_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let file_path: FfiId = format!("{}/{}", base_path.0, file.name().unwrap()).into();

	db.update_dir_children(base_path.clone()).await.unwrap();

	// Move file to the same directory (should succeed)
	let new_file_path = db
		.move_item(file_path.clone(), base_path.clone())
		.await
		.unwrap();

	// File should still be in the same location
	assert_eq!(new_file_path.id, file_path);

	// Verify file still exists
	let moved_file = db.query_item(&new_file_path.id).unwrap();
	assert!(moved_file.is_some());
	match moved_file.unwrap() {
		FfiObject::File(f) => {
			assert_eq!(f.meta.unwrap().name, file.name().unwrap());
			assert_eq!(f.uuid, file.uuid().to_string());
		}
		_ => panic!("Expected file object"),
	}
}

#[shared_test_runtime]
pub async fn test_move_item_nested_directory_structure() {
	let (db, rss) = get_db_resources().await;

	// Create nested structure: base/level1/level2/file.txt
	let level1 = rss
		.client
		.create_dir(&(&rss.dir).into(), "level1")
		.await
		.unwrap();

	let level2 = rss
		.client
		.create_dir(&(&level1).into(), "level2")
		.await
		.unwrap();

	let file = rss
		.client
		.make_file_builder("nested_file.txt", level2.uuid())
		.unwrap();
	let file = rss
		.client
		.upload_file(file, b"Nested content")
		.await
		.unwrap();

	// Create destination directory at root level
	let dest_dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "destination")
		.await
		.unwrap();

	// Set up paths
	let base_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let level1_path: FfiId = format!("{}/{}", base_path.0, level1.name().unwrap()).into();
	let level2_path: FfiId = format!("{}/{}", level1_path.0, level2.name().unwrap()).into();
	let file_path: FfiId = format!("{}/{}", level2_path.0, file.name().unwrap()).into();
	let dest_path: FfiId = format!("{}/{}", base_path.0, dest_dir.name().unwrap()).into();

	// Update all levels
	db.update_dir_children(base_path.clone()).await.unwrap();
	db.update_dir_children(level1_path).await.unwrap();
	db.update_dir_children(level2_path.clone()).await.unwrap();
	db.update_dir_children(dest_path.clone()).await.unwrap();

	// Move file from deep nested location to destination
	let new_file_path = db
		.move_item(file_path.clone(), dest_path.clone())
		.await
		.unwrap();

	// Verify new path
	let expected_new_path: FfiId = format!("{}/{}", dest_path.0, file.name().unwrap()).into();
	assert_eq!(new_file_path.id, expected_new_path);

	// Verify file no longer exists at original location
	assert!(db.query_item(&file_path).unwrap().is_none());

	// Verify file exists at new location
	let moved_file = db.query_item(&new_file_path.id).unwrap();
	assert!(moved_file.is_some());
	match moved_file.unwrap() {
		FfiObject::File(f) => {
			assert_eq!(f.meta.unwrap().name, file.name().unwrap());
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

#[shared_test_runtime]
pub async fn test_move_item_directory_with_contents() {
	let (db, rss) = get_db_resources().await;

	// Create source directory with contents
	let source_dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "source_with_contents")
		.await
		.unwrap();

	// Create subdirectory and file in source
	let sub_dir = rss
		.client
		.create_dir(&(&source_dir).into(), "subdirectory")
		.await
		.unwrap();

	let file_in_source = rss
		.client
		.make_file_builder("file_in_source.txt", source_dir.uuid())
		.unwrap();
	let file_in_source = rss
		.client
		.upload_file(file_in_source, b"Source content")
		.await
		.unwrap();

	let file_in_sub = rss
		.client
		.make_file_builder("file_in_sub.txt", sub_dir.uuid())
		.unwrap();
	rss.client
		.upload_file(file_in_sub, b"Sub content")
		.await
		.unwrap();

	// Create destination directory
	let dest_dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "destination")
		.await
		.unwrap();

	// Set up paths
	let base_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let source_path: FfiId = format!("{}/{}", base_path.0, source_dir.name().unwrap()).into();
	let dest_path: FfiId = format!("{}/{}", base_path.0, dest_dir.name().unwrap()).into();

	// Update database
	db.update_dir_children(base_path.clone()).await.unwrap();
	db.update_dir_children(source_path.clone()).await.unwrap();
	db.update_dir_children(dest_path.clone()).await.unwrap();

	// Move the entire source directory to destination
	let new_source_path = db
		.move_item(source_path.clone(), dest_path.clone())
		.await
		.unwrap();

	// Verify new path
	let expected_new_path: FfiId = format!("{}/{}", dest_path.0, source_dir.name().unwrap()).into();
	assert_eq!(new_source_path.id, expected_new_path);

	// Verify old source directory is gone
	assert!(db.query_item(&source_path).unwrap().is_none());

	// Verify new source directory exists
	let moved_dir = db.query_item(&new_source_path.id).unwrap();
	assert!(moved_dir.is_some());
	match moved_dir.unwrap() {
		FfiObject::Dir(d) => {
			assert_eq!(d.meta.unwrap().name, source_dir.name().unwrap());
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
	db.update_dir_children(new_source_path.id.clone())
		.await
		.unwrap();
	let moved_source_children = db
		.query_dir_children(&new_source_path.id, None)
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

#[shared_test_runtime]
pub async fn test_move_item_partial_path_resolution() {
	let (db, rss) = get_db_resources().await;

	// Create nested structure but only update some levels
	let level1 = rss
		.client
		.create_dir(&(&rss.dir).into(), "level1")
		.await
		.unwrap();

	let level2 = rss
		.client
		.create_dir(&(&level1).into(), "level2")
		.await
		.unwrap();

	let file = rss
		.client
		.make_file_builder("deep_file.txt", level2.uuid())
		.unwrap();
	let file = rss.client.upload_file(file, b"Deep content").await.unwrap();

	let dest_dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "destination")
		.await
		.unwrap();

	// Only update base level, not the nested levels
	let base_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let level2_path: FfiId = format!("{}/level1/level2", base_path.0).into();
	let file_path: FfiId = format!("{}/deep_file.txt", level2_path.0).into();
	let dest_path: FfiId = format!("{}/{}", base_path.0, dest_dir.name().unwrap()).into();

	db.update_dir_children(base_path.clone()).await.unwrap();

	// Move should work with partial path resolution (sync::update_items_in_path should handle this)
	let new_file_path = db
		.move_item(file_path.clone(), dest_path.clone())
		.await
		.unwrap();

	// Verify file was moved successfully
	let expected_new_path: FfiId = format!("{}/{}", dest_path.0, file.name().unwrap()).into();
	assert_eq!(new_file_path.id, expected_new_path);

	// Verify file exists at new location
	let moved_file = db.query_item(&new_file_path.id).unwrap();
	assert!(moved_file.is_some());
}

#[shared_test_runtime]
pub async fn test_move_item_name_collision_handling() {
	let (db, rss) = get_db_resources().await;

	// Create source directory with a file
	let source_dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "source")
		.await
		.unwrap();

	let file_to_move = rss
		.client
		.make_file_builder("duplicate_name.txt", source_dir.uuid())
		.unwrap();
	let file_to_move = rss
		.client
		.upload_file(file_to_move, b"Content to move")
		.await
		.unwrap();

	// Create destination directory with a file of the same name
	let dest_dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "destination")
		.await
		.unwrap();

	let existing_file = rss
		.client
		.make_file_builder("duplicate_name.txt", dest_dir.uuid())
		.unwrap();
	rss.client
		.upload_file(existing_file, b"Existing content")
		.await
		.unwrap();

	// Set up paths
	let base_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let source_path: FfiId = format!("{}/{}", base_path.0, source_dir.name().unwrap()).into();
	let dest_path: FfiId = format!("{}/{}", base_path.0, dest_dir.name().unwrap()).into();
	let file_path: FfiId = format!("{}/{}", source_path.0, file_to_move.name().unwrap()).into();

	// Update database
	db.update_dir_children(base_path).await.unwrap();
	db.update_dir_children(source_path.clone()).await.unwrap();
	db.update_dir_children(dest_path.clone()).await.unwrap();

	// Move should succeed (the SDK should handle name conflicts)
	let new_file_path = db
		.move_item(file_path.clone(), dest_path.clone())
		.await
		.unwrap();

	// The move operation should succeed - the SDK typically handles name conflicts
	// by either overwriting or creating a new name variant
	assert!(new_file_path.id.0.contains(&dest_path.0));

	// Verify file no longer exists in source
	assert!(db.query_item(&file_path).unwrap().is_none());

	// Verify some file exists at the new location (name might be modified by SDK)
	let moved_file = db.query_item(&new_file_path.id).unwrap();
	assert!(moved_file.is_some());
}

#[shared_test_runtime]
pub async fn test_move_item_multiple_files_same_operation() {
	let (db, rss) = get_db_resources().await;

	// Create source directory with multiple files
	let source_dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "multi_source")
		.await
		.unwrap();

	let file1 = rss
		.client
		.make_file_builder("file1.txt", source_dir.uuid())
		.unwrap();
	let file1 = rss.client.upload_file(file1, b"Content 1").await.unwrap();

	let file2 = rss
		.client
		.make_file_builder("file2.txt", source_dir.uuid())
		.unwrap();
	let file2 = rss.client.upload_file(file2, b"Content 2").await.unwrap();

	// Create destination directory
	let dest_dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "multi_dest")
		.await
		.unwrap();

	// Set up paths
	let base_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let source_path: FfiId = format!("{}/{}", base_path.0, source_dir.name().unwrap()).into();
	let dest_path: FfiId = format!("{}/{}", base_path.0, dest_dir.name().unwrap()).into();
	let file1_path: FfiId = format!("{}/{}", source_path.0, file1.name().unwrap()).into();
	let file2_path: FfiId = format!("{}/{}", source_path.0, file2.name().unwrap()).into();

	// Update database
	db.update_dir_children(base_path).await.unwrap();
	db.update_dir_children(source_path.clone()).await.unwrap();
	db.update_dir_children(dest_path.clone()).await.unwrap();

	// Move both files
	let new_file1_path = db
		.move_item(file1_path.clone(), dest_path.clone())
		.await
		.unwrap();

	let new_file2_path = db
		.move_item(file2_path.clone(), dest_path.clone())
		.await
		.unwrap();

	// Verify both files were moved
	assert!(db.query_item(&file1_path).unwrap().is_none());
	assert!(db.query_item(&file2_path).unwrap().is_none());

	assert!(db.query_item(&new_file1_path.id).unwrap().is_some());
	assert!(db.query_item(&new_file2_path.id).unwrap().is_some());

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

#[shared_test_runtime]
pub async fn test_rename_item_file_success() {
	let (db, rss) = get_db_resources().await;

	// Create a test file
	let file = rss
		.client
		.make_file_builder("old_name.txt", rss.dir.uuid())
		.unwrap();
	let file = rss
		.client
		.upload_file(file, b"Content to rename")
		.await
		.unwrap();

	let file_path: FfiId = format!(
		"{}/{}/{}",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap(),
		file.name().unwrap()
	)
	.into();

	// Update database
	let parent_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	db.update_dir_children(parent_path.clone()).await.unwrap();

	// Rename the file
	let new_name = "new_name.txt".to_string();
	let new_file_path = db
		.rename_item(file_path.clone(), new_name.clone())
		.await
		.unwrap()
		.unwrap();

	// Verify the new path is correct
	let expected_new_path: FfiId = format!("{}/{}", parent_path.0, new_name).into();
	assert_eq!(new_file_path.id.0, expected_new_path.0);

	// Verify old file path no longer exists
	assert!(db.query_item(&file_path).unwrap().is_none());

	// Verify file exists at new path with new name
	let renamed_file = db.query_item(&new_file_path.id).unwrap();
	assert!(renamed_file.is_some());
	match renamed_file.unwrap() {
		FfiObject::File(f) => {
			assert_eq!(f.meta.unwrap().name, new_name);
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
		assert_eq!(f.meta.clone().unwrap().name, new_name);
	}
}

#[shared_test_runtime]
pub async fn test_rename_item_directory_success() {
	let (db, rss) = get_db_resources().await;

	// Create a test directory with some content
	let dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "old_dir_name")
		.await
		.unwrap();

	// Add a file to the directory to verify contents are preserved
	let file_in_dir = rss
		.client
		.make_file_builder("content.txt", dir.uuid())
		.unwrap();
	let file_in_dir = rss
		.client
		.upload_file(file_in_dir, b"Directory content")
		.await
		.unwrap();

	let dir_path: FfiId = format!(
		"{}/{}/{}",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap(),
		dir.name().unwrap()
	)
	.into();

	// Update database
	let parent_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
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
	let expected_new_path: FfiId = format!("{}/{}", parent_path.0, new_name).into();
	assert_eq!(new_dir_path.id.0, expected_new_path.0);

	// Verify old directory path no longer exists
	assert!(db.query_item(&dir_path).unwrap().is_none());

	// Verify directory exists at new path with new name
	let renamed_dir = db.query_item(&new_dir_path.id).unwrap();
	assert!(renamed_dir.is_some());
	match renamed_dir.unwrap() {
		FfiObject::Dir(d) => {
			assert_eq!(d.meta.unwrap().name, new_name);
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
		assert_eq!(d.meta.clone().unwrap().name, new_name);
	}

	// Verify directory contents are preserved
	db.update_dir_children(new_dir_path.id.clone())
		.await
		.unwrap();
	let dir_contents = db
		.query_dir_children(&new_dir_path.id, None)
		.unwrap()
		.unwrap();
	assert_eq!(dir_contents.objects.len(), 1);

	let file_in_renamed_dir = dir_contents.objects.iter().find(
		|obj| matches!(obj, FfiNonRootObject::File(f) if f.uuid == file_in_dir.uuid().to_string()),
	);
	assert!(file_in_renamed_dir.is_some());
}

#[shared_test_runtime]
pub async fn test_rename_item_file_extension_change() {
	let (db, rss) = get_db_resources().await;

	// Create a text file
	let file = rss
		.client
		.make_file_builder("document.txt", rss.dir.uuid())
		.unwrap();
	let file = rss.client.upload_file(file, b"Text content").await.unwrap();

	let file_path: FfiId = format!(
		"{}/{}/{}",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap(),
		file.name().unwrap()
	)
	.into();

	let parent_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	db.update_dir_children(parent_path.clone()).await.unwrap();

	// Rename to change extension
	let new_name = "document.md".to_string();
	let new_file_path = db
		.rename_item(file_path.clone(), new_name.clone())
		.await
		.unwrap()
		.unwrap();

	// Verify rename worked
	let renamed_file = db.query_item(&new_file_path.id.clone()).unwrap();
	assert!(renamed_file.is_some());
	match renamed_file.unwrap() {
		FfiObject::File(f) => {
			assert_eq!(f.meta.unwrap().name, new_name);
			assert_eq!(f.uuid, file.uuid().to_string());
		}
		_ => panic!("Expected file object"),
	}
}

#[shared_test_runtime]
pub async fn test_rename_item_same_name() {
	let (db, rss) = get_db_resources().await;

	// Create a test file
	let file = rss
		.client
		.make_file_builder("same_name.txt", rss.dir.uuid())
		.unwrap();
	let file = rss.client.upload_file(file, b"Content").await.unwrap();

	let file_path: FfiId = format!(
		"{}/{}/{}",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap(),
		file.name().unwrap()
	)
	.into();

	let parent_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	db.update_dir_children(parent_path.clone()).await.unwrap();

	// Rename to the same name
	let same_name = file.name().unwrap().to_string();
	let new_file_path = db
		.rename_item(file_path.clone(), same_name.clone())
		.await
		.unwrap();

	// Path should be the same
	assert!(new_file_path.is_none());

	// File should still exist and be queryable
	let file_result = db.query_item(&file_path).unwrap();
	assert!(file_result.is_some());
	match file_result.unwrap() {
		FfiObject::File(f) => {
			assert_eq!(f.meta.unwrap().name, same_name);
			assert_eq!(f.uuid, file.uuid().to_string());
		}
		_ => panic!("Expected file object"),
	}
}

#[shared_test_runtime]
pub async fn test_rename_item_nonexistent_file() {
	let (db, rss) = get_db_resources().await;

	let nonexistent_path: FfiId = format!(
		"{}/{}/nonexistent.txt",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap()
	)
	.into();

	let parent_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	db.update_dir_children(parent_path).await.unwrap();

	// Try to rename non-existent file
	let result = db
		.rename_item(nonexistent_path, "new_name.txt".to_string())
		.await;

	assert!(result.is_err());
	let error_message = format!("{}", result.unwrap_err());
	assert!(error_message.contains("does not point to an item"));
}

#[shared_test_runtime]
pub async fn test_rename_item_root_directory_error() {
	let (db, _rss) = get_db_resources().await;

	let root_path: FfiId = db.root_uuid().unwrap().into();

	// Try to rename root directory
	let result = db.rename_item(root_path, "new_root_name".to_string()).await;

	assert!(result.is_err());
	let error_message = format!("{}", result.unwrap_err());
	assert!(error_message.contains("Cannot rename item"));
}

#[shared_test_runtime]
pub async fn test_rename_item_invalid_path() {
	let (db, _rss) = get_db_resources().await;

	let invalid_path: FfiId = "not-a-uuid/invalid/path".into();

	// Try to rename with invalid path
	let result = db
		.rename_item(invalid_path, "new_name.txt".to_string())
		.await;

	assert!(result.is_err());
	// Should fail with UUID parsing error
}

#[shared_test_runtime]
pub async fn test_rename_item_empty_name() {
	let (db, rss) = get_db_resources().await;

	// Create a test file
	let file = rss
		.client
		.make_file_builder("test_file.txt", rss.dir.uuid())
		.unwrap();
	let file = rss.client.upload_file(file, b"Content").await.unwrap();

	let file_path: FfiId = format!(
		"{}/{}/{}",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap(),
		file.name().unwrap()
	)
	.into();

	let parent_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	db.update_dir_children(parent_path).await.unwrap();

	// Try to rename to empty string
	let result = db.rename_item(file_path, "".to_string()).await;

	let err = result.unwrap_err();
	assert!(err.to_string().contains("filename is empty"));
}

#[shared_test_runtime]
pub async fn test_rename_item_special_characters() {
	let (db, rss) = get_db_resources().await;

	// Create a test file
	let file = rss
		.client
		.make_file_builder("normal_name.txt", rss.dir.uuid())
		.unwrap();
	let file = rss.client.upload_file(file, b"Content").await.unwrap();

	let file_path: FfiId = format!(
		"{}/{}/{}",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap(),
		file.name().unwrap()
	)
	.into();

	let parent_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
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
			let renamed_file = db.query_item(&new_path.id).unwrap();
			assert!(renamed_file.is_some());

			match renamed_file.unwrap() {
				FfiObject::File(f) => {
					assert_eq!(f.meta.unwrap().name, special_name);
					assert_eq!(f.uuid, file.uuid().to_string());
				}
				_ => panic!("Expected file object"),
			}

			// Reset for next test by renaming back
			let _ = db
				.rename_item(new_path.id, file.name().unwrap().to_string())
				.await;
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

#[shared_test_runtime]
pub async fn test_rename_item_name_collision() {
	let (db, rss) = get_db_resources().await;

	// Create two files in the same directory
	let file1 = rss
		.client
		.make_file_builder("file1.txt", rss.dir.uuid())
		.unwrap();
	let file1 = rss.client.upload_file(file1, b"Content 1").await.unwrap();

	let file2 = rss
		.client
		.make_file_builder("file2.txt", rss.dir.uuid())
		.unwrap();
	let file2 = rss.client.upload_file(file2, b"Content 2").await.unwrap();

	let file1_path: FfiId = format!(
		"{}/{}/{}",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap(),
		file1.name().unwrap()
	)
	.into();

	let parent_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	db.update_dir_children(parent_path.clone()).await.unwrap();

	// Try to rename file1 to file2's name (collision)
	let result = db
		.rename_item(file1_path, file2.name().unwrap().to_string())
		.await;

	assert!(result.is_err());
	assert!(
		result
			.unwrap_err()
			.to_string()
			.contains("File with the same name already exists at destination")
	);
}

#[shared_test_runtime]
pub async fn test_rename_item_nested_file() {
	let (db, rss) = get_db_resources().await;

	// Create nested directory structure
	let level1 = rss
		.client
		.create_dir(&(&rss.dir).into(), "level1")
		.await
		.unwrap();

	let level2 = rss
		.client
		.create_dir(&(&level1).into(), "level2")
		.await
		.unwrap();

	let nested_file = rss
		.client
		.make_file_builder("nested_file.txt", level2.uuid())
		.unwrap();
	let nested_file = rss
		.client
		.upload_file(nested_file, b"Nested content")
		.await
		.unwrap();

	// Set up paths
	let base_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let level1_path: FfiId = format!("{}/level1", base_path.0).into();
	let level2_path: FfiId = format!("{}/level2", level1_path.0).into();
	let file_path: FfiId = format!("{}/{}", level2_path.0, nested_file.name().unwrap()).into();

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
	let expected_new_path: FfiId = format!("{}/{}", level2_path.0, new_name).into();
	assert_eq!(new_file_path.id.0, expected_new_path.0);

	// Verify old path no longer exists
	assert!(db.query_item(&file_path).unwrap().is_none());

	// Verify file exists at new path
	let renamed_file = db.query_item(&new_file_path.id).unwrap();
	assert!(renamed_file.is_some());
	match renamed_file.unwrap() {
		FfiObject::File(f) => {
			assert_eq!(f.meta.unwrap().name, new_name);
			assert_eq!(f.uuid, nested_file.uuid().to_string());
		}
		_ => panic!("Expected file object"),
	}
}

#[shared_test_runtime]
pub async fn test_rename_item_long_name() {
	let (db, rss) = get_db_resources().await;

	// Create a test file
	let file = rss
		.client
		.make_file_builder("short.txt", rss.dir.uuid())
		.unwrap();
	let file = rss.client.upload_file(file, b"Content").await.unwrap();

	let file_path: FfiId = format!(
		"{}/{}/{}",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap(),
		file.name().unwrap()
	)
	.into();

	let parent_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	db.update_dir_children(parent_path).await.unwrap();

	// Try to rename to a very long name
	let long_name = "a".repeat(251) + ".txt"; // longest name is now 255 chars including extension
	let result = db.rename_item(file_path, long_name.clone()).await;

	let new_path = result.unwrap().unwrap();
	let renamed_file = db.query_item(&new_path.id).unwrap();
	assert!(renamed_file.is_some());

	match renamed_file.unwrap() {
		FfiObject::File(f) => {
			// Name might be truncated by the system
			assert!(!f.meta.unwrap().name.is_empty());
			assert_eq!(f.uuid, file.uuid().to_string());
		}
		_ => panic!("Expected file object"),
	}
}

#[shared_test_runtime]
pub async fn test_rename_item_multiple_renames() {
	let (db, rss) = get_db_resources().await;

	// Create a test file
	let file = rss
		.client
		.make_file_builder("original.txt", rss.dir.uuid())
		.unwrap();
	let file = rss.client.upload_file(file, b"Content").await.unwrap();

	let mut current_path: FfiId = format!(
		"{}/{}/{}",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap(),
		file.name().unwrap()
	)
	.into();

	let parent_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
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
		let renamed_file = db.query_item(&new_path.id).unwrap();
		assert!(renamed_file.is_some());
		match renamed_file.unwrap() {
			FfiObject::File(f) => {
				assert_eq!(f.meta.unwrap().name, name);
				assert_eq!(f.uuid, file.uuid().to_string());
			}
			_ => panic!("Expected file object"),
		}

		// Update current path for next iteration
		current_path = new_path.id;
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
		assert_eq!(f.meta.clone().unwrap().name, "final_name.txt");
	}
}

#[shared_test_runtime]
pub async fn test_get_all_descendant_paths_empty_directory() {
	let (db, rss) = get_db_resources().await;

	// Create an empty directory
	let empty_dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "empty_dir")
		.await
		.unwrap();

	let dir_path: FfiId = format!(
		"{}/{}/{}",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap(),
		empty_dir.name().unwrap()
	)
	.into();

	// Update database to include the directory
	let parent_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	db.update_dir_children(parent_path).await.unwrap();
	db.update_dir_children(dir_path.clone()).await.unwrap();

	// Get descendant paths - should be empty
	let descendant_paths = db.get_all_descendant_paths(&dir_path).unwrap();
	assert_eq!(descendant_paths.len(), 0);
}

#[shared_test_runtime]
pub async fn test_get_all_descendant_paths_files_only() {
	let (db, rss) = get_db_resources().await;

	// Create a directory with multiple files
	let test_dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "files_dir")
		.await
		.unwrap();

	// Create several files
	let file1 = rss
		.client
		.make_file_builder("file1.txt", test_dir.uuid())
		.unwrap();
	let file1 = rss.client.upload_file(file1, b"Content 1").await.unwrap();

	let file2 = rss
		.client
		.make_file_builder("file2.txt", test_dir.uuid())
		.unwrap();
	let file2 = rss.client.upload_file(file2, b"Content 2").await.unwrap();

	let file3 = rss
		.client
		.make_file_builder("file3.md", test_dir.uuid())
		.unwrap();
	let file3 = rss
		.client
		.upload_file(file3, b"Markdown content")
		.await
		.unwrap();

	let dir_path: FfiId = format!(
		"{}/{}/{}",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap(),
		test_dir.name().unwrap()
	)
	.into();

	// Update database
	let parent_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	db.update_dir_children(parent_path).await.unwrap();
	db.update_dir_children(dir_path.clone()).await.unwrap();

	// Get descendant paths
	let descendant_paths = db.get_all_descendant_paths(&dir_path).unwrap();
	assert_eq!(descendant_paths.len(), 3);

	// Verify all file paths are present
	let expected_paths = vec![
		format!("{}/{}", dir_path.0, file1.name().unwrap()),
		format!("{}/{}", dir_path.0, file2.name().unwrap()),
		format!("{}/{}", dir_path.0, file3.name().unwrap()),
	];

	for expected_path in expected_paths {
		let found = descendant_paths.iter().any(|p| p.0 == expected_path);
		assert!(
			found,
			"Expected path {expected_path} not found in descendant paths"
		);
	}
}

#[shared_test_runtime]
pub async fn test_get_all_descendant_paths_directories_only() {
	let (db, rss) = get_db_resources().await;

	// Create a directory with subdirectories
	let test_dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "dirs_dir")
		.await
		.unwrap();

	let subdir1 = rss
		.client
		.create_dir(&(&test_dir).into(), "subdir1")
		.await
		.unwrap();

	let subdir2 = rss
		.client
		.create_dir(&(&test_dir).into(), "subdir2")
		.await
		.unwrap();

	let subdir3 = rss
		.client
		.create_dir(&(&test_dir).into(), "subdir3")
		.await
		.unwrap();

	let dir_path: FfiId = format!(
		"{}/{}/{}",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap(),
		test_dir.name().unwrap()
	)
	.into();

	// Update database
	let parent_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	db.update_dir_children(parent_path).await.unwrap();
	db.update_dir_children(dir_path.clone()).await.unwrap();

	// Get descendant paths
	let descendant_paths = db.get_all_descendant_paths(&dir_path).unwrap();
	assert_eq!(descendant_paths.len(), 3);

	// Verify all directory paths are present
	let expected_paths = vec![
		format!("{}/{}", dir_path.0, subdir1.name().unwrap()),
		format!("{}/{}", dir_path.0, subdir2.name().unwrap()),
		format!("{}/{}", dir_path.0, subdir3.name().unwrap()),
	];

	for expected_path in expected_paths {
		let found = descendant_paths.iter().any(|p| p.0 == expected_path);
		assert!(
			found,
			"Expected path {expected_path} not found in descendant paths"
		);
	}
}

#[shared_test_runtime]
pub async fn test_get_all_descendant_paths_mixed_content() {
	let (db, rss) = get_db_resources().await;

	// Create a directory with mixed files and subdirectories
	let test_dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "mixed_dir")
		.await
		.unwrap();

	// Create files
	let file1 = rss
		.client
		.make_file_builder("readme.txt", test_dir.uuid())
		.unwrap();
	let file1 = rss
		.client
		.upload_file(file1, b"Readme content")
		.await
		.unwrap();

	// Create subdirectory
	let subdir = rss
		.client
		.create_dir(&(&test_dir).into(), "subfolder")
		.await
		.unwrap();

	// Create another file
	let file2 = rss
		.client
		.make_file_builder("config.json", test_dir.uuid())
		.unwrap();
	let file2 = rss.client.upload_file(file2, b"{}").await.unwrap();

	let dir_path: FfiId = format!(
		"{}/{}/{}",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap(),
		test_dir.name().unwrap()
	)
	.into();

	// Update database
	let parent_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	db.update_dir_children(parent_path).await.unwrap();
	db.update_dir_children(dir_path.clone()).await.unwrap();

	// Get descendant paths
	let descendant_paths = db.get_all_descendant_paths(&dir_path).unwrap();
	assert_eq!(descendant_paths.len(), 3);

	// Verify all paths are present
	let expected_paths = vec![
		format!("{}/{}", dir_path.0, file1.name().unwrap()),
		format!("{}/{}", dir_path.0, subdir.name().unwrap()),
		format!("{}/{}", dir_path.0, file2.name().unwrap()),
	];

	for expected_path in expected_paths {
		let found = descendant_paths.iter().any(|p| p.0 == expected_path);
		assert!(
			found,
			"Expected path {expected_path} not found in descendant paths"
		);
	}
}

#[shared_test_runtime]
pub async fn test_get_all_descendant_paths_nested_structure() {
	let (db, rss) = get_db_resources().await;

	// Create a deeply nested structure
	let level1 = rss
		.client
		.create_dir(&(&rss.dir).into(), "level1")
		.await
		.unwrap();

	let level2 = rss
		.client
		.create_dir(&(&level1).into(), "level2")
		.await
		.unwrap();

	let level3 = rss
		.client
		.create_dir(&(&level2).into(), "level3")
		.await
		.unwrap();

	// Add files at different levels
	let file_l1 = rss
		.client
		.make_file_builder("file_level1.txt", level1.uuid())
		.unwrap();
	let file_l1 = rss
		.client
		.upload_file(file_l1, b"Level 1 content")
		.await
		.unwrap();

	let file_l2 = rss
		.client
		.make_file_builder("file_level2.txt", level2.uuid())
		.unwrap();
	let file_l2 = rss
		.client
		.upload_file(file_l2, b"Level 2 content")
		.await
		.unwrap();

	let file_l3 = rss
		.client
		.make_file_builder("file_level3.txt", level3.uuid())
		.unwrap();
	let file_l3 = rss
		.client
		.upload_file(file_l3, b"Level 3 content")
		.await
		.unwrap();

	// Set up paths
	let base_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let level1_path: FfiId = format!("{}/{}", base_path.0, level1.name().unwrap()).into();
	let level2_path: FfiId = format!("{}/{}", level1_path.0, level2.name().unwrap()).into();
	let level3_path: FfiId = format!("{}/{}", level2_path.0, level3.name().unwrap()).into();

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
		format!("{}/{}", level1_path.0, file_l1.name().unwrap()), // Direct file in level1
		format!("{}/{}", level1_path.0, level2.name().unwrap()),  // level2 directory
		format!(
			"{}/{}/{}",
			level1_path.0,
			level2.name().unwrap(),
			file_l2.name().unwrap()
		), // File in level2
		format!(
			"{}/{}/{}",
			level1_path.0,
			level2.name().unwrap(),
			level3.name().unwrap()
		), // level3 directory
		format!(
			"{}/{}/{}/{}",
			level1_path.0,
			level2.name().unwrap(),
			level3.name().unwrap(),
			file_l3.name().unwrap()
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

#[shared_test_runtime]
pub async fn test_get_all_descendant_paths_complex_nested_structure() {
	let (db, rss) = get_db_resources().await;

	// Create a complex structure with multiple branches
	let root_dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "complex_root")
		.await
		.unwrap();

	// Branch 1: documents
	let docs_dir = rss
		.client
		.create_dir(&(&root_dir).into(), "documents")
		.await
		.unwrap();

	let doc_file = rss
		.client
		.make_file_builder("readme.md", docs_dir.uuid())
		.unwrap();
	let doc_file = rss
		.client
		.upload_file(doc_file, b"Documentation")
		.await
		.unwrap();

	// Branch 2: images with subdirectories
	let images_dir = rss
		.client
		.create_dir(&(&root_dir).into(), "images")
		.await
		.unwrap();

	let thumbnails_dir = rss
		.client
		.create_dir(&(&images_dir).into(), "thumbnails")
		.await
		.unwrap();

	let thumb_file = rss
		.client
		.make_file_builder("thumb1.jpg", thumbnails_dir.uuid())
		.unwrap();
	let thumb_file = rss
		.client
		.upload_file(thumb_file, b"thumbnail data")
		.await
		.unwrap();

	let full_image = rss
		.client
		.make_file_builder("photo.png", images_dir.uuid())
		.unwrap();
	let full_image = rss
		.client
		.upload_file(full_image, b"image data")
		.await
		.unwrap();

	// Branch 3: config file at root level
	let config_file = rss
		.client
		.make_file_builder("config.json", root_dir.uuid())
		.unwrap();
	let config_file = rss.client.upload_file(config_file, b"{}").await.unwrap();

	// Set up paths
	let base_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let root_path: FfiId = format!("{}/{}", base_path.0, root_dir.name().unwrap()).into();
	let docs_path: FfiId = format!("{}/{}", root_path.0, docs_dir.name().unwrap()).into();
	let images_path: FfiId = format!("{}/{}", root_path.0, images_dir.name().unwrap()).into();
	let thumbnails_path: FfiId =
		format!("{}/{}", images_path.0, thumbnails_dir.name().unwrap()).into();

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
		format!("{}/{}", root_path.0, config_file.name().unwrap()),
		format!("{}/{}", root_path.0, docs_dir.name().unwrap()),
		format!(
			"{}/{}/{}",
			root_path.0,
			docs_dir.name().unwrap(),
			doc_file.name().unwrap()
		),
		format!("{}/{}", root_path.0, images_dir.name().unwrap()),
		format!(
			"{}/{}/{}",
			root_path.0,
			images_dir.name().unwrap(),
			full_image.name().unwrap()
		),
		format!(
			"{}/{}/{}",
			root_path.0,
			images_dir.name().unwrap(),
			thumbnails_dir.name().unwrap()
		),
		format!(
			"{}/{}/{}/{}",
			root_path.0,
			images_dir.name().unwrap(),
			thumbnails_dir.name().unwrap(),
			thumb_file.name().unwrap()
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

#[shared_test_runtime]
pub async fn test_get_all_descendant_paths_nonexistent_path() {
	let (db, rss) = get_db_resources().await;

	let nonexistent_path: FfiId = format!(
		"{}/{}/nonexistent_dir",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap()
	)
	.into();

	// Get descendant paths for non-existent directory
	let descendant_paths = db.get_all_descendant_paths(&nonexistent_path).unwrap();

	// Should return empty vector for non-existent path
	assert_eq!(descendant_paths.len(), 0);
}

#[shared_test_runtime]
pub async fn test_get_all_descendant_paths_invalid_path() {
	let (db, _rss) = get_db_resources().await;

	let invalid_path: FfiId = "not-a-uuid/invalid/path".into();

	// Should fail with UUID parsing error
	let result = db.get_all_descendant_paths(&invalid_path);
	assert!(result.is_err());
}

#[shared_test_runtime]
pub async fn test_get_all_descendant_paths_file_path() {
	let (db, rss) = get_db_resources().await;

	// Create a file
	let file = rss
		.client
		.make_file_builder("test_file.txt", rss.dir.uuid())
		.unwrap();
	let file = rss.client.upload_file(file, b"Test content").await.unwrap();

	let file_path: FfiId = format!(
		"{}/{}/{}",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap(),
		file.name().unwrap()
	)
	.into();

	// Update database
	let parent_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	db.update_dir_children(parent_path).await.unwrap();

	// Get descendant paths for a file (files have no descendants)
	let descendant_paths = db.get_all_descendant_paths(&file_path).unwrap();

	// Should return empty vector for file paths
	assert_eq!(descendant_paths.len(), 0);
}

#[shared_test_runtime]
pub async fn test_get_all_descendant_paths_root_directory() {
	let (db, rss) = get_db_resources().await;

	// Create some content in the test root directory
	let file_in_root = rss
		.client
		.make_file_builder("root_file.txt", rss.dir.uuid())
		.unwrap();
	let file_in_root = rss
		.client
		.upload_file(file_in_root, b"Root content")
		.await
		.unwrap();

	let subdir_in_root = rss
		.client
		.create_dir(&(&rss.dir).into(), "root_subdir")
		.await
		.unwrap();

	// Use the test directory as root (since we can't access the absolute root easily)
	let root_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();

	// Update database
	let parent_path: FfiId = db.root_uuid().unwrap().into();
	db.update_dir_children(parent_path).await.unwrap();
	db.update_dir_children(root_path.clone()).await.unwrap();

	// Get descendant paths from our test "root"
	let descendant_paths = db.get_all_descendant_paths(&root_path).unwrap();

	// Should include both the file and directory
	assert_eq!(descendant_paths.len(), 2);

	println!("Descendant paths: {descendant_paths:?}");

	let expected_paths = vec![
		format!("{}/{}", root_path.0, file_in_root.name().unwrap()),
		format!("{}/{}", root_path.0, subdir_in_root.name().unwrap()),
	];

	for expected_path in expected_paths {
		let found = descendant_paths.iter().any(|p| p.0 == expected_path);
		assert!(
			found,
			"Expected path {expected_path} not found in descendant paths"
		);
	}
}

#[shared_test_runtime]
pub async fn test_get_all_descendant_paths_partial_database_state() {
	let (db, rss) = get_db_resources().await;

	// Create nested structure but only update some levels in database
	let level1 = rss
		.client
		.create_dir(&(&rss.dir).into(), "level1")
		.await
		.unwrap();

	let level2 = rss
		.client
		.create_dir(&(&level1).into(), "level2")
		.await
		.unwrap();

	let file_l2 = rss
		.client
		.make_file_builder("file_level2.txt", level2.uuid())
		.unwrap();
	rss.client
		.upload_file(file_l2, b"Level 2 content")
		.await
		.unwrap();

	// Only update the base and level1, not level2
	let base_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let level1_path: FfiId = format!("{}/{}", base_path.0, level1.name().unwrap()).into();

	db.update_dir_children(base_path).await.unwrap();
	db.update_dir_children(level1_path.clone()).await.unwrap();
	// Note: NOT updating level2 contents

	// Get descendant paths from level1
	let descendant_paths = db.get_all_descendant_paths(&level1_path).unwrap();

	// Should only include level2 directory, not its contents since they're not in database
	assert_eq!(descendant_paths.len(), 1);

	let expected_level2_path = format!("{}/{}", level1_path.0, level2.name().unwrap());
	assert_eq!(descendant_paths[0].0, expected_level2_path);
}

#[shared_test_runtime]
pub async fn test_get_all_descendant_paths_special_characters_in_names() {
	let (db, rss) = get_db_resources().await;

	// Create items with special characters in names
	let special_dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "dir with spaces")
		.await
		.unwrap();

	let special_file1 = rss
		.client
		.make_file_builder("file-with-dashes.txt", special_dir.uuid())
		.unwrap();
	let special_file1 = rss
		.client
		.upload_file(special_file1, b"Content 1")
		.await
		.unwrap();

	let special_file2 = rss
		.client
		.make_file_builder("file_with_underscores.txt", special_dir.uuid())
		.unwrap();
	let special_file2 = rss
		.client
		.upload_file(special_file2, b"Content 2")
		.await
		.unwrap();

	let unicode_file = rss
		.client
		.make_file_builder("файл.txt", special_dir.uuid())
		.unwrap();
	let unicode_file = rss
		.client
		.upload_file(unicode_file, b"Unicode content")
		.await
		.unwrap();

	let dir_path: FfiId = format!(
		"{}/{}/{}",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap(),
		special_dir.name().unwrap()
	)
	.into();

	// Update database
	let parent_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	db.update_dir_children(parent_path).await.unwrap();
	db.update_dir_children(dir_path.clone()).await.unwrap();

	// Get descendant paths
	let descendant_paths = db.get_all_descendant_paths(&dir_path).unwrap();
	assert_eq!(descendant_paths.len(), 3);

	// Verify all special-named files are present
	let expected_paths = vec![
		format!("{}/{}", dir_path.0, special_file1.name().unwrap()),
		format!("{}/{}", dir_path.0, special_file2.name().unwrap()),
		format!("{}/{}", dir_path.0, unicode_file.name().unwrap()),
	];

	for expected_path in expected_paths {
		let found = descendant_paths.iter().any(|p| p.0 == expected_path);
		assert!(
			found,
			"Expected path {expected_path} not found in descendant paths"
		);
	}
}

#[shared_test_runtime]
pub async fn test_get_all_descendant_paths_path_ordering() {
	let (db, rss) = get_db_resources().await;

	// Create structure to test path ordering
	let test_dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "ordered_test")
		.await
		.unwrap();

	// Create items in a specific order to see how paths are returned
	let b_file = rss
		.client
		.make_file_builder("b_file.txt", test_dir.uuid())
		.unwrap();
	let b_file = rss.client.upload_file(b_file, b"B content").await.unwrap();

	let a_dir = rss
		.client
		.create_dir(&(&test_dir).into(), "a_directory")
		.await
		.unwrap();

	let c_file = rss
		.client
		.make_file_builder("c_file.txt", test_dir.uuid())
		.unwrap();
	let c_file = rss.client.upload_file(c_file, b"C content").await.unwrap();

	// Add file to subdirectory
	let nested_file = rss
		.client
		.make_file_builder("nested.txt", a_dir.uuid())
		.unwrap();
	let nested_file = rss
		.client
		.upload_file(nested_file, b"Nested content")
		.await
		.unwrap();

	let dir_path: FfiId = format!(
		"{}/{}/{}",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap(),
		test_dir.name().unwrap()
	)
	.into();

	// Update database
	let parent_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let a_dir_path: FfiId = format!("{}/{}", dir_path.0, a_dir.name().unwrap()).into();

	db.update_dir_children(parent_path).await.unwrap();
	db.update_dir_children(dir_path.clone()).await.unwrap();
	db.update_dir_children(a_dir_path).await.unwrap();

	// Get descendant paths
	let descendant_paths = db.get_all_descendant_paths(&dir_path).unwrap();
	assert_eq!(descendant_paths.len(), 4);

	// Verify all items are present (order may vary based on SQL query order)
	let expected_paths = vec![
		format!("{}/{}", dir_path.0, b_file.name().unwrap()),
		format!("{}/{}", dir_path.0, a_dir.name().unwrap()),
		format!("{}/{}", dir_path.0, c_file.name().unwrap()),
		format!(
			"{}/{}/{}",
			dir_path.0,
			a_dir.name().unwrap(),
			nested_file.name().unwrap()
		),
	];

	for expected_path in expected_paths {
		let found = descendant_paths.iter().any(|p| p.0 == expected_path);
		assert!(
			found,
			"Expected path {expected_path} not found in descendant paths"
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

#[shared_test_runtime]
pub async fn test_get_all_descendant_paths_large_directory() {
	let (db, rss) = get_db_resources().await;

	// Create a directory with many files to test performance and correctness
	let large_dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "large_directory")
		.await
		.unwrap();

	let mut created_files = Vec::new();

	// Create 20 files
	for i in 0..20 {
		let file = rss
			.client
			.make_file_builder(&format!("file_{i:02}.txt"), large_dir.uuid())
			.unwrap();
		let file = rss
			.client
			.upload_file(file, format!("Content {i}").as_bytes())
			.await
			.unwrap();
		created_files.push(file);
	}

	// Create a few subdirectories
	let subdir1 = rss
		.client
		.create_dir(&(&large_dir).into(), "subdir_01")
		.await
		.unwrap();

	let subdir2 = rss
		.client
		.create_dir(&(&large_dir).into(), "subdir_02")
		.await
		.unwrap();

	// Add files to subdirectories
	let nested_file1 = rss
		.client
		.make_file_builder("nested_1.txt", subdir1.uuid())
		.unwrap();
	let nested_file1 = rss
		.client
		.upload_file(nested_file1, b"Nested content 1")
		.await
		.unwrap();

	let nested_file2 = rss
		.client
		.make_file_builder("nested_2.txt", subdir2.uuid())
		.unwrap();
	let nested_file2 = rss
		.client
		.upload_file(nested_file2, b"Nested content 2")
		.await
		.unwrap();

	let dir_path: FfiId = format!(
		"{}/{}/{}",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap(),
		large_dir.name().unwrap()
	)
	.into();

	// Update database
	let parent_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let subdir1_path: FfiId = format!("{}/{}", dir_path.0, subdir1.name().unwrap()).into();
	let subdir2_path: FfiId = format!("{}/{}", dir_path.0, subdir2.name().unwrap()).into();

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
		let expected_path = format!("{}/{}", dir_path.0, file.name().unwrap());
		let found = descendant_paths.iter().any(|p| p.0 == expected_path);
		assert!(found, "Expected file path {expected_path} not found");
	}

	// Verify subdirectories are present
	let subdir1_expected = format!("{}/{}", dir_path.0, subdir1.name().unwrap());
	let subdir2_expected = format!("{}/{}", dir_path.0, subdir2.name().unwrap());
	assert!(descendant_paths.iter().any(|p| p.0 == subdir1_expected));
	assert!(descendant_paths.iter().any(|p| p.0 == subdir2_expected));

	// Verify nested files are present
	let nested1_expected = format!(
		"{}/{}/{}",
		dir_path.0,
		subdir1.name().unwrap(),
		nested_file1.name().unwrap()
	);
	let nested2_expected = format!(
		"{}/{}/{}",
		dir_path.0,
		subdir2.name().unwrap(),
		nested_file2.name().unwrap()
	);
	assert!(descendant_paths.iter().any(|p| p.0 == nested1_expected));
	assert!(descendant_paths.iter().any(|p| p.0 == nested2_expected));
}

#[shared_test_runtime]
pub async fn test_get_all_descendant_paths_empty_names() {
	let (db, rss) = get_db_resources().await;

	// Test edge case with empty or unusual names (if the SDK allows them)
	let test_dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "edge_case_dir")
		.await
		.unwrap();

	// Try to create files with edge case names
	let normal_file = rss
		.client
		.make_file_builder("normal.txt", test_dir.uuid())
		.unwrap();
	let normal_file = rss
		.client
		.upload_file(normal_file, b"Normal content")
		.await
		.unwrap();

	// Test with just extension
	let dot_file = rss
		.client
		.make_file_builder(".hidden", test_dir.uuid())
		.unwrap();
	let dot_file = rss
		.client
		.upload_file(dot_file, b"Hidden file content")
		.await
		.unwrap();

	let dir_path: FfiId = format!(
		"{}/{}/{}",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap(),
		test_dir.name().unwrap()
	)
	.into();

	// Update database
	let parent_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	db.update_dir_children(parent_path).await.unwrap();
	db.update_dir_children(dir_path.clone()).await.unwrap();

	// Get descendant paths
	let descendant_paths = db.get_all_descendant_paths(&dir_path).unwrap();
	assert_eq!(descendant_paths.len(), 2);

	// Verify both files are present
	let expected_paths = vec![
		format!("{}/{}", dir_path.0, normal_file.name().unwrap()),
		format!("{}/{}", dir_path.0, dot_file.name().unwrap()),
	];

	for expected_path in expected_paths {
		let found = descendant_paths.iter().any(|p| p.0 == expected_path);
		assert!(
			found,
			"Expected path {expected_path} not found in descendant paths"
		);
	}
}

#[shared_test_runtime]
pub async fn test_get_all_descendant_paths_concurrent_modifications() {
	let (db, rss) = get_db_resources().await;

	// Create initial structure
	let test_dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "concurrent_test")
		.await
		.unwrap();

	let initial_file = rss
		.client
		.make_file_builder("initial.txt", test_dir.uuid())
		.unwrap();
	let initial_file = rss
		.client
		.upload_file(initial_file, b"Initial content")
		.await
		.unwrap();

	let dir_path: FfiId = format!(
		"{}/{}/{}",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap(),
		test_dir.name().unwrap()
	)
	.into();

	// Update database
	let parent_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	db.update_dir_children(parent_path).await.unwrap();
	db.update_dir_children(dir_path.clone()).await.unwrap();

	// Get initial descendant paths
	let initial_paths = db.get_all_descendant_paths(&dir_path).unwrap();
	assert_eq!(initial_paths.len(), 1);

	// Add more files after initial query
	let additional_file = rss
		.client
		.make_file_builder("additional.txt", test_dir.uuid())
		.unwrap();
	let additional_file = rss
		.client
		.upload_file(additional_file, b"Additional content")
		.await
		.unwrap();

	// Update database again
	db.update_dir_children(dir_path.clone()).await.unwrap();

	// Get updated descendant paths
	let updated_paths = db.get_all_descendant_paths(&dir_path).unwrap();
	assert_eq!(updated_paths.len(), 2);

	// Verify both files are present
	let expected_paths = vec![
		format!("{}/{}", dir_path.0, initial_file.name().unwrap()),
		format!("{}/{}", dir_path.0, additional_file.name().unwrap()),
	];

	for expected_path in expected_paths {
		let found = updated_paths.iter().any(|p| p.0 == expected_path);
		assert!(
			found,
			"Expected path {expected_path} not found in updated descendant paths"
		);
	}
}

#[shared_test_runtime]
pub async fn test_get_all_descendant_paths_path_format_consistency() {
	let (db, rss) = get_db_resources().await;

	// Create nested structure to test path format consistency
	let root_dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "path_test")
		.await
		.unwrap();

	let sub_dir = rss
		.client
		.create_dir(&(&root_dir).into(), "subdir")
		.await
		.unwrap();

	let file_in_sub = rss
		.client
		.make_file_builder("file.txt", sub_dir.uuid())
		.unwrap();
	let file_in_sub = rss
		.client
		.upload_file(file_in_sub, b"File content")
		.await
		.unwrap();

	let dir_path: FfiId = format!(
		"{}/{}/{}",
		db.root_uuid().unwrap(),
		rss.dir.name().unwrap(),
		root_dir.name().unwrap()
	)
	.into();

	// Update database
	let parent_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let sub_dir_path: FfiId = format!("{}/{}", dir_path.0, sub_dir.name().unwrap()).into();

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
	let expected_subdir = format!("{}/{}", dir_path.0, sub_dir.name().unwrap());
	let expected_file = format!(
		"{}/{}/{}",
		dir_path.0,
		sub_dir.name().unwrap(),
		file_in_sub.name().unwrap()
	);

	assert!(
		descendant_paths.iter().any(|p| p.0 == expected_subdir),
		"Expected subdirectory path not found"
	);
	assert!(
		descendant_paths.iter().any(|p| p.0 == expected_file),
		"Expected file path not found"
	);
}

#[shared_test_runtime]
pub async fn test_query_path_for_uuid() {
	let (db, rss) = get_db_resources().await;
	let root_path: FfiId = db.root_uuid().unwrap().to_string().into();
	let parent_path = root_path.join(rss.dir.name().unwrap());
	let dir_path = parent_path.join("path_test");

	let dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "path_test")
		.await
		.unwrap();

	assert_eq!(
		db.query_path_for_uuid(dir.uuid().to_string()).unwrap(),
		None
	);
	db.update_dir_children(parent_path.clone()).await.unwrap();
	assert_eq!(
		db.query_path_for_uuid(dir.uuid().to_string())
			.unwrap()
			.unwrap(),
		dir_path
	);

	assert_eq!(
		db.query_path_for_uuid(rss.dir.uuid().to_string())
			.unwrap()
			.unwrap(),
		parent_path
	);

	assert_eq!(
		db.query_path_for_uuid(db.root_uuid().unwrap())
			.unwrap()
			.unwrap(),
		root_path
	);
}

#[shared_test_runtime]
pub async fn test_last_listed() {
	let (db, rss) = get_db_resources().await;

	// Create a directory and a file
	let test_dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "test_last_listed")
		.await
		.unwrap();

	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let dir_path: FfiId = test_dir_path.join(test_dir.name().unwrap());

	db.update_dir_children(test_dir_path.clone()).await.unwrap();

	let dir = match db.query_item(&dir_path).unwrap().unwrap() {
		FfiObject::Dir(dir) => dir,
		_ => panic!("Expected a directory item"),
	};

	assert_eq!(dir.last_listed, 0);

	let now = chrono::Utc::now().timestamp_millis();
	db.update_dir_children(dir_path.clone()).await.unwrap();

	let dir = match db.query_item(&dir_path).unwrap().unwrap() {
		FfiObject::Dir(dir) => dir,
		_ => panic!("Expected a directory item"),
	};

	let later = chrono::Utc::now().timestamp_millis();

	assert!(now <= dir.last_listed);
	assert!(dir.last_listed <= later);
}

#[shared_test_runtime]
pub async fn test_update_local_data() {
	let (db, rss) = get_db_resources().await;

	let file = rss
		.client
		.upload_file(
			rss.client
				.make_file_builder("file", rss.dir.uuid())
				.unwrap(),
			b"",
		)
		.await
		.unwrap();

	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let file_path: FfiId = test_dir_path.join(file.name().unwrap());

	db.update_dir_children(test_dir_path.clone()).await.unwrap();
	let mut ffi_file = match db.query_item(&file_path).unwrap().unwrap() {
		FfiObject::File(file) => file,
		_ => panic!("Expected a file item"),
	};

	let mut local_data = ffi_file.local_data.unwrap_or_default();
	local_data.insert("k".to_string(), "v".to_string());
	db.update_local_data(&file.uuid().to_string(), local_data.clone())
		.unwrap();
	ffi_file.local_data = Some(local_data.clone());
	let updated_file = match db.query_item(&file_path).unwrap().unwrap() {
		FfiObject::File(file) => file,
		_ => panic!("Expected a file item"),
	};
	assert_eq!(updated_file, ffi_file);

	db.update_dir_children(test_dir_path.clone()).await.unwrap();
	let updated_file = match db.query_item(&file_path).unwrap().unwrap() {
		FfiObject::File(file) => file,
		_ => panic!("Expected a file item"),
	};
	assert_eq!(updated_file, ffi_file);
}

#[shared_test_runtime]
pub async fn test_update_local_data_move() {
	let (db, rss) = get_db_resources().await;

	let file = rss
		.client
		.upload_file(
			rss.client
				.make_file_builder("file", rss.dir.uuid())
				.unwrap(),
			b"",
		)
		.await
		.unwrap();

	let new_parent = rss
		.client
		.create_dir(&(&rss.dir).into(), "new_parent")
		.await
		.unwrap();

	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let file_path: FfiId = test_dir_path.join(file.name().unwrap());
	let new_parent_path: FfiId = test_dir_path.join(new_parent.name().unwrap());

	db.update_dir_children(test_dir_path.clone()).await.unwrap();
	let mut ffi_file = match db.query_item(&file_path).unwrap().unwrap() {
		FfiObject::File(file) => file,
		_ => panic!("Expected a file item"),
	};

	let mut local_data = ffi_file.local_data.unwrap_or_default();
	local_data.insert("k".to_string(), "v".to_string());
	db.update_local_data(&file.uuid().to_string(), local_data.clone())
		.unwrap();
	ffi_file.local_data = Some(local_data);

	let resp = db.move_item(file_path, new_parent_path).await.unwrap();

	let moved_file = match resp.object {
		FfiObject::File(file) => file,
		_ => panic!("Expected a file item after move"),
	};
	assert_eq!(moved_file.local_data, ffi_file.local_data);
}

#[shared_test_runtime]
pub async fn test_update_local_data_remote_move() {
	let (db, rss) = get_db_resources().await;

	let mut file = rss
		.client
		.upload_file(
			rss.client
				.make_file_builder("file", rss.dir.uuid())
				.unwrap(),
			b"",
		)
		.await
		.unwrap();

	let new_parent = rss
		.client
		.create_dir(&(&rss.dir).into(), "new_parent")
		.await
		.unwrap();

	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let file_path: FfiId = test_dir_path.join(file.name().unwrap());
	let new_parent_path: FfiId = test_dir_path.join(new_parent.name().unwrap());
	let new_file_path: FfiId = new_parent_path.join(file.name().unwrap());

	db.update_dir_children(test_dir_path.clone()).await.unwrap();
	let mut ffi_file = match db.query_item(&file_path).unwrap().unwrap() {
		FfiObject::File(file) => file,
		_ => panic!("Expected a file item"),
	};

	let mut local_data = ffi_file.local_data.unwrap_or_default();
	local_data.insert("k".to_string(), "v".to_string());
	db.update_local_data(&file.uuid().to_string(), local_data.clone())
		.unwrap();
	ffi_file.local_data = Some(local_data);

	rss.client
		.move_file(&mut file, &(&new_parent).into())
		.await
		.unwrap();

	db.update_dir_children(new_parent_path.clone())
		.await
		.unwrap();
	let moved_file = match db.query_item(&new_file_path).unwrap().unwrap() {
		FfiObject::File(file) => file,
		_ => panic!("Expected a file item after remote move"),
	};

	assert_eq!(moved_file.local_data, ffi_file.local_data);
}

#[shared_test_runtime]
pub async fn test_update_local_data_remote_update() {
	let (db, rss) = get_db_resources().await;

	let file = rss
		.client
		.upload_file(
			rss.client
				.make_file_builder("file", rss.dir.uuid())
				.unwrap(),
			b"",
		)
		.await
		.unwrap();

	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let file_path: FfiId = test_dir_path.join(file.name().unwrap());

	db.update_dir_children(test_dir_path.clone()).await.unwrap();

	let ffi_file = match db.query_item(&file_path).unwrap().unwrap() {
		FfiObject::File(file) => file,
		_ => panic!("Expected a file item"),
	};

	let mut local_data = ffi_file.local_data.unwrap_or_default();
	local_data.insert("k".to_string(), "v".to_string());
	db.update_local_data(&file.uuid().to_string(), local_data.clone())
		.unwrap();

	let _ = rss
		.client
		.upload_file(
			rss.client
				.make_file_builder(file.name().unwrap(), rss.dir.uuid())
				.unwrap(),
			b"1",
		)
		.await
		.unwrap();

	db.update_dir_children(test_dir_path.clone()).await.unwrap();
	let updated_file = match db.query_item(&file_path).unwrap().unwrap() {
		FfiObject::File(file) => file,
		_ => panic!("Expected a file item after remote update"),
	};
	assert_eq!(updated_file.local_data, Some(local_data));
}

#[shared_test_runtime]
pub async fn test_update_local_data_move_name_collision() {
	let (db, rss) = get_db_resources().await;

	let file = rss
		.client
		.upload_file(
			rss.client
				.make_file_builder("file", rss.dir.uuid())
				.unwrap(),
			b"",
		)
		.await
		.unwrap();

	let new_parent = rss
		.client
		.create_dir(&(&rss.dir).into(), "new_parent")
		.await
		.unwrap();

	let _ = rss
		.client
		.upload_file(
			rss.client
				.make_file_builder("file", new_parent.uuid())
				.unwrap(),
			b"",
		)
		.await
		.unwrap();

	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let file_path: FfiId = test_dir_path.join(file.name().unwrap());
	let new_parent_path: FfiId = test_dir_path.join(new_parent.name().unwrap());
	let new_path: FfiId = new_parent_path.join(file.name().unwrap());

	db.update_dir_children(test_dir_path.clone()).await.unwrap();
	db.update_dir_children(new_parent_path.clone())
		.await
		.unwrap();

	let ffi_file = match db.query_item(&file_path).unwrap().unwrap() {
		FfiObject::File(file) => file,
		_ => panic!("Expected a file item"),
	};

	let mut local_data = ffi_file.local_data.unwrap_or_default();
	local_data.insert("k".to_string(), "v".to_string());
	db.update_local_data(&file.uuid().to_string(), local_data.clone())
		.unwrap();

	let resp = db
		.move_item(file_path.clone(), new_parent_path.clone())
		.await
		.unwrap();

	let moved_file = match resp.object {
		FfiObject::File(file) => file,
		_ => panic!("Expected a file item after move"),
	};

	assert_eq!(moved_file.local_data, Some(local_data.clone()));

	let moved_file = match db.query_item(&new_path).unwrap().unwrap() {
		FfiObject::File(file) => file,
		_ => panic!("Expected a file item after move"),
	};
	assert_eq!(moved_file.local_data, Some(local_data));
}

// `change_seq` is a per-database-incarnation watermark (stamped from that
// database's own `change_meta` counter), so two caches that agree on content
// still legitimately disagree on it — comparisons across instances mask it.
fn ignoring_change_seq(mut objects: Vec<FfiNonRootObject>) -> Vec<FfiNonRootObject> {
	for object in &mut objects {
		match object {
			FfiNonRootObject::File(file) => file.change_seq = 0,
			FfiNonRootObject::Dir(dir) => dir.change_seq = 0,
		}
	}
	objects
}

#[shared_test_runtime]
pub async fn test_init_from_file() {
	let (db, rss) = get_db_resources().await;
	let config = rss.client.to_sdk_config();
	let json_config = serde_json::to_string(&AuthFile {
		sdk_config: Some(config),
		provider_enabled: true,
		max_thumbnail_files_budget: Some(1024 * 1024 * 6),
		max_cache_files_budget: Some(1024 * 1024 * 10),
	})
	.unwrap();

	rss.client
		.create_dir(&(&rss.dir).into(), "dir")
		.await
		.unwrap();

	let tmp_dir = std::env::temp_dir();
	let dek_bytes = [0x42u8; 32];
	let dek = EncryptionKey::new(dek_bytes);
	let auth_file = tmp_dir.join("auth.json").to_string_lossy().into_owned();
	tokio::fs::write(&auth_file, encrypt_auth_json(json_config.as_bytes(), &dek))
		.await
		.unwrap();
	let files_path = tmp_dir.join("files").to_string_lossy().into_owned();
	tokio::fs::remove_file(&format!("{files_path}/{DB_FILE_NAME}"))
		.await
		.ok();
	tokio::fs::create_dir_all(&files_path).await.unwrap();

	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();

	// sync
	let new = FilenMobileCacheState::new(files_path.clone(), auth_file.clone(), dek_bytes.to_vec());
	assert_eq!(new.root_uuid().unwrap(), db.root_uuid().unwrap());
	// make sure it still works after authentication
	assert_eq!(new.root_uuid().unwrap(), db.root_uuid().unwrap());
	// into async
	assert_eq!(
		ignoring_change_seq(
			new.update_and_query_dir_children(test_dir_path.clone(), None)
				.await
				.unwrap()
				.unwrap()
				.objects
		),
		ignoring_change_seq(
			db.update_and_query_dir_children(test_dir_path.clone(), None)
				.await
				.unwrap()
				.unwrap()
				.objects
		)
	);

	// async
	let new = FilenMobileCacheState::new(files_path.clone(), auth_file.clone(), dek_bytes.to_vec());
	assert_eq!(
		ignoring_change_seq(
			new.update_and_query_dir_children(test_dir_path.clone(), None)
				.await
				.unwrap()
				.unwrap()
				.objects
		),
		ignoring_change_seq(
			db.update_and_query_dir_children(test_dir_path.clone(), None)
				.await
				.unwrap()
				.unwrap()
				.objects
		)
	);
	// make sure it still works after authentication
	assert_eq!(
		ignoring_change_seq(
			new.update_and_query_dir_children(test_dir_path.clone(), None)
				.await
				.unwrap()
				.unwrap()
				.objects
		),
		ignoring_change_seq(
			db.update_and_query_dir_children(test_dir_path.clone(), None)
				.await
				.unwrap()
				.unwrap()
				.objects
		)
	);
	// into sync
	assert_eq!(new.root_uuid().unwrap(), db.root_uuid().unwrap());

	// async overload
	let new = FilenMobileCacheState::new(files_path, auth_file, dek_bytes.to_vec());

	let mut futures = FuturesUnordered::new();

	for _ in 0..10 {
		let new = &new;
		let test_dir_path = test_dir_path.clone();
		futures.push(Box::pin(async move {
			new.update_and_query_dir_children(test_dir_path, None)
				.await
				.unwrap();
		}) as BoxFuture<()>);
	}

	futures.push(Box::pin(async {
		assert_eq!(
			ignoring_change_seq(
				new.update_and_query_dir_children(test_dir_path.clone(), None)
					.await
					.unwrap()
					.unwrap()
					.objects
			),
			ignoring_change_seq(
				db.update_and_query_dir_children(test_dir_path.clone(), None)
					.await
					.unwrap()
					.unwrap()
					.objects
			)
		)
	}));

	while (futures.next().await).is_some() {}
}

#[shared_test_runtime]
pub async fn test_recents() {
	let (db, rss) = get_db_resources().await;
	let recents = db.query_recents(None).unwrap();
	assert!(
		recents.objects.is_empty(),
		"Recents should be empty initially"
	);

	let file = rss
		.client
		.upload_file(
			rss.client
				.make_file_builder("recent_file.txt", rss.dir.uuid())
				.unwrap(),
			b"Recent content",
		)
		.await
		.unwrap();

	let recents = db.update_and_query_recents(None).await.unwrap();

	assert!(recents.objects.iter().any(|o| {
		match o {
			FfiNonRootObject::File(f) => f.uuid == file.uuid().to_string(),
			_ => false,
		}
	}))
}

#[cfg(feature = "malformed")]
#[shared_test_runtime]
pub async fn test_malformed() {
	let (db, rss) = get_db_resources().await;

	let test_dir_id: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();

	// Create a directory with a malformed name
	let dir = rss
		.client
		.create_malformed_dir(&(&rss.dir).into(), "asdfs", "dsfsdf")
		.await
		.unwrap();

	db.update_dir_children(test_dir_id.clone()).await.unwrap();
	let resp = db.query_dir_children(&test_dir_id, None).unwrap().unwrap();
	assert_eq!(resp.objects.len(), 1);
	let FfiNonRootObject::Dir(malformed_dir) = &resp.objects[0] else {
		panic!("Expected a directory object");
	};
	assert!(malformed_dir.meta.is_none());
	assert_eq!(malformed_dir.uuid, dir.uuid().to_string());

	let file = rss
		.client
		.create_malformed_file(&(&rss.dir).into(), "asdfsa", "asdfsa", "asdf", "asdf")
		.await
		.unwrap();

	db.update_dir_children(test_dir_id.clone()).await.unwrap();
	let resp = db.query_dir_children(&test_dir_id, None).unwrap().unwrap();
	println!("Resp: {:?}", resp);
	assert_eq!(resp.objects.len(), 2);
	let malformed_file = resp
		.objects
		.into_iter()
		.filter_map(|obj| {
			if let FfiNonRootObject::File(f) = obj {
				Some(f)
			} else {
				None
			}
		})
		.find(|f| f.uuid == file.uuid().to_string())
		.unwrap();
	assert!(malformed_file.meta.is_none());
	assert_eq!(malformed_file.uuid, file.uuid().to_string());
}

struct NoopSearchUpdate;
impl SearchUpdateCallback for NoopSearchUpdate {
	fn on_update(&self) {}
}

fn noop_update() -> Arc<dyn SearchUpdateCallback> {
	Arc::new(NoopSearchUpdate)
}

fn base_search_args(name: &str) -> SearchQueryArgs {
	SearchQueryArgs {
		name: Some(name.to_string()),
		item_type: None,
		exclude_media_on_device: false,
		mime_types: Vec::new(),
		file_size_min: None,
		last_modified_min: None,
	}
}

fn entry_file_uuid_is(entry: &SearchQueryResponseEntry, uuid: &str) -> bool {
	matches!(&entry.object, FfiNonRootObject::File(f) if f.uuid == uuid)
}

fn entry_dir_uuid_is(entry: &SearchQueryResponseEntry, uuid: &str) -> bool {
	matches!(&entry.object, FfiNonRootObject::Dir(d) if d.uuid == uuid)
}

/// Poll the live search until it converges to at least `want` results (the on-demand resync fills
/// the cache asynchronously), or fail after the convergence window. 240s mirrors the sdk cache
/// tests' CACHE_CONVERGE_TIMEOUT: the backing resync serializes on the account-wide drive lock,
/// whose patient acquisition alone can take ~110s under contention, so a shorter fixed window
/// sits inside a guaranteed-miss band (a 60s window expired twice on nightly macOS legs). The
/// happy path still exits in a few seconds. The panic reports the last observed count so a
/// timeout distinguishes zero-progress (events/resync never arrived) from slow-progress.
async fn poll_search(
	db: &FilenMobileCacheState,
	root_id: &str,
	args: SearchQueryArgs,
	want: usize,
) -> Vec<SearchQueryResponseEntry> {
	let mut last_len = 0;
	for _ in 0..240 {
		let resp = db
			.query_search(root_id.to_string(), args.clone(), noop_update())
			.await
			.unwrap();
		if resp.len() >= want {
			return resp;
		}
		last_len = resp.len();
		tokio::time::sleep(std::time::Duration::from_secs(1)).await;
	}
	panic!("search did not converge to {want} results within 240s (last poll saw {last_len})");
}

/// Live cache-search integration test for the documents provider's `query_search`: creates an
/// isolated subtree on the server, then searches it via the new engine and asserts the resync
/// surfaces the matches, honouring the name / mime / item-type filters. Scoped to a fresh dir so
/// the subtree resync is small and isolated from other tests.
#[shared_test_runtime]
pub async fn test_search() {
	let (db, rss) = get_db_resources().await;

	let name = BASE64_URL_SAFE_NO_PAD.encode(rand::random::<[u8; 10]>());

	// Isolated search root under the shared test dir, so create_search resyncs only this subtree.
	let search_root = rss
		.client
		.create_dir(&(&rss.dir).into(), &format!("search-{name}"))
		.await
		.unwrap();
	let a = rss
		.client
		.create_dir(&(&search_root).into(), "a")
		.await
		.unwrap();
	let b = rss.client.create_dir(&(&a).into(), "b").await.unwrap();

	// An image nested at a/b/<name>.png and a text file directly under the search root.
	let img = rss
		.client
		.upload_file(
			rss.client
				.make_file_builder(&format!("{name}.png"), b.uuid())
				.unwrap()
				.mime("image/png".to_string()),
			b"image",
		)
		.await
		.unwrap();
	let txt = rss
		.client
		.upload_file(
			rss.client
				.make_file_builder(&format!("other_{name}.txt"), search_root.uuid())
				.unwrap()
				.mime("text/plain".to_string()),
			b"text",
		)
		.await
		.unwrap();
	// A directory whose name carries the needle — a POSITIVE item_type=Dir match target.
	let match_dir = rss
		.client
		.create_dir(&(&search_root).into(), &format!("dir_{name}"))
		.await
		.unwrap();

	let root_id = search_root.uuid().to_string();

	// The engine resyncs the subtree asynchronously, so poll until all three matches surface.
	let resp = poll_search(&db, &root_id, base_search_args(&name), 3).await;
	assert_eq!(
		resp.len(),
		3,
		"both name-matching files and the matching directory should be found"
	);
	assert!(
		resp.iter()
			.any(|e| entry_file_uuid_is(e, &img.uuid().to_string())),
		"the nested image should be in the results",
	);
	assert!(
		resp.iter()
			.any(|e| entry_file_uuid_is(e, &txt.uuid().to_string())),
		"the text file should be in the results",
	);
	assert!(
		resp.iter()
			.any(|e| entry_dir_uuid_is(e, &match_dir.uuid().to_string())),
		"the matching directory should be in the results",
	);
	// Path is `<search root>/<relative path>`; the nested image climbs a/b.
	let img_entry = resp
		.iter()
		.find(|e| entry_file_uuid_is(e, &img.uuid().to_string()))
		.unwrap();
	assert_eq!(img_entry.path, format!("{root_id}/a/b/{name}.png"));

	// mime filter narrows to just the image.
	let resp = db
		.query_search(
			root_id.clone(),
			SearchQueryArgs {
				mime_types: vec!["image/*".to_string()],
				item_type: Some(ItemType::File),
				..base_search_args(&name)
			},
			noop_update(),
		)
		.await
		.unwrap();
	assert_eq!(resp.len(), 1, "mime filter should keep only the image");
	assert!(entry_file_uuid_is(&resp[0], &img.uuid().to_string()));

	// item_type=Dir keeps ONLY the matching directory (positive coverage of the Dir filter — a
	// broken Dir mapping that returned nothing would fail here, not silently pass).
	let resp = db
		.query_search(
			root_id.clone(),
			SearchQueryArgs {
				item_type: Some(ItemType::Dir),
				..base_search_args(&name)
			},
			noop_update(),
		)
		.await
		.unwrap();
	assert_eq!(resp.len(), 1, "only the matching directory should remain");
	assert!(
		entry_dir_uuid_is(&resp[0], &match_dir.uuid().to_string()),
		"the Dir filter should return the matching directory",
	);

	// A non-matching needle returns nothing.
	let resp = db
		.query_search(
			root_id.clone(),
			SearchQueryArgs {
				name: Some("zzz-definitely-no-match-zzz".to_string()),
				..base_search_args(&name)
			},
			noop_update(),
		)
		.await
		.unwrap();
	assert_eq!(resp.len(), 0, "a non-matching needle finds nothing");
}

// Schema changes ship as a CACHE_VERSION bump: an on-disk cache written by an older version
// must be wiped and rebuilt on open (there is deliberately no ALTER-based migration), after
// which the cache re-syncs from the server and works normally on the new schema.
#[shared_test_runtime]
pub async fn test_cache_version_bump_reinitializes_db() {
	let (db, rss) = get_db_resources().await;
	let config = rss.client.to_sdk_config();
	let json_config = serde_json::to_string(&AuthFile {
		sdk_config: Some(config),
		provider_enabled: true,
		max_thumbnail_files_budget: None,
		max_cache_files_budget: None,
	})
	.unwrap();

	let tmp_dir = std::env::temp_dir().join("stable_uuid_reinit_test");
	let files_path = tmp_dir.join("files");
	std::fs::remove_dir_all(&files_path).ok();
	std::fs::create_dir_all(&files_path).unwrap();
	let dek_bytes = [0x43u8; 32];
	let dek = EncryptionKey::new(dek_bytes);
	let auth_file = tmp_dir.join("auth.json").to_string_lossy().into_owned();
	tokio::fs::write(&auth_file, encrypt_auth_json(json_config.as_bytes(), &dek))
		.await
		.unwrap();
	let files_path_str = files_path.to_string_lossy().into_owned();

	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();

	// first open: fresh cache on the current schema, primed with a listing
	let state = FilenMobileCacheState::new(
		files_path_str.clone(),
		auth_file.clone(),
		dek_bytes.to_vec(),
	);
	state
		.update_dir_children(test_dir_path.clone())
		.await
		.unwrap();
	assert!(state.query_item(&test_dir_path).unwrap().is_some());
	drop(state);

	// tamper: pretend the DB was written by the previous cache version
	let state_file = files_path.join("db_state.json");
	let mut saved: serde_json::Value =
		serde_json::from_str(&std::fs::read_to_string(&state_file).unwrap()).unwrap();
	saved["version"] = serde_json::Value::from(2u64);
	std::fs::write(&state_file, serde_json::to_string(&saved).unwrap()).unwrap();

	// reopen: the outdated cache must be wiped...
	let state = FilenMobileCacheState::new(files_path_str, auth_file, dek_bytes.to_vec());
	assert!(
		state.query_item(&test_dir_path).unwrap().is_none(),
		"an outdated on-disk cache must be reinitialized, not reused"
	);
	// ...and the rebuilt cache repopulates and serves normally
	state
		.update_dir_children(test_dir_path.clone())
		.await
		.unwrap();
	assert!(state.query_item(&test_dir_path).unwrap().is_some());
}

// The `stable/<id>` FFI namespace is the identity the providers persist. It must address a
// file across a remote content edit (which re-mints the uuid), and — for app-migration
// compat — a stale current-version uuid passed through the same namespace must keep
// resolving to the row for as long as the cache knows it.
#[shared_test_runtime]
pub async fn test_stable_id_namespace_addresses_files_across_edits() {
	let (db, rss) = get_db_resources().await;

	let old_file = rss
		.client
		.upload_file(
			rss.client
				.make_file_builder("stable_ns.txt", rss.dir.uuid())
				.unwrap(),
			b"stable ns v1",
		)
		.await
		.unwrap();

	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let file_path: FfiId = test_dir_path.join("stable_ns.txt");
	db.update_dir_children(test_dir_path.clone()).await.unwrap();

	let ffi_file = match db.query_item(&file_path).unwrap().unwrap() {
		FfiObject::File(f) => f,
		other => panic!("expected a file, got {other:?}"),
	};
	assert_eq!(ffi_file.stable_uuid, old_file.stable_uuid().to_string());

	let stable_id: FfiId = format!("stable/{}", ffi_file.stable_uuid).into();

	// the stable id resolves to the same item as the path
	let by_stable = match db.query_item(&stable_id).unwrap().unwrap() {
		FfiObject::File(f) => f,
		other => panic!("expected a file, got {other:?}"),
	};
	assert_eq!(by_stable, ffi_file);

	// remote content edit: uuid re-mints, stable id stays
	let edited = rss
		.client
		.upload_file(
			rss.client
				.make_file_builder("stable_ns.txt", rss.dir.uuid())
				.unwrap(),
			b"stable ns v2",
		)
		.await
		.unwrap();
	assert_ne!(edited.uuid(), old_file.uuid());
	db.update_dir_children(test_dir_path.clone()).await.unwrap();

	// same stable id still addresses the file, now at its new current uuid...
	let by_stable = match db.query_item(&stable_id).unwrap().unwrap() {
		FfiObject::File(f) => f,
		other => panic!("expected a file, got {other:?}"),
	};
	assert_eq!(by_stable.uuid, edited.uuid().to_string());
	assert_eq!(by_stable.stable_uuid, ffi_file.stable_uuid);

	// ...and mutations through the stable namespace work end to end
	let downloaded = db
		.download_file_if_changed_by_path(stable_id.clone(), None)
		.await
		.unwrap();
	assert_eq!(std::fs::read(&downloaded).unwrap(), b"stable ns v2");

	// app-migration compat: the retired current-version uuid still resolves
	// through the stable namespace while the cache knows the lineage
	let stale_id: FfiId = format!("stable/{}", old_file.uuid()).into();
	let by_stale = match db.query_item(&stale_id).unwrap().unwrap() {
		FfiObject::File(f) => f,
		other => panic!("expected a file, got {other:?}"),
	};
	// (here the stale uuid IS the lineage id, since the first upload minted it)
	assert_eq!(by_stale.uuid, edited.uuid().to_string());

	std::fs::remove_file(&downloaded).ok();
}

// Directories have no stable id at all — the server never re-mints a dir's uuid, so the
// uuid already is its whole-life id — and the `stable/<uuid>` namespace must therefore
// address them by uuid. The providers use it as the document id for dirs too, since
// name-path ids retire on every rename/move.
#[shared_test_runtime]
pub async fn test_stable_id_namespace_addresses_dirs_across_renames() {
	let (db, rss) = get_db_resources().await;

	let dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "stable_ns_dir_a")
		.await
		.unwrap();

	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	db.update_dir_children(test_dir_path.clone()).await.unwrap();

	let stable_id: FfiId = format!("stable/{}", dir.uuid()).into();
	let by_stable = match db.query_item(&stable_id).unwrap().unwrap() {
		FfiObject::Dir(d) => d,
		other => panic!("expected a dir, got {other:?}"),
	};
	assert_eq!(by_stable.uuid, dir.uuid().to_string());

	// mutations through the namespace work, and the id survives the rename
	// that retires the dir's name-path form
	db.rename_item(stable_id.clone(), "stable_ns_dir_b".to_string())
		.await
		.unwrap();
	let renamed = match db.query_item(&stable_id).unwrap().unwrap() {
		FfiObject::Dir(d) => d,
		other => panic!("expected a dir, got {other:?}"),
	};
	assert_eq!(renamed.uuid, dir.uuid().to_string());
	assert_eq!(renamed.meta.unwrap().name, "stable_ns_dir_b");
}

// A local edit that fails to upload must not be silently lost. The cache marks the file before
// attempting the upload and clears the marker only once the bytes are on the server, so an edit
// interrupted at any point — a dead process, a dropped connection — is still known to be
// outstanding and can be drained later.
//
// These assert on the SPECIFIC file's marker rather than the global pending count: the live tests
// share one cache and run in parallel, so a global count would see other tests' markers and be
// flaky.
fn pending_marker(db: &FilenMobileCacheState, path: &FfiId) -> Option<i64> {
	match db.query_item(path).unwrap().unwrap() {
		FfiObject::File(f) => f.pending_upload_at,
		other => panic!("expected a file, got {other:?}"),
	}
}

// Nothing writes the marker from outside — the cache marks a file itself, just before it tries to
// upload it — so tests provoke the real thing instead of seeding a column: an upload attempt with
// no local bytes to send marks the file and then fails on the missing copy, which is exactly the
// state a dropped connection leaves behind.
async fn mark_pending_upload(db: &FilenMobileCacheState, path: &FfiId) {
	assert!(
		db.upload_file_if_changed(path.clone(), None).await.is_err(),
		"an upload with no local bytes to send must fail"
	);
	assert!(
		pending_marker(db, path).is_some(),
		"the failed attempt must leave the edit marked"
	);
}

// The same marker with `bytes` sitting behind it: a real edit that never reached the server. The
// copy is taken away only for the duration of the attempt, which is what makes the attempt fail.
async fn mark_pending_upload_over(db: &FilenMobileCacheState, path: &FfiId, bytes: &[u8]) {
	let local_path = db
		.download_file_if_changed_by_path(path.clone(), None)
		.await
		.unwrap();
	tokio::fs::remove_file(&local_path).await.unwrap();
	mark_pending_upload(db, path).await;
	// The cache slot is a directory of its own, and the failed attempt is free to have tidied it.
	tokio::fs::create_dir_all(std::path::Path::new(&local_path).parent().unwrap())
		.await
		.unwrap();
	tokio::fs::write(&local_path, bytes).await.unwrap();
}

#[shared_test_runtime]
pub async fn test_a_successful_upload_leaves_nothing_pending() {
	let (db, rss) = get_db_resources().await;

	rss.client
		.upload_file(
			rss.client
				.make_file_builder("pending_ok.txt", rss.dir.uuid())
				.unwrap(),
			b"v1",
		)
		.await
		.unwrap();

	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let file_path: FfiId = test_dir_path.join("pending_ok.txt");
	db.update_dir_children(test_dir_path.clone()).await.unwrap();

	// Materialise it, then edit the local copy so the upload has something to do.
	let local_path = db
		.download_file_if_changed_by_path(file_path.clone(), None)
		.await
		.unwrap();
	tokio::fs::write(&local_path, b"v2 edited locally")
		.await
		.unwrap();

	assert!(
		db.upload_file_if_changed(file_path.clone(), None)
			.await
			.unwrap(),
		"the locally edited file should upload"
	);
	assert_eq!(
		pending_marker(&db, &file_path),
		None,
		"a successful upload must leave no marker behind"
	);

	// A second call has nothing to send and must not resurrect a marker.
	assert!(
		!db.upload_file_if_changed(file_path.clone(), None)
			.await
			.unwrap(),
		"an unchanged file should report no upload"
	);
	assert_eq!(pending_marker(&db, &file_path), None);
}

// The marker is a column `upsert_item` never names, so nothing a directory refresh does — not the
// identity reconciliation, not the stale sweep — can write it. If a refresh dropped it, an edit
// that failed while the user was browsing would stop being retried and would be lost for good.
#[shared_test_runtime]
pub async fn test_a_pending_marker_survives_a_directory_refresh() {
	let (db, rss) = get_db_resources().await;

	let file = rss
		.client
		.upload_file(
			rss.client
				.make_file_builder("pending_refresh.txt", rss.dir.uuid())
				.unwrap(),
			b"v1",
		)
		.await
		.unwrap();

	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let file_path: FfiId = test_dir_path.join("pending_refresh.txt");
	db.update_dir_children(test_dir_path.clone()).await.unwrap();

	// The app's own local data goes in alongside, since the refresh has to leave both intact.
	let mut local_data = std::collections::HashMap::new();
	local_data.insert("TagData".to_string(), "keep me".to_string());
	db.update_local_data(&file.uuid().to_string(), local_data.clone())
		.unwrap();
	mark_pending_upload(&db, &file_path).await;
	let marked_at = pending_marker(&db, &file_path);

	db.update_dir_children(test_dir_path).await.unwrap();

	assert_eq!(
		pending_marker(&db, &file_path),
		marked_at,
		"a directory refresh must not drop the pending marker"
	);
	match db.query_item(&file_path).unwrap().unwrap() {
		FfiObject::File(f) => assert_eq!(
			f.local_data,
			Some(local_data),
			"nor the app's own local data"
		),
		other => panic!("expected a file, got {other:?}"),
	}
}

// Draining must not strand a marker forever. A marked file whose local copy is gone — the cache was
// cleared, or the item evicted — has nothing left to upload, so the drain drops its marker instead
// of failing on it on every future attempt.
#[shared_test_runtime]
pub async fn test_draining_drops_markers_for_files_with_no_local_copy() {
	let (db, rss) = get_isolated_db_resources("drain_gone").await;

	rss.client
		.upload_file(
			rss.client
				.make_file_builder("pending_gone.txt", rss.dir.uuid())
				.unwrap(),
			b"v1",
		)
		.await
		.unwrap();

	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let file_path: FfiId = test_dir_path.join("pending_gone.txt");
	db.update_dir_children(test_dir_path).await.unwrap();

	// Marked, but never materialised locally.
	mark_pending_upload(&db, &file_path).await;

	db.retry_pending_uploads().await.unwrap();

	assert_eq!(
		pending_marker(&db, &file_path),
		None,
		"an unusable marker must be dropped, not retried forever"
	);
}

// A cache of its very own, for tests that call `retry_pending_uploads`.
//
// The drain is global by design — it services every outstanding edit in the cache. The rest of the
// suite shares one cache and runs in parallel, so a drain running there picks up other tests' live
// markers and uploads their files underneath them: two uploads race the same `cache_dir/<uuid>`
// rename (ENOENT), and the drain re-marks a file the owning test has just cleared. Isolating the
// cache is what makes the drain's own behaviour observable without perturbing anything else.
async fn get_isolated_db_resources(tag: &str) -> (Arc<FilenMobileCacheState>, TestResources) {
	let files_path = std::env::temp_dir().join(format!("test_files_{tag}"));
	std::fs::create_dir_all(&files_path).unwrap();
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = resources.client.to_stringified();
	let state = Arc::new(
		FilenMobileCacheState::from_stringified_in_memory(
			client,
			files_path.to_string_lossy().as_ref(),
		)
		.unwrap(),
	);
	(state, resources)
}

// Returns the cached file's (uuid, stable_uuid) pair. The two diverge as soon as the server
// re-mints the uuid on a content edit, which is the precondition every test below depends on.
fn file_ids(db: &FilenMobileCacheState, path: &FfiId) -> (String, String) {
	match db.query_item(path).unwrap().unwrap() {
		FfiObject::File(f) => (f.uuid, f.stable_uuid),
		other => panic!("expected a file, got {other:?}"),
	}
}

// An upload that fails must leave the edit marked, or the local changes are lost with nothing left
// to reconcile them. The marker is written before the attempt precisely so an upload interrupted at
// the worst moment — a dead process, a dropped connection — is still known to be outstanding.
//
// The failure is provoked by removing the cached copy after the freshness check has been primed:
// the upload then reaches `io_upload_file`, whose `metadata()` call fails NotFound, so the attempt
// returns Err without ever clearing the marker.
#[shared_test_runtime]
pub async fn test_a_failed_upload_leaves_a_pending_marker_on_a_re_minted_file() {
	let (db, rss) = get_db_resources().await;

	rss.client
		.upload_file(
			rss.client
				.make_file_builder("pending_remint.txt", rss.dir.uuid())
				.unwrap(),
			b"v1",
		)
		.await
		.unwrap();

	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let file_path: FfiId = test_dir_path.join("pending_remint.txt");
	db.update_dir_children(test_dir_path.clone()).await.unwrap();

	// A first edit that succeeds: this is what makes the server re-mint the uuid, leaving the row
	// with uuid != stable_uuid for every subsequent edit.
	let local_path = db
		.download_file_if_changed_by_path(file_path.clone(), None)
		.await
		.unwrap();
	tokio::fs::write(&local_path, b"v2 edited locally")
		.await
		.unwrap();
	assert!(
		db.upload_file_if_changed(file_path.clone(), None)
			.await
			.unwrap()
	);
	db.update_dir_children(test_dir_path).await.unwrap();

	let (uuid, stable_uuid) = file_ids(&db, &file_path);
	assert_ne!(
		uuid, stable_uuid,
		"the edit must have re-minted the uuid, or this test proves nothing"
	);

	// Materialise it, then take the local copy away so the upload attempt fails.
	let local_path = db
		.download_file_if_changed_by_path(file_path.clone(), None)
		.await
		.unwrap();
	tokio::fs::remove_file(&local_path).await.unwrap();

	assert!(
		db.upload_file_if_changed(file_path.clone(), None)
			.await
			.is_err(),
		"an upload with no local bytes to send must fail"
	);
	assert!(
		pending_marker(&db, &file_path).is_some(),
		"a failed upload must leave the edit marked as outstanding"
	);
}

// The drain must actually retry a marked edit. A file whose uuid the server has re-minted is the
// normal case for anything edited more than once, and it is exactly the case whose bytes are worth
// the most — dropping its marker discards an edit that exists only on the device.
#[shared_test_runtime]
pub async fn test_the_drain_retries_a_file_whose_uuid_was_re_minted() {
	let (db, rss) = get_isolated_db_resources("drain_remint").await;

	rss.client
		.upload_file(
			rss.client
				.make_file_builder("pending_drain.txt", rss.dir.uuid())
				.unwrap(),
			b"v1",
		)
		.await
		.unwrap();

	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let file_path: FfiId = test_dir_path.join("pending_drain.txt");
	db.update_dir_children(test_dir_path.clone()).await.unwrap();

	// A successful edit first, so the server re-mints the uuid.
	let local_path = db
		.download_file_if_changed_by_path(file_path.clone(), None)
		.await
		.unwrap();
	tokio::fs::write(&local_path, b"v2 edited locally")
		.await
		.unwrap();
	assert!(
		db.upload_file_if_changed(file_path.clone(), None)
			.await
			.unwrap()
	);
	db.update_dir_children(test_dir_path.clone()).await.unwrap();

	let (uuid, stable_uuid) = file_ids(&db, &file_path);
	assert_ne!(
		uuid, stable_uuid,
		"the edit must have re-minted the uuid, or this test proves nothing"
	);

	// A second edit whose upload never happened: local bytes present, marker outstanding.
	const DRAINED: &[u8] = b"v3 waiting to be drained";
	mark_pending_upload_over(&db, &file_path, DRAINED).await;

	assert_eq!(
		db.retry_pending_uploads().await.unwrap(),
		1,
		"the drain must retry the marked edit, not discard it"
	);
	assert_eq!(
		pending_marker(&db, &file_path),
		None,
		"a drained edit must leave no marker behind"
	);

	db.update_dir_children(test_dir_path).await.unwrap();
	match db.query_item(&file_path).unwrap().unwrap() {
		FfiObject::File(f) => assert_eq!(
			f.size as usize,
			DRAINED.len(),
			"the drained bytes must be the ones now on the server"
		),
		other => panic!("expected a file, got {other:?}"),
	}
}

// The drain addresses a marked file through the stable namespace, and that id canonicalises to a
// display-name path built from the cached row. When the server no longer has anything at that path
// the walk comes back Partial — the arm that creates a file at the requested name. Creating one
// there uploads the EMPTY slot `io_upload_new_file` makes, and the upsert's `(parent, name)` tier
// merges that empty file onto the marked row: the edit is discarded, its marker cleared, and the
// drain counts it as delivered. Whatever the drain does with a stale path, the bytes it was sent to
// deliver have to reach the server.
#[shared_test_runtime]
pub async fn test_the_drain_never_replaces_a_marked_edit_with_an_empty_file() {
	let (db, rss) = get_isolated_db_resources("drain_stale_path").await;

	let file = rss
		.client
		.upload_file(
			rss.client
				.make_file_builder("pending_stale_path.txt", rss.dir.uuid())
				.unwrap(),
			b"v1",
		)
		.await
		.unwrap();

	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let file_path: FfiId = test_dir_path.join("pending_stale_path.txt");
	db.update_dir_children(test_dir_path.clone()).await.unwrap();

	// The state a failed upload leaves behind: local bytes ahead of the server, edit marked.
	const EDITED: &[u8] = b"v2 edited locally, never uploaded";
	mark_pending_upload_over(&db, &file_path, EDITED).await;

	// Taken off the server behind the cache's back, so the path the stable id canonicalises to
	// resolves to nothing and the drain reaches the create-a-file arm.
	rss.client.delete_file_permanently(file).await.unwrap();

	db.retry_pending_uploads().await.unwrap();

	db.update_dir_children(test_dir_path).await.unwrap();
	match db.query_item(&file_path).unwrap() {
		Some(FfiObject::File(f)) => assert_eq!(
			f.size as usize,
			EDITED.len(),
			"the drain must send the edited bytes, not create an empty file over them"
		),
		other => panic!("the drained edit must be on the server, got {other:?}"),
	}
	assert_eq!(
		pending_marker(&db, &file_path),
		None,
		"a marker may only be cleared once the bytes it stood for have landed"
	);
}

// Trashing a file with an outstanding edit must not strand its marker. The drain skips trashed
// rows, so a marker left behind here is never retried and never cleared — it just sits on the row
// for the life of the cache, and any count of outstanding edits built from it lies.
#[shared_test_runtime]
pub async fn test_trashing_a_marked_file_does_not_strand_its_marker() {
	let (db, rss) = get_db_resources().await;

	let file = rss
		.client
		.upload_file(
			rss.client
				.make_file_builder("pending_trash.txt", rss.dir.uuid())
				.unwrap(),
			b"v1",
		)
		.await
		.unwrap();

	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let file_path: FfiId = test_dir_path.join("pending_trash.txt");
	db.update_dir_children(test_dir_path).await.unwrap();

	mark_pending_upload(&db, &file_path).await;

	db.trash_item(file_path).await.unwrap();

	let trashed_path: FfiId = format!("trash/{}", file.uuid()).into();
	assert_eq!(
		pending_marker(&db, &trashed_path),
		None,
		"trashing must not leave a marker the drain can never reach"
	);
}

// An app that persisted a file's `uuid` before the stable-id migration must keep resolving it after
// the server re-mints that uuid. `resolve_uuid_or_stable` covers this by falling back to a
// stable-id match once the exact uuid match misses — the branch every uuid-string entry point
// depends on, and the one a plain `Uuid::from_str` would have skipped.
#[shared_test_runtime]
pub async fn test_a_pre_edit_uuid_still_resolves_after_the_server_re_mints_it() {
	let (db, rss) = get_db_resources().await;

	let original = rss
		.client
		.upload_file(
			rss.client
				.make_file_builder("stale_uuid.txt", rss.dir.uuid())
				.unwrap(),
			b"v1",
		)
		.await
		.unwrap();

	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let file_path: FfiId = test_dir_path.join("stale_uuid.txt");
	db.update_dir_children(test_dir_path.clone()).await.unwrap();

	// The id the app would have persisted before this branch existed.
	let persisted_uuid = original.uuid().to_string();

	let edited = rss
		.client
		.upload_file(
			rss.client
				.make_file_builder("stale_uuid.txt", rss.dir.uuid())
				.unwrap(),
			b"v2",
		)
		.await
		.unwrap();
	assert_ne!(edited.uuid(), original.uuid());
	db.update_dir_children(test_dir_path).await.unwrap();

	// The persisted value is nobody's `uuid` any more — only the stable fallback can find it.
	let found = match db.query_item_by_uuid(&persisted_uuid).unwrap() {
		Some(FfiObject::File(f)) => f,
		other => panic!("a pre-edit uuid must still resolve, got {other:?}"),
	};
	assert_eq!(found.uuid, edited.uuid().to_string());
	assert_eq!(found.stable_uuid, original.stable_uuid().to_string());

	// The same fallback backs the path lookup the provider uses to locate an item.
	assert_eq!(
		db.query_path_for_uuid(persisted_uuid)
			.unwrap()
			.map(|id| id.0),
		Some(file_path.0.clone()),
	);
}

// A trashed item is addressed as `trash/<uuid>`, not by a display-name path. `canonicalize_id` has
// to notice that and produce the trash form, otherwise every stable id would collapse to a path
// lookup that cannot find a trashed row.
#[shared_test_runtime]
pub async fn test_the_stable_namespace_addresses_a_trashed_item() {
	let (db, rss) = get_db_resources().await;

	let file = rss
		.client
		.upload_file(
			rss.client
				.make_file_builder("stable_trash.txt", rss.dir.uuid())
				.unwrap(),
			b"v1",
		)
		.await
		.unwrap();

	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let file_path: FfiId = test_dir_path.join("stable_trash.txt");
	db.update_dir_children(test_dir_path).await.unwrap();

	let stable_id: FfiId = format!("stable/{}", file.stable_uuid()).into();
	db.trash_item(file_path).await.unwrap();

	let by_stable = match db.query_item(&stable_id).unwrap() {
		Some(FfiObject::File(f)) => f,
		other => panic!("a trashed item must resolve through its stable id, got {other:?}"),
	};
	assert_eq!(by_stable.uuid, file.uuid().to_string());
	// `original_parent` is Some only for a trashed item (it is where a restore would put it back).
	assert_eq!(
		by_stable.original_parent,
		Some(rss.dir.uuid().to_string()),
		"the item must come back as trashed, not as a live child"
	);

	// Resolving is not enough: a bare uuid also finds the row, so it would not tell us which form
	// canonicalize_id produced. The echoed id does. Re-asserting the rank the item already has
	// short-circuits before any server call, so this observes the id and changes nothing.
	assert_eq!(
		db.set_favorite_rank(stable_id, 0).await.unwrap().id.0,
		format!("trash/{}", file.uuid()),
		"a trashed item's stable id must canonicalise to the trash form, not a bare uuid"
	);
}

// An edit that has not reached the server yet must survive a download of that same file. The
// freshness check sees the local bytes differ from the server's, which is exactly what an
// outstanding edit looks like, so without a guard a routine refresh — a preview, a thumbnail, any
// re-open — overwrites the edit with the copy it was supposed to replace. The drain would then see
// local and server agreeing and clear the marker, reporting success for work it destroyed.
#[shared_test_runtime]
pub async fn test_a_download_does_not_clobber_an_unuploaded_edit() {
	let (db, rss) = get_db_resources().await;

	rss.client
		.upload_file(
			rss.client
				.make_file_builder("download_clobber.txt", rss.dir.uuid())
				.unwrap(),
			b"server copy",
		)
		.await
		.unwrap();

	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let file_path: FfiId = test_dir_path.join("download_clobber.txt");
	db.update_dir_children(test_dir_path).await.unwrap();

	// The state a failed upload leaves behind: local bytes ahead of the server, edit marked.
	const EDITED: &[u8] = b"edited locally, never uploaded";
	mark_pending_upload_over(&db, &file_path, EDITED).await;

	let served = db
		.download_file_if_changed_by_path(file_path.clone(), None)
		.await
		.unwrap();
	assert_eq!(
		tokio::fs::read(&served).await.unwrap(),
		EDITED,
		"a download must not overwrite an edit that is still waiting to be uploaded"
	);
	assert!(
		pending_marker(&db, &file_path).is_some(),
		"the marker must survive, so the drain still knows to retry"
	);
}

// The marker arms a bypass of the freshness check, so it must not outlive the bytes it describes.
// A cache clear and the size-budget sweep both delete the local copy without touching the row, and
// neither the drain nor an upload runs afterwards to tidy up — so the download path drops the
// marker itself once it finds nothing on disk, rather than leaving the bypass armed forever.
#[shared_test_runtime]
pub async fn test_a_download_drops_a_marker_whose_local_copy_is_gone() {
	let (db, rss) = get_db_resources().await;

	rss.client
		.upload_file(
			rss.client
				.make_file_builder("marker_no_copy.txt", rss.dir.uuid())
				.unwrap(),
			b"server copy",
		)
		.await
		.unwrap();

	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let file_path: FfiId = test_dir_path.join("marker_no_copy.txt");
	db.update_dir_children(test_dir_path).await.unwrap();

	// Marked, but the local copy was never materialised — the state a cache clear leaves behind.
	mark_pending_upload(&db, &file_path).await;

	let served = db
		.download_file_if_changed_by_path(file_path.clone(), None)
		.await
		.unwrap();
	assert_eq!(
		tokio::fs::read(&served).await.unwrap(),
		b"server copy",
		"with no local copy to protect the download must proceed normally"
	);
	assert_eq!(
		pending_marker(&db, &file_path),
		None,
		"a marker with no bytes behind it must be dropped, not left arming the bypass"
	);
}

// The replicated-provider substrate: the change feed a replica diffs against, the working set it
// keeps current, and the item-level FFI it drives.
//
// The whole live suite shares one cache and one account, so the feed — which is domain-global by
// design — carries whatever else is running alongside. Every assertion below is therefore about
// ONE item, located by its own id; a count is only ever taken over a single item's own entries.

/// Every file the feed carries under this stable id. A file's `stable_uuid` is what a replica
/// persists, so it is what identifies the file across the content edits that re-mint its `uuid`.
fn feed_files<'a>(changes: &'a FfiChanges, stable_uuid: &str) -> Vec<&'a FfiFile> {
	changes
		.updated
		.iter()
		.filter_map(|obj| match obj {
			FfiObject::File(f) if f.stable_uuid == stable_uuid => Some(f),
			_ => None,
		})
		.collect()
}

/// Every dir the feed carries under this uuid — a dir's uuid is its whole-life id.
fn feed_dirs<'a>(changes: &'a FfiChanges, uuid: &str) -> Vec<&'a FfiDir> {
	changes
		.updated
		.iter()
		.filter_map(|obj| match obj {
			FfiObject::Dir(d) if d.uuid == uuid => Some(d),
			_ => None,
		})
		.collect()
}

fn one_feed_file<'a>(changes: &'a FfiChanges, stable_uuid: &str, what: &str) -> &'a FfiFile {
	let found = feed_files(changes, stable_uuid);
	assert_eq!(
		found.len(),
		1,
		"{what}: expected exactly one feed entry for {stable_uuid}, got {found:#?}"
	);
	found[0]
}

fn one_feed_dir<'a>(changes: &'a FfiChanges, uuid: &str, what: &str) -> &'a FfiDir {
	let found = feed_dirs(changes, uuid);
	assert_eq!(
		found.len(),
		1,
		"{what}: expected exactly one feed entry for {uuid}, got {found:#?}"
	);
	found[0]
}

/// Whether the feed retires this id. Both kinds ride in the one `stable/<id>` namespace.
fn feed_retires(changes: &FfiChanges, id: &str) -> bool {
	changes
		.deleted_ids
		.iter()
		.any(|d| d == &format!("stable/{id}"))
}

/// Everything a replica renders about one item, through its whole life on the server: it appears,
/// it is renamed, it goes to the trash, and it is destroyed. Each step is a real server mutation
/// learned the way the app learns one, and each is diffed from the anchor the previous step handed
/// back — which is exactly the sequence a replica walks.
#[shared_test_runtime]
pub async fn test_the_change_feed_follows_an_item_from_creation_to_deletion() {
	let (db, rss) = get_db_resources().await;

	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	// The parent has to be in the cache before the anchor is taken, or inserting it lands in the
	// first diff as a change of its own.
	db.update_dir_children(test_dir_path.clone()).await.unwrap();

	let anchor = db.current_sync_anchor().unwrap();

	let dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "feed_subdir")
		.await
		.unwrap();
	let mut file = rss
		.client
		.upload_file(
			rss.client
				.make_file_builder("feed_file.txt", rss.dir.uuid())
				.unwrap(),
			b"feed v1",
		)
		.await
		.unwrap();
	db.update_dir_children(test_dir_path.clone()).await.unwrap();

	let stable = file.stable_uuid().to_string();
	let dir_uuid = dir.uuid().to_string();

	let changes = db.enumerate_changes(Some(anchor)).unwrap();
	let created_file = one_feed_file(&changes, &stable, "creation").clone();
	assert_eq!(created_file.uuid, file.uuid().to_string());
	assert_eq!(created_file.meta.as_ref().unwrap().name, "feed_file.txt");
	assert_eq!(created_file.parent, rss.dir.uuid().to_string());
	let created_dir = one_feed_dir(&changes, &dir_uuid, "creation").clone();
	assert_eq!(created_dir.meta.as_ref().unwrap().name, "feed_subdir");
	assert!(
		!feed_retires(&changes, &stable) && !feed_retires(&changes, &dir_uuid),
		"nothing was retired, so nothing may be reported as deleted"
	);

	// Renamed on the server, learned the way the app learns it: by listing the directory again.
	let anchor = changes.anchor;
	rss.client
		.update_file_metadata(
			&mut file,
			FileMetaChanges::default()
				.name("feed_file_renamed.txt")
				.unwrap(),
		)
		.await
		.unwrap();
	db.update_dir_children(test_dir_path.clone()).await.unwrap();

	let changes = db.enumerate_changes(Some(anchor)).unwrap();
	let renamed = one_feed_file(&changes, &stable, "remote rename").clone();
	assert_eq!(renamed.meta.as_ref().unwrap().name, "feed_file_renamed.txt");
	assert!(
		renamed.change_seq > created_file.change_seq,
		"a rename must move the item's metadata version"
	);
	assert!(
		feed_dirs(&changes, &dir_uuid).is_empty(),
		"a sibling nothing happened to must not be restamped by the relisting"
	);
	assert!(!feed_retires(&changes, &stable));

	// Trashed: a move into another container, not a disappearance.
	let anchor = changes.anchor;
	let trashed = db
		.trash_item(test_dir_path.join("feed_file_renamed.txt"))
		.await
		.unwrap();

	let changes = db.enumerate_changes(Some(anchor)).unwrap();
	let in_trash = one_feed_file(&changes, &stable, "trash").clone();
	assert_eq!(
		in_trash.original_parent,
		Some(rss.dir.uuid().to_string()),
		"a trashed item must arrive as an update, carrying where it came from"
	);
	assert!(in_trash.change_seq > renamed.change_seq);
	assert!(
		!feed_retires(&changes, &stable),
		"trashing is a move, not a deletion"
	);

	// Destroyed: now, and only now, the id is retired.
	let anchor = changes.anchor;
	db.delete_item(trashed.id).await.unwrap();

	let changes = db.enumerate_changes(Some(anchor)).unwrap();
	assert!(
		feed_files(&changes, &stable).is_empty(),
		"a destroyed item has nothing left to render"
	);
	assert!(
		feed_retires(&changes, &stable),
		"a permanent delete must retire the file's stable id"
	);

	// A replica starting from nothing is told what exists, never what does not.
	let from_scratch = db.enumerate_changes(None).unwrap();
	assert!(
		from_scratch.deleted_ids.is_empty(),
		"there is nothing to drop for a replica that holds nothing"
	);
	assert!(feed_files(&from_scratch, &stable).is_empty());
	assert_eq!(
		one_feed_dir(&from_scratch, &dir_uuid, "full enumeration")
			.meta
			.as_ref()
			.unwrap()
			.name,
		"feed_subdir"
	);
}

// An edit on a versioning-disabled account replaces the file in place: the server mints a new uuid
// and keeps the stable id, and the old uuid lives on briefly as a trashed ghost. That is the same
// provider identity with new content, so a replica must be handed one update and no deletion —
// retiring the id here would make it evict a file the user still has.
#[shared_test_runtime]
pub async fn test_a_versioning_disabled_edit_is_one_update_and_no_retirement() {
	let (db, rss) = get_db_resources().await;

	let _version_lock = rss
		.client
		.acquire_lock_with_default("test:versions")
		.await
		.unwrap();
	rss.client.set_versioning_enabled(false).await.unwrap();

	let old_file = rss
		.client
		.upload_file(
			rss.client
				.make_file_builder("feed_versioning_off.txt", rss.dir.uuid())
				.unwrap(),
			b"versioning off v1",
		)
		.await
		.unwrap();

	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	db.update_dir_children(test_dir_path.clone()).await.unwrap();

	let anchor = db.current_sync_anchor().unwrap();

	let new_file = rss
		.client
		.upload_file(
			rss.client
				.make_file_builder("feed_versioning_off.txt", rss.dir.uuid())
				.unwrap(),
			b"versioning off v2",
		)
		.await
		.unwrap();
	assert_ne!(new_file.uuid(), old_file.uuid());
	assert_eq!(new_file.stable_uuid(), old_file.stable_uuid());
	db.update_dir_children(test_dir_path).await.unwrap();

	let changes = db.enumerate_changes(Some(anchor)).unwrap();
	let stable = old_file.stable_uuid().to_string();
	let edited = one_feed_file(&changes, &stable, "versioning-disabled edit");
	assert_eq!(
		edited.uuid,
		new_file.uuid().to_string(),
		"the feed must carry the new head, not the ghost the edit left behind"
	);
	assert!(
		!feed_retires(&changes, &stable),
		"the file's identity survived the edit; retiring it would evict the file"
	);
	assert!(
		!feed_retires(&changes, &old_file.uuid().to_string()),
		"a re-minted uuid is not an identity any replica was ever given"
	);

	rss.client.set_versioning_enabled(true).await.unwrap();
}

// An anchor names a sequence in one incarnation of the database. A wipe reinitialises the schema
// and mints a new instance id, so an anchor from before it names a history that no longer exists —
// honouring it would silently under-report everything the wipe destroyed. It has to come back as
// its own error, because the answer to it is to enumerate from scratch rather than to fail.
#[shared_test_runtime]
pub async fn test_an_anchor_from_a_previous_database_incarnation_expires() {
	let (db, _rss) = get_db_resources().await;
	let before_the_wipe = db.current_sync_anchor().unwrap();

	// A cache built from nothing is exactly what a wipe leaves behind: fresh schema, fresh
	// instance id, no history.
	let (wiped, _rss) = get_isolated_db_resources("anchor_expiry").await;

	match wiped.enumerate_changes(Some(before_the_wipe)) {
		Err(CacheError::SyncAnchorExpired(_)) => {}
		other => panic!("an anchor from another incarnation must expire, got {other:?}"),
	}
	// Garbage in the same position answers the same way, because the remedy is the same.
	match wiped.enumerate_changes(Some(vec![0u8; 4])) {
		Err(CacheError::SyncAnchorExpired(_)) => {}
		other => panic!("a malformed anchor must expire, got {other:?}"),
	}
	// Its own anchors still work, so what expired was the anchor and not the feed.
	wiped
		.enumerate_changes(Some(wiped.current_sync_anchor().unwrap()))
		.unwrap();
}

// `modify_file_content` is the provider's save: the new bytes arrive as a file the caller owns,
// outside this cache entirely, which is the one thing `upload_file_if_changed` cannot take. It has
// to land them as a new version of an existing file, hand back what that file became, and not
// re-upload bytes the server already holds.
#[shared_test_runtime]
pub async fn test_modify_file_content_lands_external_bytes_as_a_new_version() {
	let (db, rss) = get_db_resources().await;

	rss.client
		.upload_file(
			rss.client
				.make_file_builder("modify_content.txt", rss.dir.uuid())
				.unwrap(),
			b"server v1",
		)
		.await
		.unwrap();

	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let file_path: FfiId = test_dir_path.join("modify_content.txt");
	db.update_dir_children(test_dir_path).await.unwrap();
	let (uuid_before, stable) = file_ids(&db, &file_path);

	const EDITED: &[u8] = b"external bytes, edited elsewhere";
	let external = std::env::temp_dir().join("modify_content_external.txt");
	tokio::fs::write(&external, EDITED).await.unwrap();
	let external_str = external.to_string_lossy().into_owned();

	let modified = db
		.modify_file_content(file_path.clone(), external_str.clone(), None, None)
		.await
		.unwrap();
	assert_eq!(
		modified.file.stable_uuid, stable,
		"an edit keeps the file's identity"
	);
	assert_ne!(
		modified.file.uuid, uuid_before,
		"...and takes a freshly minted version id"
	);
	assert_eq!(modified.file.size as usize, EDITED.len());
	assert_eq!(
		modified.file.pending_upload_at, None,
		"the edit reached the server, so nothing is left outstanding"
	);
	assert_eq!(
		db.query_item(&modified.id).unwrap(),
		Some(FfiObject::File(modified.file.clone())),
		"the item handed back must be the item the cache now holds"
	);
	assert_eq!(
		tokio::fs::read(
			&db.download_file_if_changed_by_path(modified.id.clone(), None)
				.await
				.unwrap()
		)
		.await
		.unwrap(),
		EDITED,
		"the bytes handed in must be the file's content"
	);

	// The same bytes again: a provider saves on close whether the user typed anything or not.
	let unchanged = db
		.modify_file_content(file_path.clone(), external_str.clone(), None, None)
		.await
		.unwrap();
	assert_eq!(
		unchanged.file.uuid, modified.file.uuid,
		"bytes the server already holds must not make a new version"
	);
	assert_eq!(
		unchanged.file.change_seq, modified.file.change_seq,
		"nor restamp the item"
	);
	assert_eq!(unchanged.file.pending_upload_at, None);

	// Renamed, then addressed by the id a provider actually persists — the display path it was
	// created under does not name it any more.
	db.rename_item(file_path.clone(), "modify_content_renamed.txt".to_string())
		.await
		.unwrap()
		.unwrap();
	const AGAIN: &[u8] = b"external bytes, a second edit";
	tokio::fs::write(&external, AGAIN).await.unwrap();

	let by_stable = db
		.modify_file_content(format!("stable/{stable}").into(), external_str, None, None)
		.await
		.unwrap();
	assert_eq!(by_stable.file.stable_uuid, stable);
	assert_ne!(by_stable.file.uuid, modified.file.uuid);
	assert_eq!(
		by_stable.file.meta.as_ref().unwrap().name,
		"modify_content_renamed.txt",
		"the stable id must have reached the file under its current name"
	);
	assert_eq!(by_stable.file.size as usize, AGAIN.len());

	tokio::fs::remove_file(&external).await.ok();
}

// `update_and_query_item` is the provider's `item(for:)`: what this id is right now, asked of the
// server. Only the server saying it does not have it may read as a deletion — and when it does,
// the row goes with it, which is what retires the id for every replica.
#[shared_test_runtime]
pub async fn test_update_and_query_item_follows_a_file_to_its_deletion() {
	let (db, rss) = get_db_resources().await;

	let mut file = rss
		.client
		.upload_file(
			rss.client
				.make_file_builder("refresh_item.txt", rss.dir.uuid())
				.unwrap(),
			b"refresh v1",
		)
		.await
		.unwrap();

	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let file_path: FfiId = test_dir_path.join("refresh_item.txt");
	db.update_dir_children(test_dir_path).await.unwrap();

	let stable = file.stable_uuid().to_string();
	let stable_id: FfiId = format!("stable/{stable}").into();

	// Nothing happened: the same item comes back, and the refresh restamps nothing.
	let cached = db.query_item(&file_path).unwrap().unwrap();
	assert_eq!(
		db.update_and_query_item(stable_id.clone()).await.unwrap(),
		Some(cached),
		"a refresh that finds nothing new must change nothing"
	);

	// Renamed behind the cache's back, with no listing in between.
	rss.client
		.update_file_metadata(
			&mut file,
			FileMetaChanges::default()
				.name("refresh_item_renamed.txt")
				.unwrap(),
		)
		.await
		.unwrap();
	let refreshed = match db.update_and_query_item(stable_id.clone()).await.unwrap() {
		Some(FfiObject::File(f)) => f,
		other => panic!("expected the renamed file, got {other:?}"),
	};
	assert_eq!(
		refreshed.meta.as_ref().unwrap().name,
		"refresh_item_renamed.txt"
	);

	// Edited behind its back. The server re-mints the uuid on a content edit, so the id the cache
	// holds names a version that is no longer the file — the one thing a by-uuid refresh cannot see
	// past. Asked by the lineage's id, the answer is the new head.
	const EDITED: &[u8] = b"refresh v2, edited elsewhere";
	let pre_edit_uuid = file.uuid();
	let edited = rss
		.client
		.upload_file(
			rss.client
				.make_file_builder("refresh_item_renamed.txt", rss.dir.uuid())
				.unwrap(),
			EDITED,
		)
		.await
		.unwrap();
	assert_ne!(edited.uuid(), pre_edit_uuid);
	assert_eq!(
		edited.stable_uuid(),
		file.stable_uuid(),
		"the edit must keep the lineage, or this test proves nothing"
	);
	let head = match db.update_and_query_item(stable_id.clone()).await.unwrap() {
		Some(FfiObject::File(f)) => f,
		other => panic!("expected the edited head, got {other:?}"),
	};
	assert_eq!(
		head.uuid,
		edited.uuid().to_string(),
		"the refresh must follow the file across the edit, not report the version it held"
	);
	assert_eq!(head.stable_uuid, stable, "and it is still the same file");
	assert_eq!(head.size as usize, EDITED.len());
	assert_eq!(
		db.query_item_by_uuid(&stable).unwrap(),
		Some(FfiObject::File(head)),
		"the head must land on the row, not merely be reported"
	);
	let archived = std::mem::replace(&mut file, edited);

	// Trashed behind its back: still an item, and still this item.
	rss.client.trash_file(&mut file).await.unwrap();
	let trashed = match db.update_and_query_item(stable_id.clone()).await.unwrap() {
		Some(FfiObject::File(f)) => f,
		other => panic!("a trashed file is still an item, got {other:?}"),
	};
	assert_eq!(
		trashed.original_parent,
		Some(rss.dir.uuid().to_string()),
		"it must come back as trashed, carrying where a restore would put it"
	);

	// Gone from the server: gone here too, and every replica is told. The whole lineage has to go —
	// the version the edit archived answers a by-stable ask just as the head does, so a file is only
	// "not found" once nothing of it is left.
	let anchor = db.current_sync_anchor().unwrap();
	rss.client.delete_file_permanently(file).await.unwrap();
	rss.client.delete_file_permanently(archived).await.unwrap();
	assert_eq!(
		db.update_and_query_item(stable_id).await.unwrap(),
		None,
		"only a not-found from the server may read as a deletion"
	);
	assert_eq!(
		db.query_item_by_uuid(&stable).unwrap(),
		None,
		"the row must be gone, not merely unreported"
	);
	assert!(
		feed_retires(&db.enumerate_changes(Some(anchor)).unwrap(), &stable),
		"dropping the row must retire its id for every replica"
	);
}

// The same edit on a versioning-disabled account, where the server replaces the file in place: the
// uuid the cache holds lives on for ~60s as a trashed ghost, re-stamped with a stable id of its
// own. That ghost is what a by-uuid refresh gets — an answer that is neither this file nor a
// deletion. By the lineage's id there is only the head, and it is not trashed.
#[shared_test_runtime]
pub async fn test_update_and_query_item_follows_a_versioning_disabled_edit() {
	let (db, rss) = get_db_resources().await;

	let _version_lock = rss
		.client
		.acquire_lock_with_default("test:versions")
		.await
		.unwrap();
	rss.client.set_versioning_enabled(false).await.unwrap();

	let old_file = rss
		.client
		.upload_file(
			rss.client
				.make_file_builder("refresh_versioning_off.txt", rss.dir.uuid())
				.unwrap(),
			b"versioning off v1",
		)
		.await
		.unwrap();

	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	db.update_dir_children(test_dir_path).await.unwrap();

	const EDITED: &[u8] = b"versioning off v2";
	let new_file = rss
		.client
		.upload_file(
			rss.client
				.make_file_builder("refresh_versioning_off.txt", rss.dir.uuid())
				.unwrap(),
			EDITED,
		)
		.await
		.unwrap();
	assert_ne!(new_file.uuid(), old_file.uuid());
	assert_eq!(new_file.stable_uuid(), old_file.stable_uuid());

	let stable = old_file.stable_uuid().to_string();
	let head = match db
		.update_and_query_item(format!("stable/{stable}").into())
		.await
		.unwrap()
	{
		Some(FfiObject::File(f)) => f,
		other => panic!("expected the replaced file's head, got {other:?}"),
	};
	assert_eq!(head.uuid, new_file.uuid().to_string());
	assert_eq!(head.size as usize, EDITED.len());
	assert_eq!(
		head.original_parent, None,
		"the ghost the edit left behind is trashed; the head is not"
	);

	rss.client.set_versioning_enabled(true).await.unwrap();
}

// The same call over a directory, and over ids that name nothing. Both forms below still answer
// `None`, by different routes: the bare uuid earns it from the server (see the unlisted-dir test
// beneath), while `stable/<id>` is a namespace only this cache hands out, so there is nothing to
// ask about at all and no network is touched.
#[shared_test_runtime]
pub async fn test_update_and_query_item_refreshes_a_dir_and_knows_nothing_of_unknown_ids() {
	let (db, rss) = get_db_resources().await;

	let mut dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "refresh_dir")
		.await
		.unwrap();

	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	db.update_dir_children(test_dir_path).await.unwrap();

	// A dir has no stable id of its own; the same namespace resolves it by its uuid, which is its
	// whole-life id anyway.
	let dir_id: FfiId = format!("stable/{}", dir.uuid()).into();
	rss.client
		.update_dir_metadata(
			&mut dir,
			DirectoryMetaChanges::default()
				.name("refresh_dir_renamed")
				.unwrap(),
		)
		.await
		.unwrap();

	let refreshed = match db.update_and_query_item(dir_id).await.unwrap() {
		Some(FfiObject::Dir(d)) => d,
		other => panic!("expected the renamed directory, got {other:?}"),
	};
	assert_eq!(refreshed.uuid, dir.uuid().to_string());
	assert_eq!(refreshed.meta.as_ref().unwrap().name, "refresh_dir_renamed");

	let unknown = Uuid::new_v4();
	assert_eq!(
		db.update_and_query_item(unknown.to_string().into())
			.await
			.unwrap(),
		None,
		"an id that resolves to no row of ours is one we retired"
	);
	assert_eq!(
		db.update_and_query_item(format!("stable/{unknown}").into())
			.await
			.unwrap(),
		None,
		"and the same in the namespace the provider persists"
	);
}

// A bare uuid is the one id form the server can be asked about, and it is the form the providers
// use for directories — including directories this cache has never listed, which is what a remote
// move into a fresh one leaves a replica holding. Such an id must be asked about, not declared
// deleted.
#[shared_test_runtime]
pub async fn test_update_and_query_item_learns_an_unlisted_dir_from_the_server() {
	let (db, rss) = get_db_resources().await;

	// Made on the server and never listed here — nothing below puts it in the cache but the query
	// under test.
	let dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "unlisted_dir")
		.await
		.unwrap();
	let uuid = dir.uuid().to_string();
	assert_eq!(
		db.query_item_by_uuid(&uuid).unwrap(),
		None,
		"the fixture only means anything while the cache has never heard of this directory"
	);

	let learned = match db.update_and_query_item(uuid.clone().into()).await.unwrap() {
		Some(FfiObject::Dir(d)) => d,
		other => panic!("expected the unlisted directory, got {other:?}"),
	};
	assert_eq!(learned.uuid, uuid);
	assert_eq!(learned.meta.as_ref().unwrap().name, "unlisted_dir");
	match db.query_item_by_uuid(&uuid).unwrap() {
		Some(FfiObject::Dir(d)) => assert_eq!(d.uuid, uuid),
		other => panic!("the probe must land the row, not merely answer with it: {other:?}"),
	}

	// And a uuid nothing ever had still answers `None` — now earned from the server rather than
	// assumed.
	assert_eq!(
		db.update_and_query_item(Uuid::new_v4().to_string().into())
			.await
			.unwrap(),
		None,
		"only a not-found from the server may read as a deletion"
	);
}

// A replica asks for a file's bytes and its metadata together; re-querying the item after the
// download would race the download itself. The freshness check is unchanged by that: a second ask
// for a file nothing has touched is served from the cache, with nothing pulled over the network.
#[shared_test_runtime]
pub async fn test_download_file_if_changed_with_item_serves_bytes_and_item_together() {
	let (db, rss) = get_db_resources().await;

	const CONTENT: &[u8] = b"downloaded with its item";
	let file = rss
		.client
		.upload_file(
			rss.client
				.make_file_builder("download_with_item.txt", rss.dir.uuid())
				.unwrap(),
			CONTENT,
		)
		.await
		.unwrap();

	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let file_path: FfiId = test_dir_path.join("download_with_item.txt");
	db.update_dir_children(test_dir_path).await.unwrap();

	let progress = Arc::new(SumProgressCallback::default());
	let fresh = db
		.download_file_if_changed_with_item(file_path.clone(), Some(progress.clone()), None)
		.await
		.unwrap();
	assert_eq!(tokio::fs::read(&fresh.path).await.unwrap(), CONTENT);
	assert_eq!(fresh.file.uuid, file.uuid().to_string());
	assert_eq!(fresh.file.stable_uuid, file.stable_uuid().to_string());
	assert_eq!(fresh.file.size as usize, CONTENT.len());
	assert_eq!(fresh.file.pending_upload_at, None);
	assert_eq!(
		progress.max.load(std::sync::atomic::Ordering::Relaxed),
		CONTENT.len() as u64,
		"the first ask has to actually fetch the bytes"
	);

	let progress = Arc::new(SumProgressCallback::default());
	let cached = db
		.download_file_if_changed_with_item(file_path.clone(), Some(progress.clone()), None)
		.await
		.unwrap();
	assert_eq!(cached.path, fresh.path);
	assert_eq!(cached.file, fresh.file);
	assert_eq!(
		progress.max.load(std::sync::atomic::Ordering::Relaxed),
		0,
		"an unchanged file must be served from the cache, not fetched again"
	);

	// The path-only call the app has always used answers for the same file, unchanged.
	assert_eq!(
		db.download_file_if_changed_by_path(file_path, None)
			.await
			.unwrap(),
		fresh.path
	);

	std::fs::remove_file(&fresh.path).ok();
}

// The working set is what this device has a stake in, and so the only thing kept current
// incrementally: bytes on the device, an edit that has not gone out, or a favourite. Membership
// has to follow those, not a listing — an item drops out the moment the stake does.
#[shared_test_runtime]
pub async fn test_the_working_set_follows_cached_bytes_and_favourites() {
	let (db, rss) = get_db_resources().await;

	let file = rss
		.client
		.upload_file(
			rss.client
				.make_file_builder("working_set_file.txt", rss.dir.uuid())
				.unwrap(),
			b"working set bytes",
		)
		.await
		.unwrap();
	let dir = rss
		.client
		.create_dir(&(&rss.dir).into(), "working_set_dir")
		.await
		.unwrap();

	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let file_path: FfiId = test_dir_path.join("working_set_file.txt");
	let dir_path: FfiId = test_dir_path.join("working_set_dir");
	db.update_dir_children(test_dir_path).await.unwrap();

	let stable = file.stable_uuid().to_string();
	let dir_uuid = dir.uuid().to_string();
	// The set is global to the cache, which the whole suite shares, so membership is only ever
	// asked about one item at a time.
	let holds_file = |db: &FilenMobileCacheState| {
		db.query_working_set()
			.unwrap()
			.iter()
			.any(|obj| matches!(obj, FfiObject::File(f) if f.stable_uuid == stable))
	};
	let holds_dir = |db: &FilenMobileCacheState| {
		db.query_working_set()
			.unwrap()
			.iter()
			.any(|obj| matches!(obj, FfiObject::Dir(d) if d.uuid == dir_uuid))
	};

	assert!(
		!holds_file(&db) && !holds_dir(&db),
		"merely being listed is not a stake in anything"
	);

	let local_path = db
		.download_file_if_changed_by_path(file_path.clone(), None)
		.await
		.unwrap();
	assert!(
		holds_file(&db),
		"bytes on the device are a stake, so the file joins the working set"
	);
	assert!(!holds_dir(&db));

	db.set_favorite_rank(dir_path.clone(), 1).await.unwrap();
	assert!(
		holds_dir(&db),
		"a favourite is a stake even with nothing cached"
	);

	db.clear_local_cache(file_path).await.unwrap();
	assert!(
		!holds_file(&db),
		"with the bytes gone and nothing else claiming it, the file drops out"
	);
	assert!(holds_dir(&db), "which says nothing about the favourite");

	db.set_favorite_rank(dir_path, 0).await.unwrap();
	assert!(
		!holds_dir(&db),
		"and the favourite drops out when it stops being one"
	);

	std::fs::remove_file(&local_path).ok();
}

// A provider persists `stable/<id>` and nothing else, so every entry point has to take one —
// including the two that were only ever handed a raw uuid string. The file here has had its uuid
// re-minted by an edit, which is precisely when the persisted id and the current uuid differ.
#[shared_test_runtime]
pub async fn test_the_stable_namespace_reaches_restore_and_download_after_an_edit() {
	let (db, rss) = get_db_resources().await;

	rss.client
		.upload_file(
			rss.client
				.make_file_builder("stable_reach.txt", rss.dir.uuid())
				.unwrap(),
			b"v1",
		)
		.await
		.unwrap();

	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let file_path: FfiId = test_dir_path.join("stable_reach.txt");
	db.update_dir_children(test_dir_path.clone()).await.unwrap();

	// An edit, so the server re-mints the uuid and the two ids part ways.
	const EDITED: &[u8] = b"v2 edited locally";
	let local_path = db
		.download_file_if_changed_by_path(file_path.clone(), None)
		.await
		.unwrap();
	tokio::fs::write(&local_path, EDITED).await.unwrap();
	assert!(
		db.upload_file_if_changed(file_path.clone(), None)
			.await
			.unwrap()
	);
	db.update_dir_children(test_dir_path).await.unwrap();

	let (uuid, stable) = file_ids(&db, &file_path);
	assert_ne!(
		uuid, stable,
		"the edit must have re-minted the uuid, or this test proves nothing"
	);
	let stable_id = format!("stable/{stable}");

	db.trash_item(file_path).await.unwrap();
	let restored = db.restore_item(&stable_id, None).await.unwrap();
	match restored.object {
		FfiObject::File(f) => {
			assert_eq!(f.stable_uuid, stable);
			assert_eq!(f.uuid, uuid, "a restore is not an edit");
			assert_eq!(
				f.original_parent, None,
				"it must be out of the trash, not merely reported"
			);
			assert_eq!(f.parent, rss.dir.uuid().to_string());
		}
		other => panic!("expected the restored file, got {other:?}"),
	}

	let served = db
		.download_file_if_changed_by_uuid(stable_id, None)
		.await
		.unwrap();
	assert_eq!(tokio::fs::read(&served).await.unwrap(), EDITED);

	std::fs::remove_file(&served).ok();
}

// Cancellation for the calls that move bytes. uniffi cannot cancel a Rust future from Swift, so it
// travels in-band: the caller keeps an `FfiAbortController` and hands its signal to the call. What
// matters is not that the call stops — it is what it leaves behind when it does, which the tests
// below pin per op.

/// Trips the abort from inside the transfer, on the first thing it reports — `set_total`, which the
/// progress task emits as the transfer starts, or the first `on_progress`. That is the deterministic
/// mid-flight hook: the callback only fires because the network work is under way, and the payloads
/// below are large enough that it has nowhere near finished.
struct AbortOnProgress {
	controller: Arc<FfiAbortController>,
	calls: std::sync::atomic::AtomicUsize,
}

impl AbortOnProgress {
	fn new(controller: Arc<FfiAbortController>) -> Self {
		Self {
			controller,
			calls: std::sync::atomic::AtomicUsize::new(0),
		}
	}

	fn trip(&self) {
		self.calls
			.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
		self.controller.abort();
	}

	/// Whether the transfer ever reported anything — i.e. whether the abort really was raised
	/// mid-flight rather than before the op got going.
	fn tripped(&self) -> bool {
		self.calls.load(std::sync::atomic::Ordering::Relaxed) > 0
	}
}

impl ProgressCallback for AbortOnProgress {
	fn set_total(&self, _size: u64) {
		self.trip();
	}

	fn on_progress(&self, _bytes_processed: u64) {
		self.trip();
	}
}

fn random_bytes(len: usize) -> Vec<u8> {
	let mut bytes = vec![0u8; len];
	rand::rng().try_fill_bytes(&mut bytes).unwrap();
	bytes
}

/// Big enough that the transfer is unmistakably still in flight when the callback trips the abort.
const ABORT_PAYLOAD: usize = 8 * 1024 * 1024;

// Aborting a save means "stop working now", never "undo the edit": the bytes are already in the
// slot and the edit already marked by the time the upload starts, and that pair is the only record
// of the user's change. So an abort has to leave exactly what a dropped connection leaves — and the
// drain has to still deliver it.
#[shared_test_runtime]
pub async fn test_an_aborted_modify_leaves_the_edit_for_the_drain() {
	let (db, rss) = get_isolated_db_resources("abort_modify").await;

	rss.client
		.upload_file(
			rss.client
				.make_file_builder("abort_modify.bin", rss.dir.uuid())
				.unwrap(),
			b"server v1",
		)
		.await
		.unwrap();

	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let file_path: FfiId = test_dir_path.join("abort_modify.bin");
	db.update_dir_children(test_dir_path.clone()).await.unwrap();
	let (uuid_before, stable) = file_ids(&db, &file_path);

	let edited = random_bytes(ABORT_PAYLOAD);
	let external = std::env::temp_dir().join("abort_modify_external.bin");
	tokio::fs::write(&external, &edited).await.unwrap();
	let external_str = external.to_string_lossy().into_owned();

	let controller = Arc::new(FfiAbortController::new());
	let callback = Arc::new(AbortOnProgress::new(controller.clone()));
	let err = db
		.modify_file_content(
			file_path.clone(),
			external_str,
			Some(callback.clone()),
			Some(controller.signal()),
		)
		.await
		.expect_err("an aborted save must not report success");
	assert!(
		callback.tripped(),
		"the abort must have been raised from inside the transfer, not before it"
	);
	assert!(matches!(err, CacheError::Aborted(_)), "got {err:?}");

	assert!(
		pending_marker(&db, &file_path).is_some(),
		"an aborted upload must leave the edit marked as outstanding"
	);
	assert_eq!(
		file_ids(&db, &file_path).0,
		uuid_before,
		"nothing reached the server, so no version was minted"
	);
	// Served straight from the slot — the marker is what makes a download hand back local bytes
	// instead of overwriting them — so this is the staged edit itself.
	let slot = db
		.download_file_if_changed_by_path(file_path.clone(), None)
		.await
		.unwrap();
	assert_eq!(
		tokio::fs::read(&slot).await.unwrap(),
		edited,
		"the bytes handed in must still be in the slot"
	);

	assert_eq!(
		db.retry_pending_uploads().await.unwrap(),
		1,
		"the drain must deliver the edit the abort interrupted"
	);
	assert_eq!(
		pending_marker(&db, &file_path),
		None,
		"and leave nothing outstanding once it has"
	);
	db.update_dir_children(test_dir_path).await.unwrap();
	match db.query_item(&file_path).unwrap().unwrap() {
		FfiObject::File(f) => {
			assert_eq!(
				f.size as usize,
				edited.len(),
				"the drained bytes must be the ones now on the server"
			);
			assert_eq!(f.stable_uuid, stable, "and still the same file");
		}
		other => panic!("expected a file, got {other:?}"),
	}

	tokio::fs::remove_file(&external).await.ok();
}

// A download writes to the tmp directory and only renames into the cache slot once it has all the
// bytes, so giving up mid-flight has to leave no cached copy at all — not a truncated one, and not a
// marker claiming the device holds the file. The retry is what proves it: a half-written slot would
// be served as-is.
#[shared_test_runtime]
pub async fn test_an_aborted_download_leaves_no_cached_copy() {
	let (db, rss) = get_db_resources().await;

	let contents = random_bytes(ABORT_PAYLOAD);
	let file = rss
		.client
		.upload_file(
			rss.client
				.make_file_builder("abort_download.bin", rss.dir.uuid())
				.unwrap(),
			&contents,
		)
		.await
		.unwrap();

	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let file_path: FfiId = test_dir_path.join("abort_download.bin");
	db.update_dir_children(test_dir_path).await.unwrap();

	// Bytes on the device are a stake, so the working set is where the materialisation marker
	// shows. The set is global to the cache the suite shares, so it is only ever asked about this
	// one file.
	let stable = file.stable_uuid().to_string();
	let is_cached = |db: &FilenMobileCacheState| {
		db.query_working_set()
			.unwrap()
			.iter()
			.any(|obj| matches!(obj, FfiObject::File(f) if f.stable_uuid == stable))
	};
	assert!(!is_cached(&db), "nothing of it is on the device yet");

	let controller = Arc::new(FfiAbortController::new());
	let callback = Arc::new(AbortOnProgress::new(controller.clone()));
	let err = db
		.download_file_if_changed_with_item(
			file_path.clone(),
			Some(callback.clone()),
			Some(controller.signal()),
		)
		.await
		.expect_err("an aborted download must not report success");
	assert!(
		callback.tripped(),
		"the abort must have been raised from inside the transfer, not before it"
	);
	assert!(matches!(err, CacheError::Aborted(_)), "got {err:?}");
	assert!(
		!is_cached(&db),
		"an aborted download must not claim the device holds the file"
	);

	let served = db
		.download_file_if_changed_with_item(file_path, Some(Arc::new(NoOpProgressCallback)), None)
		.await
		.unwrap();
	assert_eq!(
		tokio::fs::read(&served.path).await.unwrap(),
		contents,
		"the retry must fetch the whole file, not finish someone else's half"
	);
	assert!(is_cached(&db));

	std::fs::remove_file(&served.path).ok();
}

// A new file is nothing here until the upload produces one: the row is written from what the upload
// returned. Aborting may leave chunks on the server that belong to no file — the server collects
// those — but it must never leave a row for a file that never came into being, which every replica
// would then be told about.
#[shared_test_runtime]
pub async fn test_an_aborted_new_upload_leaves_no_row() {
	let (db, rss) = get_db_resources().await;

	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	db.update_dir_children(test_dir_path.clone()).await.unwrap();
	let file_path: FfiId = test_dir_path.join("abort_new.bin");

	let contents = random_bytes(ABORT_PAYLOAD);
	let external = std::env::temp_dir().join("abort_new_external.bin");
	tokio::fs::write(&external, &contents).await.unwrap();
	let external_str = external.to_string_lossy().into_owned();
	let info = || UploadFileInfo {
		name: "abort_new.bin".to_string(),
		creation: None,
		modification: None,
		mime: None,
	};

	let controller = Arc::new(FfiAbortController::new());
	let callback = Arc::new(AbortOnProgress::new(controller.clone()));
	let err = db
		.upload_new_file_abortable(
			external_str.clone(),
			test_dir_path.clone(),
			info(),
			Some(callback.clone()),
			Some(controller.signal()),
		)
		.await
		.expect_err("an aborted upload must not report success");
	assert!(
		callback.tripped(),
		"the abort must have been raised from inside the transfer, not before it"
	);
	assert!(matches!(err, CacheError::Aborted(_)), "got {err:?}");
	assert_eq!(
		db.query_item(&file_path).unwrap(),
		None,
		"a file that never landed must not have a row"
	);

	let uploaded = db
		.upload_new_file_abortable(external_str, test_dir_path, info(), None, None)
		.await
		.unwrap();
	assert_eq!(uploaded.id, file_path);
	assert_eq!(uploaded.file.size as usize, contents.len());
	assert_eq!(
		db.query_item(&file_path).unwrap(),
		Some(FfiObject::File(uploaded.file)),
		"the un-aborted retry lands the file the abort refused"
	);

	tokio::fs::remove_file(&external).await.ok();
}

// A signal that is already aborted means the call was cancelled before it began, and nothing of it
// may happen — no marker, no bytes into the slot, nothing asked of the server. The check sits ahead
// of all three, which is the only reason that holds.
#[shared_test_runtime]
pub async fn test_a_pre_aborted_signal_stops_before_anything_happens() {
	let (db, rss) = get_db_resources().await;

	let file = rss
		.client
		.upload_file(
			rss.client
				.make_file_builder("abort_early.bin", rss.dir.uuid())
				.unwrap(),
			b"server v1",
		)
		.await
		.unwrap();

	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let file_path: FfiId = test_dir_path.join("abort_early.bin");
	db.update_dir_children(test_dir_path).await.unwrap();
	let (uuid_before, stable) = file_ids(&db, &file_path);

	let external = std::env::temp_dir().join("abort_early_external.bin");
	tokio::fs::write(&external, b"external bytes that must not be imported")
		.await
		.unwrap();

	let controller = Arc::new(FfiAbortController::new());
	controller.abort();
	assert!(controller.is_aborted() && controller.signal().is_aborted());
	let progress = Arc::new(SumProgressCallback::default());
	let err = db
		.modify_file_content(
			file_path.clone(),
			external.to_string_lossy().into_owned(),
			Some(progress.clone()),
			Some(controller.signal()),
		)
		.await
		.expect_err("a pre-aborted call must not report success");
	assert!(matches!(err, CacheError::Aborted(_)), "got {err:?}");
	assert_eq!(
		progress.max.load(std::sync::atomic::Ordering::Relaxed),
		0,
		"no transfer may have started"
	);

	// Neither the marker nor the imported bytes exist, and either one would put the file in the
	// working set.
	assert!(
		!db.query_working_set()
			.unwrap()
			.iter()
			.any(|obj| matches!(obj, FfiObject::File(f) if f.stable_uuid == stable)),
		"a call that never started may leave neither a marker nor bytes behind"
	);
	assert_eq!(pending_marker(&db, &file_path), None);
	match db
		.update_and_query_item(format!("stable/{stable}").into())
		.await
		.unwrap()
		.unwrap()
	{
		FfiObject::File(f) => {
			assert_eq!(
				f.uuid, uuid_before,
				"the server minted no new version, so the file is the one it was"
			);
			assert_eq!(f.size, file.size() as i64);
		}
		other => panic!("expected a file, got {other:?}"),
	}

	tokio::fs::remove_file(&external).await.ok();
}

/// Counts the "something in your working set moved" signals the cache raises.
#[derive(Default)]
struct CountingWorkingSetListener(std::sync::atomic::AtomicU64);

impl WorkingSetUpdateListener for CountingWorkingSetListener {
	fn working_set_changed(&self) {
		self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
	}
}

impl CountingWorkingSetListener {
	fn count(&self) -> u64 {
		self.0.load(std::sync::atomic::Ordering::Relaxed)
	}
}

/// Waits until tracking is actually carrying events, by driving changes we do not care about
/// until one comes back.
///
/// The engine's socket connects ASYNCHRONOUSLY behind the registration — the ack fires before the
/// convergence resync, let alone the socket — and an event that lands before it is up is never
/// redelivered. A test that mutates the moment the registration returns is racing that window; one
/// that waits for a signal first is not. Each pass is a real drive change, so the first one after
/// the socket is up is delivered.
async fn wait_until_tracking_is_live(
	db: &Arc<FilenMobileCacheState>,
	item: &FfiId,
	listener: &CountingWorkingSetListener,
) {
	let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
	let mut rank = 0;
	while listener.count() == 0 {
		assert!(
			std::time::Instant::now() < deadline,
			"tracking never carried a change; the socket loop is not live"
		);
		rank = 1 - rank;
		db.set_favorite_rank(item.clone(), rank).await.unwrap();
		let settle = std::time::Instant::now() + std::time::Duration::from_secs(6);
		while listener.count() == 0 && std::time::Instant::now() < settle {
			tokio::time::sleep(std::time::Duration::from_millis(250)).await;
		}
	}
}

// The whole point of tracking: a file this device has a stake in is edited SOMEWHERE ELSE, and the
// change reaches the cache — and through it the replica — without anybody asking for the item.
// Nothing here calls a refreshing API for the file: the only path from the edit to the assertion
// is the engine's socket consumer, the file sync root registered for the lineage, and the bridge
// that lands the record in `native_cache.db`.
#[shared_test_runtime]
pub async fn test_tracking_delivers_a_remote_edit_without_being_asked() {
	let (db, rss) = get_db_resources().await;

	let file = rss
		.client
		.upload_file(
			rss.client
				.make_file_builder("tracked_file.txt", rss.dir.uuid())
				.unwrap(),
			b"before the edit",
		)
		.await
		.unwrap();
	let stable = file.stable_uuid().to_string();

	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let file_path: FfiId = test_dir_path.join("tracked_file.txt");
	db.update_dir_children(test_dir_path).await.unwrap();

	let listener = Arc::new(CountingWorkingSetListener::default());
	db.set_working_set_listener(Some(listener.clone()));

	// Bytes on the device are the stake that puts it in the working set, and so under tracking.
	let local_path = db
		.download_file_if_changed_by_path(file_path.clone(), None)
		.await
		.unwrap();
	db.refresh_working_set_tracking().await.unwrap();
	wait_until_tracking_is_live(&db, &file_path, &listener).await;
	let signals_before = listener.count();

	// The edit, made the way another device would make it: same name, new content — the server
	// re-mints the uuid and announces it, which is precisely what a uuid-keyed cache cannot follow.
	let anchor = db.current_sync_anchor().unwrap();
	let edited = rss
		.client
		.upload_file(
			rss.client
				.make_file_builder("tracked_file.txt", rss.dir.uuid())
				.unwrap(),
			b"after the edit, which is longer",
		)
		.await
		.unwrap();
	assert_eq!(
		edited.stable_uuid().to_string(),
		stable,
		"an edit keeps the lineage; without that there is nothing to track"
	);

	// No refreshing call in this loop — only the feed, which is local. Every poll's retirements are
	// kept: the diff is since-anchor, so a tombstone raised mid-wait shows up in exactly one page.
	let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
	let mut retired: Vec<String> = Vec::new();
	let delivered = loop {
		let changes = db.enumerate_changes(Some(anchor.clone())).unwrap();
		retired.extend(changes.deleted_ids.iter().cloned());
		if let Some(file) = feed_files(&changes, &stable)
			.into_iter()
			.find(|f| f.uuid == edited.uuid().to_string())
		{
			break file.clone();
		}
		assert!(
			std::time::Instant::now() < deadline,
			"the edit never reached the cache through tracking"
		);
		tokio::time::sleep(std::time::Duration::from_millis(500)).await;
	};

	assert_eq!(
		delivered.stable_uuid, stable,
		"the row must be the same identity, re-filed under the new uuid in place"
	);
	assert_eq!(delivered.size, edited.size() as i64);
	assert!(
		listener.count() > signals_before,
		"the replica has to be told, or it will never come and ask"
	);
	// On a versioning-enabled account the edit arrives as `fileArchived` + `fileNew`, which is the
	// same unordered supersede pair a `fileTrash` makes on a versioning-disabled one. Treating
	// either as a removal would retire the id the replica persists — an authoritative delete for a
	// file that is very much still there.
	assert!(
		!retired.iter().any(|id| id.contains(&stable)),
		"the lineage must never be retired by its own edit: {retired:?}"
	);
	// And the local slot survives it. The bytes under the predecessor's uuid are stale, which is
	// fine (the content version moved, so the system re-fetches); evicting them here would be the
	// forget path, which is what a supersede must not take.
	assert!(
		std::fs::metadata(&local_path).is_ok(),
		"the edit must not evict the device's copy"
	);

	db.set_working_set_listener(None);
	db.stop_working_set_tracking();
	std::fs::remove_file(&local_path).ok();
}

/// A remote TRASH is not a delete. The row has to follow the file into the trash — kept, marked,
/// with its original parent and its local bytes — because the user can put it back, and a
/// retirement would have told every replica the file is gone for good.
#[shared_test_runtime]
pub async fn test_a_remote_trash_of_a_tracked_file_trashes_the_row() {
	let (db, rss) = get_db_resources().await;

	let mut file = rss
		.client
		.upload_file(
			rss.client
				.make_file_builder("tracked_trash.txt", rss.dir.uuid())
				.unwrap(),
			b"trash me",
		)
		.await
		.unwrap();
	let stable = file.stable_uuid().to_string();

	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let file_path: FfiId = test_dir_path.join("tracked_trash.txt");
	db.update_dir_children(test_dir_path).await.unwrap();

	// The stake, and with it the tracking.
	let local_path = db
		.download_file_if_changed_by_path(file_path.clone(), None)
		.await
		.unwrap();
	let listener = Arc::new(CountingWorkingSetListener::default());
	db.set_working_set_listener(Some(listener.clone()));
	db.refresh_working_set_tracking().await.unwrap();
	wait_until_tracking_is_live(&db, &file_path, &listener).await;

	let anchor = db.current_sync_anchor().unwrap();
	rss.client.trash_file(&mut file).await.unwrap();

	let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
	let mut retired: Vec<String> = Vec::new();
	loop {
		let changes = db.enumerate_changes(Some(anchor.clone())).unwrap();
		retired.extend(changes.deleted_ids.iter().cloned());
		if feed_files(&changes, &stable)
			.into_iter()
			.any(|f| f.original_parent.is_some())
		{
			break;
		}
		assert!(
			std::time::Instant::now() < deadline,
			"the trash never reached the cache through tracking (retired: {retired:?})"
		);
		tokio::time::sleep(std::time::Duration::from_millis(500)).await;
	}

	assert!(
		!retired.iter().any(|id| id.contains(&stable)),
		"a trash is an update, never a retirement: {retired:?}"
	);
	assert!(
		std::fs::metadata(&local_path).is_ok(),
		"a trashed file keeps its bytes — a restore must not have to re-download them"
	);

	// And back out again: a restore follows the same way, with the original parent restored.
	let anchor = db.current_sync_anchor().unwrap();
	rss.client.restore_file(&mut file).await.unwrap();
	let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
	loop {
		let changes = db.enumerate_changes(Some(anchor.clone())).unwrap();
		if feed_files(&changes, &stable)
			.into_iter()
			.any(|f| f.original_parent.is_none() && f.parent == rss.dir.uuid().to_string())
		{
			break;
		}
		assert!(
			std::time::Instant::now() < deadline,
			"the restore never reached the cache through tracking"
		);
		tokio::time::sleep(std::time::Duration::from_millis(500)).await;
	}

	db.set_working_set_listener(None);
	db.stop_working_set_tracking();
	std::fs::remove_file(&local_path).ok();
	rss.client.delete_file_permanently(file).await.unwrap();
}

/// Tracking delivers a moved file wherever it went, including into a directory this cache has
/// never listed — so the row lands naming a parent no row of ours answers to. The replica renders
/// that parent and then asks about it by uuid, and the answer has to be the directory: it plainly
/// exists, and reporting it deleted would tear the item out from under the OS.
#[shared_test_runtime]
pub async fn test_a_remote_move_into_an_unlisted_dir_leaves_a_resolvable_parent() {
	let (db, rss) = get_db_resources().await;

	let mut file = rss
		.client
		.upload_file(
			rss.client
				.make_file_builder("tracked_move.txt", rss.dir.uuid())
				.unwrap(),
			b"move me",
		)
		.await
		.unwrap();
	let stable = file.stable_uuid().to_string();

	let test_dir_path: FfiId =
		format!("{}/{}", db.root_uuid().unwrap(), rss.dir.name().unwrap()).into();
	let file_path: FfiId = test_dir_path.join("tracked_move.txt");
	db.update_dir_children(test_dir_path).await.unwrap();

	// The stake, and with it the tracking.
	let local_path = db
		.download_file_if_changed_by_path(file_path.clone(), None)
		.await
		.unwrap();
	let listener = Arc::new(CountingWorkingSetListener::default());
	db.set_working_set_listener(Some(listener.clone()));
	db.refresh_working_set_tracking().await.unwrap();
	wait_until_tracking_is_live(&db, &file_path, &listener).await;

	// The destination is made on the server and never listed: the only thing that will ever name
	// it here is the moved file's own row.
	let destination = rss
		.client
		.create_dir(&(&rss.dir).into(), "unlisted_move_target")
		.await
		.unwrap();
	let destination_uuid = destination.uuid().to_string();

	let anchor = db.current_sync_anchor().unwrap();
	rss.client
		.move_file(&mut file, &(&destination).into())
		.await
		.unwrap();

	let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
	loop {
		let changes = db.enumerate_changes(Some(anchor.clone())).unwrap();
		if feed_files(&changes, &stable)
			.into_iter()
			.any(|f| f.parent == destination_uuid)
		{
			break;
		}
		assert!(
			std::time::Instant::now() < deadline,
			"the move never reached the cache through tracking"
		);
		tokio::time::sleep(std::time::Duration::from_millis(500)).await;
	}

	assert_eq!(
		db.query_item_by_uuid(&destination_uuid).unwrap(),
		None,
		"the delivery names the destination without listing it — that dangling parent IS the case"
	);
	match db
		.update_and_query_item(destination_uuid.clone().into())
		.await
		.unwrap()
	{
		Some(FfiObject::Dir(d)) => assert_eq!(d.uuid, destination_uuid),
		other => panic!("the parent the replica asks about must resolve, got {other:?}"),
	}

	db.set_working_set_listener(None);
	db.stop_working_set_tracking();
	std::fs::remove_file(&local_path).ok();
	rss.client.delete_file_permanently(file).await.unwrap();
}

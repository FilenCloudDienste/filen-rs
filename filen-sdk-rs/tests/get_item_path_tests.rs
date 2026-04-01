use std::borrow::Cow;

use filen_macros::shared_test_runtime;

use filen_sdk_rs::{
	ErrorKind,
	fs::{
		HasName, HasParent, HasUUID,
		categories::{NonRootItemType, Normal},
	},
};
use filen_types::fs::ParentUuid;

#[shared_test_runtime]
async fn get_item_path_file_at_root() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = &resources.client;

	// Create a file directly under root
	let file = client
		.make_file_builder("root_file.txt", *client.root().uuid())
		.unwrap()
		.build();
	let file = client.upload_file(file.into(), b"hello").await.unwrap();

	let item = NonRootItemType::<Normal>::File(Cow::Borrowed(&file));
	let (path, ancestors) = client.get_item_path(&item).await.unwrap();

	// File path does not end with /
	assert_eq!(path, "root_file.txt");
	assert!(!path.ends_with('/'));
	assert!(ancestors.is_empty());

	client.delete_file_permanently(file).await.unwrap();
}

#[shared_test_runtime]
async fn get_item_path_dir_at_root() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	// test_dir is itself a child of root
	let item = NonRootItemType::<Normal>::Dir(Cow::Borrowed(test_dir));
	let (path, ancestors) = client.get_item_path(&item).await.unwrap();

	// Dir path ends with /
	assert_eq!(path, format!("{}/", test_dir.name().unwrap()));
	assert!(path.ends_with('/'));
	assert!(ancestors.is_empty());
}

#[shared_test_runtime]
async fn get_item_path_file_vs_dir_same_parent() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let dir = client
		.create_dir(&test_dir.into(), "sibling_dir")
		.await
		.unwrap();

	let file = client
		.make_file_builder("sibling_file", *test_dir.uuid())
		.unwrap()
		.build();
	let file = client.upload_file(file.into(), b"data").await.unwrap();

	let dir_item = NonRootItemType::<Normal>::Dir(Cow::Owned(dir));
	let file_item = NonRootItemType::<Normal>::File(Cow::Owned(file));

	let (dir_path, _) = client.get_item_path(&dir_item).await.unwrap();
	let (file_path, _) = client.get_item_path(&file_item).await.unwrap();

	// Dir ends with /, file does not
	assert!(dir_path.ends_with('/'));
	assert!(!file_path.ends_with('/'));

	let test_dir_name = test_dir.name().unwrap();
	assert_eq!(dir_path, format!("{test_dir_name}/sibling_dir/"));
	assert_eq!(file_path, format!("{test_dir_name}/sibling_file"));
}

#[shared_test_runtime]
async fn get_item_path_nested() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let mid_dir = client.create_dir(&test_dir.into(), "mid").await.unwrap();
	let deep_dir = client
		.create_dir(&mid_dir.clone().into(), "deep")
		.await
		.unwrap();

	let file = client
		.make_file_builder("nested_file.txt", *deep_dir.uuid())
		.unwrap()
		.build();
	let file = client.upload_file(file.into(), b"deep").await.unwrap();

	let item = NonRootItemType::<Normal>::File(Cow::Owned(file));
	let (path, ancestors) = client.get_item_path(&item).await.unwrap();

	assert_eq!(
		path,
		format!("{}/mid/deep/nested_file.txt", test_dir.name().unwrap())
	);
	assert!(!path.ends_with('/'));
	assert_eq!(ancestors.len(), 3);
	assert_eq!(ancestors[0].uuid(), test_dir.uuid());
	assert_eq!(ancestors[1].uuid(), mid_dir.uuid());
	assert_eq!(ancestors[2].uuid(), deep_dir.uuid());
}

#[shared_test_runtime]
async fn get_item_path_nested_dir() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let mid_dir = client.create_dir(&test_dir.into(), "mid2").await.unwrap();
	let deep_dir = client
		.create_dir(&mid_dir.clone().into(), "deep2")
		.await
		.unwrap();

	let item = NonRootItemType::<Normal>::Dir(Cow::Owned(deep_dir));
	let (path, ancestors) = client.get_item_path(&item).await.unwrap();

	assert_eq!(path, format!("{}/mid2/deep2/", test_dir.name().unwrap()));
	assert!(path.ends_with('/'));
	assert_eq!(ancestors.len(), 2);
	assert_eq!(ancestors[0].uuid(), test_dir.uuid());
	assert_eq!(ancestors[1].uuid(), mid_dir.uuid());
}

#[shared_test_runtime]
async fn get_item_path_favorited_file() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let file = client
		.make_file_builder("fav_file.txt", *test_dir.uuid())
		.unwrap()
		.build();
	let mut file = client.upload_file(file.into(), b"fav").await.unwrap();

	client.set_file_favorite(&mut file, true).await.unwrap();
	assert!(file.favorited);

	// Fetch from favorites list — items should have real parent UUIDs
	let (_, fav_files) = client
		.list_favorites(None::<&fn(u64, Option<u64>)>)
		.await
		.unwrap();
	let fav_file = fav_files
		.into_iter()
		.find(|f| f.uuid == file.uuid)
		.expect("File not found in favorites");

	let item = NonRootItemType::<Normal>::File(Cow::Owned(fav_file));
	let (path, ancestors) = client.get_item_path(&item).await.unwrap();

	assert_eq!(path, format!("{}/fav_file.txt", test_dir.name().unwrap()));
	assert!(!path.ends_with('/'));
	assert_eq!(ancestors.len(), 1);
	assert_eq!(ancestors[0].uuid(), test_dir.uuid());
}

#[shared_test_runtime]
async fn get_item_path_favorited_dir() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let mut dir = client
		.create_dir(&test_dir.into(), "fav_dir")
		.await
		.unwrap();

	client.set_dir_favorite(&mut dir, true).await.unwrap();
	assert!(dir.favorited);

	// Fetch from favorites list
	let (fav_dirs, _) = client
		.list_favorites(None::<&fn(u64, Option<u64>)>)
		.await
		.unwrap();
	let fav_dir = fav_dirs
		.into_iter()
		.find(|d| d.uuid == dir.uuid)
		.expect("Dir not found in favorites");

	let item = NonRootItemType::<Normal>::Dir(Cow::Owned(fav_dir));
	let (path, ancestors) = client.get_item_path(&item).await.unwrap();

	assert_eq!(path, format!("{}/fav_dir/", test_dir.name().unwrap()));
	assert!(path.ends_with('/'));
	assert_eq!(ancestors.len(), 1);
	assert_eq!(ancestors[0].uuid(), test_dir.uuid());
}

#[shared_test_runtime]
async fn get_item_path_recent_file() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let file = client
		.make_file_builder("recent_file.txt", *test_dir.uuid())
		.unwrap()
		.build();
	let file = client.upload_file(file.into(), b"recent").await.unwrap();

	// Recently uploaded files should appear in recents
	let (_, recent_files) = client
		.list_recents(None::<&fn(u64, Option<u64>)>)
		.await
		.unwrap();
	let recent_file = recent_files
		.into_iter()
		.find(|f| f.uuid == file.uuid)
		.expect("File not found in recents");

	let item = NonRootItemType::<Normal>::File(Cow::Owned(recent_file));
	let (path, ancestors) = client.get_item_path(&item).await.unwrap();

	assert_eq!(
		path,
		format!("{}/recent_file.txt", test_dir.name().unwrap())
	);
	assert!(!path.ends_with('/'));
	assert_eq!(ancestors.len(), 1);
	assert_eq!(ancestors[0].uuid(), test_dir.uuid());
}

#[shared_test_runtime]
async fn get_item_path_trashed_file_from_list_trash() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let file = client
		.make_file_builder("trash_file.txt", *test_dir.uuid())
		.unwrap()
		.build();
	let mut file = client.upload_file(file.into(), b"trash").await.unwrap();

	client.trash_file(&mut file).await.unwrap();
	assert_eq!(*file.parent(), ParentUuid::Trash);

	// Items from list_trash retain their real parent UUIDs
	let (_, trash_files) = client
		.list_trash(None::<&fn(u64, Option<u64>)>)
		.await
		.unwrap();
	let trash_file = trash_files
		.into_iter()
		.find(|f| f.uuid == file.uuid)
		.expect("File not found in trash");

	// The list_trash API returns real parent, so get_item_path should succeed
	assert!(matches!(*trash_file.parent(), ParentUuid::Uuid(_)));

	let item = NonRootItemType::<Normal>::File(Cow::Owned(trash_file));
	let (path, ancestors) = client.get_item_path(&item).await.unwrap();

	assert_eq!(path, format!("{}/trash_file.txt", test_dir.name().unwrap()));
	assert!(!path.ends_with('/'));
	assert_eq!(ancestors.len(), 1);
	assert_eq!(ancestors[0].uuid(), test_dir.uuid());
}

#[shared_test_runtime]
async fn get_item_path_trashed_file_with_trash_parent_errors() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let file = client
		.make_file_builder("trash_err.txt", *test_dir.uuid())
		.unwrap()
		.build();
	let mut file = client.upload_file(file.into(), b"trash").await.unwrap();

	client.trash_file(&mut file).await.unwrap();
	assert_eq!(*file.parent(), ParentUuid::Trash);

	// After trash_file, the local file object has ParentUuid::Trash.
	// get_item_path refetches via get_file, which also returns ParentUuid::Trash → error.
	let item = NonRootItemType::<Normal>::File(Cow::Owned(file));
	let err = client.get_item_path(&item).await.unwrap_err();
	assert_eq!(err.kind(), ErrorKind::MetadataWasNotDecrypted);
}

#[shared_test_runtime]
async fn get_item_path_trashed_dir_with_trash_parent_errors() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let mut dir = client
		.create_dir(&test_dir.into(), "trash_dir")
		.await
		.unwrap();

	client.trash_dir(&mut dir).await.unwrap();
	assert_eq!(*dir.parent(), ParentUuid::Trash);

	// After trash_dir, the local dir has ParentUuid::Trash.
	// get_item_path refetches via get_dir, which also returns ParentUuid::Trash → error.
	let item = NonRootItemType::<Normal>::Dir(Cow::Owned(dir));
	let err = client.get_item_path(&item).await.unwrap_err();
	assert_eq!(err.kind(), ErrorKind::MetadataWasNotDecrypted);
}

#[shared_test_runtime]
async fn get_item_path_trashed_dir_from_list_trash() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let mut dir = client
		.create_dir(&test_dir.into(), "trash_list_dir")
		.await
		.unwrap();

	client.trash_dir(&mut dir).await.unwrap();

	// Items from list_trash retain their real parent UUIDs
	let (trash_dirs, _) = client
		.list_trash(None::<&fn(u64, Option<u64>)>)
		.await
		.unwrap();
	let trash_dir = trash_dirs
		.into_iter()
		.find(|d| d.uuid == dir.uuid)
		.expect("Dir not found in trash");

	assert!(matches!(*trash_dir.parent(), ParentUuid::Uuid(_)));

	let item = NonRootItemType::<Normal>::Dir(Cow::Owned(trash_dir));
	let (path, ancestors) = client.get_item_path(&item).await.unwrap();

	assert_eq!(
		path,
		format!("{}/trash_list_dir/", test_dir.name().unwrap())
	);
	assert!(path.ends_with('/'));
	assert_eq!(ancestors.len(), 1);
	assert_eq!(ancestors[0].uuid(), test_dir.uuid());
}

#[shared_test_runtime]
async fn get_item_path_nested_favorited() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let sub_dir = client.create_dir(&test_dir.into(), "sub").await.unwrap();

	let file = client
		.make_file_builder("nested_fav.txt", *sub_dir.uuid())
		.unwrap()
		.build();
	let mut file = client.upload_file(file.into(), b"nfav").await.unwrap();

	client.set_file_favorite(&mut file, true).await.unwrap();

	let (_, fav_files) = client
		.list_favorites(None::<&fn(u64, Option<u64>)>)
		.await
		.unwrap();
	let fav_file = fav_files
		.into_iter()
		.find(|f| f.uuid == file.uuid)
		.expect("File not found in favorites");

	let item = NonRootItemType::<Normal>::File(Cow::Owned(fav_file));
	let (path, ancestors) = client.get_item_path(&item).await.unwrap();

	assert_eq!(
		path,
		format!("{}/sub/nested_fav.txt", test_dir.name().unwrap())
	);
	assert!(!path.ends_with('/'));
	assert_eq!(ancestors.len(), 2);
	assert_eq!(ancestors[0].uuid(), test_dir.uuid());
	assert_eq!(ancestors[1].uuid(), sub_dir.uuid());
}

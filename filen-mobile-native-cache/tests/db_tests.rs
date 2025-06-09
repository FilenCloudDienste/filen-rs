use filen_mobile_native_cache::{
	CacheClient, FilenMobileDB,
	ffi::{FfiNonRootObject, FfiObject, FfiRoot, PathWithRoot},
	sql::{DBDir, DBFile},
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

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_query_root() {
	let (db, client, rss) = get_db_resources().await;

	let res = db
		.query_roots_info(rss.client.root().uuid().to_string())
		.unwrap()
		.unwrap();

	assert_eq!(res.max_storage, 0);
	assert_eq!(res.storage_used, 0);
	assert_eq!(res.last_updated, 0);
	assert_eq!(res.uuid, rss.client.root().uuid().to_string());
	assert_eq!(res.last_listed, 0);

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
pub async fn test_query_children() {
	let (db, client, rss) = get_db_resources().await;
	let test_dir_path: PathWithRoot = format!("{}/{}", client.root_uuid(), rss.dir.name()).into();

	let resp = db.query_dir_children(&test_dir_path, None).unwrap();
	// should be none because we haven't updated the children yet
	assert!(resp.is_none());

	db.update_dir_children(&client, test_dir_path.clone())
		.await
		.unwrap();

	let resp = db
		.query_dir_children(&test_dir_path, None)
		.unwrap()
		.unwrap();
	// should be empty because we haven't created any children yet
	assert_eq!(resp.objects.len(), 0);
	assert_eq!(resp.parent.uuid, rss.dir.uuid().to_string());

	let dir = rss
		.client
		.create_dir(&rss.dir, "tmp".to_string())
		.await
		.unwrap();

	let file = rss.client.make_file_builder("file.txt", &rss.dir).build();
	let mut file = rss.client.get_file_writer(file);
	file.write_all(b"Hello, world!").await.unwrap();
	file.close().await.unwrap();
	let file = file.into_remote_file().unwrap();

	db.update_dir_children(&client, test_dir_path.clone())
		.await
		.unwrap();
	let resp = db
		.query_dir_children(&test_dir_path, None)
		.unwrap()
		.unwrap();
	assert_eq!(resp.objects.len(), 2);
	assert_eq!(resp.parent.uuid, rss.dir.uuid().to_string());
	assert!(matches!(
		&resp.objects[0],
		FfiNonRootObject::File(f) if f.uuid == file.uuid().to_string()
	));
	assert!(matches!(
		&resp.objects[1],
		FfiNonRootObject::Dir(d) if d.uuid == dir.uuid().to_string()
	));

	let other_file = rss.client.make_file_builder("other.txt", &rss.dir).build();
	let mut writer = rss.client.get_file_writer(other_file);
	writer.close().await.unwrap();
	let other_file = writer.into_remote_file().unwrap();
	db.update_dir_children(&client, test_dir_path.clone())
		.await
		.unwrap();

	let resp = db
		.query_dir_children(&test_dir_path, Some("size ASC".to_string()))
		.unwrap()
		.unwrap();
	assert_eq!(resp.objects.len(), 3);
	assert!(matches!(
		&resp.objects[2],
		FfiNonRootObject::File(f) if f.uuid == file.uuid().to_string()
	));

	let resp = db
		.query_dir_children(&test_dir_path, Some("size DESC".to_string()))
		.unwrap()
		.unwrap();
	assert_eq!(resp.objects.len(), 3);
	assert!(matches!(
		&resp.objects[0],
		FfiNonRootObject::File(f) if f.uuid == file.uuid().to_string()
	));

	let resp = db
		.query_dir_children(&test_dir_path, Some("display_name ASC".to_string()))
		.unwrap()
		.unwrap();
	assert_eq!(resp.objects.len(), 3);
	assert!(matches!(
		&resp.objects[0],
		FfiNonRootObject::File(f) if f.uuid == file.uuid().to_string()
	));
	assert!(matches!(
		&resp.objects[1],
		FfiNonRootObject::File(f) if f.uuid == other_file.uuid().to_string()
	));
	assert!(matches!(
		&resp.objects[2],
		FfiNonRootObject::Dir(d) if d.uuid == dir.uuid().to_string()
	));

	rss.client.trash_dir(&dir).await.unwrap();
	db.update_dir_children(&client, test_dir_path.clone())
		.await
		.unwrap();
	let resp = db
		.query_dir_children(&test_dir_path, None)
		.unwrap()
		.unwrap();
	assert_eq!(resp.objects.len(), 2);
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
pub async fn test_query_item() {
	let (db, client, rss) = get_db_resources().await;

	let dir = rss
		.client
		.create_dir(&rss.dir, "tmp".to_string())
		.await
		.unwrap();

	let file = rss.client.make_file_builder("file.txt", &rss.dir).build();
	let mut file = rss.client.get_file_writer(file);
	file.write_all(b"Hello, world!").await.unwrap();
	file.close().await.unwrap();
	let file = file.into_remote_file().unwrap();
	let file_path: PathWithRoot =
		format!("{}/{}/{}", client.root_uuid(), rss.dir.name(), file.name()).into();
	let child_dir_path: PathWithRoot =
		format!("{}/{}/{}", client.root_uuid(), rss.dir.name(), dir.name()).into();
	let dir_path: PathWithRoot = format!("{}/{}", client.root_uuid(), rss.dir.name()).into();

	assert_eq!(db.query_item(&file_path).unwrap(), None);

	db.update_dir_children(&client, dir_path.clone())
		.await
		.unwrap();

	assert_eq!(
		db.query_item(&file_path).unwrap(),
		Some(FfiObject::File((Into::<DBFile>::into(file.clone())).into()))
	);

	assert_eq!(
		db.query_item(&child_dir_path).unwrap(),
		Some(FfiObject::Dir((Into::<DBDir>::into(dir.clone())).into()))
	);

	assert_eq!(
		db.query_item(&rss.client.root().uuid().to_string().into())
			.unwrap(),
		Some(FfiObject::Root(FfiRoot {
			uuid: rss.client.root().uuid().to_string(),
			max_storage: 0,
			storage_used: 0,
			last_updated: 0,
			last_listed: 0,
		}))
	);
}

use filen_mobile_native_cache::{CacheClient, FilenMobileDB};
use filen_sdk_rs::fs::{HasUUID, file::traits::HasFileInfo};
use futures::AsyncWriteExt;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
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

	db.update_roots_info(&client, &rss.client.root().uuid().to_string())
		.await
		.unwrap();
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

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
pub async fn test_query_children() {
	let (db, client, rss) = get_db_resources().await;

	let resp = db.query_dir_children(&rss.dir.uuid().to_string()).unwrap();
	// should be none because we haven't updated the children yet
	assert!(resp.is_none());

	db.update_dir_children(&client, &rss.dir.uuid().to_string())
		.await
		.unwrap();

	let resp = db
		.query_dir_children(&rss.dir.uuid().to_string())
		.unwrap()
		.unwrap();
	// should be empty because we haven't created any children yet
	assert_eq!(resp.dirs.len(), 0);
	assert_eq!(resp.files.len(), 0);
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

	db.update_dir_children(&client, &rss.dir.uuid().to_string())
		.await
		.unwrap();
	let resp = db
		.query_dir_children(&rss.dir.uuid().to_string())
		.unwrap()
		.unwrap();
	assert_eq!(resp.dirs.len(), 1);
	assert_eq!(resp.files.len(), 1);
	assert_eq!(resp.parent.uuid, rss.dir.uuid().to_string());
	assert_eq!(resp.dirs[0].name, "tmp");
	assert_eq!(resp.files[0].name, "file.txt");
	assert_eq!(resp.files[0].size, file.size() as i64);

	rss.client.trash_dir(&dir).await.unwrap();
	db.update_dir_children(&client, &rss.dir.uuid().to_string())
		.await
		.unwrap();
	let resp = db
		.query_dir_children(&rss.dir.uuid().to_string())
		.unwrap()
		.unwrap();
	assert_eq!(resp.dirs.len(), 0);
	assert_eq!(resp.files.len(), 1);
}

use std::borrow::Cow;

use chrono::{SubsecRound, Utc};
use filen_sdk_rs::{
	crypto::shared::generate_random_base64_values,
	fs::{FSObjectType, HasUUID, NonRootFSObject},
};

mod test_utils;

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn create_list_trash() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let dir = client.create_dir(test_dir, "test_dir").await.unwrap();
	assert_eq!(dir.name(), "test_dir");

	let (dirs, _) = client.list_dir(test_dir).await.unwrap();

	client.trash_dir(&dir).await.unwrap();

	if !dirs.contains(&dir) {
		panic!("Directory not found in root directory");
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn find_at_path() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let dir_a = client.create_dir(test_dir, "a").await.unwrap();
	let dir_b = client.create_dir(&dir_a, "b").await.unwrap();
	let dir_c = client.create_dir(&dir_b, "c").await.unwrap();

	assert_eq!(
		client
			.find_item_at_path(format!("{}/a/b/c", test_dir.name()))
			.await
			.unwrap(),
		Some(FSObjectType::Dir(std::borrow::Cow::Borrowed(&dir_c)))
	);

	assert_eq!(
		client
			.find_item_at_path(format!("{}/a/bc", test_dir.name()))
			.await
			.unwrap(),
		None
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn find_or_create() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let path = format!("{}/a/b/c", test_dir.name());
	let nested_dir = client.find_or_create_dir(&path).await.unwrap();

	assert_eq!(
		Some(Into::<FSObjectType<'_>>::into(nested_dir)),
		client.find_item_at_path(&path).await.unwrap()
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn list_recursive() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let dir_a = client.create_dir(test_dir, "a").await.unwrap();
	let dir_b = client.create_dir(&dir_a, "b").await.unwrap();
	let dir_c = client.create_dir(&dir_b, "c").await.unwrap();

	let (dirs, _) = client.list_dir_recursive(test_dir).await.unwrap();

	assert!(dirs.contains(&dir_a));
	assert!(dirs.contains(&dir_b));
	assert!(dirs.contains(&dir_c));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn exists() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	assert!(client.dir_exists(test_dir, "a").await.unwrap().is_none());

	let dir_a = client.create_dir(test_dir, "a").await.unwrap();

	assert_eq!(
		Some(dir_a.uuid()),
		client.dir_exists(test_dir, "a").await.unwrap()
	);

	client.trash_dir(&dir_a).await.unwrap();
	assert!(client.dir_exists(test_dir, "a").await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn dir_move() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let mut dir_a = client.create_dir(test_dir, "a").await.unwrap();
	let dir_b = client.create_dir(test_dir, "b").await.unwrap();

	assert!(client.list_dir(&dir_b).await.unwrap().0.is_empty());

	client.move_dir(&mut dir_a, &dir_b).await.unwrap();
	assert!(client.list_dir(&dir_b).await.unwrap().0.contains(&dir_a));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn size() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	assert_eq!(
		client.get_dir_size(test_dir, false).await.unwrap(),
		filen_types::api::v3::dir::size::Response {
			size: 0,
			files: 0,
			dirs: 0
		}
	);

	client.create_dir(test_dir, "a").await.unwrap();
	assert_eq!(
		client.get_dir_size(test_dir, false).await.unwrap(),
		filen_types::api::v3::dir::size::Response {
			size: 0,
			files: 0,
			dirs: 1
		}
	);

	client.create_dir(test_dir, "b").await.unwrap();
	assert_eq!(
		client.get_dir_size(test_dir, false).await.unwrap(),
		filen_types::api::v3::dir::size::Response {
			size: 0,
			files: 0,
			dirs: 2
		}
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn dir_search() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let second_dir = client.create_dir(test_dir, "second_dir").await.unwrap();

	let dir_random_part_long = generate_random_base64_values(16);
	let dir_random_part_short = generate_random_base64_values(2);

	let dir_name = format!("{}{}", dir_random_part_long, dir_random_part_short);

	let dir = client.create_dir(&second_dir, dir_name).await.unwrap();

	let found_items = client
		.find_item_matches_for_name(dir_random_part_long)
		.await
		.unwrap();

	assert_eq!(
		found_items,
		vec![(
			NonRootFSObject::Dir(Cow::Owned(dir.clone())),
			format!("/{}/{}", test_dir.name(), second_dir.name())
		)]
	);

	let found_items = client
		.find_item_matches_for_name(dir_random_part_short)
		.await
		.unwrap();

	assert!(found_items.iter().any(|(item, _)| {
		if let NonRootFSObject::Dir(found_dir) = item {
			*found_dir.clone() == dir
		} else {
			false
		}
	}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn dir_update_meta() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let dir_name = "dir";
	let mut dir = client.create_dir(test_dir, dir_name).await.unwrap();

	assert_eq!(
		client
			.find_item_at_path(format!("{}/{}", test_dir.name(), dir_name))
			.await
			.unwrap(),
		Some(FSObjectType::Dir(Cow::Borrowed(&dir)))
	);

	let mut meta = dir.get_meta();
	meta.set_name("new_name");

	client.update_dir_metadata(&mut dir, meta).await.unwrap();

	assert_eq!(dir.name(), "new_name");
	assert_eq!(
		client
			.find_item_at_path(format!("{}/{}", test_dir.name(), dir.name()))
			.await
			.unwrap(),
		Some(FSObjectType::Dir(Cow::Borrowed(&dir)))
	);

	let mut meta = dir.get_meta();
	let created = Utc::now() - chrono::Duration::days(1);
	meta.set_created(created);

	client.update_dir_metadata(&mut dir, meta).await.unwrap();
	assert_eq!(dir.created(), Some(created.round_subsecs(3)));

	let found_dir = client.get_dir(dir.uuid()).await.unwrap();
	assert_eq!(found_dir.created(), Some(created.round_subsecs(3)));
	assert_eq!(found_dir, dir);
}

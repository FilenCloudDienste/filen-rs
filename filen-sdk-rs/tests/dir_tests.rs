use std::{borrow::Cow, sync::Arc};

use chrono::{SubsecRound, Utc};
use filen_sdk_rs::{
	fs::{FSObjectType, NonRootFSObject},
	prelude::*,
};

mod test_utils;

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn create_list_trash() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let dir = create_dir(client, test_dir, "test_dir".to_string())
		.await
		.unwrap();
	assert_eq!(dir.name(), "test_dir");

	let (dirs, _) = list_dir(client, test_dir).await.unwrap();

	trash_dir(client, dir.clone()).await.unwrap();

	if !dirs.contains(&dir) {
		panic!("Directory not found in root directory");
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn find_at_path() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let dir_a = create_dir(client, test_dir, "a".to_string()).await.unwrap();
	let dir_b = create_dir(client, &dir_a, "b".to_string()).await.unwrap();
	let dir_c = create_dir(client, &dir_b, "c".to_string()).await.unwrap();

	assert_eq!(
		find_item_at_path(client, format!("{}/a/b/c", test_dir.name()))
			.await
			.unwrap(),
		Some(FSObjectType::Dir(std::borrow::Cow::Borrowed(&dir_c)))
	);

	assert_eq!(
		find_item_at_path(client, format!("{}/a/bc", test_dir.name()))
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
	let nested_dir = find_or_create_dir(client, &path).await.unwrap();

	assert_eq!(
		Some(Into::<FSObjectType<'_>>::into(nested_dir)),
		find_item_at_path(client, &path).await.unwrap()
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn list_recursive() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let dir_a = create_dir(client, test_dir, "a".to_string()).await.unwrap();
	let dir_b = create_dir(client, &dir_a, "b".to_string()).await.unwrap();
	let dir_c = create_dir(client, &dir_b, "c".to_string()).await.unwrap();

	let (dirs, _) = list_dir_recursive(client, test_dir).await.unwrap();

	assert!(dirs.contains(&dir_a));
	assert!(dirs.contains(&dir_b));
	assert!(dirs.contains(&dir_c));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn exists() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	assert!(dir_exists(client, test_dir, "a").await.unwrap().is_none());

	let dir_a = create_dir(client, test_dir, "a".to_string()).await.unwrap();

	assert_eq!(
		Some(dir_a.uuid()),
		dir_exists(client, test_dir, "a").await.unwrap()
	);

	trash_dir(client, dir_a).await.unwrap();
	assert!(dir_exists(client, test_dir, "a").await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn dir_move() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let mut dir_a = create_dir(client, test_dir, "a".to_string()).await.unwrap();
	let dir_b = create_dir(client, test_dir, "b".to_string()).await.unwrap();

	assert!(list_dir(client, &dir_b).await.unwrap().0.is_empty());

	move_dir(client, &mut dir_a, &dir_b).await.unwrap();
	assert!(list_dir(client, &dir_b).await.unwrap().0.contains(&dir_a));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn size() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	assert_eq!(
		get_dir_size(client, test_dir, false).await.unwrap(),
		filen_types::api::v3::dir::size::Response {
			size: 0,
			files: 0,
			dirs: 0
		}
	);

	create_dir(client, test_dir, "a".to_string()).await.unwrap();
	assert_eq!(
		get_dir_size(client, test_dir, false).await.unwrap(),
		filen_types::api::v3::dir::size::Response {
			size: 0,
			files: 0,
			dirs: 1
		}
	);

	create_dir(client, test_dir, "b".to_string()).await.unwrap();
	assert_eq!(
		get_dir_size(client, test_dir, false).await.unwrap(),
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
	let client = Arc::new(resources.client.clone());
	let test_dir = &resources.dir;

	let second_dir = create_dir(&client, test_dir, "second_dir").await.unwrap();

	let dir_random_part_long = generate_random_base64_values(16);
	let dir_random_part_short = generate_random_base64_values(2);

	let dir_name = format!("{}{}", dir_random_part_long, dir_random_part_short);

	let dir = create_dir(&client, &second_dir, dir_name).await.unwrap();

	let found_items = find_item_matches_for_name(&client, dir_random_part_long)
		.await
		.unwrap();

	assert_eq!(
		found_items,
		vec![(
			NonRootFSObject::Dir(Cow::Owned(dir.clone())),
			format!("/{}/{}", test_dir.name(), second_dir.name())
		)]
	);

	let found_items = find_item_matches_for_name(&client, dir_random_part_short)
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
	let client = Arc::new(resources.client.clone());
	let test_dir = &resources.dir;

	let dir_name = "dir";
	let mut dir = create_dir(&client, test_dir, dir_name).await.unwrap();

	assert_eq!(
		find_item_at_path(&client, format!("{}/{}", test_dir.name(), dir_name))
			.await
			.unwrap(),
		Some(FSObjectType::Dir(Cow::Borrowed(&dir)))
	);

	let mut meta = dir.get_meta();
	meta.set_name("new_name");

	update_dir_metadata(&client, &mut dir, meta).await.unwrap();

	assert_eq!(dir.name(), "new_name");
	assert_eq!(
		find_item_at_path(&client, format!("{}/{}", test_dir.name(), dir.name()))
			.await
			.unwrap(),
		Some(FSObjectType::Dir(Cow::Borrowed(&dir)))
	);

	let mut meta = dir.get_meta();
	let created = Utc::now() - chrono::Duration::days(1);
	meta.set_created(created);

	update_dir_metadata(&client, &mut dir, meta).await.unwrap();
	assert_eq!(dir.created(), Some(created.round_subsecs(3)));

	let found_dir = get_dir(&client, dir.uuid()).await.unwrap();
	assert_eq!(found_dir.created(), Some(created.round_subsecs(3)));
	assert_eq!(found_dir, dir);
}

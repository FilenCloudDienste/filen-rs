use std::borrow::Cow;

use chrono::{SubsecRound, Utc};
use filen_sdk_rs::{
	crypto::shared::generate_random_base64_values,
	fs::{
		FSObject, HasName, HasUUID, NonRootFSObject, UnsharedFSObject,
		client_impl::ObjectOrRemainingPath,
		dir::{UnsharedDirectoryType, traits::HasDirMeta},
	},
};
use filen_sdk_rs_macros::shared_test_runtime;
use tokio::time;

#[shared_test_runtime]
async fn create_list_trash() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let dir = client
		.create_dir(test_dir, "test_dir".to_string())
		.await
		.unwrap();
	assert_eq!(dir.name(), "test_dir");

	let (dirs, _) = client.list_dir(test_dir).await.unwrap();

	client.trash_dir(&dir).await.unwrap();

	if !dirs.contains(&dir) {
		panic!("Directory not found in root directory");
	}
}

#[shared_test_runtime]
async fn find_at_path() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let dir_a = client.create_dir(test_dir, "a".to_string()).await.unwrap();
	let dir_b = client.create_dir(&dir_a, "b".to_string()).await.unwrap();
	let dir_c = client.create_dir(&dir_b, "c".to_string()).await.unwrap();

	assert_eq!(
		client
			.find_item_at_path(format!("{}/a/b/c", test_dir.name()))
			.await
			.unwrap(),
		Some(FSObject::Dir(std::borrow::Cow::Borrowed(&dir_c)))
	);

	assert_eq!(
		client
			.find_item_at_path(format!("{}/a/bc", test_dir.name()))
			.await
			.unwrap(),
		None
	);

	let path = format!("{}/a/b/c", test_dir.name());

	let items = client.get_items_in_path(&path).await.unwrap();

	assert_eq!(items.0.len(), 4);
	assert!(
		items
			.0
			.contains(&UnsharedDirectoryType::Dir(Cow::Borrowed(&dir_a)))
	);
	assert!(
		items
			.0
			.contains(&UnsharedDirectoryType::Dir(Cow::Borrowed(&dir_b)))
	);
	assert!(
		!items
			.0
			.contains(&UnsharedDirectoryType::Dir(Cow::Borrowed(&dir_c)))
	);
	assert!(matches!(
		items.1,
		ObjectOrRemainingPath::Object(UnsharedFSObject::Dir(dir)) if *dir == dir_c
	));

	let items = client
		.get_items_in_path_starting_at("b/c", UnsharedDirectoryType::Dir(Cow::Borrowed(&dir_a)))
		.await
		.unwrap();
	assert_eq!(items.0.len(), 2);
	assert!(
		items
			.0
			.contains(&UnsharedDirectoryType::Dir(Cow::Borrowed(&dir_a)))
	);
	assert!(
		items
			.0
			.contains(&UnsharedDirectoryType::Dir(Cow::Borrowed(&dir_b)))
	);
	assert!(matches!(
		items.1,
		ObjectOrRemainingPath::Object(UnsharedFSObject::Dir(dir)) if *dir == dir_c
	));

	let resp = client
		.get_items_in_path_starting_at("c/d/e", UnsharedDirectoryType::Dir(Cow::Borrowed(&dir_b)))
		.await
		.unwrap();
	// Expecting None because "c/d/e" does not exist in the path
	// and the last item in dirs be the directory "c"
	match resp {
		(mut dirs, ObjectOrRemainingPath::RemainingPath(path)) => match dirs.pop() {
			Some(UnsharedDirectoryType::Dir(dir)) if *dir == dir_c => {
				assert_eq!(path, "d/e");
			}
			other => panic!("Expected last directory to be 'c', but got: {other:?}"),
		},
		other => panic!("Expected dir_c, but got: {other:?}"),
	}
}

#[shared_test_runtime]
async fn find_or_create() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let path = format!("{}/a/b/c", test_dir.name());
	let nested_dir = client.find_or_create_dir(&path).await.unwrap();

	assert_eq!(
		Some(Into::<FSObject<'_>>::into(nested_dir.clone())),
		client.find_item_at_path(&path).await.unwrap()
	);

	let nested_dir = client
		.find_or_create_dir_starting_at(nested_dir, "d/e")
		.await
		.unwrap();
	assert_eq!(
		Some(Into::<FSObject<'_>>::into(nested_dir.clone())),
		client
			.find_item_at_path(&format!("{path}/d/e"))
			.await
			.unwrap()
	);
}

#[shared_test_runtime]
async fn list_recursive() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let dir_a = client.create_dir(test_dir, "a".to_string()).await.unwrap();
	let dir_b = client.create_dir(&dir_a, "b".to_string()).await.unwrap();
	let dir_c = client.create_dir(&dir_b, "c".to_string()).await.unwrap();

	let (dirs, _) = client.list_dir_recursive(test_dir).await.unwrap();

	assert!(dirs.contains(&dir_a));
	assert!(dirs.contains(&dir_b));
	assert!(dirs.contains(&dir_c));
}

#[shared_test_runtime]
async fn exists() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	assert!(client.dir_exists(test_dir, "a").await.unwrap().is_none());

	let dir_a = client.create_dir(test_dir, "a".to_string()).await.unwrap();

	assert_eq!(
		Some(dir_a.uuid()),
		client.dir_exists(test_dir, "a").await.unwrap()
	);

	client.trash_dir(&dir_a).await.unwrap();
	assert!(client.dir_exists(test_dir, "a").await.unwrap().is_none());
}

#[shared_test_runtime]
async fn dir_move() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let mut dir_a = client.create_dir(test_dir, "a".to_string()).await.unwrap();
	let dir_b = client.create_dir(test_dir, "b".to_string()).await.unwrap();

	assert!(client.list_dir(&dir_b).await.unwrap().0.is_empty());

	client.move_dir(&mut dir_a, &dir_b).await.unwrap();
	assert!(client.list_dir(&dir_b).await.unwrap().0.contains(&dir_a));
}

#[shared_test_runtime]
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

	client.create_dir(test_dir, "a".to_string()).await.unwrap();
	time::sleep(time::Duration::from_secs(100)).await; // ddos protection
	assert_eq!(
		client.get_dir_size(test_dir, false).await.unwrap(),
		filen_types::api::v3::dir::size::Response {
			size: 0,
			files: 0,
			dirs: 1
		}
	);

	client.create_dir(test_dir, "b".to_string()).await.unwrap();
	time::sleep(time::Duration::from_secs(100)).await; // ddos protection
	assert_eq!(
		client.get_dir_size(test_dir, false).await.unwrap(),
		filen_types::api::v3::dir::size::Response {
			size: 0,
			files: 0,
			dirs: 2
		}
	);
}

#[shared_test_runtime]
async fn dir_search() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let second_dir = client
		.create_dir(test_dir, "second_dir".to_string())
		.await
		.unwrap();

	let dir_random_part_long = generate_random_base64_values(16);
	let dir_random_part_short = generate_random_base64_values(2);

	let dir_name = format!("{dir_random_part_long}{dir_random_part_short}");

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

#[shared_test_runtime]
async fn dir_update_meta() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let dir_name = "dir";
	let mut dir = client
		.create_dir(test_dir, dir_name.to_string())
		.await
		.unwrap();

	assert_eq!(
		client
			.find_item_at_path(format!("{}/{}", test_dir.name(), dir_name))
			.await
			.unwrap(),
		Some(FSObject::Dir(Cow::Borrowed(&dir)))
	);

	let mut meta = dir.get_meta();
	meta.set_name("new_name").unwrap();

	client.update_dir_metadata(&mut dir, meta).await.unwrap();

	assert_eq!(dir.name(), "new_name");
	assert_eq!(
		client
			.find_item_at_path(format!("{}/{}", test_dir.name(), dir.name()))
			.await
			.unwrap(),
		Some(FSObject::Dir(Cow::Borrowed(&dir)))
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

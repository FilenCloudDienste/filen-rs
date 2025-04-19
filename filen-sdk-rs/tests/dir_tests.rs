use filen_sdk_rs::{fs::FSObjectType, prelude::*};

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

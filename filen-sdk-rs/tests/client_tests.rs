use filen_sdk_rs::{auth::Client, fs::HasName};
use filen_sdk_rs_macros::shared_test_runtime;
use futures::{StreamExt, stream::FuturesUnordered};

// all tests must be multi_threaded, otherwise drop will deadlock for TestResources
#[shared_test_runtime]
async fn test_login() {
	test_utils::RESOURCES.client().await;
}

#[shared_test_runtime]
async fn test_stringification() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let stringified = client.to_stringified();
	assert_eq!(
		Client::from_strings(
			stringified.email,
			&stringified.root_uuid,
			&stringified.auth_info,
			&stringified.private_key,
			stringified.api_key,
			stringified.auth_version
		)
		.unwrap(),
		**client
	)
}

#[shared_test_runtime]
async fn cleanup_test_dirs() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let _lock = client.lock_drive().await.unwrap();

	let (dirs, _) = client.list_dir(client.root()).await.unwrap();
	let mut futures = FuturesUnordered::new();
	let now = chrono::Utc::now();
	for dir in dirs {
		if dir.name().starts_with("rs-")
			&& dir
				.created()
				.is_none_or(|c| c - now > chrono::Duration::days(1))
		{
			futures.push(async { client.delete_dir_permanently(dir).await });
		}
	}

	while futures.next().await.is_some() {}
}

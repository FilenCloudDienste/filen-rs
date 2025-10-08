use filen_macros::shared_test_runtime;
use filen_sdk_rs::{auth::Client, fs::HasName};
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
	assert_eq!(Client::from_stringified(stringified).unwrap(), **client)
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
		if dir.name().is_some_and(|n| n.starts_with("rs-"))
			&& dir
				.created()
				.is_none_or(|c| now - c > chrono::Duration::days(1))
		{
			futures.push(async { client.delete_dir_permanently(dir).await });
		}
	}

	while futures.next().await.is_some() {}
}

#[shared_test_runtime]
async fn test_2fa() {
	let client = test_utils::RESOURCES.client().await;

	let _lock = client.lock_auth().await.unwrap();

	let secret = client.generate_2fa_secret().await.unwrap();

	let recovery_key = client
		.enable_2fa(
			&secret
				.make_totp_code(chrono::Utc::now())
				.unwrap()
				.to_string(),
		)
		.await
		.unwrap();
	// we print this in case we have to recover the account
	println!("Recovery key: {recovery_key:?}");

	// we use the recovery key here rather than the 2fa code
	// to make sure the test doesn't fail due to a race condition
	client.disable_2fa(&recovery_key).await.unwrap();
}

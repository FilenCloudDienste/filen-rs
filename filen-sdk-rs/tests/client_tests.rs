use filen_sdk_rs::auth::Client;

mod test_utils;

// all tests must be multi_threaded, otherwise drop will deadlock for TestResources
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_login() {
	test_utils::RESOURCES.client().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
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
		*client
	)
}

use filen_sdk_rs::auth::Client;
use filen_sdk_rs_macros::shared_test_runtime;

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
		*client
	)
}

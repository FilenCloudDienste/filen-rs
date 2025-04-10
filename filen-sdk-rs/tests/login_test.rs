use std::env;

use filen_sdk_rs::prelude::*;

#[tokio::test]
async fn test_login() {
	dotenv::dotenv().ok();
	login(
		env::var("TEST_EMAIL").unwrap(),
		&env::var("TEST_PASSWORD").unwrap(),
		&env::var("TEST_2FA_CODE").unwrap_or("XXXXXX".to_string()),
	)
	.await
	.unwrap();
}

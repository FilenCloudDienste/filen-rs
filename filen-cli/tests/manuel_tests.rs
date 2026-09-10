#[cfg(target_os = "linux")]
#[filen_macros::shared_test_runtime]
async fn run_manuel_tests() {
	// export auth config to tmp file
	use std::io::Write;
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let auth_config =
		filen_cli::serialize_auth_config(client).expect("Failed to serialize auth config");
	let mut temp_file = tempfile::NamedTempFile::new().expect("Failed to create temp file");
	let auth_config_path = temp_file
		.path()
		.to_str()
		.expect("Failed to convert temp file path to str")
		.to_string();
	write!(temp_file, "{}", auth_config).expect("Failed to write auth config to temp file");
	dotenv::dotenv().ok(); // loads TEST_EMAIL and TEST_PASSWORD from .env file, just like test_utils does
	unsafe {
		std::env::set_var("OVERRIDE_TEST_AUTH_CONFIG_PATH", auth_config_path);
	}

	let write_diffs_to_file = {
		let term_program = std::env::var("TERM_PROGRAM").unwrap_or_default();
		term_program == "vscode" || term_program == "tmux" // non-exhaustive
	};

	manuel::run_manuel_tests_in_dir(
		"tests/manuel_recordings",
		true,
		std::time::Duration::from_secs(300),
		!write_diffs_to_file,
	);
}

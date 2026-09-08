use std::time::Duration;

#[test]
fn run_manuel_tests() {
	manuel::run_manuel_tests_in_dir("tests/manuel_recordings", true, Duration::from_secs(300));
}

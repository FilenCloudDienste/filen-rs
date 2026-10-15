//! CLI processes started at once on one config directory share its log files. These need no
//! account: each process only prints the in-app help, which it does after setting up logging.

use std::{
	fs::{self, File, FileTimes},
	path::Path,
	process::{Command, Stdio},
	time::{Duration, SystemTime},
};

use assert_fs::TempDir;
use chrono::Local;

const PROCESSES: usize = 8;

/// Starts `PROCESSES` CLIs on `config_dir` at once and asserts that every one exits successfully.
fn run_at_once(config_dir: &Path) {
	let children = (0..PROCESSES)
		.map(|_| {
			Command::new(assert_cmd::cargo::cargo_bin!())
				.arg("--config-dir")
				.arg(config_dir)
				.args(["--skip-update", "--help"])
				.stdout(Stdio::null())
				.stderr(Stdio::piped())
				.spawn()
				.unwrap()
		})
		.collect::<Vec<_>>();
	for child in children {
		let output = child.wait_with_output().unwrap();
		assert!(
			output.status.success(),
			"{}: {}",
			output.status,
			String::from_utf8_lossy(&output.stderr)
		);
	}
}

fn sizes_of_files_starting_with(dir: &Path, prefix: &str) -> Vec<u64> {
	fs::read_dir(dir)
		.unwrap()
		.map(|entry| entry.unwrap())
		.filter(|entry| entry.file_name().to_string_lossy().starts_with(prefix))
		.map(|entry| entry.metadata().unwrap().len())
		.collect()
}

#[test]
fn processes_deleting_the_same_expired_log_files_at_once_all_succeed() {
	let config_dir = TempDir::new().unwrap();
	let logs = config_dir.path().join("logs");
	fs::create_dir(&logs).unwrap();
	let ten_days_ago = SystemTime::now() - Duration::from_secs(10 * 24 * 60 * 60);
	for day in 0..300 {
		File::create(logs.join(format!("2020-01-01-{day}.log")))
			.unwrap()
			.set_times(FileTimes::new().set_modified(ten_days_ago))
			.unwrap();
	}

	run_at_once(config_dir.path());

	assert_eq!(
		sizes_of_files_starting_with(&logs, "2020-01-01-"),
		Vec::<u64>::new()
	);
}

#[test]
fn processes_moving_the_same_full_log_files_aside_at_once_all_succeed_and_keep_them() {
	let config_dir = TempDir::new().unwrap();
	let logs = config_dir.path().join("logs");
	fs::create_dir(&logs).unwrap();
	// Past the CLI's 10 MB cap, so the first line each process writes moves the file aside.
	let full = 11 * 1024 * 1024;
	let today = Local::now().format("%Y-%m-%d.log").to_string();
	for name in ["latest.log", &today] {
		File::create(logs.join(name))
			.unwrap()
			.set_len(full)
			.unwrap();
	}

	run_at_once(config_dir.path());

	for prefix in ["latest.log", &today] {
		let sizes = sizes_of_files_starting_with(&logs, prefix);
		assert_eq!(
			sizes.iter().filter(|&&size| size == full).count(),
			1,
			"{prefix}: {sizes:?}"
		);
	}
}

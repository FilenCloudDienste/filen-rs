#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::{
	ffi::{OsStr, OsString},
	path::{Path, PathBuf},
};

use filen_types::auth::{AuthVersion, FilenSDKConfig};
use serde_json::{Value, json};

use super::{
	Htpasswd, RcloneDaemonConfig, RemoteBackend, StderrTail, check_version, environment_overrides,
	htpasswd_line, rcd_args,
};
#[cfg(unix)]
use super::{RcloneDaemon, SPAWN_ATTEMPTS};
use crate::{
	ErrorKind,
	rclone::{RcloneBinary, RcloneRefusal, VfsTuning},
};

fn sdk_config() -> FilenSDKConfig {
	FilenSDKConfig {
		email: "user@example.com".to_owned(),
		password: String::new(),
		two_factor_code: String::new(),
		master_keys: vec!["master-key-one".to_owned(), "master-key-two".to_owned()],
		api_key: "api-key-secret".to_owned(),
		public_key: "public-key".to_owned(),
		private_key: "private-key-secret".to_owned(),
		auth_version: AuthVersion::V2,
		base_folder_uuid: "00000000-0000-0000-0000-000000000001".to_owned(),
		user_id: 1,
		metadata_cache: false,
		tmp_path: String::new(),
		connect_to_socket: false,
	}
}

fn daemon_config(log_file: Option<PathBuf>) -> RcloneDaemonConfig {
	RcloneDaemonConfig {
		binary: RcloneBinary::Bundled {
			path: PathBuf::from("/opt/filen/rclone"),
			sha256: None,
		},
		state_dir: PathBuf::from("/home/user/.filen/mounts/state"),
		cache_dir: PathBuf::from("/home/user/.filen/mounts/cache"),
		log_file,
		user_agent: "filen-desktop/4.0.0".to_owned(),
		vfs: VfsTuning::default(),
	}
}

#[test]
fn daemon_args_carry_no_secret() {
	let config = daemon_config(Some(PathBuf::from("/home/user/.filen/logs/rclone.log")));
	let password = "a3f1c0ffee";
	let htpasswd = Path::new("/home/user/.filen/mounts/state/rc.htpasswd");

	let args = rcd_args(&config, "127.0.0.1:5572", htpasswd);

	let RemoteBackend { parameters, .. } = RemoteBackend::filen(&sdk_config());
	let mut secrets = parameters
		.values()
		.filter_map(Value::as_str)
		.filter(|value| !matches!(*value, "INTERNAL" | "2" | "24"))
		.map(str::to_owned)
		.collect::<Vec<_>>();
	secrets.push(password.to_owned());
	secrets.push(htpasswd_line("filen", password));
	for arg in &args {
		let arg = arg.to_string_lossy();
		for secret in &secrets {
			assert!(!arg.contains(secret.as_str()), "{arg} holds {secret}");
		}
	}
	assert_eq!(
		args,
		[
			"rcd",
			"--config",
			"",
			"--rc-addr",
			"127.0.0.1:5572",
			"--rc-htpasswd",
			"/home/user/.filen/mounts/state/rc.htpasswd",
			"--cache-dir",
			"/home/user/.filen/mounts/cache",
			"--user-agent",
			"filen-desktop/4.0.0",
			"--log-file",
			"/home/user/.filen/logs/rclone.log",
			"--log-file-max-size",
			"10M",
			"--log-file-max-backups",
			"3",
			"--transfers",
			"8",
			"--multi-thread-streams",
			"4",
			"--multi-thread-cutoff",
			"64Mi",
			"--buffer-size",
			"32Mi",
			"--low-level-retries",
			"10",
			"--timeout",
			"5m",
			"--contimeout",
			"1m",
			"--disable-http2",
			"--no-gzip-encoding",
			"--use-mmap",
			"--log-level",
			"INFO",
			#[cfg(windows)]
			"--no-console",
		]
		.map(OsStr::new)
	);
}

#[test]
fn without_a_log_file_rclone_gets_no_log_file_args() {
	let config = daemon_config(None);

	let args = rcd_args(&config, "127.0.0.1:5572", Path::new("/state/rc.htpasswd"));

	assert!(
		!args
			.iter()
			.any(|arg| arg.to_string_lossy().starts_with("--log-file")),
		"{args:?}"
	);
}

#[test]
fn rc_htpasswd_holds_only_a_hash() {
	let dir = tempfile::tempdir().unwrap();

	let htpasswd = Htpasswd::write(dir.path(), "password").unwrap();

	let content = std::fs::read_to_string(&htpasswd.path).unwrap();
	// The SHA-1 of "password", base64-encoded.
	assert_eq!(content, "filen:{SHA}W6ph5Mm5Pz8GgiULbPgzG37mj9g=\n");
	#[cfg(unix)]
	assert_eq!(
		std::fs::metadata(&htpasswd.path)
			.unwrap()
			.permissions()
			.mode() & 0o777,
		0o600
	);
}

#[test]
fn rc_htpasswd_is_removed_when_dropped() {
	let dir = tempfile::tempdir().unwrap();
	let htpasswd = Htpasswd::write(dir.path(), "password").unwrap();
	let path = dir.path().join("rc.htpasswd");
	assert!(path.exists());

	drop(htpasswd);

	assert!(!path.exists());
}

#[test]
fn config_create_body_from_sdk_config() {
	let RemoteBackend { kind, parameters } = RemoteBackend::filen(&sdk_config());

	assert_eq!(kind, "filen");
	assert_eq!(
		Value::Object(parameters),
		json!({
			"email": "user@example.com",
			"password": "INTERNAL",
			"api_key": "api-key-secret",
			"master_keys": "master-key-one|master-key-two",
			"private_key": "private-key-secret",
			"public_key": "public-key",
			"auth_version": "2",
			"base_folder_uuid": "00000000-0000-0000-0000-000000000001",
			"upload_concurrency": "24",
		})
	);
}

#[test]
fn version_below_floor_is_refused() {
	let error = check_version("v1.74.2".to_owned()).unwrap_err();

	assert_eq!(error.kind(), ErrorKind::IO);
	assert_eq!(
		error.downcast_ref::<RcloneRefusal>(),
		Some(&RcloneRefusal::UnsupportedVersion {
			found: "v1.74.2".to_owned()
		})
	);
}

#[test]
fn an_unreadable_version_is_refused() {
	let error = check_version("unknown".to_owned()).unwrap_err();

	assert_eq!(
		error.downcast_ref::<RcloneRefusal>(),
		Some(&RcloneRefusal::UnsupportedVersion {
			found: "unknown".to_owned()
		})
	);
}

#[test]
fn the_version_floor_itself_is_accepted() {
	check_version("v1.74.3".to_owned()).unwrap();
	check_version("v1.75.2".to_owned()).unwrap();
}

#[test]
fn rclone_variables_are_removed_from_its_environment() {
	let vars = [
		("RCLONE_RC_NO_AUTH", "true"),
		("rclone_config_filen_type", "local"),
		("HOME", "/home/user"),
		("PATH", "/usr/local/bin:/usr/bin"),
	]
	.map(|(key, value)| (OsString::from(key), OsString::from(value)));

	let overrides = environment_overrides(vars.into_iter());

	let removed = overrides
		.iter()
		.filter(|(_, value)| value.is_none())
		.map(|(key, _)| key.as_os_str())
		.collect::<Vec<_>>();
	assert_eq!(removed, ["RCLONE_RC_NO_AUTH", "rclone_config_filen_type"]);
	assert!(
		!overrides.iter().any(|(key, _)| key == "HOME"),
		"{overrides:?}"
	);
}

#[cfg(unix)]
#[test]
fn the_system_directories_are_added_to_path() {
	let vars = [("PATH", "/usr/local/bin:/usr/bin")]
		.map(|(key, value)| (OsString::from(key), OsString::from(value)));

	let overrides = environment_overrides(vars.into_iter());

	assert_eq!(
		overrides,
		[(
			OsString::from("PATH"),
			Some(OsString::from(
				"/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
			))
		)]
	);
}

#[cfg(unix)]
#[test]
fn without_a_path_rclone_gets_the_system_directories() {
	let overrides = environment_overrides(std::iter::empty());

	assert_eq!(
		overrides,
		[(
			OsString::from("PATH"),
			Some(OsString::from("/usr/bin:/bin:/usr/sbin:/sbin"))
		)]
	);
}

#[test]
fn the_stderr_tail_keeps_the_last_lines_cut_short() {
	let mut tail = StderrTail::default();

	for n in 0..25 {
		tail.push(&format!("line {n}"));
	}
	tail.push(&"é".repeat(300));

	let text = tail.text();
	let lines = text.lines().collect::<Vec<_>>();
	assert_eq!(lines.len(), 20);
	assert_eq!(lines.first(), Some(&"line 6"));
	assert_eq!(lines.last().map(|line| line.len()), Some(512));
}

#[cfg(unix)]
#[tokio::test]
async fn an_rclone_that_exits_before_its_rc_answers_is_started_again() {
	let dir = tempfile::tempdir().unwrap();
	let starts = dir.path().join("starts");
	let binary = dir.path().join("rclone");
	std::fs::write(
		&binary,
		format!(
			"#!/bin/sh\necho started >> '{}'\nexit 1\n",
			starts.display()
		),
	)
	.unwrap();
	std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
	let config = RcloneDaemonConfig {
		state_dir: dir.path().to_path_buf(),
		..daemon_config(None)
	};

	let error = RcloneDaemon::spawn_until_ready(&binary, &config, "password")
		.await
		.err()
		.unwrap();

	assert_eq!(
		error.to_string(),
		"Error of kind IO: context: rclone rcd exited before its rc answered: Crashed { code: \
		 Some(1), stderr_tail: \"\" }"
	);
	assert_eq!(
		std::fs::read_to_string(&starts).unwrap().lines().count(),
		usize::try_from(SPAWN_ATTEMPTS).unwrap()
	);
}

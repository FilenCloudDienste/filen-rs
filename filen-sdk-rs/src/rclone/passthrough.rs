//! rclone run in the foreground with the account as its `filen` remote, for filen-cli's `rclone`
//! command.
//!
//! The remote reaches rclone through `RCLONE_CONFIG_FILEN_*` variables, its password options
//! obscured as a config file holds them, and nothing is written to disk: rclone runs with an
//! in-memory config (`--config ""`). The environment is the one place a remote can be given to a
//! command rclone runs once, and the process it is set on lives only as long as the command.

use std::{
	path::Path,
	process::{ExitStatus, Stdio},
};

use filen_types::auth::FilenSDKConfig;
use tokio::{io::AsyncWriteExt as _, process::Command};

use crate::{
	Error, ErrorKind,
	auth::Client,
	error::ResultExt as _,
	rclone::{RcloneBinary, system::spawn_rclone},
};

/// The remote's name, which filen-cli documents.
const REMOTE: &str = "FILEN";

/// Runs `rclone <args>` with `client`'s account as the `filen` remote, inheriting the terminal, and
/// waits for it to exit.
pub async fn run_rclone(
	client: &Client,
	binary: &RcloneBinary,
	args: &[String],
) -> Result<ExitStatus, Error> {
	let binary = binary.resolve().await?;
	let remote = remote_env(&binary, &client.to_sdk_config()).await?;
	spawn_rclone(&mut passthrough_command(&binary, args, remote))
		.await
		.context("failed to start rclone")?
		.wait()
		.await
		.context("failed to wait for rclone")
}

/// The command running `rclone <args>` with the remote's variables `remote`.
fn passthrough_command(binary: &Path, args: &[String], remote: Vec<(String, String)>) -> Command {
	let mut command = Command::new(binary);
	command
		.args(["--config", ""])
		.args(args)
		.envs(remote)
		.kill_on_drop(true);
	command
}

/// The `RCLONE_CONFIG_FILEN_*` variables of the account `config` describes, its password options
/// obscured by `binary`.
async fn remote_env(
	binary: &Path,
	config: &FilenSDKConfig,
) -> Result<Vec<(String, String)>, Error> {
	// The backend logs in with the keys, never with a password, but requires the option to be set.
	// This is the value it documents for that.
	// https://github.com/FilenCloudDienste/filen-rclone/blob/784979078ecd573af0d9809d2c5a07bafaad2c41/backend/filen/filen.go#L136
	let password = obscure(binary, "INTERNAL").await?;
	let api_key = obscure(binary, &config.api_key).await?;
	// Owned copies: the variables are built once per command.
	Ok([
		("TYPE", "filen".to_owned()),
		("EMAIL", config.email.clone()),
		("PASSWORD", password),
		("API_KEY", api_key),
		("MASTER_KEYS", config.master_keys.join("|")),
		("PUBLIC_KEY", config.public_key.clone()),
		("PRIVATE_KEY", config.private_key.clone()),
		("AUTH_VERSION", (config.auth_version as u8).to_string()),
		("BASE_FOLDER_UUID", config.base_folder_uuid.clone()),
	]
	.into_iter()
	.map(|(option, value)| (format!("RCLONE_CONFIG_{REMOTE}_{option}"), value))
	.collect())
}

/// `secret` obscured as rclone's config holds a password option, by `rclone obscure`, which reads
/// it from stdin: its argv is readable by every user.
async fn obscure(binary: &Path, secret: &str) -> Result<String, Error> {
	let mut child = spawn_rclone(
		Command::new(binary)
			.args(["obscure", "-"])
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::null())
			.kill_on_drop(true),
	)
	.await
	.context("failed to start rclone obscure")?;
	let mut stdin = child.stdin.take().ok_or_else(|| {
		Error::custom(ErrorKind::Internal, "rclone obscure's stdin was not piped")
	})?;
	stdin
		.write_all(secret.as_bytes())
		.await
		.context("failed to write to rclone obscure")?;
	drop(stdin);
	let output = child
		.wait_with_output()
		.await
		.context("failed to wait for rclone obscure")?;
	if !output.status.success() {
		return Err(Error::custom(
			ErrorKind::IO,
			format!("rclone obscure exited with {}", output.status),
		));
	}
	let obscured = String::from_utf8(output.stdout).map_err(|e| {
		Error::custom_with_source(
			ErrorKind::Conversion,
			e,
			Some("rclone obscure's output is not UTF-8"),
		)
	})?;
	Ok(obscured.trim().to_owned())
}

#[cfg(test)]
mod tests {
	use std::ffi::OsStr;

	use super::*;

	#[test]
	fn rclone_passthrough_writes_no_config() {
		let remote = vec![("RCLONE_CONFIG_FILEN_TYPE".to_owned(), "filen".to_owned())];
		let args = ["lsf".to_owned(), "filen:".to_owned()];

		let command = passthrough_command(Path::new("/opt/rclone"), &args, remote);

		let std = command.as_std();
		assert_eq!(
			std.get_args().collect::<Vec<_>>(),
			["--config", "", "lsf", "filen:"].map(OsStr::new)
		);
		assert_eq!(
			std.get_envs().collect::<Vec<_>>(),
			[(
				OsStr::new("RCLONE_CONFIG_FILEN_TYPE"),
				Some(OsStr::new("filen"))
			)]
		);
	}
}

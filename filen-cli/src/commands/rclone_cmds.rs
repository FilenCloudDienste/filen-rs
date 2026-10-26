//! [cli-doc] managed-rclone
//! The Filen CLI includes a managed installation of Rclone, which can be used to [access Filen](https://rclone.org/filen).
//! It is automatically downloaded and configured (authenticated) when you run the commands like `rclone`, `mount`, etc.
//! Your credentials are only passed to it in memory, never written to disk. `serve` listens on 127.0.0.1 unless told otherwise,
//! and prints random credentials when you do not give any.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow};
use filen_sdk_rs::{
	auth::Client,
	rclone::{
		MountOptions, RcloneBinary, RcloneDaemon, RcloneDaemonConfig, RcloneExit, ServeAuth,
		ServeOptions, ServeProtocol, ServeTls, StopOptions, VfsTuning, available_drive_letters,
		run_rclone,
	},
};
use tokio::select;

use crate::{CliConfig, auth::LazyClient, ui::UI};

/// How long uploads waiting in the cache may finish when the CLI stops a drive or server.
const STOP: StopOptions = StopOptions {
	drain_timeout_ms: Some(30_000),
	grace_ms: 10_000,
};

/// What `serve` serves.
pub(crate) struct ServeArgs {
	pub(crate) protocol: ServeProtocol,
	/// `host:port`; 127.0.0.1 and the protocol's default port without it.
	pub(crate) address: Option<String>,
	pub(crate) user: Option<String>,
	pub(crate) password: Option<String>,
	pub(crate) read_only: bool,
	pub(crate) cache_size: Option<u64>,
	pub(crate) cert: Option<String>,
	pub(crate) key: Option<String>,
}

pub(crate) async fn mount(
	config: &CliConfig,
	ui: &mut UI,
	client: &mut LazyClient,
	mount_point: Option<String>,
	cache_size: Option<u64>,
) -> Result<()> {
	let client = client.get(ui).await?;
	let mount_point = match mount_point {
		Some(mount_point) => mount_point,
		None => default_mount_point().await?,
	};
	let daemon = start_daemon(config, ui, client, cache_size).await?;
	let mounted = daemon
		.mount(MountOptions {
			mount_point,
			read_only: false,
		})
		.await
		.context("Failed to mount network drive (use --verbose for more info)")?;
	ui.print_success(&format!(
		"Mounted network drive at {} (press Ctrl-C to unmount and exit)",
		mounted.mount_point
	));
	wait_and_stop(ui, &daemon, "Unmounting network drive").await
}

pub(crate) async fn start_server(
	config: &CliConfig,
	ui: &mut UI,
	client: &mut LazyClient,
	args: ServeArgs,
) -> Result<()> {
	let name = display_name(args.protocol);
	let (host, port) = match &args.address {
		Some(address) => parse_address(address)?,
		None => ("127.0.0.1".to_owned(), default_port(args.protocol)),
	};
	let auth = serve_auth(args.protocol, args.user, args.password)?;
	let tls = match (args.cert, args.key) {
		(Some(cert_path), Some(key_path)) => Some(ServeTls {
			cert_path,
			key_path,
		}),
		(None, None) => None,
		_ => return Err(UI::failure("Give both --cert and --key, or neither")),
	};
	let scheme = if tls.is_some() { "https" } else { "http" };
	let client = client.get(ui).await?;
	let daemon = start_daemon(config, ui, client, args.cache_size).await?;
	let serve = daemon
		.start_serve(ServeOptions {
			protocol: args.protocol,
			host,
			port,
			// One copy for the server, one to print.
			auth: auth.clone(),
			tls,
			read_only: args.read_only,
		})
		.await
		.with_context(|| format!("Failed to start {name} server"))?;
	let address = match args.protocol {
		ServeProtocol::Webdav | ServeProtocol::S3 | ServeProtocol::Http | ServeProtocol::Restic => {
			format!("{scheme}://{}", serve.addr)
		}
		ServeProtocol::Ftp | ServeProtocol::Sftp | ServeProtocol::Nfs => serve.addr,
	};
	let credentials = match auth {
		ServeAuth::Basic { user, password } => {
			format!("with username \"{user}\" and password \"{password}\"")
		}
		ServeAuth::S3 {
			access_key_id,
			secret_access_key,
		} => format!(
			"with Access Key ID \"{access_key_id}\" and Secret Access Key \"{secret_access_key}\""
		),
		ServeAuth::Unauthenticated => "without authentication".to_owned(),
	};
	ui.print_success(&format!(
		"Started {name} server on {address} {credentials} (press Ctrl-C to stop)"
	));
	wait_and_stop(ui, &daemon, &format!("Stopping {name} server")).await
}

pub(crate) async fn execute_rclone(
	config: &CliConfig,
	ui: &mut UI,
	client: &mut LazyClient,
	cmd: Vec<String>,
) -> Result<()> {
	let binary = prepare_binary(config, ui).await;
	let client = client.get(ui).await?;
	let exit_code = run_rclone(client, &binary, &cmd)
		.await
		.context("Failed to run Rclone")?
		.code();
	if let Some(exit_code) = exit_code
		&& exit_code != 0
	{
		return Err(crate::construct_exit_code_error(exit_code));
	}
	Ok(())
}

fn rclone_dir(config: &CliConfig) -> PathBuf {
	config.config_dir.join("rclone")
}

/// The managed rclone, once the `rclone.conf` with the account's keys that earlier versions wrote
/// is removed; says so when it is about to be downloaded.
async fn prepare_binary(config: &CliConfig, ui: &mut UI) -> RcloneBinary {
	let dir = rclone_dir(config);
	remove_old_config(&dir).await;
	let binary = RcloneBinary::Managed { dir };
	if !binary.is_installed().await {
		ui.print_muted("Downloading managed Rclone...");
	}
	binary
}

async fn remove_old_config(dir: &Path) {
	let path = dir.join("rclone.conf");
	if let Err(e) = tokio::fs::remove_file(&path).await
		&& e.kind() != std::io::ErrorKind::NotFound
	{
		log::warn!("Failed to remove {}: {e}", path.display());
	}
}

async fn start_daemon(
	config: &CliConfig,
	ui: &mut UI,
	client: &Client,
	cache_size: Option<u64>,
) -> Result<RcloneDaemon> {
	let binary = prepare_binary(config, ui).await;
	let dir = rclone_dir(config);
	let defaults = VfsTuning::default();
	RcloneDaemon::start(
		client,
		RcloneDaemonConfig {
			binary,
			state_dir: dir.join("daemon"),
			cache_dir: dir.join("cache"),
			log_file: None,
			user_agent: format!("filen-cli/{}", env!("CARGO_PKG_VERSION")),
			vfs: VfsTuning {
				cache_max_size: cache_size.unwrap_or(defaults.cache_max_size),
				..defaults
			},
		},
	)
	.await
	.context("Failed to start Rclone (use --verbose for more info)")
}

/// Waits for Ctrl-C, then stops the daemon, letting uploads finish for a while; fails if rclone
/// exits first.
async fn wait_and_stop(ui: &mut UI, daemon: &RcloneDaemon, stopping: &str) -> Result<()> {
	let mut stop_rx = crate::CTRLC_TX.subscribe();
	select! {
		_ = stop_rx.recv() => {}
		exit = daemon.exited() => {
			return Err(anyhow!(match exit {
				RcloneExit::Crashed { code: Some(code), .. } => {
					format!("Rclone exited with code {code} (use --verbose for more info)")
				}
				RcloneExit::Crashed { code: None, .. } | RcloneExit::Stopped => {
					"Rclone exited unexpectedly (use --verbose for more info)".to_owned()
				}
			}));
		}
	}
	ui.print_muted(&format!(
		"{stopping}, letting uploads finish for up to 30 seconds..."
	));
	let report = daemon.stop(STOP).await.context("Failed to stop Rclone")?;
	if report.pending_uploads > 0 {
		ui.print_warning(&format!(
			"{} uploads did not finish; they continue the next time you run `mount` or `serve`",
			report.pending_uploads
		));
	}
	Ok(())
}

/// `/tmp/filen` (created if missing), or on Windows the drive letter X, or the last free one.
async fn default_mount_point() -> Result<String> {
	if cfg!(windows) {
		let free = available_drive_letters().await;
		return free
			.iter()
			.find(|letter| *letter == "X")
			.or(free.last())
			.cloned()
			.ok_or_else(|| UI::failure("No drive letter is free to mount the network drive on"));
	}
	let mount_point = "/tmp/filen";
	tokio::fs::create_dir_all(mount_point)
		.await
		.with_context(|| format!("Failed to create {mount_point}"))?;
	Ok(mount_point.to_owned())
}

/// The protocol `name` names, as `serve` takes it.
pub(crate) fn parse_protocol(name: &str) -> Result<ServeProtocol> {
	Ok(match name {
		"webdav" => ServeProtocol::Webdav,
		"s3" => ServeProtocol::S3,
		"ftp" => ServeProtocol::Ftp,
		"sftp" => ServeProtocol::Sftp,
		"http" => ServeProtocol::Http,
		"restic" => ServeProtocol::Restic,
		"nfs" => ServeProtocol::Nfs,
		_ => {
			return Err(UI::failure(&format!(
				"Unsupported server type: {name}. Supported types are: webdav, s3, ftp, sftp, http, restic, nfs"
			)));
		}
	})
}

fn display_name(protocol: ServeProtocol) -> &'static str {
	match protocol {
		ServeProtocol::Webdav => "WebDAV",
		ServeProtocol::S3 => "S3",
		ServeProtocol::Ftp => "FTP",
		ServeProtocol::Sftp => "SFTP",
		ServeProtocol::Http => "HTTP",
		ServeProtocol::Restic => "restic",
		ServeProtocol::Nfs => "NFS",
	}
}

/// The port a server listens on when `--addr` is not given, the desktop app's.
fn default_port(protocol: ServeProtocol) -> u16 {
	match protocol {
		ServeProtocol::Webdav => 1900,
		ServeProtocol::S3 => 1800,
		ServeProtocol::Ftp => 2121,
		ServeProtocol::Sftp => 2022,
		ServeProtocol::Http => 1700,
		ServeProtocol::Restic => 1600,
		ServeProtocol::Nfs => 2049,
	}
}

/// The host and port of `address`, `<host>:<port>` with an IPv6 host in brackets. The daemon
/// refuses a host that is not an IP address or `localhost`, such as the empty one of `:<port>`.
fn parse_address(address: &str) -> Result<(String, u16)> {
	let invalid = || UI::failure(&format!("Invalid address {address}: expected <ip>:<port>"));
	let (host, port) = address.rsplit_once(':').ok_or_else(invalid)?;
	let port = port.parse().map_err(|_| invalid())?;
	let host = host
		.strip_prefix('[')
		.and_then(|host| host.strip_suffix(']'))
		.unwrap_or(host);
	Ok((host.to_owned(), port))
}

/// The credentials a server takes: the given ones, or random ones when neither is given.
fn serve_auth(
	protocol: ServeProtocol,
	user: Option<String>,
	password: Option<String>,
) -> Result<ServeAuth> {
	match (protocol, user, password) {
		(_, None, None) => Ok(ServeAuth::random(protocol)),
		(ServeProtocol::Nfs, _, _) => Err(UI::failure("NFS servers take no credentials")),
		(ServeProtocol::S3, Some(access_key_id), Some(secret_access_key)) => Ok(ServeAuth::S3 {
			access_key_id,
			secret_access_key,
		}),
		(_, Some(user), Some(password)) => Ok(ServeAuth::Basic { user, password }),
		_ => Err(UI::failure("Give both --user and --password, or neither")),
	}
}

/// A size such as `500Mi` or `10Gi` (`10G` is the same, binary), or bytes without a suffix.
pub(crate) fn parse_size(size: &str) -> Result<u64, String> {
	let not_a_size = || format!("{size} is not a size such as 500Mi or 10Gi");
	let digits = size
		.find(|c: char| !c.is_ascii_digit())
		.unwrap_or(size.len());
	let (number, suffix) = size.split_at(digits);
	let number: u64 = number.parse().map_err(|_| not_a_size())?;
	let shift = match suffix.trim_end_matches('B').trim_end_matches('i') {
		"" => 0,
		"K" | "k" => 10,
		"M" => 20,
		"G" => 30,
		"T" => 40,
		_ => return Err(not_a_size()),
	};
	number
		.checked_mul(1 << shift)
		.ok_or_else(|| format!("{size} is too large"))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn sizes_parse_with_binary_suffixes() {
		assert_eq!(parse_size("500Mi"), Ok(500 << 20));
		assert_eq!(parse_size("10Gi"), Ok(10 << 30));
		assert_eq!(parse_size("10G"), Ok(10 << 30));
		assert_eq!(parse_size("1024"), Ok(1024));
		assert_eq!(parse_size("2KiB"), Ok(2048));
		assert!(parse_size("ten").is_err());
		assert!(parse_size("10X").is_err());
		assert!(parse_size("20000000T").is_err());
	}

	#[test]
	fn addresses_parse_into_host_and_port() {
		assert_eq!(
			parse_address("127.0.0.1:8080").unwrap(),
			("127.0.0.1".to_owned(), 8080)
		);
		assert_eq!(parse_address("[::1]:80").unwrap(), ("::1".to_owned(), 80));
		assert_eq!(parse_address(":80").unwrap(), (String::new(), 80));
		assert!(parse_address("127.0.0.1").is_err());
		assert!(parse_address("127.0.0.1:port").is_err());
	}

	#[test]
	fn serve_without_credentials_generates_them() {
		assert!(matches!(
			serve_auth(ServeProtocol::Webdav, None, None).unwrap(),
			ServeAuth::Basic { .. }
		));
		assert!(matches!(
			serve_auth(ServeProtocol::S3, None, None).unwrap(),
			ServeAuth::S3 { .. }
		));
		assert_eq!(
			serve_auth(ServeProtocol::Nfs, None, None).unwrap(),
			ServeAuth::Unauthenticated
		);
		assert_eq!(
			serve_auth(
				ServeProtocol::Ftp,
				Some("user".to_owned()),
				Some("pass".to_owned())
			)
			.unwrap(),
			ServeAuth::Basic {
				user: "user".to_owned(),
				password: "pass".to_owned()
			}
		);
		assert!(serve_auth(ServeProtocol::Ftp, Some("user".to_owned()), None).is_err());
		assert!(
			serve_auth(
				ServeProtocol::Nfs,
				Some("user".to_owned()),
				Some("pass".to_owned())
			)
			.is_err()
		);
	}
}

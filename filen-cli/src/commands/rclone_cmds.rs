//! [cli-doc] managed-rclone
//! The Filen CLI includes a managed installation of Rclone, which can be used to [access Filen](https://rclone.org/filen).
//! It is automatically downloaded and configured (authenticated) when you run the commands like `rclone`, `mount`, etc.
//! Your credentials are only passed to it in memory, never written to disk. `serve` listens on 127.0.0.1 unless told otherwise,
//! and prints random credentials when you do not give any.
//!
//! [cli-doc] rclone-passthrough
//! `filen rclone` runs any Rclone command, with your drive as the `filen` remote, so it covers what
//! `mount` and `serve` have no option for: more parallel transfers (`--transfers`), serving or
//! mounting a directory instead of the whole drive, or any other Rclone flag. For example:
//!
//! ```sh
//! filen rclone copy ./photos filen:Photos --transfers 16
//! filen rclone serve webdav filen:Photos --addr 127.0.0.1:8080 --user me --pass secret
//! ```
//!
//! A mount or server run this way lacks two things `mount` and `serve` do: when it stops, nothing
//! gives the uploads still in its cache up to 30 seconds to finish, and nothing checks that the
//! server refuses requests without credentials, so give it `--user` and `--pass` (or the protocol's
//! equivalent) yourself.
//!
//! [cli-doc] mount-prerequisites
//! `mount` mounts the drive on `/tmp/filen` (created if it does not exist), or on Windows on the
//! drive letter X, or the last free one when X is taken. A mount point you give must be an empty
//! directory. Mounting needs:
//! - on Linux, FUSE 3 from your distribution (e.g. `apt install fuse3` or `dnf install fuse3`):
//!   `fusermount3` on the `PATH` (or in /usr/bin or /bin) and /dev/fuse;
//! - on Windows, [WinFsp](https://winfsp.dev/rel/);
//! - on macOS, nothing: the drive is mounted through Rclone's NFS mount and listed as
//!   `localhost:/Filen`. When [macFUSE](https://macfuse.github.io) is installed, it is used
//!   instead.
//!
//! [cli-doc] serve-s3-clients
//! S3 clients connect to `http://127.0.0.1:1800` by default (`https://` with `--cert`), with the
//! Access Key ID and Secret Access Key `serve` prints (or the `--user` and `--password` given), and
//! must use path-style addressing: the bucket in the path, not in the host name. Any region is
//! accepted, e.g. `us-east-1`. Each directory in the root of your drive is a bucket; files directly
//! in the root are not listed.
//!
//! [cli-doc] serve-https
//! `serve --cert <file> --key <file>` serves WebDAV, S3, HTTP and restic over HTTPS, with a
//! certificate and private key in PEM format. To make a self-signed certificate for this computer
//! with OpenSSL, valid for a year:
//!
//! ```sh
//! openssl req -x509 -newkey rsa:4096 -nodes -days 365 -subj "/CN=localhost" -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" -keyout key.pem -out cert.pem
//! filen serve webdav --cert cert.pem --key key.pem
//! ```
//!
//! Clients reject a self-signed certificate until you tell them to trust it (for example by
//! adding `cert.pem` to the system's trusted certificates).

use std::{
	fmt,
	net::{Ipv4Addr, Ipv6Addr},
	path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, anyhow};
use clap::{Subcommand, ValueEnum};
use filen_sdk_rs::{
	ErrorKind,
	auth::Client,
	rclone::{
		MountOptions, RcloneBinary, RcloneDaemon, RcloneDaemonConfig, RcloneExit, RcloneRefusal,
		ServeAuth, ServeOptions, ServeProtocol, ServeTls, StopOptions, VfsTuning,
		available_drive_letters, run_rclone,
	},
};
use serde_json::json;
use tokio::select;

use crate::{
	CliConfig, CommandResult, auth::LazyClient, commands::CommandContext, docs::DocElement,
	error::CliError, interrupt, ui::UI,
};

#[derive(Debug, Subcommand)]
pub(crate) enum RcloneCommands {
	/// Execute an Rclone command using the managed installation
	Rclone {
		/// The command to execute. Your Filen drive is available as the "filen" remote.
		#[arg(trailing_var_arg = true, allow_hyphen_values = true)]
		cmd: Vec<String>,
	},
	/// Mount Filen as a network drive
	Mount {
		/// Where to mount the network drive: an empty directory (default: /tmp/filen), or on Windows a drive letter (default: X)
		mount_point: Option<String>,
		/// The maximum cache size (e.g. "500Mi", "10Gi") (default: 32Gi)
		#[arg(long, value_parser = parse_size)]
		cache_size: Option<u64>,
	},
	/// Runs a WebDAV, S3, FTP, SFTP, HTTP, restic or NFS server exposing your Filen drive
	Serve {
		/// The type of server to run
		server: ServerType,
		/// IP and port for the server (`<ip>:<port>`; `0.0.0.0:<port>` for every network interface) (default: 127.0.0.1 and a port per type)
		#[arg(long = "addr")]
		address: Option<String>,
		/// Username for authentication to the server (default: random, printed when the server starts).
		/// On S3 servers, this is the Access Key ID. NFS servers have no authentication and listen on 127.0.0.1 only.
		#[arg(long)]
		user: Option<String>,
		/// Password for authentication to the server (default: random, printed when the server starts).
		/// On S3 servers, this is the Secret Access Key.
		#[arg(long)]
		password: Option<String>,
		/// The server is read-only
		#[arg(long)]
		read_only: bool,
		/// The maximum cache size (e.g. "500Mi", "10Gi") (default: 32Gi)
		#[arg(long, value_parser = parse_size)]
		cache_size: Option<u64>,
		/// TLS certificate (PEM) to serve HTTPS with (WebDAV, S3, HTTP and restic; requires --key; `filen help managed-rclone` shows how to make a self-signed one)
		#[arg(long)]
		cert: Option<String>,
		/// TLS private key (PEM) for --cert
		#[arg(long)]
		key: Option<String>,
	},
	/// Exports your user API key (for use with non-managed Rclone)
	ExportApiKey,
	/// Replaced by `serve webdav`
	#[command(hide = true)]
	Webdav {
		/// The options the command took, which are ignored
		#[arg(
			trailing_var_arg = true,
			allow_hyphen_values = true,
			value_name = "ARGS"
		)]
		_args: Vec<String>,
	},
	/// Replaced by `serve s3`
	#[command(hide = true)]
	S3 {
		/// The options the command took, which are ignored
		#[arg(
			trailing_var_arg = true,
			allow_hyphen_values = true,
			value_name = "ARGS"
		)]
		_args: Vec<String>,
	},
	/// No longer available
	#[command(hide = true)]
	WebdavProxy {
		/// The options the command took, which are ignored
		#[arg(
			trailing_var_arg = true,
			allow_hyphen_values = true,
			value_name = "ARGS"
		)]
		_args: Vec<String>,
	},
}

/// The type of server `serve` runs.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum ServerType {
	/// WebDAV (default address 127.0.0.1:1900)
	Webdav,
	/// S3 (default address 127.0.0.1:1800)
	S3,
	/// FTP (default address 127.0.0.1:2121)
	Ftp,
	/// SFTP (default address 127.0.0.1:2022)
	Sftp,
	/// HTTP, viewable in a web browser (default address 127.0.0.1:1700)
	Http,
	/// restic's REST server (default address 127.0.0.1:1600)
	Restic,
	/// NFS, without authentication (default address 127.0.0.1:2049)
	Nfs,
}

impl From<ServerType> for ServeProtocol {
	fn from(server: ServerType) -> Self {
		match server {
			ServerType::Webdav => Self::Webdav,
			ServerType::S3 => Self::S3,
			ServerType::Ftp => Self::Ftp,
			ServerType::Sftp => Self::Sftp,
			ServerType::Http => Self::Http,
			ServerType::Restic => Self::Restic,
			ServerType::Nfs => Self::Nfs,
		}
	}
}

/// The docs' "Managed Rclone" section.
pub(crate) const DOCS: &[DocElement] = &[
	DocElement::DocFragment("managed-rclone"),
	DocElement::CommandHelp("rclone"),
	DocElement::DocFragment("rclone-passthrough"),
	DocElement::CommandHelp("mount"),
	DocElement::DocFragment("mount-prerequisites"),
	DocElement::CommandHelp("serve"),
	DocElement::DocFragment("serve-s3-clients"),
	DocElement::DocFragment("serve-https"),
	DocElement::CommandHelp("export-api-key"),
];

impl RcloneCommands {
	pub(super) async fn execute(self, ctx: CommandContext<'_>) -> Result<CommandResult> {
		let CommandContext {
			config, ui, client, ..
		} = ctx;
		match self {
			Self::Rclone { cmd } => execute_rclone(config, ui, client, cmd).await?,
			Self::Mount {
				mount_point,
				cache_size,
			} => mount(config, ui, client, mount_point, cache_size).await?,
			Self::Serve {
				server,
				address,
				user,
				password,
				read_only,
				cache_size,
				cert,
				key,
			} => {
				start_server(
					config,
					ui,
					client,
					ServeArgs {
						protocol: server.into(),
						address,
						user,
						password,
						read_only,
						cache_size,
						cert,
						key,
					},
				)
				.await?;
			}
			Self::ExportApiKey => {
				let client = client.get(ui).await?.to_stringified();
				if ui.json {
					ui.print_json(json!({
						"email": client.email,
						"apiKey": client.api_key,
					}))?;
				} else {
					ui.print_warning("Keep your API key secret! Do not share it with anyone.");
					ui.print_key_value_table(&[(
						&format!("API Key for {}:", client.email),
						client.api_key.as_str(),
					)]);
				}
			}
			Self::Webdav { .. } => return Err(moved_to_serve(ServerType::Webdav)),
			Self::S3 { .. } => return Err(moved_to_serve(ServerType::S3)),
			Self::WebdavProxy { .. } => return Err(webdav_proxy_removed()),
		}
		Ok(CommandResult::default())
	}
}

/// The usage error of `webdav-proxy`, which nothing replaced.
fn webdav_proxy_removed() -> anyhow::Error {
	UI::usage_failure(
		"`filen webdav-proxy` is no longer available: the multi-user proxy mode was removed. Run `filen serve webdav` for each account instead",
	)
}

/// The usage error of a command that `filen serve <server>` replaced, whose defaults differ from
/// the command's.
fn moved_to_serve(server: ServerType) -> anyhow::Error {
	let name = server
		.to_possible_value()
		.expect("no server type is skipped (should be impossible)");
	let name = name.get_name();
	let port = default_port(server.into());
	UI::usage_failure(&format!(
		"`filen {name}` was replaced by `filen serve {name}`, which listens on 127.0.0.1:{port} by default instead of 0.0.0.0:80; add `--addr 0.0.0.0:{port}` to listen on every network interface"
	))
}

/// How long uploads waiting in the cache may finish when the CLI stops a drive or server.
const STOP: StopOptions = StopOptions {
	drain_timeout_ms: Some(30_000),
	grace_ms: 10_000,
};

/// How many mounts and servers may run at once, each on a cache of its own: two Rclones on one
/// cache would corrupt it.
const CACHES: u32 = 8;

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
	let (daemon, ctrl_c) = start_daemon(config, ui, client, cache_size).await?;
	let mounted = daemon
		.daemon
		.mount(MountOptions {
			mount_point,
			read_only: false,
		})
		.await
		.map_err(|e| {
			rclone_error(
				e,
				"Failed to mount network drive (use --verbose for more info)",
			)
		})?;
	ui.print_success(&format!(
		"Mounted network drive at {} (press Ctrl-C to unmount and exit)",
		mounted.mount_point
	));
	wait_and_stop(ui, &daemon, ctrl_c, "Unmounting network drive").await
}

pub(crate) async fn start_server(
	config: &CliConfig,
	ui: &mut UI,
	client: &mut LazyClient,
	args: ServeArgs,
) -> Result<()> {
	check_serve_args(args.protocol, args.read_only)?;
	let name = display_name(args.protocol);
	let (host, port) = match &args.address {
		Some(address) => parse_address(address)?,
		None => ("127.0.0.1".to_owned(), default_port(args.protocol)),
	};
	let credentials_given = args.user.is_some() || args.password.is_some();
	let auth = serve_auth(args.protocol, args.user, args.password)?;
	let credentials_generated = !credentials_given && auth != ServeAuth::Unauthenticated;
	let tls = match (args.cert, args.key) {
		(Some(cert_path), Some(key_path)) => Some(ServeTls {
			cert_path,
			key_path,
		}),
		(None, None) => None,
		_ => return Err(UI::usage_failure("Give both --cert and --key, or neither")),
	};
	let scheme = if tls.is_some() { "https" } else { "http" };
	let client = client.get(ui).await?;
	let (daemon, ctrl_c) = start_daemon(config, ui, client, args.cache_size).await?;
	let serve = daemon
		.daemon
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
		.map_err(|e| rclone_error(e, format!("Failed to start {name} server")))?;
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
	print_started(
		ui,
		&format!("Started {name} server on {address} {credentials} (press Ctrl-C to stop)"),
		credentials_generated,
	);
	wait_and_stop(ui, &daemon, ctrl_c, &format!("Stopping {name} server")).await
}

/// Prints that the server started, with its credentials. Even `--quiet` prints it when the
/// credentials were generated, as they are printed nowhere else and the server cannot be used
/// without them.
fn print_started(ui: &mut UI, started: &str, credentials_generated: bool) {
	if credentials_generated && ui.is_quiet() {
		ui.print(started);
	} else {
		ui.print_success(started);
	}
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
		return Err(CliError::ChildExit(exit_code).into());
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

/// A running daemon, and which cache it uses.
struct Daemon {
	daemon: RcloneDaemon,
	/// 0 for the cache every mount or server uses when it runs alone, 1 and up for those it uses
	/// while others run.
	cache: u32,
	/// The cache's directory.
	cache_dir: PathBuf,
}

/// Starts the daemon on the first cache no other mount or server uses, and has Ctrl-C stop it from
/// then on: a Ctrl-C while it starts, or while the command mounts or serves through it, waits in
/// the returned [`interrupt::Owning`] for [`wait_and_stop`], which stops the daemon cleanly, rather
/// than dropping it.
async fn start_daemon(
	config: &CliConfig,
	ui: &mut UI,
	client: &Client,
	cache_size: Option<u64>,
) -> Result<(Daemon, interrupt::Owning)> {
	// a Ctrl-C while the managed rclone downloads still cancels the command
	let binary = prepare_binary(config, ui).await;
	let ctrl_c = interrupt::own();
	let dir = rclone_dir(config);
	let defaults = VfsTuning::default();
	for cache in 0..CACHES {
		let cache_dir = cache_dir(&dir, cache);
		let started = RcloneDaemon::start(
			client,
			RcloneDaemonConfig {
				// One copy per attempt; it is a path.
				binary: binary.clone(),
				state_dir: dir.join("daemon"),
				// One copy per attempt; it is a path, kept for the warning on stopping.
				cache_dir: cache_dir.clone(),
				log_file: None,
				user_agent: format!("filen-cli/{}", env!("CARGO_PKG_VERSION")),
				vfs: VfsTuning {
					cache_max_size: cache_size.unwrap_or(defaults.cache_max_size),
					..defaults
				},
			},
		)
		.await;
		match started {
			Ok(daemon) => {
				let daemon = Daemon {
					daemon,
					cache,
					cache_dir,
				};
				return Ok((daemon, ctrl_c));
			}
			Err(e) if e.downcast_ref::<RcloneRefusal>() == Some(&RcloneRefusal::CacheInUse) => {
				log::info!("Cache {cache} is in use by another mount or server");
			}
			Err(e) => {
				return Err(rclone_error(
					e,
					"Failed to start Rclone (use --verbose for more info)",
				));
			}
		}
	}
	Err(UI::failure(&format!(
		"{CACHES} mounts and servers are running already; stop one first"
	)))
}

/// `e` wrapped in `context`. A refusal is reported as it is, with the context the SDK gave it
/// (the path or address it was about): as an invalid command line when the daemon does not run the
/// options it was given, and as a failure when something on this machine is in the way. Any other
/// error stays unexpected.
fn rclone_error(
	e: filen_sdk_rs::Error,
	context: impl fmt::Display + Send + Sync + 'static,
) -> anyhow::Error {
	let (refusal, about) = match e.downcast::<RcloneRefusal>() {
		Ok(refusal) => refusal,
		Err(e) => return anyhow::Error::from(e).context(context),
	};
	let mut message = if about.is_empty() {
		refusal.to_string()
	} else {
		format!("{refusal} ({})", about.join("; "))
	};
	if refusal == RcloneRefusal::FuseMissing
		&& let Some(install) = fuse_install_hint()
	{
		message = format!("{message}; {install}");
	}
	let err = if refusal.kind() == ErrorKind::InvalidState {
		UI::usage_failure(&message)
	} else {
		UI::failure(&message)
	};
	err.context(context)
}

/// How to install the mount backend on this platform, for when there is none. macOS has no such
/// hint: without macFUSE it mounts through Rclone's NFS mount, so a mount there is never refused
/// for want of FUSE.
fn fuse_install_hint() -> Option<&'static str> {
	if cfg!(target_os = "linux") {
		Some(
			"install FUSE 3 (e.g. `apt install fuse3` or `dnf install fuse3`) and make sure /dev/fuse exists",
		)
	} else if cfg!(windows) {
		Some("install WinFsp from https://winfsp.dev/rel/")
	} else {
		None
	}
}

/// The `cache`th cache directory in `dir`: `cache`, then `cache-2`, `cache-3` and so on.
fn cache_dir(dir: &Path, cache: u32) -> PathBuf {
	match cache {
		0 => dir.join("cache"),
		n => dir.join(format!("cache-{}", n.saturating_add(1))),
	}
}

/// Waits for Ctrl-C, then stops the daemon, letting uploads finish for a while unless Ctrl-C is
/// pressed again; fails if rclone exits first.
async fn wait_and_stop(
	ui: &mut UI,
	daemon: &Daemon,
	mut ctrl_c: interrupt::Owning,
	stopping: &str,
) -> Result<()> {
	let Daemon {
		daemon,
		cache,
		cache_dir,
	} = daemon;
	select! {
		() = ctrl_c.pressed() => {}
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
	let _stopping = ctrl_c.into_stopping(ui);
	let report = daemon.stop(STOP).await.context("Failed to stop Rclone")?;
	if report.pending_uploads > 0 {
		ui.print_warning(&pending_uploads_warning(
			report.pending_uploads,
			*cache,
			cache_dir,
		));
	}
	Ok(())
}

/// The warning that `pending` uploads were left in the `cache`th cache, at `cache_dir`, saying when
/// they continue: when a mount or server next uses that cache, which it picks only while every
/// cache before it is in use.
fn pending_uploads_warning(pending: u32, cache: u32, cache_dir: &Path) -> String {
	if cache == 0 {
		format!(
			"{pending} uploads did not finish; they continue the next time you run `mount` or `serve`"
		)
	} else {
		format!(
			"{pending} uploads did not finish in {}; they continue the next time `mount` or `serve` starts while every cache before it is in use",
			cache_dir.display()
		)
	}
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

/// The options of a `serve` that the daemon would refuse whatever the address and credentials, so
/// that they are refused before logging in: a restic server writes to the drive without the cache,
/// so it cannot be read-only.
fn check_serve_args(protocol: ServeProtocol, read_only: bool) -> Result<()> {
	if protocol == ServeProtocol::Restic && read_only {
		return Err(UI::usage_failure("A restic server cannot be read-only"));
	}
	Ok(())
}

/// The host and port of `address`, `<ip>:<port>` with an IPv6 address in brackets, or
/// `localhost:<port>`: the hosts the daemon listens on. Any other host, such as the empty one of
/// `:<port>`, is refused.
fn parse_address(address: &str) -> Result<(String, u16)> {
	let invalid = || {
		UI::usage_failure(&format!(
			"Invalid address {address}: expected <ip>:<port> (an IPv6 address in brackets) or localhost:<port>"
		))
	};
	let (host, port) = address.rsplit_once(':').ok_or_else(invalid)?;
	let port = port.parse().map_err(|_| invalid())?;
	let host = match host
		.strip_prefix('[')
		.and_then(|host| host.strip_suffix(']'))
	{
		Some(v6) if v6.parse::<Ipv6Addr>().is_ok() => v6,
		Some(_) => return Err(invalid()),
		None if host == "localhost" || host.parse::<Ipv4Addr>().is_ok() => host,
		None => return Err(invalid()),
	};
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
		(ServeProtocol::Nfs, _, _) => Err(UI::usage_failure("NFS servers take no credentials")),
		(ServeProtocol::S3, Some(access_key_id), Some(secret_access_key)) => Ok(ServeAuth::S3 {
			access_key_id,
			secret_access_key,
		}),
		(_, Some(user), Some(password)) => Ok(ServeAuth::Basic { user, password }),
		_ => Err(UI::usage_failure(
			"Give both --user and --password, or neither",
		)),
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
	use std::collections::HashSet;

	use super::*;
	use crate::{
		commands::Commands,
		error::{exit_code, find_cli_error},
		parse_args,
		ui::test_support::Printed,
	};

	/// The protocol of `filen serve <server_type>`.
	fn parsed_protocol(server_type: &str) -> Result<ServeProtocol, clap::Error> {
		let args = parse_args(["filen", "serve", server_type])?.args;
		let Some(Commands::Rclone(RcloneCommands::Serve { server, .. })) = args.command else {
			panic!("`serve {server_type}` parsed as another command");
		};
		Ok(server.into())
	}

	#[test]
	fn serve_takes_the_server_type_by_name() {
		assert_eq!(parsed_protocol("webdav").unwrap(), ServeProtocol::Webdav);
		assert_eq!(parsed_protocol("s3").unwrap(), ServeProtocol::S3);
		assert_eq!(parsed_protocol("ftp").unwrap(), ServeProtocol::Ftp);
		assert_eq!(parsed_protocol("sftp").unwrap(), ServeProtocol::Sftp);
		assert_eq!(parsed_protocol("http").unwrap(), ServeProtocol::Http);
		assert_eq!(parsed_protocol("restic").unwrap(), ServeProtocol::Restic);
		assert_eq!(parsed_protocol("nfs").unwrap(), ServeProtocol::Nfs);
	}

	#[test]
	fn an_unknown_server_type_is_a_usage_error() {
		let err = parsed_protocol("gopher").unwrap_err();

		assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
		assert_eq!(err.exit_code(), 2);
	}

	/// The command `args` parse into.
	fn parsed_command(args: &[&str]) -> RcloneCommands {
		let Some(Commands::Rclone(command)) = parse_args(args).unwrap().args.command else {
			panic!("{args:?} parsed as another command");
		};
		command
	}

	#[test]
	fn the_replaced_server_commands_take_their_old_options() {
		assert!(matches!(
			parsed_command(&[
				"filen",
				"webdav",
				"--w-user",
				"u",
				"--w-password",
				"p",
				"--w-port",
				"80"
			]),
			RcloneCommands::Webdav { .. }
		));
		assert!(matches!(
			parsed_command(&["filen", "s3", "--s3-access-key-id", "k", "--s3-https"]),
			RcloneCommands::S3 { .. }
		));
		assert!(matches!(
			parsed_command(&["filen", "webdav-proxy", "--w-port", "80"]),
			RcloneCommands::WebdavProxy { .. }
		));
	}

	#[test]
	fn the_webdav_command_names_its_replacement_and_new_default_address() {
		let err = moved_to_serve(ServerType::Webdav);

		assert_eq!(exit_code(&err), 2);
		assert_eq!(
			format!("{err:#}"),
			"`filen webdav` was replaced by `filen serve webdav`, which listens on 127.0.0.1:1900 by default instead of 0.0.0.0:80; add `--addr 0.0.0.0:1900` to listen on every network interface"
		);
	}

	#[test]
	fn the_s3_command_names_its_replacement_and_new_default_address() {
		let err = moved_to_serve(ServerType::S3);

		assert_eq!(exit_code(&err), 2);
		assert_eq!(
			format!("{err:#}"),
			"`filen s3` was replaced by `filen serve s3`, which listens on 127.0.0.1:1800 by default instead of 0.0.0.0:80; add `--addr 0.0.0.0:1800` to listen on every network interface"
		);
	}

	#[test]
	fn the_webdav_proxy_command_says_it_is_no_longer_available() {
		let err = webdav_proxy_removed();

		assert_eq!(exit_code(&err), 2);
		assert_eq!(
			format!("{err:#}"),
			"`filen webdav-proxy` is no longer available: the multi-user proxy mode was removed. Run `filen serve webdav` for each account instead"
		);
	}

	#[test]
	fn every_server_type_has_a_port_of_its_own() {
		let ports = ServerType::value_variants()
			.iter()
			.map(|server| default_port((*server).into()))
			.collect::<HashSet<u16>>();

		assert_eq!(ports.len(), ServerType::value_variants().len());
	}

	#[test]
	fn the_help_names_each_server_types_default_address() {
		for server in ServerType::value_variants() {
			let value = server.to_possible_value().unwrap();
			let help = value.get_help().unwrap().to_string();
			let port = default_port((*server).into());

			assert!(
				help.ends_with(&format!("(default address 127.0.0.1:{port})")),
				"{}: {help}",
				value.get_name()
			);
		}
	}

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
	fn concurrent_daemons_get_caches_of_their_own() {
		let dir = Path::new("/home/user/.config/filen-cli/rclone");

		assert_eq!(cache_dir(dir, 0), dir.join("cache"));
		assert_eq!(cache_dir(dir, 1), dir.join("cache-2"));
		assert_eq!(cache_dir(dir, 7), dir.join("cache-8"));
	}

	#[test]
	fn uploads_left_in_the_main_cache_continue_on_the_next_start() {
		let dir = Path::new("/home/user/.config/filen-cli/rclone");

		assert_eq!(
			pending_uploads_warning(3, 0, &cache_dir(dir, 0)),
			"3 uploads did not finish; they continue the next time you run `mount` or `serve`"
		);
	}

	#[test]
	fn uploads_left_in_another_cache_continue_when_that_cache_is_used_again() {
		let dir = Path::new("/home/user/.config/filen-cli/rclone");

		// Joined as the platform joins paths.
		let cache = dir.join("cache-3");

		assert_eq!(
			pending_uploads_warning(3, 2, &cache),
			format!(
				"3 uploads did not finish in {}; they continue the next time `mount` or `serve` starts while every cache before it is in use",
				cache.display()
			)
		);
	}

	#[test]
	fn a_port_in_use_is_a_failure() {
		let err = rclone_error(
			RcloneRefusal::PortInUse { port: 1900 }.into(),
			"Failed to start WebDAV server",
		);

		assert_eq!(exit_code(&err), 1);
		assert_eq!(
			format!("{err:#}"),
			"Failed to start WebDAV server: port 1900 is in use"
		);
	}

	#[test]
	fn a_mount_point_that_is_not_empty_is_a_failure() {
		let err = rclone_error(
			RcloneRefusal::MountPointNotEmpty.into(),
			"Failed to mount network drive (use --verbose for more info)",
		);

		assert_eq!(exit_code(&err), 1);
		assert_eq!(
			format!("{err:#}"),
			"Failed to mount network drive (use --verbose for more info): the mount point is not empty"
		);
	}

	#[test]
	fn an_address_the_daemon_does_not_listen_on_is_a_usage_error() {
		let err = rclone_error(
			RcloneRefusal::ImplicitAddress.into(),
			"Failed to start WebDAV server",
		);

		assert_eq!(exit_code(&err), 2);
		assert_eq!(
			format!("{err:#}"),
			"Failed to start WebDAV server: the address must name an IP address or localhost and a port"
		);
	}

	#[test]
	fn a_refusal_keeps_what_it_was_about() {
		let err = rclone_error(
			filen_sdk_rs::Error::from(RcloneRefusal::MountPointInvalid)
				.with_context("/tmp/filen: not a directory"),
			"Failed to mount network drive",
		);

		assert_eq!(exit_code(&err), 1);
		assert_eq!(
			format!("{err:#}"),
			"Failed to mount network drive: the mount point is not a usable directory or drive letter (/tmp/filen: not a directory)"
		);
	}

	#[test]
	fn a_missing_fuse_says_how_to_install_it() {
		let err = rclone_error(
			RcloneRefusal::FuseMissing.into(),
			"Failed to mount network drive",
		);

		assert_eq!(exit_code(&err), 1);
		let expected = if cfg!(target_os = "linux") {
			"Failed to mount network drive: no FUSE is installed; install FUSE 3 (e.g. `apt install fuse3` or `dnf install fuse3`) and make sure /dev/fuse exists"
		} else if cfg!(windows) {
			"Failed to mount network drive: no FUSE is installed; install WinFsp from https://winfsp.dev/rel/"
		} else {
			"Failed to mount network drive: no FUSE is installed"
		};
		assert_eq!(format!("{err:#}"), expected);
	}

	#[test]
	fn an_error_that_is_not_a_refusal_stays_unexpected() {
		let err = rclone_error(
			filen_sdk_rs::Error::custom(ErrorKind::IO, "rc request failed"),
			"Failed to start Rclone",
		);

		assert!(find_cli_error(&err).is_none());
	}

	#[test]
	fn addresses_parse_into_host_and_port() {
		assert_eq!(
			parse_address("127.0.0.1:8080").unwrap(),
			("127.0.0.1".to_owned(), 8080)
		);
		assert_eq!(parse_address("[::1]:80").unwrap(), ("::1".to_owned(), 80));
		assert_eq!(
			parse_address("localhost:80").unwrap(),
			("localhost".to_owned(), 80)
		);
		assert!(parse_address("127.0.0.1").is_err());
		assert!(parse_address("127.0.0.1:port").is_err());
	}

	#[test]
	fn an_address_without_an_ip_is_a_usage_error() {
		for address in [
			":80",
			"example.com:80",
			"[localhost]:80",
			"::1:80",
			"[127.0.0.1]:80",
		] {
			let err = parse_address(address).unwrap_err();

			assert_eq!(exit_code(&err), 2, "{address}");
		}
		assert_eq!(
			format!("{:#}", parse_address(":80").unwrap_err()),
			"Invalid address :80: expected <ip>:<port> (an IPv6 address in brackets) or localhost:<port>"
		);
	}

	#[test]
	fn a_read_only_restic_server_is_a_usage_error() {
		let err = check_serve_args(ServeProtocol::Restic, true).unwrap_err();

		assert_eq!(exit_code(&err), 2);
		assert_eq!(format!("{err:#}"), "A restic server cannot be read-only");
		assert!(check_serve_args(ServeProtocol::Restic, false).is_ok());
		assert!(check_serve_args(ServeProtocol::Webdav, true).is_ok());
	}

	#[test]
	fn generated_credentials_are_printed_when_quiet() {
		let printed = Printed::default();
		let mut ui = printed.ui();
		ui.initialize(true, false, None, true, false);

		print_started(
			&mut ui,
			"Started WebDAV server on http://127.0.0.1:1900 with username \"u\" and password \"p\" (press Ctrl-C to stop)",
			true,
		);

		assert_eq!(
			printed.lines(),
			"Started WebDAV server on http://127.0.0.1:1900 with username \"u\" and password \"p\" (press Ctrl-C to stop)"
		);
	}

	#[test]
	fn given_credentials_are_not_printed_when_quiet() {
		let printed = Printed::default();
		let mut ui = printed.ui();
		ui.initialize(true, false, None, true, false);

		print_started(
			&mut ui,
			"Started WebDAV server on http://127.0.0.1:1900 with username \"u\" and password \"p\" (press Ctrl-C to stop)",
			false,
		);

		assert_eq!(printed.lines_or_empty(), "");
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

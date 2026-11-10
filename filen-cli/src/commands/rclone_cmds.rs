//! [cli-doc] managed-rclone
//! The Filen CLI includes a managed installation of Rclone, which can be used to [access Filen](https://rclone.org/filen).
//! It is automatically downloaded and configured (authenticated) when you run the commands like `rclone`, `mount`, etc.
//! Your credentials are only passed to it in memory, never written to disk. `serve` listens on 127.0.0.1 unless told otherwise,
//! and prints random credentials when you do not give any.
//!
//! [cli-doc] rclone-passthrough
//! `filen rclone` runs any Rclone command, with your drive as the `filen` remote, so it covers what
//! `mount` and `serve` have no option for: more parallel transfers (`--transfers`), or any other
//! Rclone flag. For example:
//!
//! ```sh
//! filen rclone copy ./photos filen:Photos --transfers 16
//! filen rclone serve webdav filen:Photos --addr 127.0.0.1:8080 --user me --pass secret
//! ```
//!
//! Everything after `rclone` goes to Rclone as it is: `filen rclone -v ls filen:` runs `rclone -v
//! ls filen:`, and `filen rclone --help` shows Rclone's help. The CLI's own options go before
//! `rclone` (`filen -v rclone ls filen:`), except `--rclone-binary <path>`, which goes right after
//! it.
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
//! S3 clients connect to `http://127.0.0.1:1800` by default (`https://` with `--cert` or
//! `--self-signed`), with the Access Key ID and Secret Access Key `serve` prints (or the `--user`
//! and `--password` given), and must use path-style addressing: the bucket in the path, not in the
//! host name. Any region is accepted, e.g. `us-east-1`. Each directory in the root of your drive
//! is a bucket; files directly in the root are not listed.
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
//!
//! `serve --self-signed` does without OpenSSL: it makes a self-signed certificate (an ECDSA P-256
//! key, valid for a year) in `serve-tls` in the config directory, and reuses it on every later
//! `serve`, so that a client that trusts it keeps trusting it. It names `localhost`, `127.0.0.1`
//! and `::1`, and the addresses servers listened on, and is made again when it is about to expire
//! or a server listens on an address it does not name. A server listening on every interface
//! (`0.0.0.0`) is reached by an address the certificate cannot name: clients pin the SHA-256
//! fingerprint that `serve` prints (`certificateSha256` with `--json`) instead.

use std::{
	fmt,
	net::{IpAddr, Ipv4Addr, Ipv6Addr},
	path::{Path, PathBuf},
	time::Duration,
};

use anyhow::{Context as _, Result, anyhow};
use chrono::Utc;
use clap::{Subcommand, ValueEnum};
use filen_sdk_rs::{
	ErrorKind,
	auth::Client,
	fs::categories::NonRootFileType,
	rclone::{
		FuseProvider, MountInfo, MountOptions, RcloneBinary, RcloneDaemon, RcloneDaemonConfig,
		RcloneExit, RcloneRefusal, RcloneStats, ServeAuth, ServeOptions, ServeProtocol, ServeTls,
		StopOptions, VfsTuning, available_drive_letters, clear_cache, run_rclone,
	},
};
use serde_json::{Value, json};
use tokio::{
	select,
	time::{Interval, MissedTickBehavior, interval},
};

use crate::{
	CliConfig, CommandResult,
	auth::LazyClient,
	commands::CommandContext,
	docs::DocElement,
	error::{self, CliError},
	interrupt,
	serve_cert::{self, CertOrigin, SelfSignedCert},
	ui::{self, StderrLine, UI},
	util::RemotePath,
};

#[derive(Debug, Subcommand)]
pub(crate) enum RcloneCommands {
	/// Execute an Rclone command using the managed installation
	Rclone {
		#[command(flatten)]
		binary: RcloneBinaryArg,
		/// The command to execute. Your Filen drive is available as the "filen" remote.
		#[arg(trailing_var_arg = true, allow_hyphen_values = true)]
		cmd: Vec<String>,
	},
	/// Mount Filen as a network drive
	Mount {
		#[command(flatten)]
		binary: RcloneBinaryArg,
		/// Where to mount the network drive: an empty directory (default: /tmp/filen), or on Windows a drive letter (default: X)
		mount_point: Option<String>,
		/// The maximum cache size (e.g. "500Mi", "10Gi") (default: 32Gi)
		#[arg(long, value_parser = parse_size)]
		cache_size: Option<u64>,
		/// Mount the drive read-only
		#[arg(long)]
		read_only: bool,
		/// Mount only this directory, by its path from the root of your drive, not from the working directory (default: the whole drive)
		#[arg(long, value_name = "PATH")]
		root: Option<String>,
	},
	/// Runs WebDAV, S3, FTP, SFTP, HTTP, restic or NFS servers exposing your Filen drive
	// Boxed: its options make up most of the enum's size, which every other command would pay for.
	Serve(Box<ServeCommand>),
	/// Shows the version and path of the Rclone that mount, serve and rclone run
	RcloneInfo {
		#[command(flatten)]
		binary: RcloneBinaryArg,
	},
	/// Deletes the files Rclone cached for mount and serve (refused while one runs, or while files are waiting to be uploaded)
	ClearRcloneCache,
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

/// The options of `serve`.
#[derive(Debug, clap::Args)]
pub(crate) struct ServeCommand {
	#[command(flatten)]
	binary: RcloneBinaryArg,
	/// The types of server to run: one, or several on one Rclone, each on its default port
	#[arg(required = true, num_args = 1..)]
	servers: Vec<ServerType>,
	/// IP and port for the server (`<ip>:<port>`; `0.0.0.0:<port>` for every network interface), with a single server type only (default: 127.0.0.1 and a port per type)
	#[arg(long = "addr", conflicts_with = "host")]
	address: Option<String>,
	/// IP address every server listens on, each on its default port (`0.0.0.0` for every network interface) (default: 127.0.0.1)
	#[arg(long)]
	host: Option<String>,
	/// Username for authentication to the server (default: random, printed when the server starts).
	/// On S3 servers, this is the Access Key ID. NFS servers have no authentication and listen on 127.0.0.1 only.
	#[arg(long)]
	user: Option<String>,
	/// Password for authentication to the server (default: random, printed when the server starts).
	/// On S3 servers, this is the Secret Access Key. Other users on this computer can see it on
	/// the command line: prefer the environment variable or --password-file.
	#[arg(long, env = "FILEN_CLI_SERVE_PASSWORD", hide_env_values = true)]
	password: Option<ServePassword>,
	/// File to read the password from, instead of --password (one trailing newline is ignored)
	#[arg(long, value_name = "PATH", conflicts_with = "password")]
	password_file: Option<PathBuf>,
	/// The server is read-only
	#[arg(long)]
	read_only: bool,
	/// Serve only this directory, by its path from the root of your drive, not from the working directory (default: the whole drive)
	#[arg(long, value_name = "PATH")]
	root: Option<String>,
	/// The maximum cache size (e.g. "500Mi", "10Gi") (default: 32Gi)
	#[arg(long, value_parser = parse_size)]
	cache_size: Option<u64>,
	/// TLS certificate (PEM) to serve HTTPS with (WebDAV, S3, HTTP and restic; requires --key; or use --self-signed)
	#[arg(long)]
	cert: Option<String>,
	/// TLS private key (PEM) for --cert
	#[arg(long)]
	key: Option<String>,
	/// Serve HTTPS (WebDAV, S3, HTTP and restic) with a self-signed certificate, made once in the config directory and reused; its SHA-256 fingerprint is printed for clients to pin
	#[arg(long, conflicts_with_all = ["cert", "key"])]
	self_signed: bool,
	/// Also mount the drive, on the same Rclone and cache as the servers (read-only with --read-only, of --root only with --root)
	#[arg(long)]
	mount: bool,
	/// Where --mount mounts the drive: an empty directory (default: /tmp/filen), or on Windows a drive letter (default: X)
	#[arg(long, value_name = "PATH", requires = "mount")]
	mount_point: Option<String>,
}

/// The Rclone binary a command runs: the managed one, unless the user gives their own.
#[derive(Debug, clap::Args)]
pub(crate) struct RcloneBinaryArg {
	/// Run this Rclone binary instead of the managed one (mount and serve need Rclone 1.74.3 or later)
	#[arg(long, value_name = "PATH")]
	rclone_binary: Option<PathBuf>,
}

/// The password `serve` is given, which its `Debug` leaves out.
#[derive(Clone)]
pub(crate) struct ServePassword(String);

impl From<String> for ServePassword {
	fn from(password: String) -> Self {
		Self(password)
	}
}

impl fmt::Debug for ServePassword {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("ServePassword(<redacted>)")
	}
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
	DocElement::CommandHelp("rclone-info"),
	DocElement::CommandHelp("clear-rclone-cache"),
	DocElement::CommandHelp("mount"),
	DocElement::DocFragment("mount-prerequisites"),
	DocElement::CommandHelp("serve"),
	DocElement::DocFragment("serve-s3-clients"),
	DocElement::DocFragment("serve-https"),
	DocElement::CommandHelp("export-api-key"),
];

impl RcloneCommands {
	/// Whether this command uses the drive: see [`super::Commands::uses_drive`]. `rclone-info`
	/// and `clear-rclone-cache` work on the local Rclone only, and the replaced commands only fail.
	pub(super) fn uses_drive(&self) -> bool {
		match self {
			Self::Rclone { .. } | Self::Mount { .. } | Self::Serve(_) | Self::ExportApiKey => true,
			Self::RcloneInfo { .. }
			| Self::ClearRcloneCache
			| Self::Webdav { .. }
			| Self::S3 { .. }
			| Self::WebdavProxy { .. } => false,
		}
	}

	pub(super) async fn execute(self, ctx: CommandContext<'_>) -> Result<CommandResult> {
		let CommandContext {
			config, ui, client, ..
		} = ctx;
		match self {
			Self::Rclone { binary, cmd } => {
				execute_rclone(config, ui, client, &binary, cmd).await?;
			}
			Self::Mount {
				binary,
				mount_point,
				cache_size,
				read_only,
				root,
			} => {
				mount(
					config,
					ui,
					client,
					&binary,
					MountArgs {
						mount_point,
						cache_size,
						read_only,
						root,
					},
				)
				.await?;
			}
			Self::Serve(serve) => {
				let ServeCommand {
					binary,
					servers,
					address,
					host,
					user,
					password,
					password_file,
					read_only,
					root,
					cache_size,
					cert,
					key,
					self_signed,
					mount,
					mount_point,
				} = *serve;
				let password = serve_password(password, password_file.as_deref()).await?;
				start_server(
					config,
					ui,
					client,
					&binary,
					ServeArgs {
						protocols: serve_protocols(&servers),
						address,
						host,
						user,
						password,
						read_only,
						root,
						cache_size,
						cert,
						key,
						self_signed,
						mount: mount.then_some(match mount_point {
							Some(mount_point) => MountPoint::At(mount_point),
							None => MountPoint::Default,
						}),
					},
				)
				.await?;
			}
			Self::RcloneInfo { binary } => rclone_info(config, ui, &binary).await?,
			Self::ClearRcloneCache => clear_rclone_cache(config, ui).await?,
			Self::ExportApiKey => {
				if !crate::auth::confirm_api_key_print(ui)? {
					return Ok(CommandResult::default());
				}
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

/// How often the status line of a running mount or server is redrawn.
const STATUS_INTERVAL: Duration = Duration::from_secs(2);

/// How many mounts and servers may run at once, each on a cache of its own: two Rclones on one
/// cache would corrupt it.
const CACHES: u32 = 8;

/// What `serve` serves.
pub(crate) struct ServeArgs {
	/// The servers' types, each once, in the order given.
	pub(crate) protocols: Vec<ServeProtocol>,
	/// `host:port` of the one server; `host` (or 127.0.0.1) and the protocol's default port
	/// without it.
	pub(crate) address: Option<String>,
	/// The IP address, or `localhost`, every server listens on, each on its default port;
	/// 127.0.0.1 without it.
	pub(crate) host: Option<String>,
	pub(crate) user: Option<String>,
	pub(crate) password: Option<String>,
	pub(crate) read_only: bool,
	/// The directory served, from the drive's root; the whole drive without one.
	pub(crate) root: Option<String>,
	pub(crate) cache_size: Option<u64>,
	pub(crate) cert: Option<String>,
	pub(crate) key: Option<String>,
	/// Whether the servers that serve HTTPS do so with the certificate in the config directory,
	/// made if there is none that fits.
	pub(crate) self_signed: bool,
	/// Where the drive is also mounted, on the servers' Rclone; not mounted without one.
	pub(crate) mount: Option<MountPoint>,
}

/// Where `serve --mount` mounts the drive.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MountPoint {
	/// Where `mount` mounts it without a mount point.
	Default,
	At(String),
}

/// One server `serve` starts.
#[derive(Debug, PartialEq, Eq)]
struct Server {
	protocol: ServeProtocol,
	host: String,
	port: u16,
	auth: ServeAuth,
	/// Whether `auth` was generated, so that it is printed even with `--quiet`.
	credentials_generated: bool,
	tls: Option<ServeTls>,
	/// The fingerprint of the self-signed certificate `tls` names, for a client to pin.
	self_signed_fingerprint: Option<String>,
}

/// A server that `serve` started.
struct StartedServer {
	protocol: ServeProtocol,
	/// Where it listens, as `host:port`.
	addr: String,
	/// The URL of a server spoken to over HTTP.
	url: Option<String>,
	auth: ServeAuth,
	credentials_generated: bool,
	self_signed_fingerprint: Option<String>,
}

/// What `mount` mounts.
pub(crate) struct MountArgs {
	/// The default mount point without one.
	pub(crate) mount_point: Option<String>,
	pub(crate) cache_size: Option<u64>,
	pub(crate) read_only: bool,
	/// The directory mounted, from the drive's root; the whole drive without one.
	pub(crate) root: Option<String>,
}

pub(crate) async fn mount(
	config: &CliConfig,
	ui: &mut UI,
	client: &mut LazyClient,
	binary: &RcloneBinaryArg,
	args: MountArgs,
) -> Result<()> {
	let client = client.get(ui).await?;
	let root = drive_dir(client, args.root.as_deref()).await?;
	let mount_point = match args.mount_point {
		Some(mount_point) => mount_point,
		None => default_mount_point().await?,
	};
	let (daemon, ctrl_c) = start_daemon(config, ui, client, binary, args.cache_size).await?;
	let mounted = daemon
		.daemon
		.mount(MountOptions {
			mount_point,
			read_only: args.read_only,
			root,
		})
		.await
		.map_err(|e| {
			rclone_error(
				e,
				"Failed to mount network drive (use --verbose for more info)",
			)
		})?;
	if ui.json {
		ui.print_json(mount_json(&mounted))?;
	} else {
		ui.print_success(&format!(
			"Mounted network drive at {} (press Ctrl-C to unmount and exit)",
			mounted.mount_point
		));
	}
	wait_and_stop(ui, &daemon, ctrl_c, "Unmounting network drive").await
}

pub(crate) async fn start_server(
	config: &CliConfig,
	ui: &mut UI,
	client: &mut LazyClient,
	binary: &RcloneBinaryArg,
	args: ServeArgs,
) -> Result<()> {
	let mut servers = plan_servers(&args)?;
	let self_signed = if args.self_signed {
		Some(use_self_signed_cert(&config.config_dir, &mut servers).await?)
	} else {
		None
	};
	let client = client.get(ui).await?;
	let root = drive_dir(client, args.root.as_deref()).await?;
	let mount_point = match args.mount {
		None => None,
		Some(MountPoint::Default) => Some(default_mount_point().await?),
		Some(MountPoint::At(mount_point)) => Some(mount_point),
	};
	let (daemon, ctrl_c) = start_daemon(config, ui, client, binary, args.cache_size).await?;
	let started = start_servers_and_mount(
		&daemon.daemon,
		servers,
		mount_point,
		args.read_only,
		root.as_deref(),
	)
	.await;
	let (started, mounted) = match started {
		Ok(started) => started,
		Err(e) => {
			// A Ctrl-C now cancels the command, stopping Rclone at once.
			drop(ctrl_c);
			stop_after_failure(&daemon.daemon).await;
			return Err(e);
		}
	};
	let alone = started.len() == 1 && mounted.is_none();
	// on stderr with --json, whose output carries the fingerprint too
	if let Some(cert) = &self_signed {
		print_notice(ui, &self_signed_notice(cert));
	}
	if ui.json {
		let mut printed: Vec<Value> = started.iter().map(started_json).collect();
		if let Some(mounted) = &mounted {
			printed.push(mount_json(mounted));
		}
		if alone && let Some(server) = printed.pop() {
			ui.print_json(server)?;
		} else {
			ui.print_json(Value::Array(printed))?;
		}
	} else {
		for server in &started {
			let line = started_line(server);
			let line = if alone {
				format!("{line} (press Ctrl-C to stop)")
			} else {
				line
			};
			print_started(ui, &line, server.credentials_generated);
		}
		if let Some(mounted) = &mounted {
			ui.print_success(&format!("Mounted network drive at {}", mounted.mount_point));
		}
		if !alone {
			print_notice(ui, "Press Ctrl-C to stop");
		}
	}
	let names = started
		.iter()
		.map(|server| display_name(server.protocol))
		.collect::<Vec<_>>();
	wait_and_stop(
		ui,
		&daemon,
		ctrl_c,
		&stopping_notice(&names, mounted.is_some()),
	)
	.await
}

/// The servers `args` describe, refused before logging in when the options do not fit them: an
/// `--addr` with several types, a read-only restic server, or credentials or a certificate given
/// halfway.
fn plan_servers(args: &ServeArgs) -> Result<Vec<Server>> {
	let several = args.protocols.len() > 1;
	if several && args.address.is_some() {
		return Err(UI::usage_failure(
			"--addr takes a single server type; give several servers an IP address with --host instead, and each listens on its default port",
		));
	}
	let tls = match (&args.cert, &args.key) {
		(Some(cert_path), Some(key_path)) => Some(ServeTls {
			// Owned copies, one per server that takes them.
			cert_path: cert_path.clone(),
			key_path: key_path.clone(),
		}),
		(None, None) => None,
		_ => return Err(UI::usage_failure("Give both --cert and --key, or neither")),
	};
	if args.self_signed && !args.protocols.iter().copied().any(serves_tls) {
		return Err(UI::usage_failure(
			"--self-signed serves HTTPS, which only WebDAV, S3, HTTP and restic servers do",
		));
	}
	let host = args.host.as_deref().map(parse_host).transpose()?;
	let credentials_given = args.user.is_some() || args.password.is_some();
	let mut servers = Vec::with_capacity(args.protocols.len());
	for &protocol in &args.protocols {
		check_serve_args(protocol, args.read_only)?;
		let (host, port) = match (&args.address, &host) {
			(Some(address), _) => parse_address(address)?,
			// One copy per server.
			(None, Some(host)) => (host.clone(), default_port(protocol)),
			(None, None) => ("127.0.0.1".to_owned(), default_port(protocol)),
		};
		// Several servers share the credentials given, but for NFS, which takes none.
		let auth = if several && protocol == ServeProtocol::Nfs {
			ServeAuth::Unauthenticated
		} else {
			// Each server takes its own copy of the credentials given.
			serve_auth(protocol, args.user.clone(), args.password.clone())?
		};
		let credentials_generated = !credentials_given && auth != ServeAuth::Unauthenticated;
		// With several servers, the certificate is for those that serve HTTPS; with one, the
		// server refuses it when it cannot.
		let tls = if several && !serves_tls(protocol) {
			None
		} else {
			tls.clone()
		};
		servers.push(Server {
			protocol,
			host,
			port,
			auth,
			credentials_generated,
			tls,
			self_signed_fingerprint: None,
		});
	}
	Ok(servers)
}

/// Gives the servers among `servers` that serve HTTPS the self-signed certificate in
/// `config_dir`, made now if there is none that names every address they listen on.
async fn use_self_signed_cert(config_dir: &Path, servers: &mut [Server]) -> Result<SelfSignedCert> {
	let config_dir = config_dir.to_owned();
	let hosts = servers
		.iter()
		.filter(|server| serves_tls(server.protocol))
		.map(|server| server.host.clone())
		.collect::<Vec<_>>();
	let cert = tokio::task::spawn_blocking(move || {
		let hosts = hosts.iter().map(String::as_str).collect::<Vec<_>>();
		serve_cert::ensure(&config_dir, &hosts, Utc::now())
	})
	.await
	.context("Failed to make the self-signed certificate")?
	.context("Failed to make the self-signed certificate")?;
	for server in servers
		.iter_mut()
		.filter(|server| serves_tls(server.protocol))
	{
		// One copy per server.
		server.tls = Some(cert.tls.clone());
		server.self_signed_fingerprint = Some(cert.fingerprint.clone());
	}
	Ok(cert)
}

/// What `serve` says about the self-signed certificate it serves HTTPS with.
fn self_signed_notice(cert: &SelfSignedCert) -> String {
	let made = match cert.origin {
		CertOrigin::Generated => "made now",
		CertOrigin::Reused => "made earlier",
	};
	format!(
		"HTTPS uses the self-signed certificate {} ({made}), SHA-256 fingerprint {}",
		cert.tls.cert_path, cert.fingerprint
	)
}

/// Starts `servers`, and then the mount at `mount_point` if there is one, on `daemon`, all of
/// them read-only with `read_only` and of the directory `root` with one. Fails at the first that
/// does not start, leaving the others running.
async fn start_servers_and_mount(
	daemon: &RcloneDaemon,
	servers: Vec<Server>,
	mount_point: Option<String>,
	read_only: bool,
	root: Option<&str>,
) -> Result<(Vec<StartedServer>, Option<MountInfo>)> {
	let mut started = Vec::with_capacity(servers.len());
	for server in servers {
		let scheme = if server.tls.is_some() {
			"https"
		} else {
			"http"
		};
		let name = display_name(server.protocol);
		let info = daemon
			.start_serve(ServeOptions {
				protocol: server.protocol,
				host: server.host,
				port: server.port,
				// One copy for the server, one to print.
				auth: server.auth.clone(),
				tls: server.tls,
				read_only,
				root: root.map(str::to_owned),
			})
			.await
			.map_err(|e| rclone_error(e, format!("Failed to start {name} server")))?;
		let url = match server.protocol {
			ServeProtocol::Webdav
			| ServeProtocol::S3
			| ServeProtocol::Http
			| ServeProtocol::Restic => Some(format!("{scheme}://{}", info.addr)),
			ServeProtocol::Ftp | ServeProtocol::Sftp | ServeProtocol::Nfs => None,
		};
		started.push(StartedServer {
			protocol: server.protocol,
			addr: info.addr,
			url,
			auth: server.auth,
			credentials_generated: server.credentials_generated,
			self_signed_fingerprint: server.self_signed_fingerprint,
		});
	}
	let Some(mount_point) = mount_point else {
		return Ok((started, None));
	};
	let mounted = daemon
		.mount(MountOptions {
			mount_point,
			read_only,
			root: root.map(str::to_owned),
		})
		.await
		.map_err(|e| {
			rclone_error(
				e,
				"Failed to mount network drive (use --verbose for more info)",
			)
		})?;
	Ok((started, Some(mounted)))
}

/// Stops `daemon` after a server or the mount failed to start, letting what the others uploaded
/// meanwhile finish.
async fn stop_after_failure(daemon: &RcloneDaemon) {
	match daemon.stop(STOP).await {
		Ok(report) if report.pending_uploads > 0 => log::warn!(
			"{} uploads did not finish while Rclone stopped",
			report.pending_uploads
		),
		Ok(_) => {}
		Err(e) => log::warn!("Failed to stop Rclone: {e}"),
	}
}

/// What a started server's line says: its type, address and credentials.
fn started_line(server: &StartedServer) -> String {
	let name = display_name(server.protocol);
	let address = server.url.as_deref().unwrap_or(&server.addr);
	let credentials = match &server.auth {
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
	format!("Started {name} server on {address} {credentials}")
}

/// What `serve --json` prints for a started server.
fn started_json(server: &StartedServer) -> Value {
	let mut started = serve_json(
		server.protocol,
		&server.addr,
		server.url.as_deref(),
		&server.auth,
	);
	if let Some(fingerprint) = &server.self_signed_fingerprint {
		started["certificateSha256"] = json!(fingerprint);
	}
	started
}

/// What `serve` says when it stops the servers named `names` and, with `mounted`, the mount.
fn stopping_notice(names: &[&str], mounted: bool) -> String {
	let servers = match names {
		[] => String::new(),
		[name] => format!("{name} server"),
		[first @ .., last] => format!("{} and {last} servers", first.join(", ")),
	};
	if mounted {
		format!("Stopping {servers} and unmounting the network drive")
	} else {
		format!("Stopping {servers}")
	}
}

/// The directory `root` names, by its path from the drive's root, once it is known to be one: the
/// path without its leading `/`, or `None` for the drive's root itself or without a `root`.
async fn drive_dir(client: &Client, root: Option<&str>) -> Result<Option<String>> {
	let Some(root) = root else {
		return Ok(None);
	};
	let path = RemotePath::new("/").navigate(root);
	match client
		.find_item_at_path(&path.0)
		.await
		.context("Failed to find directory")?
	{
		Some(NonRootFileType::Root(_)) => Ok(None),
		Some(NonRootFileType::Dir(_)) => Ok(Some(dir_below_root(&path).to_owned())),
		Some(_) => Err(UI::failure(&format!("Not a directory: {}", path.0))),
		None => Err(UI::failure(&format!("No such directory: {}", path.0))),
	}
}

/// `path` as the SDK takes a root directory: from the drive's root, without a leading or trailing
/// `/`.
fn dir_below_root(path: &RemotePath) -> &str {
	path.0.trim_matches('/')
}

/// What `mount --json` prints once the drive is mounted.
fn mount_json(mounted: &MountInfo) -> Value {
	json!({
		"mountPoint": mounted.mount_point,
		"provider": provider_name(mounted.provider),
	})
}

/// The name of a mount backend in `mount --json`'s output.
fn provider_name(provider: FuseProvider) -> &'static str {
	match provider {
		FuseProvider::MacFuse => "macfuse",
		FuseProvider::NfsMount => "nfsmount",
		FuseProvider::WinFsp => "winfsp",
		FuseProvider::Fuse3 => "fuse3",
	}
}

/// What `serve --json` prints once the server listens on `addr` (`host:port`): the server type as
/// `serve` takes it, the address, the URL of a server spoken to over HTTP, and the credentials,
/// given or generated, in the protocol's terms.
fn serve_json(protocol: ServeProtocol, addr: &str, url: Option<&str>, auth: &ServeAuth) -> Value {
	let mut started = json!({
		"protocol": protocol_name(protocol),
		"address": addr,
	});
	if let Some(url) = url {
		started["url"] = json!(url);
	}
	match auth {
		ServeAuth::Basic { user, password } => {
			started["user"] = json!(user);
			started["password"] = json!(password);
		}
		ServeAuth::S3 {
			access_key_id,
			secret_access_key,
		} => {
			started["accessKeyId"] = json!(access_key_id);
			started["secretAccessKey"] = json!(secret_access_key);
		}
		ServeAuth::Unauthenticated => {}
	}
	started
}

/// The name `serve` takes the server type by.
fn protocol_name(protocol: ServeProtocol) -> &'static str {
	match protocol {
		ServeProtocol::Webdav => "webdav",
		ServeProtocol::S3 => "s3",
		ServeProtocol::Ftp => "ftp",
		ServeProtocol::Sftp => "sftp",
		ServeProtocol::Http => "http",
		ServeProtocol::Restic => "restic",
		ServeProtocol::Nfs => "nfs",
	}
}

/// Prints a notice about the managed Rclone, a mount or a server: on stdout, or on stderr under
/// `--json`, so that stdout holds the JSON alone.
fn print_notice(ui: &mut UI, msg: &str) {
	if ui.json {
		ui.print_muted_to_stderr(msg);
	} else {
		ui.print_muted(msg);
	}
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
	binary: &RcloneBinaryArg,
	cmd: Vec<String>,
) -> Result<()> {
	let binary = prepare_binary(config, ui, binary).await;
	let client = client.get(ui).await?;
	let status = run_rclone(client, &binary, &cmd)
		.await
		.context("Failed to run Rclone")?;
	match error::exit_code_of(status) {
		0 => Ok(()),
		code => Err(CliError::ChildExit(code).into()),
	}
}

/// Prints the version and path of the Rclone binary `binary` names, downloading the managed one
/// first if it is not there yet.
async fn rclone_info(config: &CliConfig, ui: &mut UI, binary: &RcloneBinaryArg) -> Result<()> {
	let binary = prepare_binary(config, ui, binary).await;
	let path = binary
		.resolve()
		.await
		.map_err(|e| rclone_error(e, "Failed to find Rclone"))?;
	let version = binary
		.version()
		.await
		.map_err(|e| rclone_error(e, "Failed to read Rclone's version"))?;
	let managed = matches!(binary, RcloneBinary::Managed { .. });
	if ui.json {
		ui.print_json(rclone_info_json(&version, &path, managed))?;
	} else {
		ui.print_key_value_table(&[
			("Version:", version.as_str()),
			("Path:", &path.display().to_string()),
			("Managed:", if managed { "yes" } else { "no" }),
		]);
	}
	Ok(())
}

/// What `rclone-info --json` prints.
fn rclone_info_json(version: &str, path: &Path, managed: bool) -> Value {
	json!({
		"version": version,
		"path": path.display().to_string(),
		"managed": managed,
	})
}

/// Deletes the cached files of every cache `mount` and `serve` use, refused while one runs or while
/// a cache holds files not uploaded yet. Stops at the first cache it cannot clear.
async fn clear_rclone_cache(config: &CliConfig, ui: &mut UI) -> Result<()> {
	let dir = rclone_dir(config);
	let state_dir = state_dir(&dir);
	for cache in 0..CACHES {
		let cache_dir = cache_dir(&dir, cache);
		clear_cache(&state_dir, &cache_dir).await.map_err(|e| {
			rclone_error(
				e,
				format!("Failed to clear the Rclone cache {}", cache_dir.display()),
			)
		})?;
	}
	ui.print_success("Cleared the Rclone cache");
	Ok(())
}

fn rclone_dir(config: &CliConfig) -> PathBuf {
	config.config_dir.join("rclone")
}

/// The daemons' state directory in `dir`, which every mount and server shares.
fn state_dir(dir: &Path) -> PathBuf {
	dir.join("daemon")
}

/// The Rclone binary `binary` names, or else the managed one, once the `rclone.conf` with the
/// account's keys that earlier versions wrote is removed; says so when the managed one is about
/// to be downloaded.
async fn prepare_binary(config: &CliConfig, ui: &mut UI, binary: &RcloneBinaryArg) -> RcloneBinary {
	let dir = rclone_dir(config);
	remove_old_config(&dir).await;
	let binary = rclone_binary(dir, binary);
	// A given binary is never downloaded: a missing one fails when it is run.
	if matches!(binary, RcloneBinary::Managed { .. }) && !binary.is_installed().await {
		print_notice(ui, "Downloading managed Rclone...");
	}
	binary
}

/// The binary at `--rclone-binary`, which the SDK does not check against a checksum, or the managed
/// one in `dir`.
fn rclone_binary(dir: PathBuf, binary: &RcloneBinaryArg) -> RcloneBinary {
	match &binary.rclone_binary {
		// One copy, for the binary the SDK owns.
		Some(path) => RcloneBinary::Bundled {
			path: path.clone(),
			sha256: None,
		},
		None => RcloneBinary::Managed { dir },
	}
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
	binary: &RcloneBinaryArg,
	cache_size: Option<u64>,
) -> Result<(Daemon, interrupt::Owning)> {
	// a Ctrl-C while the managed rclone downloads still cancels the command
	let binary = prepare_binary(config, ui, binary).await;
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
				state_dir: state_dir(&dir),
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
	if let Some(hint) = refusal_hint(&refusal) {
		message = format!("{message}; {hint}");
	}
	let err = if refusal.kind() == ErrorKind::InvalidState {
		UI::usage_failure(&message)
	} else {
		UI::failure(&message)
	};
	err.context(context)
}

/// What the user can do about `refusal`, where the CLI knows.
fn refusal_hint(refusal: &RcloneRefusal) -> Option<&'static str> {
	match refusal {
		RcloneRefusal::FuseMissing => fuse_install_hint(),
		RcloneRefusal::CacheInUse => Some("stop every running `mount` and `serve` first"),
		RcloneRefusal::UploadsPending { .. } => {
			Some("run `mount` or `serve` to let them upload first")
		}
		RcloneRefusal::BinaryMissing
		| RcloneRefusal::BinaryChecksum
		| RcloneRefusal::UnsupportedVersion { .. }
		| RcloneRefusal::WatchdogUnavailable
		| RcloneRefusal::MountPointInUse
		| RcloneRefusal::MountPointNotEmpty
		| RcloneRefusal::MountPointInvalid
		| RcloneRefusal::DriveLetterInUse
		| RcloneRefusal::PortInUse { .. }
		| RcloneRefusal::AddressNotLocal
		| RcloneRefusal::ImplicitAddress
		| RcloneRefusal::AuthRequired
		| RcloneRefusal::InvalidCredentials
		| RcloneRefusal::UnauthenticatedOffLoopback
		| RcloneRefusal::UnsupportedProtocol
		| RcloneRefusal::TlsUnsupported => None,
	}
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
	let mut status = StatusLine::new(ui);
	let mut ticks = interval(STATUS_INTERVAL);
	ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
	loop {
		select! {
			biased;
			() = ctrl_c.pressed() => break,
			exit = daemon.exited() => {
				status.clear();
				return Err(anyhow!(match exit {
					RcloneExit::Crashed { code: Some(code), .. } => {
						format!("Rclone exited with code {code} (use --verbose for more info)")
					}
					RcloneExit::Crashed { code: None, .. } | RcloneExit::Stopped => {
						"Rclone exited unexpectedly (use --verbose for more info)".to_owned()
					}
				}));
			}
			stats = next_stats(&mut ticks, daemon), if status.enabled() => match stats {
				Ok(stats) => status.show(&status_text(&stats)),
				// The line keeps what it last showed; the mount or server is not affected.
				Err(e) => log::warn!("Failed to read Rclone's stats: {e}"),
			},
		}
	}
	status.clear();
	print_notice(
		ui,
		&format!("{stopping}, letting uploads finish for up to 30 seconds..."),
	);
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

/// Rclone's stats at the next tick of `ticks`. Dropping it, when Ctrl-C is pressed or Rclone
/// exits first, abandons the request.
async fn next_stats(
	ticks: &mut Interval,
	daemon: &RcloneDaemon,
) -> Result<RcloneStats, filen_sdk_rs::Error> {
	ticks.tick().await;
	daemon.stats().await
}

/// The line a running mount or server redraws on stderr with what its cache is doing, shown only
/// to a person at a terminal, as an announcement is: not with `--quiet` or `--json`, nor when
/// stdout or stderr is redirected.
struct StatusLine {
	line: Option<StderrLine>,
}

impl StatusLine {
	fn new(ui: &UI) -> Self {
		let line = StderrLine::on_terminal();
		let enabled = status_line_wanted(ui, line.is_some());
		Self {
			line: line.filter(|_| enabled),
		}
	}

	fn enabled(&self) -> bool {
		self.line.is_some()
	}

	fn show(&mut self, text: &str) {
		if let Some(line) = &mut self.line {
			line.show(text);
		}
	}

	fn clear(&mut self) {
		if let Some(line) = &mut self.line {
			line.clear();
		}
	}
}

/// Whether the status line is drawn, with stderr a terminal or not.
fn status_line_wanted(ui: &UI, stderr_is_terminal: bool) -> bool {
	stderr_is_terminal && ui.output_is_read_by_a_person() && !ui.is_quiet()
}

/// The status line's text: the files being uploaded and waiting in the cache, the cache's size on
/// disk, and the files whose upload failed or a full disk, when there are any.
fn status_text(stats: &RcloneStats) -> String {
	let mut text = format!(
		"Uploading {}, queued {}, cache {}",
		stats.uploads_in_progress,
		stats.uploads_queued,
		ui::format_size(stats.cache_bytes_used)
	);
	if stats.errored_files > 0 {
		text.push_str(&format!(", {} failed (retrying)", stats.errored_files));
	}
	if stats.out_of_space {
		text.push_str(", cache disk full");
	}
	text
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

/// The protocols of the server types `serve` was given, each once, in the order given.
fn serve_protocols(servers: &[ServerType]) -> Vec<ServeProtocol> {
	let mut protocols = Vec::with_capacity(servers.len());
	for &server in servers {
		let protocol = ServeProtocol::from(server);
		if !protocols.contains(&protocol) {
			protocols.push(protocol);
		}
	}
	protocols
}

/// Whether a server of `protocol` serves HTTPS with `--cert`.
fn serves_tls(protocol: ServeProtocol) -> bool {
	match protocol {
		ServeProtocol::Webdav | ServeProtocol::S3 | ServeProtocol::Http | ServeProtocol::Restic => {
			true
		}
		ServeProtocol::Ftp | ServeProtocol::Sftp | ServeProtocol::Nfs => false,
	}
}

/// The host `--host` names: an IP address (an IPv6 one with or without brackets), or `localhost`.
fn parse_host(host: &str) -> Result<String> {
	let ip = host
		.strip_prefix('[')
		.and_then(|host| host.strip_suffix(']'))
		.unwrap_or(host);
	if ip == "localhost" || ip.parse::<IpAddr>().is_ok() {
		Ok(ip.to_owned())
	} else {
		Err(UI::usage_failure(&format!(
			"Invalid host {host}: expected an IP address or localhost"
		)))
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

/// The password `serve` was given: `--password` or its environment variable, or the contents of
/// `--password-file` without one trailing newline.
async fn serve_password(
	password: Option<ServePassword>,
	password_file: Option<&Path>,
) -> Result<Option<String>> {
	let Some(path) = password_file else {
		return Ok(password.map(|ServePassword(password)| password));
	};
	let contents = tokio::fs::read_to_string(path).await.map_err(|e| {
		UI::failure(&format!(
			"Failed to read the password file {}: {e}",
			path.display()
		))
	})?;
	let password = contents
		.strip_suffix('\n')
		.map_or(contents.as_str(), |line| {
			line.strip_suffix('\r').unwrap_or(line)
		});
	if password.is_empty() {
		return Err(UI::usage_failure(&format!(
			"The password file {} is empty",
			path.display()
		)));
	}
	Ok(Some(password.to_owned()))
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

/// A size such as `500Mi` or `10Gi`, in one of the forms `n` or `nB` (bytes) and `nU`, `nUi`,
/// `nUB` or `nUiB`, where the unit `U` is `K` (or `k`), `M`, `G` or `T`. Every unit is binary:
/// `10G`, `10Gi`, `10GB` and `10GiB` are all 10 GiB.
pub(crate) fn parse_size(size: &str) -> Result<u64, String> {
	let not_a_size = || format!("{size} is not a size such as 500Mi or 10Gi");
	let digits = size
		.find(|c: char| !c.is_ascii_digit())
		.unwrap_or(size.len());
	let (number, suffix) = size.split_at(digits);
	let number: u64 = number.parse().map_err(|_| not_a_size())?;
	let shift = match suffix {
		"" | "B" => 0,
		_ => {
			let unit = suffix
				.strip_suffix("iB")
				.or_else(|| suffix.strip_suffix('i'))
				.or_else(|| suffix.strip_suffix('B'))
				.unwrap_or(suffix);
			match unit {
				"K" | "k" => 10,
				"M" => 20,
				"G" => 30,
				"T" => 40,
				_ => return Err(not_a_size()),
			}
		}
	};
	number
		.checked_mul(1 << shift)
		.ok_or_else(|| format!("{size} is too large"))
}

#[cfg(test)]
mod tests {
	use std::{collections::HashSet, ffi::OsStr};

	use clap::CommandFactory as _;

	use super::*;
	use crate::{
		CliArgs,
		commands::Commands,
		error::{exit_code, find_cli_error},
		parse_args,
		ui::test_support::Printed,
	};

	/// The protocol of `filen serve <server_type>`.
	fn parsed_protocol(server_type: &str) -> Result<ServeProtocol, clap::Error> {
		let args = parse_args(["filen", "serve", server_type])?.args;
		let Some(Commands::Rclone(RcloneCommands::Serve(serve))) = args.command else {
			panic!("`serve {server_type}` parsed as another command");
		};
		let [server] = serve.servers.as_slice() else {
			panic!("`serve {server_type}` parsed as {:?}", serve.servers);
		};
		Ok((*server).into())
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
	fn mount_is_read_write_unless_asked_to_be_read_only() {
		assert!(matches!(
			parsed_command(&["filen", "mount"]),
			RcloneCommands::Mount {
				read_only: false,
				..
			}
		));
		assert!(matches!(
			parsed_command(&["filen", "mount", "/mnt/filen", "--read-only"]),
			RcloneCommands::Mount {
				read_only: true,
				..
			}
		));
	}

	#[test]
	fn mount_and_serve_take_a_root_directory() {
		assert!(matches!(
			parsed_command(&["filen", "mount", "--root", "/Photos"]),
			RcloneCommands::Mount { root: Some(root), .. } if root == "/Photos"
		));
		assert!(matches!(
			parsed_command(&["filen", "serve", "webdav", "--root", "Photos/2024"]),
			RcloneCommands::Serve(serve) if serve.root.as_deref() == Some("Photos/2024")
		));
		assert!(matches!(
			parsed_command(&["filen", "serve", "webdav"]),
			RcloneCommands::Serve(serve) if serve.root.is_none()
		));
	}

	/// The directory the SDK is given for `--root <root>`.
	fn root_dir(root: &str) -> String {
		dir_below_root(&RemotePath::new("/").navigate(root)).to_owned()
	}

	#[test]
	fn the_root_directory_is_resolved_from_the_drives_root() {
		assert_eq!(root_dir("Photos/2024"), "Photos/2024");
		assert_eq!(root_dir("/Photos/2024/"), "Photos/2024");
		assert_eq!(root_dir("Photos/../Documents"), "Documents");
		assert_eq!(root_dir("../../Photos"), "Photos");
		assert_eq!(root_dir("/"), "");
	}

	/// The `--rclone-binary` of `args`.
	fn parsed_rclone_binary(args: &[&str]) -> Option<PathBuf> {
		let binary = match parsed_command(args) {
			RcloneCommands::Rclone { binary, .. }
			| RcloneCommands::Mount { binary, .. }
			| RcloneCommands::RcloneInfo { binary } => binary,
			RcloneCommands::Serve(serve) => serve.binary,
			command => panic!("{args:?} parsed as {command:?}"),
		};
		binary.rclone_binary
	}

	#[test]
	fn mount_serve_rclone_and_rclone_info_take_an_rclone_binary() {
		let given = Some(PathBuf::from("/opt/rclone"));

		assert_eq!(
			parsed_rclone_binary(&["filen", "mount", "--rclone-binary", "/opt/rclone"]),
			given
		);
		assert_eq!(
			parsed_rclone_binary(&["filen", "serve", "webdav", "--rclone-binary", "/opt/rclone"]),
			given
		);
		assert_eq!(
			parsed_rclone_binary(&["filen", "rclone-info", "--rclone-binary", "/opt/rclone"]),
			given
		);
		assert_eq!(
			parsed_rclone_binary(&["filen", "rclone", "--rclone-binary", "/opt/rclone", "ls"]),
			given
		);
		assert_eq!(parsed_rclone_binary(&["filen", "mount"]), None);
	}

	#[test]
	fn an_rclone_binary_after_the_rclone_command_goes_to_rclone() {
		let RcloneCommands::Rclone { binary, cmd } =
			parsed_command(&["filen", "rclone", "ls", "--rclone-binary", "/opt/rclone"])
		else {
			panic!("parsed as another command");
		};

		assert_eq!(binary.rclone_binary, None);
		assert_eq!(cmd, ["ls", "--rclone-binary", "/opt/rclone"]);
	}

	#[test]
	fn a_given_rclone_binary_is_run_without_a_checksum() {
		let dir = PathBuf::from("/home/user/.config/filen-cli/rclone");
		let given = RcloneBinaryArg {
			rclone_binary: Some(PathBuf::from("/opt/rclone")),
		};
		let none = RcloneBinaryArg {
			rclone_binary: None,
		};

		assert_eq!(
			rclone_binary(dir.clone(), &given),
			RcloneBinary::Bundled {
				path: PathBuf::from("/opt/rclone"),
				sha256: None
			}
		);
		assert_eq!(
			rclone_binary(dir.clone(), &none),
			RcloneBinary::Managed { dir }
		);
	}

	#[test]
	fn rclone_info_json_names_the_version_path_and_whether_it_is_managed() {
		assert_eq!(
			rclone_info_json("v1.74.4", Path::new("/opt/rclone"), false),
			json!({"version": "v1.74.4", "path": "/opt/rclone", "managed": false})
		);
	}

	#[test]
	fn a_cache_in_use_says_to_stop_the_mounts_and_servers() {
		let err = rclone_error(
			RcloneRefusal::CacheInUse.into(),
			"Failed to clear the Rclone cache",
		);

		assert_eq!(exit_code(&err), 1);
		assert_eq!(
			format!("{err:#}"),
			"Failed to clear the Rclone cache: an rclone daemon uses the cache; stop every running `mount` and `serve` first"
		);
	}

	#[test]
	fn pending_uploads_say_how_to_let_them_upload() {
		let err = rclone_error(
			RcloneRefusal::UploadsPending { count: 2 }.into(),
			"Failed to clear the Rclone cache",
		);

		assert_eq!(exit_code(&err), 1);
		assert_eq!(
			format!("{err:#}"),
			"Failed to clear the Rclone cache: 2 files in the cache are not uploaded yet; run `mount` or `serve` to let them upload first"
		);
	}

	#[tokio::test]
	async fn clearing_the_rclone_cache_deletes_every_caches_files() {
		let config_dir = tempfile::tempdir().unwrap();
		let config = CliConfig {
			config_dir: config_dir.path().to_path_buf(),
			profiles: crate::auth::Profiles::new(config_dir.path(), None).unwrap(),
			config_dir_is_default: false,
		};
		let dir = rclone_dir(&config);
		let cached = [
			cache_dir(&dir, 0).join("vfs/Filen/photo.jpg"),
			cache_dir(&dir, 7).join("vfs/Filen/photo.jpg"),
		];
		for file in &cached {
			std::fs::create_dir_all(file.parent().unwrap()).unwrap();
			std::fs::write(file, b"cached").unwrap();
		}
		let printed = Printed::default();
		let mut ui = printed.ui();

		clear_rclone_cache(&config, &mut ui).await.unwrap();

		assert!(cached.iter().all(|file| !file.exists()), "{cached:?}");
		assert_eq!(
			console::strip_ansi_codes(&printed.lines()),
			"✔ Cleared the Rclone cache"
		);
	}

	#[tokio::test]
	async fn a_cache_with_files_not_uploaded_is_kept() {
		let config_dir = tempfile::tempdir().unwrap();
		let config = CliConfig {
			config_dir: config_dir.path().to_path_buf(),
			profiles: crate::auth::Profiles::new(config_dir.path(), None).unwrap(),
			config_dir_is_default: false,
		};
		let cache = cache_dir(&rclone_dir(&config), 1);
		let meta = cache.join("vfsMeta/Filen/photo.jpg");
		std::fs::create_dir_all(meta.parent().unwrap()).unwrap();
		std::fs::write(&meta, br#"{"Dirty": true}"#).unwrap();
		let printed = Printed::default();
		let mut ui = printed.ui();

		let err = clear_rclone_cache(&config, &mut ui).await.unwrap_err();

		assert_eq!(exit_code(&err), 1);
		assert_eq!(
			format!("{err:#}"),
			format!(
				"Failed to clear the Rclone cache {}: 1 files in the cache are not uploaded yet; run `mount` or `serve` to let them upload first",
				cache.display()
			)
		);
		assert!(meta.exists());
	}

	#[test]
	fn serve_takes_several_server_types_each_once() {
		let RcloneCommands::Serve(serve) =
			parsed_command(&["filen", "serve", "webdav", "s3", "webdav", "--read-only"])
		else {
			panic!("parsed as another command");
		};

		assert_eq!(
			serve_protocols(&serve.servers),
			[ServeProtocol::Webdav, ServeProtocol::S3]
		);
	}

	#[test]
	fn serve_without_a_server_type_is_a_usage_error() {
		let Err(err) = parse_args(["filen", "serve"]) else {
			panic!("parsed serve without a server type");
		};

		assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
		assert_eq!(err.exit_code(), 2);
	}

	#[test]
	fn the_address_and_the_host_conflict() {
		let args = [
			"filen",
			"serve",
			"webdav",
			"--addr",
			"127.0.0.1:8080",
			"--host",
			"0.0.0.0",
		];

		let Err(err) = parse_args(args) else {
			panic!("parsed both --addr and --host");
		};

		assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
	}

	#[test]
	fn a_mount_point_needs_mount() {
		let Err(err) = parse_args(["filen", "serve", "webdav", "--mount-point", "/mnt/filen"])
		else {
			panic!("parsed --mount-point without --mount");
		};

		assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
	}

	#[test]
	fn serve_mounts_where_it_is_told_or_by_default() {
		let mount = |args: &[&str]| {
			let RcloneCommands::Serve(serve) = parsed_command(args) else {
				panic!("{args:?} parsed as another command");
			};
			(serve.mount, serve.mount_point)
		};

		assert_eq!(mount(&["filen", "serve", "webdav"]), (false, None));
		assert_eq!(
			mount(&["filen", "serve", "webdav", "--mount"]),
			(true, None)
		);
		assert_eq!(
			mount(&[
				"filen",
				"serve",
				"webdav",
				"--mount",
				"--mount-point",
				"/mnt/filen"
			]),
			(true, Some("/mnt/filen".to_owned()))
		);
	}

	/// What `serve` gets for `protocols` without any option.
	fn serve_args(protocols: &[ServeProtocol]) -> ServeArgs {
		ServeArgs {
			protocols: protocols.to_vec(),
			address: None,
			host: None,
			user: None,
			password: None,
			read_only: false,
			root: None,
			cache_size: None,
			cert: None,
			key: None,
			self_signed: false,
			mount: None,
		}
	}

	/// Where each of `servers` listens.
	fn addresses(servers: &[Server]) -> Vec<(ServeProtocol, &str, u16)> {
		servers
			.iter()
			.map(|server| (server.protocol, server.host.as_str(), server.port))
			.collect()
	}

	#[test]
	fn several_servers_listen_on_their_default_ports() {
		let servers =
			plan_servers(&serve_args(&[ServeProtocol::Webdav, ServeProtocol::S3])).unwrap();

		assert_eq!(
			addresses(&servers),
			[
				(ServeProtocol::Webdav, "127.0.0.1", 1900),
				(ServeProtocol::S3, "127.0.0.1", 1800),
			]
		);
	}

	#[test]
	fn several_servers_listen_on_the_host_given() {
		let args = ServeArgs {
			host: Some("[::1]".to_owned()),
			..serve_args(&[ServeProtocol::Ftp, ServeProtocol::Http])
		};

		let servers = plan_servers(&args).unwrap();

		assert_eq!(
			addresses(&servers),
			[
				(ServeProtocol::Ftp, "::1", 2121),
				(ServeProtocol::Http, "::1", 1700),
			]
		);
	}

	#[test]
	fn an_address_with_several_servers_is_a_usage_error() {
		let args = ServeArgs {
			address: Some("127.0.0.1:8080".to_owned()),
			..serve_args(&[ServeProtocol::Webdav, ServeProtocol::S3])
		};

		let err = plan_servers(&args).unwrap_err();

		assert_eq!(exit_code(&err), 2);
		assert_eq!(
			format!("{err:#}"),
			"--addr takes a single server type; give several servers an IP address with --host instead, and each listens on its default port"
		);
	}

	#[test]
	fn one_server_listens_on_the_address_given() {
		let args = ServeArgs {
			address: Some("127.0.0.1:8080".to_owned()),
			..serve_args(&[ServeProtocol::Webdav])
		};

		let servers = plan_servers(&args).unwrap();

		assert_eq!(
			addresses(&servers),
			[(ServeProtocol::Webdav, "127.0.0.1", 8080)]
		);
	}

	#[test]
	fn an_invalid_host_is_a_usage_error() {
		let args = ServeArgs {
			host: Some("example.com".to_owned()),
			..serve_args(&[ServeProtocol::Webdav, ServeProtocol::S3])
		};

		let err = plan_servers(&args).unwrap_err();

		assert_eq!(exit_code(&err), 2);
		assert_eq!(
			format!("{err:#}"),
			"Invalid host example.com: expected an IP address or localhost"
		);
	}

	#[test]
	fn hosts_are_ip_addresses_or_localhost() {
		assert_eq!(parse_host("0.0.0.0").unwrap(), "0.0.0.0");
		assert_eq!(parse_host("::").unwrap(), "::");
		assert_eq!(parse_host("[::1]").unwrap(), "::1");
		assert_eq!(parse_host("localhost").unwrap(), "localhost");
		assert!(parse_host("").is_err());
		assert!(parse_host("127.0.0.1:80").is_err());
	}

	#[test]
	fn several_servers_get_credentials_of_their_own() {
		let servers = plan_servers(&serve_args(&[
			ServeProtocol::Webdav,
			ServeProtocol::Ftp,
			ServeProtocol::S3,
			ServeProtocol::Nfs,
		]))
		.unwrap();

		let [webdav, ftp, s3, nfs] = servers.as_slice() else {
			panic!("planned {servers:?}");
		};
		assert!(matches!(webdav.auth, ServeAuth::Basic { .. }));
		assert_ne!(webdav.auth, ftp.auth);
		assert!(matches!(s3.auth, ServeAuth::S3 { .. }));
		assert_eq!(nfs.auth, ServeAuth::Unauthenticated);
		assert!(webdav.credentials_generated && ftp.credentials_generated);
		assert!(s3.credentials_generated && !nfs.credentials_generated);
	}

	#[test]
	fn the_credentials_given_are_every_servers_but_nfs() {
		let args = ServeArgs {
			user: Some("me".to_owned()),
			password: Some("secret".to_owned()),
			..serve_args(&[ServeProtocol::Webdav, ServeProtocol::Nfs])
		};

		let servers = plan_servers(&args).unwrap();

		assert_eq!(
			servers
				.iter()
				.map(|server| &server.auth)
				.collect::<Vec<_>>(),
			[
				&ServeAuth::Basic {
					user: "me".to_owned(),
					password: "secret".to_owned()
				},
				&ServeAuth::Unauthenticated,
			]
		);
		assert!(servers.iter().all(|server| !server.credentials_generated));
	}

	#[test]
	fn credentials_given_to_nfs_alone_are_a_usage_error() {
		let args = ServeArgs {
			user: Some("me".to_owned()),
			password: Some("secret".to_owned()),
			..serve_args(&[ServeProtocol::Nfs])
		};

		let err = plan_servers(&args).unwrap_err();

		assert_eq!(exit_code(&err), 2);
		assert_eq!(format!("{err:#}"), "NFS servers take no credentials");
	}

	#[test]
	fn with_several_servers_only_those_serving_https_get_the_certificate() {
		let args = ServeArgs {
			cert: Some("cert.pem".to_owned()),
			key: Some("key.pem".to_owned()),
			..serve_args(&[ServeProtocol::Webdav, ServeProtocol::Ftp])
		};

		let servers = plan_servers(&args).unwrap();

		let tls = ServeTls {
			cert_path: "cert.pem".to_owned(),
			key_path: "key.pem".to_owned(),
		};
		assert_eq!(
			servers
				.iter()
				.map(|server| server.tls.as_ref())
				.collect::<Vec<_>>(),
			[Some(&tls), None]
		);
	}

	#[test]
	fn one_server_gets_the_certificate_even_if_it_cannot_serve_https() {
		let args = ServeArgs {
			cert: Some("cert.pem".to_owned()),
			key: Some("key.pem".to_owned()),
			..serve_args(&[ServeProtocol::Ftp])
		};

		let servers = plan_servers(&args).unwrap();

		assert!(servers[0].tls.is_some());
	}

	#[test]
	fn self_signed_and_a_certificate_are_a_usage_error() {
		let Err(err) = parse_args([
			"filen",
			"serve",
			"webdav",
			"--self-signed",
			"--cert",
			"cert.pem",
			"--key",
			"key.pem",
		]) else {
			panic!("parsed both --self-signed and --cert");
		};

		assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
	}

	#[test]
	fn self_signed_without_a_server_serving_https_is_a_usage_error() {
		let args = ServeArgs {
			self_signed: true,
			..serve_args(&[ServeProtocol::Ftp, ServeProtocol::Nfs])
		};

		let err = plan_servers(&args).unwrap_err();

		assert_eq!(exit_code(&err), 2);
		assert_eq!(
			format!("{err:#}"),
			"--self-signed serves HTTPS, which only WebDAV, S3, HTTP and restic servers do"
		);
	}

	#[tokio::test]
	async fn only_the_servers_serving_https_get_the_self_signed_certificate() {
		let config_dir = tempfile::tempdir().unwrap();
		let args = ServeArgs {
			host: Some("192.168.1.5".to_owned()),
			self_signed: true,
			..serve_args(&[ServeProtocol::Webdav, ServeProtocol::Ftp])
		};
		let mut servers = plan_servers(&args).unwrap();

		let cert = use_self_signed_cert(config_dir.path(), &mut servers)
			.await
			.unwrap();

		assert_eq!(cert.origin, CertOrigin::Generated);
		assert_eq!(
			servers
				.iter()
				.map(|server| (
					server.tls.as_ref(),
					server.self_signed_fingerprint.as_deref()
				))
				.collect::<Vec<_>>(),
			[
				(Some(&cert.tls), Some(cert.fingerprint.as_str())),
				(None, None)
			]
		);
		assert!(
			cert.tls
				.cert_path
				.starts_with(config_dir.path().to_str().unwrap())
		);
		let again = use_self_signed_cert(config_dir.path(), &mut servers)
			.await
			.unwrap();
		assert_eq!(again.origin, CertOrigin::Reused);
		assert_eq!(again.fingerprint, cert.fingerprint);
	}

	#[test]
	fn the_self_signed_notice_names_the_certificate_and_its_fingerprint() {
		let cert = SelfSignedCert {
			tls: ServeTls {
				cert_path: "/config/serve-tls/cert.pem".to_owned(),
				key_path: "/config/serve-tls/key.pem".to_owned(),
			},
			fingerprint: "AB:CD".to_owned(),
			origin: CertOrigin::Reused,
		};

		assert_eq!(
			self_signed_notice(&cert),
			"HTTPS uses the self-signed certificate /config/serve-tls/cert.pem (made earlier), \
			 SHA-256 fingerprint AB:CD"
		);
	}

	#[test]
	fn a_server_with_the_self_signed_certificate_prints_its_fingerprint_as_json() {
		let server = StartedServer {
			protocol: ServeProtocol::Http,
			addr: "127.0.0.1:1700".to_owned(),
			url: Some("https://127.0.0.1:1700".to_owned()),
			auth: ServeAuth::Unauthenticated,
			credentials_generated: false,
			self_signed_fingerprint: Some("AB:CD".to_owned()),
		};

		assert_eq!(
			started_json(&server),
			json!({
				"protocol": "http",
				"address": "127.0.0.1:1700",
				"url": "https://127.0.0.1:1700",
				"certificateSha256": "AB:CD",
			})
		);
	}

	#[test]
	fn a_read_only_restic_server_among_several_is_a_usage_error() {
		let args = ServeArgs {
			read_only: true,
			..serve_args(&[ServeProtocol::Webdav, ServeProtocol::Restic])
		};

		let err = plan_servers(&args).unwrap_err();

		assert_eq!(exit_code(&err), 2);
		assert_eq!(format!("{err:#}"), "A restic server cannot be read-only");
	}

	#[test]
	fn the_stopping_notice_names_the_servers_and_the_mount() {
		assert_eq!(
			stopping_notice(&["WebDAV"], false),
			"Stopping WebDAV server"
		);
		assert_eq!(
			stopping_notice(&["WebDAV", "S3", "FTP"], false),
			"Stopping WebDAV, S3 and FTP servers"
		);
		assert_eq!(
			stopping_notice(&["WebDAV"], true),
			"Stopping WebDAV server and unmounting the network drive"
		);
	}

	#[test]
	fn a_started_server_line_names_its_address_and_credentials() {
		let server = StartedServer {
			protocol: ServeProtocol::Ftp,
			addr: "127.0.0.1:2121".to_owned(),
			url: None,
			auth: ServeAuth::Basic {
				user: "me".to_owned(),
				password: "secret".to_owned(),
			},
			credentials_generated: false,
			self_signed_fingerprint: None,
		};

		assert_eq!(
			started_line(&server),
			"Started FTP server on 127.0.0.1:2121 with username \"me\" and password \"secret\""
		);
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
	fn malformed_size_suffixes_are_refused() {
		for size in ["10iB", "10i", "10BB", "10MBB", "10Mii", "10KiBB"] {
			assert_eq!(
				parse_size(size),
				Err(format!("{size} is not a size such as 500Mi or 10Gi")),
			);
		}
		assert_eq!(parse_size("10KB"), Ok(10 << 10));
		assert_eq!(parse_size("10kiB"), Ok(10 << 10));
		assert_eq!(parse_size("10B"), Ok(10));
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

	/// The `--password` and `--password-file` of `filen serve webdav <args>`.
	fn parsed_password_args(args: &[&str]) -> (Option<ServePassword>, Option<PathBuf>) {
		let args = [&["filen", "serve", "webdav"], args].concat();
		let RcloneCommands::Serve(serve) = parsed_command(&args) else {
			panic!("{args:?} parsed as another command");
		};
		(serve.password, serve.password_file)
	}

	#[test]
	fn the_serve_password_can_come_from_the_environment_and_is_not_shown_in_the_help() {
		let command = CliArgs::command();
		let serve = command.find_subcommand("serve").unwrap();
		let password = serve
			.get_arguments()
			.find(|arg| arg.get_id() == "password")
			.unwrap();

		assert_eq!(
			password.get_env(),
			Some(OsStr::new("FILEN_CLI_SERVE_PASSWORD"))
		);
		assert!(password.is_hide_env_values_set());
	}

	#[test]
	fn the_serve_password_is_not_debug_printed() {
		let (password, _) = parsed_password_args(&["--user", "me", "--password", "hunter2"]);

		assert_eq!(format!("{password:?}"), "Some(ServePassword(<redacted>))");
	}

	#[test]
	fn the_password_and_the_password_file_conflict() {
		let args = [
			"filen",
			"serve",
			"webdav",
			"--password",
			"p",
			"--password-file",
			"p.txt",
		];

		let Err(err) = parse_args(args) else {
			panic!("parsed both --password and --password-file");
		};

		assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
		assert_eq!(err.exit_code(), 2);
	}

	#[tokio::test]
	async fn the_password_file_is_read_without_its_trailing_newline() {
		let file = tempfile::NamedTempFile::new().unwrap();
		std::fs::write(file.path(), "hunter2\n").unwrap();
		let path = file.path().to_str().unwrap();
		let (password, password_file) = parsed_password_args(&["--password-file", path]);

		let password = serve_password(password, password_file.as_deref()).await;

		assert_eq!(password.unwrap().as_deref(), Some("hunter2"));
	}

	#[tokio::test]
	async fn only_one_trailing_newline_of_the_password_file_is_ignored() {
		let file = tempfile::NamedTempFile::new().unwrap();
		std::fs::write(file.path(), " hunter2 \r\n\n").unwrap();

		let password = serve_password(None, Some(file.path())).await;

		assert_eq!(password.unwrap().as_deref(), Some(" hunter2 \r\n"));
	}

	#[tokio::test]
	async fn a_windows_line_ending_after_the_password_is_ignored() {
		let file = tempfile::NamedTempFile::new().unwrap();
		std::fs::write(file.path(), "hunter2\r\n").unwrap();

		let password = serve_password(None, Some(file.path())).await;

		assert_eq!(password.unwrap().as_deref(), Some("hunter2"));
	}

	#[tokio::test]
	async fn an_empty_password_file_is_a_usage_error() {
		let file = tempfile::NamedTempFile::new().unwrap();
		std::fs::write(file.path(), "\n").unwrap();

		let err = serve_password(None, Some(file.path())).await.unwrap_err();

		assert_eq!(exit_code(&err), 2);
		assert_eq!(
			format!("{err:#}"),
			format!("The password file {} is empty", file.path().display())
		);
	}

	#[tokio::test]
	async fn a_missing_password_file_is_a_failure() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("missing.txt");

		let err = serve_password(None, Some(&path)).await.unwrap_err();

		assert_eq!(exit_code(&err), 1);
		assert!(
			format!("{err:#}").starts_with(&format!(
				"Failed to read the password file {}: ",
				path.display()
			)),
			"{err:#}"
		);
	}

	#[tokio::test]
	async fn the_password_is_used_without_a_password_file() {
		let (password, password_file) = parsed_password_args(&["--password", "hunter2"]);

		let password = serve_password(password, password_file.as_deref()).await;

		assert_eq!(password.unwrap().as_deref(), Some("hunter2"));
	}

	#[test]
	fn mount_json_names_the_mount_point_and_the_backend() {
		let mounted = MountInfo {
			mount_point: "/tmp/filen".to_owned(),
			provider: FuseProvider::Fuse3,
		};

		assert_eq!(
			mount_json(&mounted),
			json!({"mountPoint": "/tmp/filen", "provider": "fuse3"})
		);
	}

	#[test]
	fn serve_json_of_a_webdav_server_has_its_url_and_credentials() {
		let auth = ServeAuth::Basic {
			user: "filen-1a2b3c4d".to_owned(),
			password: "secret".to_owned(),
		};

		assert_eq!(
			serve_json(
				ServeProtocol::Webdav,
				"127.0.0.1:1900",
				Some("http://127.0.0.1:1900"),
				&auth
			),
			json!({
				"protocol": "webdav",
				"address": "127.0.0.1:1900",
				"url": "http://127.0.0.1:1900",
				"user": "filen-1a2b3c4d",
				"password": "secret",
			})
		);
	}

	#[test]
	fn serve_json_of_an_s3_server_has_its_access_key() {
		let auth = ServeAuth::S3 {
			access_key_id: "AKID".to_owned(),
			secret_access_key: "SAK".to_owned(),
		};

		assert_eq!(
			serve_json(
				ServeProtocol::S3,
				"127.0.0.1:1800",
				Some("https://127.0.0.1:1800"),
				&auth
			),
			json!({
				"protocol": "s3",
				"address": "127.0.0.1:1800",
				"url": "https://127.0.0.1:1800",
				"accessKeyId": "AKID",
				"secretAccessKey": "SAK",
			})
		);
	}

	#[test]
	fn serve_json_of_an_nfs_server_has_no_url_or_credentials() {
		assert_eq!(
			serve_json(
				ServeProtocol::Nfs,
				"127.0.0.1:2049",
				None,
				&ServeAuth::Unauthenticated
			),
			json!({"protocol": "nfs", "address": "127.0.0.1:2049"})
		);
	}

	#[test]
	fn serve_json_names_the_server_type_as_serve_takes_it() {
		for server in ServerType::value_variants() {
			let value = server.to_possible_value().unwrap();

			assert_eq!(protocol_name((*server).into()), value.get_name());
		}
	}

	#[test]
	fn notices_go_to_stderr_under_json() {
		let (out, err) = (Printed::default(), Printed::default());
		let mut ui = Printed::split_ui(&out, &err);
		ui.initialize(false, true, None, true, false);

		print_notice(&mut ui, "Stopping WebDAV server");

		assert_eq!(out.lines_or_empty(), "");
		assert!(
			err.lines().contains("Stopping WebDAV server"),
			"{}",
			err.lines()
		);
	}

	#[test]
	fn notices_go_to_stdout_without_json() {
		let (out, err) = (Printed::default(), Printed::default());
		let mut ui = Printed::split_ui(&out, &err);

		print_notice(&mut ui, "Stopping WebDAV server");

		assert!(
			out.lines().contains("Stopping WebDAV server"),
			"{}",
			out.lines()
		);
		assert_eq!(err.lines_or_empty(), "");
	}

	/// Stats with `uploading` files being uploaded, `queued` waiting and `cache_bytes` in the cache.
	fn stats(uploading: u32, queued: u32, cache_bytes: u64) -> RcloneStats {
		RcloneStats {
			uploads_in_progress: uploading,
			uploads_queued: queued,
			errored_files: 0,
			cache_bytes_used: cache_bytes,
			out_of_space: false,
			transfers: Vec::new(),
			mounts: Vec::new(),
			serves: Vec::new(),
		}
	}

	#[test]
	fn the_status_line_shows_uploads_and_the_cache_size() {
		assert_eq!(
			status_text(&stats(2, 5, 1_288_490_189)),
			"Uploading 2, queued 5, cache 1.20 GiB"
		);
	}

	#[test]
	fn the_status_line_shows_failed_uploads_and_a_full_disk() {
		let stats = RcloneStats {
			errored_files: 3,
			out_of_space: true,
			..stats(0, 0, 0)
		};

		assert_eq!(
			status_text(&stats),
			"Uploading 0, queued 0, cache 0 B, 3 failed (retrying), cache disk full"
		);
	}

	#[test]
	fn the_status_line_is_drawn_for_a_person_at_a_terminal() {
		let printed = Printed::default();
		let ui = printed.terminal_ui();

		assert!(status_line_wanted(&ui, true));
		assert!(!status_line_wanted(&ui, false));
	}

	#[test]
	fn the_status_line_is_not_drawn_when_quiet_or_json_or_stdout_is_redirected() {
		let printed = Printed::default();
		let mut quiet = printed.terminal_ui();
		quiet.initialize(true, false, None, true, false);
		let mut json = printed.terminal_ui();
		json.initialize(false, true, None, true, false);
		let redirected = printed.ui();

		assert!(!status_line_wanted(&quiet, true));
		assert!(!status_line_wanted(&json, true));
		assert!(!status_line_wanted(&redirected, true));
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

//! Serving the daemon's remotes over rclone's protocols, over rc.
//!
//! A serve is checked before rclone sees it: its address names an IP address or `localhost` and a
//! port, never an implicit one (rclone's `:port` listens on every interface); every protocol but
//! nfs takes credentials, which travel only in the rc body (loopback and rc-authenticated), never
//! in rclone's argv, environment or a file; nfs, which has no authentication, listens on loopback
//! only; sftp never reads `~/.ssh/authorized_keys`. Once rclone serves it, an unauthenticated
//! request must be refused, or the serve is stopped again.
//!
//! A serve uses `Filen:`, or `FilenRO:` when read-only, with the VFS options every role shares, so
//! it shares its VFS and cache with a mount on the same remote. restic is the exception: it writes
//! to the remote directly, without a VFS, so it is never read-only.

use std::{
	fmt, io,
	net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener},
	time::Duration,
};

use serde_json::{Map, Value};
use tokio::{
	io::{AsyncBufRead, AsyncBufReadExt as _, AsyncWriteExt as _, BufReader},
	net::TcpStream,
};
use tracing::warn;

use crate::{
	Error, ErrorKind,
	rclone::{
		RcloneDaemon, RcloneRefusal, VfsTuning,
		daemon::{FILEN_READ_ONLY_REMOTE, FILEN_REMOTE, stopped},
		rc::{ListedServe, RcCallFailed, ServeStarted},
		vfs::vfs_flags,
	},
};

/// How long the check that a new serve refuses an unauthenticated request may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// The most lines one FTP reply may take before the probe gives up on it.
const FTP_REPLY_LINES: usize = 32;
/// The longest credential a serve takes, in bytes.
const MAX_CREDENTIAL_LEN: usize = 1024;
/// The oldest TLS version an https serve accepts.
const MIN_TLS_VERSION: &str = "tls1.2";

/// A protocol rclone serves the drive over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServeProtocol {
	Webdav,
	S3,
	Ftp,
	Sftp,
	Http,
	Restic,
	/// Has no authentication, so it listens on loopback only. Not served on Windows.
	Nfs,
}

impl ServeProtocol {
	/// Whether a serve over the protocol takes credentials: every protocol but nfs.
	pub fn requires_auth(self) -> bool {
		!matches!(self, Self::Nfs)
	}

	/// rc `serve/start`'s `type`.
	fn rc_type(self) -> &'static str {
		match self {
			Self::Webdav => "webdav",
			Self::S3 => "s3",
			Self::Ftp => "ftp",
			Self::Sftp => "sftp",
			Self::Http => "http",
			Self::Restic => "restic",
			Self::Nfs => "nfs",
		}
	}

	/// The protocol rc names `kind`, if the daemon serves it here.
	fn from_rc_type(kind: &str) -> Option<Self> {
		let protocol = match kind {
			"webdav" => Self::Webdav,
			"s3" => Self::S3,
			"ftp" => Self::Ftp,
			"sftp" => Self::Sftp,
			"http" => Self::Http,
			"restic" => Self::Restic,
			"nfs" => Self::Nfs,
			_ => return None,
		};
		protocol.served_here().then_some(protocol)
	}

	/// Whether the daemon serves the protocol on this platform: nfs is not served on Windows.
	fn served_here(self) -> bool {
		cfg!(not(windows)) || self != Self::Nfs
	}

	/// Whether the protocol serves over TLS (https).
	fn serves_tls(self) -> bool {
		matches!(self, Self::Webdav | Self::S3 | Self::Http | Self::Restic)
	}
}

/// The credentials a serve takes.
#[derive(Clone, PartialEq, Eq)]
pub enum ServeAuth {
	/// A user name and password, for webdav, ftp, sftp, http and restic.
	Basic { user: String, password: String },
	/// An access key pair, for s3.
	S3 {
		access_key_id: String,
		secret_access_key: String,
	},
	/// None, for nfs only.
	Unauthenticated,
}

impl fmt::Debug for ServeAuth {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Basic { user, .. } => f
				.debug_struct("Basic")
				.field("user", user)
				.finish_non_exhaustive(),
			Self::S3 { access_key_id, .. } => f
				.debug_struct("S3")
				.field("access_key_id", access_key_id)
				.finish_non_exhaustive(),
			Self::Unauthenticated => f.write_str("Unauthenticated"),
		}
	}
}

/// The certificate an https serve presents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeTls {
	/// A PEM file with the certificate, followed by its chain.
	pub cert_path: String,
	/// A PEM file with the certificate's private key.
	pub key_path: String,
}

/// What to serve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeOptions {
	pub protocol: ServeProtocol,
	/// The IP address to listen on, or `localhost`. `0.0.0.0` or `::` listens on every interface.
	pub host: String,
	/// The port to listen on, never 0.
	pub port: u16,
	pub auth: ServeAuth,
	/// The certificate to serve https with; only webdav, s3, http and restic serve over TLS.
	pub tls: Option<ServeTls>,
	/// Whether the serve is read-only. It then uses its own remote and VFS cache, as read-only
	/// mounts do. restic is never read-only.
	pub read_only: bool,
}

/// A serve that is up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeInfo {
	/// The id [`RcloneDaemon::stop_serve`] takes.
	pub id: String,
	pub protocol: ServeProtocol,
	/// The address it listens on, as `host:port`.
	pub addr: String,
}

impl TryFrom<ListedServe> for ServeInfo {
	type Error = String;

	/// The serve rc lists, or the type it has if the daemon does not serve it.
	fn try_from(serve: ListedServe) -> Result<Self, String> {
		let protocol = ServeProtocol::from_rc_type(&serve.params.kind).ok_or(serve.params.kind)?;
		Ok(Self {
			id: serve.id,
			protocol,
			addr: serve.addr,
		})
	}
}

/// Where a serve listens.
#[derive(Debug, PartialEq, Eq)]
struct Listen {
	/// The address it binds, `localhost` read as 127.0.0.1.
	socket: SocketAddr,
	/// The address rclone is given.
	addr: String,
}

impl Listen {
	/// Where an unauthenticated request is sent to check the serve refuses it: the bound address,
	/// or loopback for every interface.
	fn probe_target(&self) -> SocketAddr {
		let ip = match self.socket.ip() {
			IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
			IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
			ip => ip,
		};
		SocketAddr::new(ip, self.socket.port())
	}
}

/// Where `options` listens, once it is known to be a serve the daemon runs.
fn check(options: &ServeOptions) -> Result<Listen, RcloneRefusal> {
	if !options.protocol.served_here() {
		return Err(RcloneRefusal::UnsupportedProtocol);
	}
	let listen = listen(&options.host, options.port)?;
	if options.tls.is_some() && !options.protocol.serves_tls() {
		return Err(RcloneRefusal::TlsUnsupported);
	}
	match (options.protocol, &options.auth) {
		(ServeProtocol::Nfs, ServeAuth::Unauthenticated) => {
			if !listen.socket.ip().is_loopback() {
				return Err(RcloneRefusal::UnauthenticatedOffLoopback);
			}
		}
		(ServeProtocol::Nfs, _) => return Err(RcloneRefusal::InvalidCredentials),
		(_, ServeAuth::Unauthenticated)
		| (ServeProtocol::S3, ServeAuth::Basic { .. })
		| (
			ServeProtocol::Webdav
			| ServeProtocol::Ftp
			| ServeProtocol::Sftp
			| ServeProtocol::Http
			| ServeProtocol::Restic,
			ServeAuth::S3 { .. },
		) => return Err(RcloneRefusal::AuthRequired),
		(
			ServeProtocol::S3,
			ServeAuth::S3 {
				access_key_id,
				secret_access_key,
			},
		) => {
			let key_id_valid = (16..=128).contains(&access_key_id.len())
				&& access_key_id.chars().all(|c| c.is_ascii_alphanumeric());
			// The pair reaches rclone as one CSV field, `"<id>,<secret>"`.
			if !key_id_valid || !credential_valid(secret_access_key, &[',', '"']) {
				return Err(RcloneRefusal::InvalidCredentials);
			}
		}
		(_, ServeAuth::Basic { user, password }) => {
			// Basic auth joins the two with a `:`, which a password may hold but a user may not.
			if !credential_valid(user, &[':']) || !credential_valid(password, &[]) {
				return Err(RcloneRefusal::InvalidCredentials);
			}
		}
	}
	Ok(listen)
}

/// Where a serve on `host` and `port` listens: `host` is an IP address or `localhost`, and `port`
/// is not 0.
fn listen(host: &str, port: u16) -> Result<Listen, RcloneRefusal> {
	if port == 0 {
		return Err(RcloneRefusal::ImplicitAddress);
	}
	if host == "localhost" {
		return Ok(Listen {
			socket: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
			addr: format!("localhost:{port}"),
		});
	}
	let ip = host
		.parse::<IpAddr>()
		.map_err(|_| RcloneRefusal::ImplicitAddress)?;
	let socket = SocketAddr::new(ip, port);
	Ok(Listen {
		socket,
		addr: socket.to_string(),
	})
}

/// Whether `credential` is non-empty, at most [`MAX_CREDENTIAL_LEN`] bytes, and holds no control
/// character and none of `forbidden`.
fn credential_valid(credential: &str, forbidden: &[char]) -> bool {
	!credential.is_empty()
		&& credential.len() <= MAX_CREDENTIAL_LEN
		&& !credential
			.chars()
			.any(|c| c.is_control() || forbidden.contains(&c))
}

/// rc `serve/start`'s parameters for `options`, listening on `addr`, with `vfs` as every role's
/// VFS options.
fn serve_params(options: &ServeOptions, addr: &str, vfs: &VfsTuning) -> Value {
	let mut params = if options.protocol == ServeProtocol::Restic {
		Map::new()
	} else {
		vfs_flags(vfs, options.read_only)
	};
	let mut set = |key: &str, value: String| params.insert(key.to_owned(), Value::String(value));
	set("type", options.protocol.rc_type().to_owned());
	let remote = if options.read_only {
		FILEN_READ_ONLY_REMOTE
	} else {
		FILEN_REMOTE
	};
	set("fs", format!("{remote}:"));
	set("addr", addr.to_owned());
	// Owned copies of the credentials for the body, which is sent once.
	match &options.auth {
		ServeAuth::Basic { user, password } => {
			set("user", user.clone());
			set("pass", password.clone());
		}
		// `--auth-key` is a list flag, read as CSV: the pair is quoted to stay one entry.
		ServeAuth::S3 {
			access_key_id,
			secret_access_key,
		} => {
			set(
				"auth_key",
				format!("\"{access_key_id},{secret_access_key}\""),
			);
		}
		ServeAuth::Unauthenticated => {}
	}
	// Its default is `~/.ssh/authorized_keys`, which would let in every key the user trusts.
	if options.protocol == ServeProtocol::Sftp {
		set("authorized_keys", String::new());
	}
	if let Some(tls) = &options.tls {
		set("cert", tls.cert_path.clone());
		set("key", tls.key_path.clone());
		set("min_tls_version", MIN_TLS_VERSION.to_owned());
	}
	Value::Object(params)
}

impl RcloneDaemon {
	/// Serves the drive as `options` say, once it has checked that a request without credentials
	/// is refused (webdav, s3, ftp, http, restic; sftp's authentication is fixed by its options and
	/// nfs has none).
	///
	/// Refused with an [`RcloneRefusal`]: on an address that is not an IP address or `localhost`
	/// and a port ([`RcloneRefusal::ImplicitAddress`]) or not this machine's
	/// ([`RcloneRefusal::AddressNotLocal`]), on a port in use ([`RcloneRefusal::PortInUse`]), on
	/// missing or invalid credentials ([`RcloneRefusal::AuthRequired`],
	/// [`RcloneRefusal::InvalidCredentials`]), on nfs off loopback
	/// ([`RcloneRefusal::UnauthenticatedOffLoopback`]), on TLS for a protocol without it
	/// ([`RcloneRefusal::TlsUnsupported`]), and on a protocol not served here
	/// ([`RcloneRefusal::UnsupportedProtocol`]). A serve that answers an unauthenticated request is
	/// stopped and refused with [`RcloneRefusal::AuthRequired`].
	pub async fn start_serve(&self, options: ServeOptions) -> Result<ServeInfo, Error> {
		let listen = check(&options)?;
		if options.protocol == ServeProtocol::Restic && options.read_only {
			return Err(Error::custom(
				ErrorKind::InvalidState,
				"a restic serve cannot be read-only: it writes to the remote without a VFS",
			));
		}
		// Keeps a stop out until the serve is up or stopped again.
		let guard = self.guard.lock().await;
		if guard.is_none() {
			return Err(stopped());
		}
		if !self.serve_types().await?.contains(&options.protocol) {
			return Err(RcloneRefusal::UnsupportedProtocol.into());
		}
		ensure_bindable(listen.socket)?;
		let ServeStarted { id, addr } = self
			.rc
			.serve_start(&serve_params(&options, &listen.addr, &self.vfs))
			.await
			.map_err(|e| bind_refusal(e, listen.socket.port()))?;
		let answered = match options.protocol {
			ServeProtocol::Webdav
			| ServeProtocol::S3
			| ServeProtocol::Http
			| ServeProtocol::Restic => {
				http_answers_anonymously(listen.probe_target(), options.tls.is_some()).await
			}
			ServeProtocol::Ftp => ftp_answers_anonymously(listen.probe_target()).await,
			ServeProtocol::Sftp | ServeProtocol::Nfs => Ok(false),
		};
		let refusal = match answered {
			Ok(false) => {
				return Ok(ServeInfo {
					id,
					protocol: options.protocol,
					addr,
				});
			}
			Ok(true) => Error::from(RcloneRefusal::AuthRequired)
				.with_context(format!("{addr} answered a request without credentials")),
			Err(e) => e.with_context(format!(
				"failed to check that {addr} refuses a request without credentials"
			)),
		};
		if let Err(e) = self.rc.serve_stop(&id).await {
			warn!("stopping the serve on {addr} after it failed its check failed: {e}");
		}
		Err(refusal)
	}

	/// Stops the serve with `id`.
	pub async fn stop_serve(&self, id: &str) -> Result<(), Error> {
		self.rc.serve_stop(id).await
	}

	/// The protocols this rclone serves here.
	pub async fn serve_types(&self) -> Result<Vec<ServeProtocol>, Error> {
		Ok(self
			.rc
			.serve_types()
			.await?
			.types
			.iter()
			.filter_map(|kind| ServeProtocol::from_rc_type(kind))
			.collect())
	}
}

/// Refuses an address another process listens on, or one that is not this machine's.
fn ensure_bindable(socket: SocketAddr) -> Result<(), Error> {
	match TcpListener::bind(socket) {
		Ok(_) => Ok(()),
		Err(e) if e.kind() == io::ErrorKind::AddrInUse => Err(RcloneRefusal::PortInUse {
			port: socket.port(),
		}
		.into()),
		Err(e) if e.kind() == io::ErrorKind::AddrNotAvailable => {
			Err(RcloneRefusal::AddressNotLocal.into())
		}
		Err(e) => Err(Error::from(e).with_context(format!("failed to bind {socket}"))),
	}
}

/// `error` from `serve/start`, as the refusal it is when rclone could not bind `port`: another
/// process took it since it was found free, or the address is not this machine's.
fn bind_refusal(error: Error, port: u16) -> Error {
	let Some(failed) = error.downcast_ref::<RcCallFailed>() else {
		return error;
	};
	let message = failed.message.to_ascii_lowercase();
	// Go's messages on Unix and Windows.
	if message.contains("address already in use")
		|| message.contains("only one usage of each socket address")
	{
		RcloneRefusal::PortInUse { port }.into()
	} else if message.contains("assign requested address")
		|| message.contains("requested address is not valid in its context")
	{
		RcloneRefusal::AddressNotLocal.into()
	} else {
		error
	}
}

/// Whether the http-based serve at `target` answers `GET /` without credentials with anything but
/// a client error. With `tls`, over https, accepting any certificate: nothing secret is sent.
async fn http_answers_anonymously(target: SocketAddr, tls: bool) -> Result<bool, Error> {
	let http = reqwest::Client::builder()
		.no_proxy()
		.danger_accept_invalid_certs(true)
		.redirect(reqwest::redirect::Policy::none())
		.timeout(PROBE_TIMEOUT)
		.build()
		.map_err(|e| {
			Error::custom_with_source(
				ErrorKind::Internal,
				e,
				Some("failed to build the serve check's client"),
			)
		})?;
	let scheme = if tls { "https" } else { "http" };
	let status = http
		.get(format!("{scheme}://{target}/"))
		.send()
		.await?
		.status();
	Ok(!status.is_client_error())
}

/// Whether the FTP serve at `target` logs in `anonymous`.
async fn ftp_answers_anonymously(target: SocketAddr) -> Result<bool, Error> {
	let login = async {
		let stream = TcpStream::connect(target).await?;
		let (read, mut write) = stream.into_split();
		let mut read = BufReader::new(read);
		let greeting = ftp_reply(&mut read).await?;
		if greeting != 220 {
			return Err(Error::custom(
				ErrorKind::IO,
				format!("the FTP serve greeted with {greeting}"),
			));
		}
		for command in [&b"USER anonymous\r\n"[..], b"PASS anonymous\r\n"] {
			write.write_all(command).await?;
			match ftp_reply(&mut read).await? {
				230 => return Ok(true),
				331 => {}
				_ => return Ok(false),
			}
		}
		Ok(false)
	};
	tokio::time::timeout(PROBE_TIMEOUT, login)
		.await
		.map_err(|_| {
			Error::custom(
				ErrorKind::IO,
				format!("the FTP serve did not answer within {PROBE_TIMEOUT:?}"),
			)
		})?
}

/// The code of the FTP reply `read` gives next, reading its lines up to the last one
/// (`<code> <text>`).
async fn ftp_reply(read: &mut (impl AsyncBufRead + Unpin)) -> Result<u16, Error> {
	let mut line = String::new();
	for _ in 0..FTP_REPLY_LINES {
		line.clear();
		if read.read_line(&mut line).await? == 0 {
			return Err(Error::custom(
				ErrorKind::IO,
				"the FTP serve closed the connection",
			));
		}
		let code = line.get(..3).and_then(|code| code.parse::<u16>().ok());
		if let Some(code) = code
			&& line.get(3..4) != Some("-")
		{
			return Ok(code);
		}
	}
	Err(Error::custom(
		ErrorKind::IO,
		format!("the FTP serve's reply ran past {FTP_REPLY_LINES} lines"),
	))
}

#[cfg(test)]
mod tests {
	use serde_json::json;

	use super::*;

	fn basic() -> ServeAuth {
		ServeAuth::Basic {
			user: "filen-0a1b2c3d".to_owned(),
			password: "correct-horse-battery".to_owned(),
		}
	}

	fn s3_keys() -> ServeAuth {
		ServeAuth::S3 {
			access_key_id: "AKIA0123456789ABCDEF".to_owned(),
			secret_access_key: "c2VjcmV0LXNlY3JldC1zZWNyZXQtc2VjcmV0LXNl".to_owned(),
		}
	}

	fn options(protocol: ServeProtocol, auth: ServeAuth) -> ServeOptions {
		ServeOptions {
			protocol,
			host: "127.0.0.1".to_owned(),
			port: 1900,
			auth,
			tls: None,
			read_only: false,
		}
	}

	fn refusal(options: &ServeOptions) -> RcloneRefusal {
		check(options).unwrap_err()
	}

	#[test]
	fn an_authenticated_serve_on_loopback_passes() {
		assert_eq!(
			check(&options(ServeProtocol::Webdav, basic())),
			Ok(Listen {
				socket: "127.0.0.1:1900".parse::<SocketAddr>().unwrap(),
				addr: "127.0.0.1:1900".to_owned(),
			})
		);
	}

	#[test]
	fn localhost_listens_on_ipv4_loopback() {
		let listen = check(&ServeOptions {
			host: "localhost".to_owned(),
			..options(ServeProtocol::Http, basic())
		})
		.unwrap();

		assert_eq!(listen.addr, "localhost:1900");
		assert_eq!(
			listen.socket,
			"127.0.0.1:1900".parse::<SocketAddr>().unwrap()
		);
	}

	#[test]
	fn an_ipv6_address_is_bracketed_for_rclone() {
		let listen = check(&ServeOptions {
			host: "::1".to_owned(),
			..options(ServeProtocol::Http, basic())
		})
		.unwrap();

		assert_eq!(listen.addr, "[::1]:1900");
	}

	#[test]
	fn implicit_all_interfaces_address_refused() {
		for host in ["", ":1900", "example.com", "[::1]"] {
			let options = ServeOptions {
				host: host.to_owned(),
				..options(ServeProtocol::Webdav, basic())
			};
			assert_eq!(refusal(&options), RcloneRefusal::ImplicitAddress, "{host}");
		}
		let port_zero = ServeOptions {
			port: 0,
			..options(ServeProtocol::Webdav, basic())
		};
		assert_eq!(refusal(&port_zero), RcloneRefusal::ImplicitAddress);
	}

	#[test]
	fn every_interface_is_allowed_when_named() {
		let all = ServeOptions {
			host: "0.0.0.0".to_owned(),
			..options(ServeProtocol::Webdav, basic())
		};

		let listen = check(&all).unwrap();

		assert_eq!(
			listen.probe_target(),
			"127.0.0.1:1900".parse::<SocketAddr>().unwrap()
		);
	}

	#[test]
	fn s3_without_a_key_is_refused() {
		assert_eq!(
			refusal(&options(ServeProtocol::S3, ServeAuth::Unauthenticated)),
			RcloneRefusal::AuthRequired
		);
		assert_eq!(
			refusal(&options(ServeProtocol::S3, basic())),
			RcloneRefusal::AuthRequired
		);
	}

	#[test]
	fn every_protocol_but_nfs_requires_credentials() {
		for protocol in [
			ServeProtocol::Webdav,
			ServeProtocol::Ftp,
			ServeProtocol::Sftp,
			ServeProtocol::Http,
			ServeProtocol::Restic,
		] {
			assert!(protocol.requires_auth());
			assert_eq!(
				refusal(&options(protocol, ServeAuth::Unauthenticated)),
				RcloneRefusal::AuthRequired,
				"{protocol:?}"
			);
			assert_eq!(
				refusal(&options(protocol, s3_keys())),
				RcloneRefusal::AuthRequired,
				"{protocol:?}"
			);
		}
		assert!(!ServeProtocol::Nfs.requires_auth());
	}

	#[cfg(not(windows))]
	#[test]
	fn unauthenticated_protocol_refused_off_loopback() {
		for host in ["0.0.0.0", "192.168.1.20", "::"] {
			let options = ServeOptions {
				host: host.to_owned(),
				..options(ServeProtocol::Nfs, ServeAuth::Unauthenticated)
			};
			assert_eq!(
				refusal(&options),
				RcloneRefusal::UnauthenticatedOffLoopback,
				"{host}"
			);
		}
		assert!(check(&options(ServeProtocol::Nfs, ServeAuth::Unauthenticated)).is_ok());
		assert_eq!(
			refusal(&options(ServeProtocol::Nfs, basic())),
			RcloneRefusal::InvalidCredentials
		);
	}

	#[cfg(windows)]
	#[test]
	fn nfs_is_not_served_on_windows() {
		assert_eq!(
			refusal(&options(ServeProtocol::Nfs, ServeAuth::Unauthenticated)),
			RcloneRefusal::UnsupportedProtocol
		);
	}

	#[test]
	fn invalid_credentials_are_refused() {
		let long = "a".repeat(MAX_CREDENTIAL_LEN + 1);
		for (user, password) in [
			("", "password"),
			("user", ""),
			("us:er", "password"),
			("user", "pass\nword"),
			("us\0er", "password"),
			("user", long.as_str()),
		] {
			let auth = ServeAuth::Basic {
				user: user.to_owned(),
				password: password.to_owned(),
			};
			assert_eq!(
				refusal(&options(ServeProtocol::Ftp, auth)),
				RcloneRefusal::InvalidCredentials,
				"{user:?} {password:?}"
			);
		}
		let longest = ServeAuth::Basic {
			user: "user".to_owned(),
			password: "a".repeat(MAX_CREDENTIAL_LEN),
		};
		assert!(check(&options(ServeProtocol::Ftp, longest)).is_ok());
		let colon_in_password = ServeAuth::Basic {
			user: "user".to_owned(),
			password: "pass:word".to_owned(),
		};
		assert!(check(&options(ServeProtocol::Ftp, colon_in_password)).is_ok());
	}

	#[test]
	fn invalid_s3_keys_are_refused() {
		let key_id = |id: &str, secret: &str| ServeAuth::S3 {
			access_key_id: id.to_owned(),
			secret_access_key: secret.to_owned(),
		};
		for auth in [
			key_id("AKIA0123456789A", "secret"),
			key_id(&"A".repeat(129), "secret"),
			key_id("AKIA0123456789ABCDE-", "secret"),
			key_id("AKIA0123456789ABCDEF", ""),
			key_id("AKIA0123456789ABCDEF", "sec,ret"),
			key_id("AKIA0123456789ABCDEF", "sec\"ret"),
		] {
			assert_eq!(
				refusal(&options(ServeProtocol::S3, auth.clone())),
				RcloneRefusal::InvalidCredentials,
				"{auth:?}"
			);
		}
		assert!(check(&options(ServeProtocol::S3, key_id(&"A".repeat(16), "s"))).is_ok());
		assert!(check(&options(ServeProtocol::S3, key_id(&"A".repeat(128), "s"))).is_ok());
	}

	#[test]
	fn tls_is_refused_where_the_protocol_has_none() {
		let tls = Some(ServeTls {
			cert_path: "/certs/serve.crt".to_owned(),
			key_path: "/certs/serve.key".to_owned(),
		});
		for protocol in [ServeProtocol::Ftp, ServeProtocol::Sftp] {
			let options = ServeOptions {
				tls: tls.clone(),
				..options(protocol, basic())
			};
			assert_eq!(
				refusal(&options),
				RcloneRefusal::TlsUnsupported,
				"{protocol:?}"
			);
		}
		let https = ServeOptions {
			tls,
			..options(ServeProtocol::Webdav, basic())
		};
		assert!(check(&https).is_ok());
	}

	#[test]
	fn webdav_params_carry_its_credentials_and_the_shared_vfs_options() {
		let options = options(ServeProtocol::Webdav, basic());

		let params = serve_params(&options, "127.0.0.1:1900", &VfsTuning::default());

		let mut expected = vfs_flags(&VfsTuning::default(), false);
		for (key, value) in [
			("type", "webdav"),
			("fs", "Filen:"),
			("addr", "127.0.0.1:1900"),
			("user", "filen-0a1b2c3d"),
			("pass", "correct-horse-battery"),
		] {
			expected.insert(key.to_owned(), json!(value));
		}
		assert_eq!(params, Value::Object(expected));
	}

	#[test]
	fn a_read_only_serve_uses_the_read_only_remote() {
		let options = ServeOptions {
			read_only: true,
			..options(ServeProtocol::Http, basic())
		};

		let params = serve_params(&options, "127.0.0.1:1900", &VfsTuning::default());

		assert_eq!(params["fs"], json!("FilenRO:"));
		assert_eq!(params["read_only"], json!("true"));
	}

	#[test]
	fn s3_params_quote_the_key_pair_as_one_entry() {
		let params = serve_params(
			&options(ServeProtocol::S3, s3_keys()),
			"127.0.0.1:1800",
			&VfsTuning::default(),
		);

		assert_eq!(
			params["auth_key"],
			json!("\"AKIA0123456789ABCDEF,c2VjcmV0LXNlY3JldC1zZWNyZXQtc2VjcmV0LXNl\"")
		);
		assert_eq!(params.get("user"), None);
		assert_eq!(params.get("pass"), None);
	}

	#[test]
	fn sftp_never_trusts_authorized_keys() {
		let params = serve_params(
			&options(ServeProtocol::Sftp, basic()),
			"127.0.0.1:2022",
			&VfsTuning::default(),
		);

		assert_eq!(params["authorized_keys"], json!(""));
		assert_eq!(params.get("no_auth"), None);
	}

	#[test]
	fn restic_params_have_no_vfs_options() {
		let params = serve_params(
			&options(ServeProtocol::Restic, basic()),
			"127.0.0.1:1600",
			&VfsTuning::default(),
		);

		assert_eq!(
			params,
			json!({
				"type": "restic",
				"fs": "Filen:",
				"addr": "127.0.0.1:1600",
				"user": "filen-0a1b2c3d",
				"pass": "correct-horse-battery",
			})
		);
	}

	#[test]
	fn https_params_name_the_certificate_and_tls_1_2() {
		let options = ServeOptions {
			tls: Some(ServeTls {
				cert_path: "/certs/serve.crt".to_owned(),
				key_path: "/certs/serve.key".to_owned(),
			}),
			..options(ServeProtocol::Webdav, basic())
		};

		let params = serve_params(&options, "127.0.0.1:1900", &VfsTuning::default());

		assert_eq!(params["cert"], json!("/certs/serve.crt"));
		assert_eq!(params["key"], json!("/certs/serve.key"));
		assert_eq!(params["min_tls_version"], json!("tls1.2"));
	}

	#[test]
	fn debug_never_prints_a_password_or_secret() {
		let printed = format!("{:?} {:?}", basic(), s3_keys());

		assert_eq!(
			printed,
			r#"Basic { user: "filen-0a1b2c3d", .. } S3 { access_key_id: "AKIA0123456789ABCDEF", .. }"#
		);
	}

	#[test]
	fn a_port_in_use_is_refused_before_rclone_sees_it() {
		let taken = TcpListener::bind("127.0.0.1:0").unwrap();
		let socket = taken.local_addr().unwrap();

		let error = ensure_bindable(socket).unwrap_err();

		assert_eq!(
			error.downcast_ref::<RcloneRefusal>(),
			Some(&RcloneRefusal::PortInUse {
				port: socket.port()
			})
		);
	}

	#[test]
	fn rclone_bind_failures_become_refusals() {
		let failed = |message: &str| {
			Error::custom_with_source(
				ErrorKind::IO,
				RcCallFailed {
					path: "serve/start",
					status: 500,
					message: message.to_owned(),
				},
				None::<&str>,
			)
		};

		let in_use = bind_refusal(
			failed(
				"could not start serve \"webdav\": failed to init server: listen tcp 127.0.0.1:1900: bind: address already in use",
			),
			1900,
		);
		let not_local = bind_refusal(
			failed("listen tcp 10.9.9.9:1900: bind: can't assign requested address"),
			1900,
		);
		let other = bind_refusal(failed("could not start serve"), 1900);

		assert_eq!(
			in_use.downcast_ref::<RcloneRefusal>(),
			Some(&RcloneRefusal::PortInUse { port: 1900 })
		);
		assert_eq!(
			not_local.downcast_ref::<RcloneRefusal>(),
			Some(&RcloneRefusal::AddressNotLocal)
		);
		assert_eq!(other.downcast_ref::<RcloneRefusal>(), None);
		assert!(other.downcast_ref::<RcCallFailed>().is_some());
	}

	#[tokio::test]
	async fn ftp_replies_are_read_to_their_last_line() {
		let mut multi = &b"220-Welcome\r\n220-to the serve\r\n220 Ready\r\n"[..];
		let mut single = &b"530 Incorrect password, not logged in\r\n"[..];
		let mut closed = &b"220-Welcome\r\n"[..];

		assert_eq!(ftp_reply(&mut multi).await.unwrap(), 220);
		assert_eq!(ftp_reply(&mut single).await.unwrap(), 530);
		assert_eq!(
			ftp_reply(&mut closed).await.unwrap_err().kind(),
			ErrorKind::IO
		);
	}

	#[test]
	fn serve_types_drop_what_the_daemon_does_not_serve() {
		assert_eq!(ServeProtocol::from_rc_type("dlna"), None);
		assert_eq!(
			ServeProtocol::from_rc_type("webdav"),
			Some(ServeProtocol::Webdav)
		);
		assert_eq!(
			ServeProtocol::from_rc_type("nfs"),
			cfg!(not(windows)).then_some(ServeProtocol::Nfs)
		);
	}
}

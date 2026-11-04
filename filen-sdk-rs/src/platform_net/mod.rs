//! TLS and proxies on the desktop targets (Windows, macOS, Linux). Requests and the websocket
//! check the server's certificate with the OS's verifier (`rustls-platform-verifier`: its trust
//! store, the roots an administrator added, revocation) and go through the proxy the system is
//! set to use, as other apps on the machine do. iOS, Android and wasm keep the bundled webpki
//! roots and connect directly.

use std::{
	sync::{Arc, OnceLock},
	time::Duration,
};

use http::Uri;
use hyper_util::client::{
	legacy::connect::{
		HttpConnector,
		proxy::{SocksV5, Tunnel},
	},
	proxy::matcher::{Intercept, Matcher},
};
use rustls::{
	CertificateError, DigitallySignedStruct, DistinguishedName, OtherError, SignatureScheme,
	client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
	crypto::CryptoProvider,
	pki_types::{CertificateDer, ServerName, UnixTime},
};
use rustls_platform_verifier::Verifier;
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;
use tokio_tungstenite::{
	Connector, MaybeTlsStream, WebSocketStream, tungstenite::client::IntoClientRequest,
};
use tower::ServiceExt;

use crate::{Error, ErrorKind};

#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
mod tests;

/// The TLS configuration both reqwest and the websocket use: the platform verifier, HTTP/1.1
/// over ALPN (reqwest is built without HTTP/2, and a websocket upgrade is HTTP/1.1). Built once
/// per client. `None` when the platform verifier cannot be built, which happens only on Linux
/// with no CA certificate in the system store: the client then keeps the bundled webpki roots,
/// as before, rather than refuse every connection.
pub(crate) fn platform_tls_config() -> Option<rustls::ClientConfig> {
	// reqwest's own choice: a process-wide default provider if the host installed one, else ring.
	let provider = CryptoProvider::get_default()
		.cloned()
		.unwrap_or_else(|| Arc::new(rustls::crypto::ring::default_provider()));
	let built = rustls::ClientConfig::builder_with_provider(provider)
		.with_safe_default_protocol_versions()
		.and_then(|builder| {
			let verifier = Verifier::new(builder.crypto_provider().clone())?;
			Ok(builder
				.dangerous()
				.with_custom_certificate_verifier(Arc::new(DistrustingVerifier::new(
					OffloadingVerifier(verifier),
				))))
		});
	match built {
		Ok(builder) => {
			let mut config = builder.with_no_client_auth();
			config.alpn_protocols = vec![b"http/1.1".to_vec()];
			Some(config)
		}
		Err(e) => {
			tracing::warn!(
				"platform certificate verifier unavailable ({e}), using the bundled webpki roots"
			);
			None
		}
	}
}

/// SHA-256 of each public key rustls-webpki 0.103.16 distrusts (`DISTRUSTED_SPKIS` in its
/// `src/verify_cert.rs`, the files under `src/data/distrusted/`): the SubjectPublicKeyInfo
/// contents, without the SEQUENCE's tag and length, of the eMudhra intermediates under emSign
/// ECC Root CA - G3, which their cross-signs under Sectigo's AAA Certificate Services share. The
/// crate keeps the list to itself; `the_distrust_list_mirrors_rustls_webpki` fails until a bump
/// that changes it is mirrored here.
const DISTRUSTED_SPKI_SHA256: [[u8; 32]; 8] = [
	// EMDVTLSCAG3B
	hex32("2d5f988ee3ceaf696845a34299ca3e05816429cdb5de92427889533175f97b95"),
	// EMDVTLSCAG3C
	hex32("616e9a92539c40f454813f7fb8ae4a6db719498495215774f02d8b8b5a615d36"),
	// EMEVTLSCAG3B
	hex32("bdf5ab18303069ebc3e3ddbb0fc0613693acef01b2fb7be1a0624aab60fbff0d"),
	// EMEVTLSCAG3C
	hex32("b941c7577b918f7b9a9374aff3b02d8eda92545c86adb5b5268d1e121fe58ce2"),
	// EMOVTLSCAG3B
	hex32("7f09de2d155cc9fbc2dc4585f2c134ea3fc04ea9d9b3e31fa2d6bf9fdc0a02a2"),
	// EMOVTLSCAG3C
	hex32("6fc13706463e00b2cd2ae357f86de2441e59d501e322c7a02a531a9871e450f6"),
	// EMOVCSCAG3B
	hex32("e8263cea915a92109a29fb497105dceeeb58fa099c2552c4082aff1e1a9a33b2"),
	// EMOVCSCAG3C
	hex32("09f75d128061fcdc6d31d8317ce3fe0aea4dab2c66777ce548858dd23f763ce7"),
];

/// 64 hex digits as 32 bytes, at compile time.
const fn hex32(hex: &str) -> [u8; 32] {
	const fn digit(c: u8) -> u8 {
		match c {
			b'0'..=b'9' => c - b'0',
			b'a'..=b'f' => c - b'a' + 10,
			_ => panic!("not a lowercase hex digit"),
		}
	}
	let hex = hex.as_bytes();
	assert!(hex.len() == 64, "not 32 bytes of hex");
	let mut bytes = [0u8; 32];
	let mut i = 0;
	while i < 32 {
		bytes[i] = (digit(hex[2 * i]) << 4) | digit(hex[2 * i + 1]);
		i += 1;
	}
	bytes
}

/// The OS's verifier, with rustls-webpki's distrust of compromised CA keys on top: on macOS and
/// Windows the platform verifier hands the whole check to the OS, which accepts such a key while
/// its own blocklist lacks it (an OS whose root and disallowed-certificate updates are off). A
/// chain the OS accepts is rejected when any certificate it presents (the end entity and the
/// intermediates) has a public key on [`DISTRUSTED_SPKI_SHA256`], with the error a rustls-webpki
/// rejection gives, so it reads as a certificate problem rather than as offline. The OS checks
/// first, so a chain it refuses fails as it did; the keys of a chain it accepts are hashed once
/// per handshake, and nothing is checked per request.
#[derive(Debug)]
struct DistrustingVerifier<V> {
	inner: V,
	/// [`DISTRUSTED_SPKI_SHA256`], or a test's own.
	distrusted: &'static [[u8; 32]],
}

impl<V> DistrustingVerifier<V> {
	fn new(inner: V) -> Self {
		Self {
			inner,
			distrusted: &DISTRUSTED_SPKI_SHA256,
		}
	}

	/// Whether `cert`'s public key is distrusted. A certificate that cannot be parsed has none:
	/// the OS accepted the chain, and only the keys are checked here.
	fn distrusts(&self, cert: &CertificateDer<'_>) -> bool {
		let Ok(parsed) = webpki::EndEntityCert::try_from(cert) else {
			return false;
		};
		let spki = parsed.subject_public_key_info();
		let digest: [u8; 32] = Sha256::digest(sequence_contents(spki.as_ref())).into();
		self.distrusted.contains(&digest)
	}
}

impl<V: ServerCertVerifier> ServerCertVerifier for DistrustingVerifier<V> {
	fn verify_server_cert(
		&self,
		end_entity: &CertificateDer<'_>,
		intermediates: &[CertificateDer<'_>],
		server_name: &ServerName<'_>,
		ocsp_response: &[u8],
		now: UnixTime,
	) -> Result<ServerCertVerified, rustls::Error> {
		let verified = self.inner.verify_server_cert(
			end_entity,
			intermediates,
			server_name,
			ocsp_response,
			now,
		)?;
		if std::iter::once(end_entity)
			.chain(intermediates)
			.any(|cert| self.distrusts(cert))
		{
			return Err(rustls::Error::InvalidCertificate(CertificateError::Other(
				OtherError(Arc::new(webpki::Error::CertDistrusted)),
			)));
		}
		Ok(verified)
	}

	fn verify_tls12_signature(
		&self,
		message: &[u8],
		cert: &CertificateDer<'_>,
		dss: &DigitallySignedStruct,
	) -> Result<HandshakeSignatureValid, rustls::Error> {
		self.inner.verify_tls12_signature(message, cert, dss)
	}

	fn verify_tls13_signature(
		&self,
		message: &[u8],
		cert: &CertificateDer<'_>,
		dss: &DigitallySignedStruct,
	) -> Result<HandshakeSignatureValid, rustls::Error> {
		self.inner.verify_tls13_signature(message, cert, dss)
	}

	fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
		self.inner.supported_verify_schemes()
	}

	fn requires_raw_public_keys(&self) -> bool {
		self.inner.requires_raw_public_keys()
	}

	fn root_hint_subjects(&self) -> Option<&[DistinguishedName]> {
		self.inner.root_hint_subjects()
	}
}

/// The contents of the DER SEQUENCE `der`, without its tag and length (a short form, or a long
/// one of one or two bytes, which covers every public key): what rustls-webpki keeps of a
/// SubjectPublicKeyInfo, and so what its distrust list holds.
fn sequence_contents(der: &[u8]) -> &[u8] {
	match der {
		[0x30, 0x00..=0x7f, contents @ ..]
		| [0x30, 0x81, _, contents @ ..]
		| [0x30, 0x82, _, _, contents @ ..] => contents,
		_ => der,
	}
}

/// Runs the certificate check off the async runtime's workers. The OS's check is a blocking
/// call (`SecTrustEvaluateWithError` on macOS, `CertGetCertificateChain` on Windows) that may
/// fetch OCSP/CRL responses, for up to 10 s on Windows and with no limit on macOS, and rustls
/// makes it from inside the TLS stream's poll. On a multi-thread runtime `block_in_place` hands
/// the worker's other tasks to another thread for that time; elsewhere (no runtime, or a
/// current-thread one, where it would panic) the check runs inline, as before.
#[derive(Debug)]
struct OffloadingVerifier<V>(V);

impl<V: ServerCertVerifier> ServerCertVerifier for OffloadingVerifier<V> {
	fn verify_server_cert(
		&self,
		end_entity: &CertificateDer<'_>,
		intermediates: &[CertificateDer<'_>],
		server_name: &ServerName<'_>,
		ocsp_response: &[u8],
		now: UnixTime,
	) -> Result<ServerCertVerified, rustls::Error> {
		let verify = || {
			self.0
				.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
		};
		match tokio::runtime::Handle::try_current().map(|handle| handle.runtime_flavor()) {
			Ok(tokio::runtime::RuntimeFlavor::MultiThread) => tokio::task::block_in_place(verify),
			_ => verify(),
		}
	}

	fn verify_tls12_signature(
		&self,
		message: &[u8],
		cert: &CertificateDer<'_>,
		dss: &DigitallySignedStruct,
	) -> Result<HandshakeSignatureValid, rustls::Error> {
		self.0.verify_tls12_signature(message, cert, dss)
	}

	fn verify_tls13_signature(
		&self,
		message: &[u8],
		cert: &CertificateDer<'_>,
		dss: &DigitallySignedStruct,
	) -> Result<HandshakeSignatureValid, rustls::Error> {
		self.0.verify_tls13_signature(message, cert, dss)
	}

	fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
		self.0.supported_verify_schemes()
	}

	fn requires_raw_public_keys(&self) -> bool {
		self.0.requires_raw_public_keys()
	}

	fn root_hint_subjects(&self) -> Option<&[DistinguishedName]> {
		self.0.root_hint_subjects()
	}
}

/// How the websocket reaches the server: the client's TLS configuration, the proxy settings,
/// and the connect timeout for a proxy. Shared by every client cloned from the one that built
/// it.
pub(crate) struct SocketTransport {
	tls: Option<Arc<rustls::ClientConfig>>,
	/// Read from the environment and the system settings on the first connection, as reqwest
	/// reads them when it builds its client: a client that never opens the socket never reads
	/// them.
	proxy: OnceLock<Matcher>,
	connect_timeout: Option<Duration>,
}

impl SocketTransport {
	pub(crate) fn new(
		tls: Option<rustls::ClientConfig>,
		connect_timeout: Option<Duration>,
	) -> Self {
		Self {
			tls: tls.map(Arc::new),
			proxy: OnceLock::new(),
			connect_timeout,
		}
	}

	#[cfg(test)]
	fn with_proxy(
		tls: Option<rustls::ClientConfig>,
		proxy: Matcher,
		connect_timeout: Option<Duration>,
	) -> Self {
		let transport = Self::new(tls, connect_timeout);
		let _ = transport.proxy.set(proxy);
		transport
	}

	/// How a connection to the websocket at `url` (`wss://`) goes: through the system's proxy
	/// unless it exempts the host, or directly.
	pub(crate) fn route(&self, url: &str) -> Result<SocketRoute, Error> {
		let request = url.into_client_request().map_err(ws_error)?;
		let target = proxy_target(request.uri())?;
		let proxy = self.proxy_for(&target);
		Ok(SocketRoute {
			request,
			target,
			proxy,
		})
	}

	/// Opens the websocket `route` leads to. Unbounded itself: the caller's deadline covers the
	/// whole setup.
	pub(crate) async fn connect(
		&self,
		route: SocketRoute,
		config: tungstenite::protocol::WebSocketConfig,
	) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, Error> {
		let SocketRoute {
			request,
			target,
			proxy,
		} = route;
		let connector = self.tls.clone().map(Connector::Rustls);
		let (stream, _) = match proxy {
			None => tokio_tungstenite::connect_async_tls_with_config(
				request,
				Some(config),
				false,
				connector,
			)
			.await
			.map_err(ws_error)?,
			Some(proxy) => {
				let tunnel = self.through(proxy, target).await?;
				tokio_tungstenite::client_async_tls_with_config(
					request,
					tunnel,
					Some(config),
					connector,
				)
				.await
				.map_err(ws_error)?
			}
		};
		Ok(stream)
	}

	/// The proxy for `target`, `None` to connect directly (no proxy set, or `NO_PROXY` and the
	/// system's exceptions name the host).
	fn proxy_for(&self, target: &Uri) -> Option<Intercept> {
		self.proxy
			.get_or_init(Matcher::from_system)
			.intercept(target)
	}

	/// A connection to `target` through `proxy`: an HTTP proxy's CONNECT tunnel, or a SOCKS5
	/// one, which resolves the host itself for `socks5h` and is handed an address resolved here
	/// for `socks5`, as reqwest does for its requests.
	async fn through(&self, proxy: Intercept, target: Uri) -> Result<TcpStream, Error> {
		match proxy.uri().scheme_str() {
			Some("http") => self.tunnel(proxy, target).await,
			Some("socks5") => self.socks5(proxy, resolved(target).await?).await,
			Some("socks5h") => self.socks5(proxy, target).await,
			// A TLS connection to the proxy would put TLS inside TLS, and SOCKS4 is left out as
			// reqwest's own requests do not need it either: neither is supported, and the socket
			// is not sent around the proxy.
			scheme => Err(Error::custom(
				ErrorKind::Server,
				format!(
					"the websocket cannot use a {} proxy, only http, socks5 and socks5h",
					scheme.unwrap_or("schemeless")
				),
			)),
		}
	}

	/// The connector a proxy is reached with: any scheme, as a SOCKS proxy's URL is not `http`.
	fn proxy_connector(&self) -> HttpConnector {
		let mut connector = HttpConnector::new();
		connector.enforce_http(false);
		connector.set_connect_timeout(self.connect_timeout);
		connector
	}

	/// A connection to `target` through the SOCKS5 proxy `proxy`, with the proxy URL's
	/// credentials when it carries them.
	async fn socks5(&self, proxy: Intercept, target: Uri) -> Result<TcpStream, Error> {
		let mut socks = SocksV5::new(proxy.uri().clone(), self.proxy_connector());
		if let Some((user, password)) = proxy.raw_auth() {
			socks = socks.with_auth(user.to_owned(), password.to_owned());
		}
		let io = socks.oneshot(target).await.map_err(|e| {
			Error::custom_with_source(
				ErrorKind::Server,
				e,
				Some("failed to connect to the websocket through the SOCKS5 proxy"),
			)
		})?;
		Ok(io.into_inner())
	}

	/// A connection to `target` through an HTTP proxy's CONNECT tunnel, with the proxy's
	/// credentials when its URL carries them.
	async fn tunnel(&self, proxy: Intercept, target: Uri) -> Result<TcpStream, Error> {
		let mut tunnel = Tunnel::new(proxy.uri().clone(), self.proxy_connector());
		if let Some(auth) = proxy.basic_auth() {
			tunnel = tunnel.with_auth(auth.clone());
		}
		let io = tunnel.oneshot(target).await.map_err(|e| {
			Error::custom_with_source(
				ErrorKind::Server,
				e,
				Some("failed to open the websocket's tunnel through the proxy"),
			)
		})?;
		Ok(io.into_inner())
	}
}

/// Where [`SocketTransport::connect`] opens the websocket: its request, and the proxy it goes
/// through, if any.
pub(crate) struct SocketRoute {
	request: tungstenite::handshake::client::Request,
	/// The websocket's `https` origin, which a proxy connects to.
	target: Uri,
	proxy: Option<Intercept>,
}

impl SocketRoute {
	/// Whether the connection goes through a proxy.
	pub(crate) fn proxied(&self) -> bool {
		self.proxy.is_some()
	}
}

/// `target` with its host resolved here, for a `socks5` proxy, which is handed an address. An
/// IPv4 address first: the SOCKS5 connector reads an IPv6 literal in a URI, which keeps its
/// brackets, as a host name. With only IPv6 addresses, the name is handed on for the proxy to
/// resolve, as `socks5h` does.
async fn resolved(target: Uri) -> Result<Uri, Error> {
	let host = target
		.host()
		.ok_or_else(|| Error::custom(ErrorKind::InvalidState, "the websocket URL has no host"))?;
	let port = target.port_u16().unwrap_or(443);
	let addresses = tokio::net::lookup_host((host, port)).await.map_err(|e| {
		Error::custom_with_source(
			ErrorKind::Server,
			e,
			Some("failed to resolve the websocket's host for the SOCKS5 proxy"),
		)
	})?;
	let Some(address) = addresses.into_iter().find(std::net::SocketAddr::is_ipv4) else {
		return Ok(target);
	};
	Uri::builder()
		.scheme("https")
		.authority(address.to_string())
		.path_and_query("/")
		.build()
		.map_err(|e| {
			Error::custom_with_source(ErrorKind::InvalidState, e, Some("invalid websocket URL"))
		})
}

/// The `https` URI a proxy is chosen for (the proxy settings know `http` and `https`, not
/// `wss`): the websocket's host and port.
fn proxy_target(ws: &Uri) -> Result<Uri, Error> {
	let authority = ws
		.authority()
		.ok_or_else(|| Error::custom(ErrorKind::InvalidState, "the websocket URL has no host"))?;
	Uri::builder()
		.scheme("https")
		.authority(authority.clone())
		.path_and_query("/")
		.build()
		.map_err(|e| {
			Error::custom_with_source(ErrorKind::InvalidState, e, Some("invalid websocket URL"))
		})
}

/// The error a websocket connect failed with: [`ErrorKind::TlsVerification`] when the TLS
/// handshake rejected the server's certificate, else [`ErrorKind::Server`].
fn ws_error(e: tungstenite::Error) -> Error {
	let rejected = match &e {
		tungstenite::Error::Tls(tungstenite::error::TlsError::Rustls(tls)) => {
			rejects_certificate(tls)
		}
		other => rejected_certificate(other),
	};
	let kind = if rejected {
		ErrorKind::TlsVerification
	} else {
		ErrorKind::Server
	};
	Error::custom(kind, format!("failed to connect to websocket: {}", e))
}

/// Whether `err` failed because the TLS handshake rejected the server's certificate, or the
/// verifier could not check it ([`rejects_certificate`]). The chain is followed through each
/// `io::Error` by the error it wraps, which its `source` skips: the TLS stream reports the
/// handshake's error inside one.
pub(crate) fn rejected_certificate(err: &(dyn std::error::Error + 'static)) -> bool {
	let mut next = Some(err);
	while let Some(e) = next {
		if let Some(tls) = e.downcast_ref::<rustls::Error>() {
			return rejects_certificate(tls);
		}
		next = match e.downcast_ref::<std::io::Error>() {
			Some(io) => io
				.get_ref()
				.map(|inner| inner as &(dyn std::error::Error + 'static)),
			None => e.source(),
		};
	}
	false
}

/// Whether a client handshake's `error` is the certificate check's: a certificate the verifier
/// rejected, or a general error, which on a client with the ring provider only the verifier
/// gives (the OS could not run the check, the Android verifier is not initialised or its JVM
/// side failed).
fn rejects_certificate(error: &rustls::Error) -> bool {
	matches!(
		error,
		rustls::Error::InvalidCertificate(_) | rustls::Error::General(_)
	)
}

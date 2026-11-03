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
	DigitallySignedStruct, DistinguishedName, SignatureScheme,
	client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
	crypto::CryptoProvider,
	pki_types::{CertificateDer, ServerName, UnixTime},
};
use rustls_platform_verifier::Verifier;
use tokio::net::TcpStream;
use tokio_tungstenite::{
	Connector, MaybeTlsStream, WebSocketStream, tungstenite::client::IntoClientRequest,
};
use tower::ServiceExt;

use crate::{Error, ErrorKind};

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
				.with_custom_certificate_verifier(Arc::new(OffloadingVerifier(verifier))))
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

fn ws_error(e: tungstenite::Error) -> Error {
	Error::custom(
		ErrorKind::Server,
		format!("failed to connect to websocket: {}", e),
	)
}

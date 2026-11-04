//! A local TLS server whose certificate no verifier accepts, for the tests of how a rejected
//! certificate is reported.

use std::sync::Arc;

use rustls::{
	ServerConfig, ServerConnection,
	pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
	server::{ClientHello, ResolvesServerCert},
	sign::CertifiedKey,
};

/// The Ed25519 key the server signs with: a PKCS#8 v1 prefix and a fixed seed.
fn signing_key_pkcs8() -> Vec<u8> {
	let mut der = vec![
		0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
		0x20,
	];
	der.extend_from_slice(&[7; 32]);
	der
}

#[derive(Debug)]
struct Fixed(Arc<CertifiedKey>);

impl ResolvesServerCert for Fixed {
	fn resolve(&self, _: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
		Some(Arc::clone(&self.0))
	}
}

/// Serves `connections` TLS connections on 127.0.0.1, presenting as its certificate a DER
/// SEQUENCE that is no certificate, and returns the port. Each handshake runs until the client
/// gives up on it.
pub(crate) fn untrusted_tls_server(connections: usize) -> u16 {
	let provider = Arc::new(rustls::crypto::ring::default_provider());
	let key = provider
		.key_provider
		.load_private_key(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
			signing_key_pkcs8(),
		)))
		.unwrap();
	let not_a_certificate = CertificateDer::from(vec![0x30, 0x03, 0x02, 0x01, 0x01]);
	let config = Arc::new(
		ServerConfig::builder_with_provider(provider)
			.with_safe_default_protocol_versions()
			.unwrap()
			.with_no_client_auth()
			.with_cert_resolver(Arc::new(Fixed(Arc::new(CertifiedKey::new(
				vec![not_a_certificate],
				key,
			))))),
	);
	let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
	let port = listener.local_addr().unwrap().port();
	std::thread::spawn(move || {
		for _ in 0..connections {
			let Ok((mut tcp, _)) = listener.accept() else {
				return;
			};
			let mut server = ServerConnection::new(Arc::clone(&config)).unwrap();
			while server.is_handshaking() {
				if server.complete_io(&mut tcp).is_err() {
					break;
				}
			}
		}
	});
	port
}

//! Android's certificate check: the platform trust manager judges the chain (against the system
//! and user-installed roots), rustls the rest. The trust manager
//! itself is reached through JNI ([`super::android`]); this is what does not need the JVM.
//!
//! The trust manager also enforces the end entity's extended key usage: Conscrypt's
//! `TrustManagerImpl` validates with its `ExtendedKeyUsagePKIXCertPathChecker`, which requires
//! serverAuth (or anyExtendedKeyUsage, or a legacy SGC usage) when the extension is present.
//!
//! No online revocation check is made, as Chrome on Android makes none: the platform trust manager
//! disables it (`TrustManagerImpl` validates with `setRevocationEnabled(false)`),
//! where rustls-platform-verifier's Kotlin verifier added one that fetched the CA's CRL on the
//! handshake's thread, without a timeout, and refused the certificate as revoked whenever that
//! fetch failed.

use std::borrow::Cow;

use rustls::{
	CertificateError, DigitallySignedStruct, SignatureScheme,
	client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
	crypto::{CryptoProvider, WebPkiSupportedAlgorithms},
	pki_types::{CertificateDer, ServerName, UnixTime},
	server::ParsedCertificate,
};

/// Why the platform trust manager did not accept a chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Refusal {
	/// A certificate the platform could not parse.
	BadEncoding,
	/// The end entity has expired.
	Expired,
	/// The end entity is not valid yet.
	NotValidYet,
	/// The trust manager refused the chain, with its reason.
	Untrusted(String),
	/// The trust manager could not be asked, with the reason.
	Failed(String),
}

impl Refusal {
	fn into_error(self) -> rustls::Error {
		match self {
			Self::BadEncoding => rustls::Error::InvalidCertificate(CertificateError::BadEncoding),
			Self::Expired => rustls::Error::InvalidCertificate(CertificateError::Expired),
			Self::NotValidYet => rustls::Error::InvalidCertificate(CertificateError::NotValidYet),
			Self::Untrusted(reason) => {
				tracing::warn!("the platform trust manager refused the certificate: {reason}");
				rustls::Error::InvalidCertificate(CertificateError::UnknownIssuer)
			}
			Self::Failed(reason) => rustls::Error::General(format!(
				"the platform trust manager could not check the certificate: {reason}"
			)),
		}
	}
}

/// The platform's chain check: whether the chain, end entity first, is trusted for `host`.
pub(super) trait TrustManager: std::fmt::Debug + Send + Sync {
	fn check(
		&self,
		end_entity: &CertificateDer<'_>,
		intermediates: &[CertificateDer<'_>],
		host: &str,
	) -> Result<(), Refusal>;
}

/// A server certificate verifier over a platform [`TrustManager`]: rustls checks the name, the
/// platform the chain, and the crypto provider the handshake's signatures.
#[derive(Debug)]
pub(super) struct TrustManagerVerifier<T> {
	trust: T,
	algorithms: WebPkiSupportedAlgorithms,
}

impl<T> TrustManagerVerifier<T> {
	pub(super) fn new(trust: T, provider: &CryptoProvider) -> Self {
		Self {
			trust,
			algorithms: provider.signature_verification_algorithms,
		}
	}
}

/// The host the trust manager is told (Conscrypt reads it only for pins and Certificate
/// Transparency, not the name check): the name without a trailing dot, or the IP address.
fn host<'a>(server_name: &'a ServerName<'_>) -> Cow<'a, str> {
	match server_name.to_str() {
		Cow::Borrowed(name) => Cow::Borrowed(name.strip_suffix('.').unwrap_or(name)),
		Cow::Owned(name) => match name.strip_suffix('.') {
			Some(stripped) => Cow::Owned(stripped.to_owned()),
			None => Cow::Owned(name),
		},
	}
}

impl<T: TrustManager> ServerCertVerifier for TrustManagerVerifier<T> {
	fn verify_server_cert(
		&self,
		end_entity: &CertificateDer<'_>,
		intermediates: &[CertificateDer<'_>],
		server_name: &ServerName<'_>,
		_ocsp_response: &[u8],
		_now: UnixTime,
	) -> Result<ServerCertVerified, rustls::Error> {
		// The name first: it needs no JVM, and the platform's check does not cover it.
		rustls::client::verify_server_name(&ParsedCertificate::try_from(end_entity)?, server_name)?;
		self.trust
			.check(end_entity, intermediates, &host(server_name))
			.map_err(Refusal::into_error)?;
		Ok(ServerCertVerified::assertion())
	}

	fn verify_tls12_signature(
		&self,
		message: &[u8],
		cert: &CertificateDer<'_>,
		dss: &DigitallySignedStruct,
	) -> Result<HandshakeSignatureValid, rustls::Error> {
		rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
	}

	fn verify_tls13_signature(
		&self,
		message: &[u8],
		cert: &CertificateDer<'_>,
		dss: &DigitallySignedStruct,
	) -> Result<HandshakeSignatureValid, rustls::Error> {
		rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
	}

	fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
		self.algorithms.supported_schemes()
	}
}

#[cfg(test)]
mod tests {
	use std::sync::Mutex;

	use super::*;

	/// A trust manager that answers with `answer` and records what it was asked.
	#[derive(Debug)]
	struct StandIn {
		answer: Result<(), Refusal>,
		asked: Mutex<Vec<(usize, String)>>,
	}

	impl StandIn {
		fn new(answer: Result<(), Refusal>) -> Self {
			Self {
				answer,
				asked: Mutex::new(Vec::new()),
			}
		}
	}

	impl TrustManager for StandIn {
		fn check(
			&self,
			_: &CertificateDer<'_>,
			intermediates: &[CertificateDer<'_>],
			host: &str,
		) -> Result<(), Refusal> {
			self.asked
				.lock()
				.unwrap()
				.push((intermediates.len(), host.to_owned()));
			self.answer.clone()
		}
	}

	/// DER `contents` under `tag` (short or one-byte long form).
	fn tlv(tag: u8, contents: &[u8]) -> Vec<u8> {
		let len = contents.len();
		let mut der = vec![tag];
		if len < 0x80 {
			der.push(u8::try_from(len).unwrap());
		} else {
			der.extend([0x81, u8::try_from(len).unwrap()]);
		}
		der.extend(contents);
		der
	}

	/// The certificate's SubjectPublicKeyInfo contents: id-ecPublicKey, P-256, and a point.
	fn spki_contents() -> Vec<u8> {
		[
			tlv(
				0x30,
				&[
					tlv(0x06, &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01]),
					tlv(0x06, &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07]),
				]
				.concat(),
			),
			tlv(0x03, &[0, 4]),
		]
		.concat()
	}

	/// A certificate rustls parses, for `gateway.filen.io` and `*.filen.io` and 127.0.0.1, with
	/// nothing but its names meant to be read: the platform stands in for the chain check.
	fn cert() -> CertificateDer<'static> {
		// ecdsa-with-SHA384
		let algorithm = tlv(
			0x30,
			&tlv(0x06, &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x03]),
		);
		let time = tlv(0x17, b"250101000000Z");
		let spki = tlv(0x30, &spki_contents());
		let names = tlv(
			0x30,
			&[
				tlv(0x82, b"gateway.filen.io"),
				tlv(0x82, b"*.filen.io"),
				tlv(0x87, &[127, 0, 0, 1]),
			]
			.concat(),
		);
		// subjectAltName
		let san = tlv(
			0x30,
			&[tlv(0x06, &[0x55, 0x1d, 0x11]), tlv(0x04, &names)].concat(),
		);
		let tbs = [
			tlv(0xa0, &tlv(0x02, &[2])),
			tlv(0x02, &[1]),
			algorithm.clone(),
			tlv(0x30, &[]),
			tlv(0x30, &[time.clone(), time].concat()),
			tlv(0x30, &[]),
			spki,
			tlv(0xa3, &tlv(0x30, &san)),
		]
		.concat();
		CertificateDer::from(tlv(
			0x30,
			&[tlv(0x30, &tbs), algorithm, tlv(0x03, &[0])].concat(),
		))
	}

	fn verifier(answer: Result<(), Refusal>) -> TrustManagerVerifier<StandIn> {
		TrustManagerVerifier::new(
			StandIn::new(answer),
			&rustls::crypto::ring::default_provider(),
		)
	}

	fn verify(
		verifier: &TrustManagerVerifier<StandIn>,
		cert: &CertificateDer<'_>,
		name: &str,
	) -> Result<(), rustls::Error> {
		verifier
			.verify_server_cert(
				cert,
				std::slice::from_ref(cert),
				&ServerName::try_from(name).unwrap(),
				&[],
				UnixTime::now(),
			)
			.map(drop)
	}

	/// The trust manager is asked about the whole chain, for the host without a trailing dot, or
	/// the IP address.
	#[test]
	fn a_chain_the_platform_trusts_for_the_name_is_accepted() {
		let verifier = verifier(Ok(()));
		for name in [
			"gateway.filen.io",
			"gateway.filen.io.",
			"egest.filen.io",
			"127.0.0.1",
		] {
			verify(&verifier, &cert(), name).unwrap();
		}
		assert_eq!(
			*verifier.trust.asked.lock().unwrap(),
			[
				(1, "gateway.filen.io".to_owned()),
				(1, "gateway.filen.io".to_owned()),
				(1, "egest.filen.io".to_owned()),
				(1, "127.0.0.1".to_owned()),
			]
		);
	}

	/// A name the certificate does not cover is refused by rustls before the JVM is asked.
	#[test]
	fn a_name_the_certificate_does_not_cover_is_refused_without_asking_the_platform() {
		let verifier = verifier(Ok(()));
		for name in ["filen.io", "a.b.filen.io", "filen.net", "127.0.0.2"] {
			let refused = verify(&verifier, &cert(), name).unwrap_err();
			assert!(
				matches!(
					refused,
					rustls::Error::InvalidCertificate(
						CertificateError::NotValidForName
							| CertificateError::NotValidForNameContext { .. }
					)
				),
				"{name}: {refused:?}"
			);
		}
		let unparsable = CertificateDer::from(vec![0x30, 0x03, 0x02, 0x01, 0x01]);
		assert!(verify(&verifier, &unparsable, "gateway.filen.io").is_err());
		assert!(verifier.trust.asked.lock().unwrap().is_empty());
	}

	/// Each refusal reads as the certificate problem it is, and a trust manager that could not be
	/// asked as a general error; both count as a rejected certificate, not as offline.
	#[test]
	fn the_platforms_refusals_map_to_certificate_errors() {
		for (refusal, expected) in [
			(
				Refusal::BadEncoding,
				rustls::Error::InvalidCertificate(CertificateError::BadEncoding),
			),
			(
				Refusal::Expired,
				rustls::Error::InvalidCertificate(CertificateError::Expired),
			),
			(
				Refusal::NotValidYet,
				rustls::Error::InvalidCertificate(CertificateError::NotValidYet),
			),
			(
				Refusal::Untrusted("Trust anchor for certification path not found.".into()),
				rustls::Error::InvalidCertificate(CertificateError::UnknownIssuer),
			),
		] {
			assert_eq!(
				verify(&verifier(Err(refusal)), &cert(), "gateway.filen.io").unwrap_err(),
				expected
			);
		}
		let failed = verify(
			&verifier(Err(Refusal::Failed("no JVM".into()))),
			&cert(),
			"gateway.filen.io",
		)
		.unwrap_err();
		assert!(
			matches!(&failed, rustls::Error::General(reason) if reason.contains("no JVM")),
			"{failed:?}"
		);
		assert!(super::super::rejects_certificate(&failed));
	}

	/// Android's stack: the ready gate fails closed before the trust manager is asked, and a chain
	/// it trusts is still refused for a distrusted key.
	#[test]
	fn androids_stack_gates_the_check_and_refuses_distrusted_keys() {
		use std::sync::atomic::{AtomicBool, Ordering};

		use sha2::{Digest, Sha256};

		use super::super::{DistrustingVerifier, NOT_INITIALISED, ReadyGateVerifier};

		static READY: AtomicBool = AtomicBool::new(false);
		let stack = |distrusted: &'static [[u8; 32]]| {
			ReadyGateVerifier::new(
				DistrustingVerifier {
					inner: verifier(Ok(())),
					distrusted,
				},
				&READY,
			)
		};
		let check = |verifier: &ReadyGateVerifier<DistrustingVerifier<_>>| {
			verifier
				.verify_server_cert(
					&cert(),
					&[],
					&ServerName::try_from("gateway.filen.io").unwrap(),
					&[],
					UnixTime::now(),
				)
				.map(drop)
		};
		let trusted = stack(&[]);
		assert_eq!(
			check(&trusted).unwrap_err(),
			rustls::Error::General(NOT_INITIALISED.to_owned())
		);
		assert!(trusted.inner.inner.trust.asked.lock().unwrap().is_empty());

		READY.store(true, Ordering::Release);
		check(&trusted).unwrap();
		let key: [u8; 32] = Sha256::digest(spki_contents()).into();
		let refused = check(&stack(Box::leak(Box::new([key])))).unwrap_err();
		assert!(
			matches!(
				&refused,
				rustls::Error::InvalidCertificate(CertificateError::Other(rustls::OtherError(e)))
					if e.downcast_ref::<webpki::Error>() == Some(&webpki::Error::CertDistrusted)
			),
			"{refused:?}"
		);
	}

	/// Signatures are the crypto provider's to check.
	#[test]
	fn signatures_are_checked_by_the_crypto_provider() {
		let provider = rustls::crypto::ring::default_provider();
		assert_eq!(
			verifier(Ok(())).supported_verify_schemes(),
			provider
				.signature_verification_algorithms
				.supported_schemes()
		);
	}
}

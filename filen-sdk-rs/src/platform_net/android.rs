//! Android's certificate check calls the platform's `X509TrustManager` through JNI
//! ([`super::trust_manager`]), so it needs the JVM before the first handshake. The app hands it
//! over once per process (and once per library: the SDK's and the Documents Provider's each carry
//! their own copy); the trust manager is built then, once, and until then
//! [`ReadyGateVerifier`](super::ReadyGateVerifier) fails every handshake closed.

use std::sync::{
	OnceLock,
	atomic::{AtomicBool, Ordering},
};

/// The `jni` the entry points take their types from, for a crate that exports its own entry
/// with another `jni` in its tree.
pub use jni;
use jni::{
	Env, EnvUnowned, JavaVM, Outcome, jni_sig, jni_str,
	objects::{JClass, JObject, JObjectArray, JString, JValue},
	refs::Global,
	sys::jboolean,
};
use rustls::pki_types::CertificateDer;

use super::trust_manager::{Refusal, TrustManager};

/// Whether [`android_init`] built the platform trust manager.
pub(super) static TLS_READY: AtomicBool = AtomicBool::new(false);

/// What a certificate check needs from the JVM, built once by [`android_init`].
static PLATFORM: OnceLock<Platform> = OnceLock::new();

struct Platform {
	vm: JavaVM,
	/// `X509TrustManagerExtensions` over the default `TrustManagerFactory`'s `X509TrustManager`:
	/// the system and user-installed roots and the app's network security config, with the
	/// host-aware check that config's per-domain trust and pins need.
	trust: Global<JObject<'static>>,
	/// `CertificateFactory.getInstance("X.509")`, which parses a DER certificate: Conscrypt's,
	/// which keeps no state between calls.
	factory: Global<JObject<'static>>,
	certificate: Global<JClass<'static>>,
	input_stream: Global<JClass<'static>>,
	/// "RSA", the auth type the trust manager is told, whatever the key: Conscrypt's
	/// `TrustManagerImpl` only refuses a null or empty one and never reads it otherwise (no key
	/// usage is checked against it), and the platform's `RootTrustManager` and
	/// `NetworkSecurityTrustManager` pass it through.
	auth_type: Global<JString<'static>>,
}

impl Platform {
	fn new(env: &mut Env<'_>) -> jni::errors::Result<Self> {
		let algorithm = env
			.call_static_method(
				jni_str!("javax/net/ssl/TrustManagerFactory"),
				jni_str!("getDefaultAlgorithm"),
				jni_sig!("()Ljava/lang/String;"),
				&[],
			)?
			.l()?;
		let factory = env
			.call_static_method(
				jni_str!("javax/net/ssl/TrustManagerFactory"),
				jni_str!("getInstance"),
				jni_sig!("(Ljava/lang/String;)Ljavax/net/ssl/TrustManagerFactory;"),
				&[JValue::from(&algorithm)],
			)?
			.l()?;
		// No KeyStore: the platform's own roots, as the app's network security config names them.
		env.call_method(
			&factory,
			jni_str!("init"),
			jni_sig!("(Ljava/security/KeyStore;)V"),
			&[JValue::from(&JObject::null())],
		)?
		.v()?;
		let managers = env
			.call_method(
				&factory,
				jni_str!("getTrustManagers"),
				jni_sig!("()[Ljavax/net/ssl/TrustManager;"),
				&[],
			)?
			.l()?;
		let managers = env.cast_local::<JObjectArray>(managers)?;
		let x509 = env.find_class(jni_str!("javax/net/ssl/X509TrustManager"))?;
		let mut manager = None;
		for index in 0..managers.len(env)? {
			let candidate = managers.get_element(env, index)?;
			if !candidate.is_null() && env.is_instance_of(&candidate, &x509)? {
				manager = Some(candidate);
				break;
			}
		}
		let manager = manager.ok_or(jni::errors::Error::NullPtr("no X509TrustManager"))?;
		let trust = env.new_object(
			jni_str!("android/net/http/X509TrustManagerExtensions"),
			jni_sig!("(Ljavax/net/ssl/X509TrustManager;)V"),
			&[JValue::from(&manager)],
		)?;
		let x509_name = env.new_string("X.509")?;
		let certificate_factory = env
			.call_static_method(
				jni_str!("java/security/cert/CertificateFactory"),
				jni_str!("getInstance"),
				jni_sig!("(Ljava/lang/String;)Ljava/security/cert/CertificateFactory;"),
				&[JValue::from(&x509_name)],
			)?
			.l()?;
		let certificate = env.find_class(jni_str!("java/security/cert/X509Certificate"))?;
		let input_stream = env.find_class(jni_str!("java/io/ByteArrayInputStream"))?;
		let auth_type = env.new_string("RSA")?;
		Ok(Self {
			vm: env.get_java_vm()?,
			trust: env.new_global_ref(trust)?,
			factory: env.new_global_ref(certificate_factory)?,
			certificate: env.new_global_ref(certificate)?,
			input_stream: env.new_global_ref(input_stream)?,
			auth_type: env.new_global_ref(auth_type)?,
		})
	}

	/// The DER certificate `cert` as an `X509Certificate`.
	fn parse<'local>(
		&self,
		env: &mut Env<'local>,
		cert: &CertificateDer<'_>,
	) -> jni::errors::Result<JObject<'local>> {
		let bytes = env.byte_array_from_slice(cert)?;
		let stream = env.new_object(
			&self.input_stream,
			jni_sig!("([B)V"),
			&[JValue::from(&bytes)],
		)?;
		let parsed = env
			.call_method(
				&self.factory,
				jni_str!("generateCertificate"),
				jni_sig!("(Ljava/io/InputStream;)Ljava/security/cert/Certificate;"),
				&[JValue::from(&stream)],
			)?
			.l()?;
		env.delete_local_ref(stream);
		env.delete_local_ref(bytes);
		Ok(parsed)
	}

	/// Whether the trust manager trusts the chain, end entity first, for `host`.
	fn check(
		&self,
		env: &mut Env<'_>,
		end_entity: &CertificateDer<'_>,
		intermediates: &[CertificateDer<'_>],
		host: &str,
	) -> Result<(), Refusal> {
		let length = i32::try_from(intermediates.len() + 1)
			.map_err(|_| Refusal::Failed("the chain is too long".to_owned()))?;
		let chain = env
			.new_object_array(length, &self.certificate, JObject::null())
			.map_err(|e| failed(env, e))?;
		let leaf = self.parse(env, end_entity).map_err(|e| unparsed(env, e))?;
		chain
			.set_element(env, 0, &leaf)
			.map_err(|e| failed(env, e))?;
		for (index, cert) in intermediates.iter().enumerate() {
			let parsed = self.parse(env, cert).map_err(|e| unparsed(env, e))?;
			chain
				.set_element(env, index + 1, &parsed)
				.map_err(|e| failed(env, e))?;
			env.delete_local_ref(parsed);
		}
		// The end entity's dates first: the trust manager reports an expired certificate as one
		// it does not trust.
		if let Err(e) = env
			.call_method(&leaf, jni_str!("checkValidity"), jni_sig!("()V"), &[])
			.and_then(|value| value.v())
		{
			let (name, message) = caught(env, e)?;
			return Err(match name.as_str() {
				"java.security.cert.CertificateExpiredException" => Refusal::Expired,
				"java.security.cert.CertificateNotYetValidException" => Refusal::NotValidYet,
				_ => Refusal::Untrusted(format!("{name}: {message}")),
			});
		}
		let host = env.new_string(host).map_err(|e| failed(env, e))?;
		if let Err(e) = env
			.call_method(
				&self.trust,
				jni_str!("checkServerTrusted"),
				jni_sig!(
					"([Ljava/security/cert/X509Certificate;Ljava/lang/String;Ljava/lang/String;)Ljava/util/List;"
				),
				&[
					JValue::from(&chain),
					JValue::Object(self.auth_type.as_ref()),
					JValue::from(&host),
				],
			)
			.and_then(|value| value.l())
		{
			let (name, message) = caught(env, e)?;
			return Err(Refusal::Untrusted(format!("{name}: {message}")));
		}
		Ok(())
	}
}

/// The class name and message of the Java exception `error` left pending, which is cleared; a
/// [`Refusal::Failed`] for any other JNI error.
fn caught(env: &mut Env<'_>, error: jni::errors::Error) -> Result<(String, String), Refusal> {
	if !matches!(error, jni::errors::Error::JavaException) {
		return Err(Refusal::Failed(error.to_string()));
	}
	match env.exception_catch() {
		Err(jni::errors::Error::CaughtJavaException { name, msg, .. }) => Ok((name, msg)),
		Err(other) => Err(Refusal::Failed(other.to_string())),
		Ok(()) => Err(Refusal::Failed(error.to_string())),
	}
}

/// A JNI error as a [`Refusal::Failed`], with a pending exception cleared.
fn failed(env: &mut Env<'_>, error: jni::errors::Error) -> Refusal {
	match caught(env, error) {
		Ok((name, message)) => Refusal::Failed(format!("{name}: {message}")),
		Err(refusal) => refusal,
	}
}

/// A certificate the platform could not parse: [`Refusal::BadEncoding`] when it threw.
fn unparsed(env: &mut Env<'_>, error: jni::errors::Error) -> Refusal {
	match caught(env, error) {
		Ok(_) => Refusal::BadEncoding,
		Err(refusal) => refusal,
	}
}

/// The platform trust manager [`android_init`] built. Until then it refuses every chain, though
/// the ready gate keeps every handshake from reaching it.
#[derive(Debug)]
pub(super) struct AndroidTrustManager;

impl TrustManager for AndroidTrustManager {
	fn check(
		&self,
		end_entity: &CertificateDer<'_>,
		intermediates: &[CertificateDer<'_>],
		host: &str,
	) -> Result<(), Refusal> {
		let Some(platform) = PLATFORM.get() else {
			return Err(Refusal::Failed(super::NOT_INITIALISED.to_owned()));
		};
		// The thread stays attached: a runtime's workers handshake again, and attaching is what
		// costs. Each call runs in a local frame of its own, which frees its references.
		platform
			.vm
			.attach_current_thread(|env| -> jni::errors::Result<Result<(), Refusal>> {
				Ok(platform.check(env, end_entity, intermediates, host))
			})
			.unwrap_or_else(|e| Err(Refusal::Failed(e.to_string())))
	}
}

/// Builds the platform trust manager the certificate checks use, in the JVM `env` belongs to.
/// `_context` (any `Context`) is not needed: the trust manager reads the app's network security
/// config by itself. Idempotent: a later call keeps the first one's trust manager. On failure
/// every handshake keeps failing closed, and [`tls_status`](super::tls_status) reports the error.
pub fn android_init(env: &mut Env<'_>, _context: JObject<'_>) -> Result<(), jni::errors::Error> {
	if PLATFORM.get().is_none() {
		match Platform::new(env) {
			// A concurrent call may have set it first; either one serves.
			Ok(platform) => drop(PLATFORM.set(platform)),
			Err(e) => {
				super::record_error(format!(
					"the Android certificate verifier could not be initialised: {e}"
				));
				return Err(e);
			}
		}
	}
	TLS_READY.store(true, Ordering::Release);
	Ok(())
}

/// The body of a static `nativeInit(context: Context): Boolean` JNI method: [`android_init`],
/// answering whether the verifier is ready. Nothing is thrown into the JVM: an exception a JNI
/// call left pending is cleared, and a failure is only reported, by `false` and
/// [`tls_status`](super::tls_status).
pub fn native_init(mut env: EnvUnowned<'_>, context: JObject<'_>) -> jboolean {
	let outcome = env
		.with_env(|env| {
			android_init(env, context).inspect_err(|_| {
				if env.exception_check() {
					env.exception_clear();
				}
			})
		})
		.into_outcome();
	match outcome {
		Outcome::Ok(()) => true,
		Outcome::Err(_) => false,
		Outcome::Panic(_) => {
			super::record_error(
				"the Android certificate verifier's initialisation panicked".to_owned(),
			);
			false
		}
	}
}

/// `io.filen.net.FilenTls.nativeInit(context)`, for the app's copy of the SDK.
#[cfg(feature = "uniffi")]
// JNI entry point: `export_name` is an unsafe attribute.
#[allow(unsafe_code)]
#[unsafe(export_name = "Java_io_filen_net_FilenTls_nativeInit")]
pub extern "system" fn filen_tls_native_init<'local>(
	env: EnvUnowned<'local>,
	_class: JObject<'local>,
	context: JObject<'local>,
) -> jboolean {
	native_init(env, context)
}

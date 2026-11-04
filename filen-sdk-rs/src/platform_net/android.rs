//! Android's platform verifier calls into the JVM (the Kotlin `CertificateVerifier` of the
//! `rustls-platform-verifier` AAR), so it needs the JVM and the app's `Context` before the first
//! handshake; without them it panics. The app hands them over once per process (and once per
//! library: the SDK's and the Documents Provider's each carry their own copy), and until then
//! [`ReadyGateVerifier`](super::ReadyGateVerifier) fails every handshake closed.

use std::sync::atomic::{AtomicBool, Ordering};

/// The `jni` the entry points take their types from, for a crate that exports its own entry
/// with another `jni` in its tree.
pub use jni;
use jni::{EnvUnowned, Outcome, objects::JObject, sys::jboolean};

/// Whether [`android_init`] handed the verifier its JVM and `Context`.
pub(super) static TLS_READY: AtomicBool = AtomicBool::new(false);

/// Hands the platform verifier the JVM and the app's `context` (any `Context`; the verifier keeps
/// a global reference to it and its class loader). Idempotent: a later call keeps the first
/// one's. On failure the verifier stays uninitialised, every handshake fails closed, and
/// [`tls_status`](super::tls_status) reports the error.
pub fn android_init(
	env: &mut jni::Env<'_>,
	context: JObject<'_>,
) -> Result<(), jni::errors::Error> {
	match rustls_platform_verifier::android::init_with_env(env, context) {
		Ok(()) => {
			TLS_READY.store(true, Ordering::Release);
			Ok(())
		}
		Err(e) => {
			super::record_error(format!(
				"the Android certificate verifier could not be initialised: {e}"
			));
			Err(e)
		}
	}
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

//! The `napi` build's binding layer (napi-rs, for the desktop app's Node).
//!
//! Its TypeScript matches what ubrn generates for the same items, so code written against the
//! react-native `@filen/sdk-rs` runs unchanged on it. `filen_macros` writes most of it: the
//! mirrors of `#[js_type(napi)]` types and the wrappers of `#[napi_export]` methods, which
//! rely on what this module provides: the wire conversions, errors thrown in ubrn's shape,
//! promises with abort, and the classes the wrappers are methods of. Every hand-written napi item
//! here names its JS name and TypeScript types explicitly, as the generated ones do, and takes
//! no raw `i64`, `u64` or `f32` (only their wire types).

mod error;
// Plain Rust, so back under the workspace's `unsafe_code` deny, which `js` lifts for this module
// because of the glue napi-derive generates for the `#[napi]` items here and in `error`. The
// hand-written impls and helpers beside those items carry the deny one by one.
#[deny(unsafe_code)]
mod promise;
#[deny(unsafe_code)]
mod wire;

use napi_derive::napi;

pub(crate) use error::throw;
pub(crate) use promise::spawn_promise;
pub(crate) use wire::{FromWire, IntoWire, OptionReturn, Wire};

use crate::auth::{self, http::LogLevel};

// Each class is named as its JS class is: napi-derive names the class a `#[napi] impl` in another
// module registers (the `#[napi_export]` wrappers) by the impl's type name whenever it has not
// expanded the class's struct first.

#[napi(js_name = "JsClient")]
pub(crate) struct JsClient(pub(crate) auth::JsClient);

#[deny(unsafe_code)]
impl Wire for auth::JsClient {
	type Wire = JsClient;
}

#[deny(unsafe_code)]
impl IntoWire for auth::JsClient {
	fn into_wire(self) -> JsClient {
		JsClient(self)
	}
}

#[napi(js_name = "UnauthJsClient")]
pub(crate) struct UnauthJsClient(pub(crate) auth::js_impls::UnauthJsClient);

#[deny(unsafe_code)]
impl Wire for auth::js_impls::UnauthJsClient {
	type Wire = UnauthJsClient;
}

#[deny(unsafe_code)]
impl IntoWire for auth::js_impls::UnauthJsClient {
	fn into_wire(self) -> UnauthJsClient {
		UnauthJsClient(self)
	}
}

#[cfg(feature = "sync-engine")]
#[napi(js_name = "JsSyncEngine")]
pub(crate) struct JsSyncEngine(pub(crate) crate::sync_engine::JsSyncEngine);

#[cfg(feature = "sync-engine")]
#[deny(unsafe_code)]
impl Wire for crate::sync_engine::JsSyncEngine {
	type Wire = JsSyncEngine;
}

#[cfg(feature = "sync-engine")]
#[deny(unsafe_code)]
impl IntoWire for crate::sync_engine::JsSyncEngine {
	fn into_wire(self) -> JsSyncEngine {
		JsSyncEngine(self)
	}
}

#[cfg(feature = "sync-engine")]
#[napi(js_name = "JsSyncPlan")]
pub(crate) struct JsSyncPlan(pub(crate) crate::sync_engine::JsSyncPlan);

#[cfg(feature = "sync-engine")]
#[deny(unsafe_code)]
impl Wire for crate::sync_engine::JsSyncPlan {
	type Wire = JsSyncPlan;
}

#[cfg(feature = "sync-engine")]
#[deny(unsafe_code)]
impl IntoWire for crate::sync_engine::JsSyncPlan {
	fn into_wire(self) -> JsSyncPlan {
		JsSyncPlan(self)
	}
}

#[cfg(feature = "sync-engine")]
#[napi(js_name = "JsSyncWatch")]
pub(crate) struct JsSyncWatch(pub(crate) crate::sync_engine::JsSyncWatch);

#[cfg(feature = "sync-engine")]
#[deny(unsafe_code)]
impl Wire for crate::sync_engine::JsSyncWatch {
	type Wire = JsSyncWatch;
}

#[cfg(feature = "sync-engine")]
#[deny(unsafe_code)]
impl IntoWire for crate::sync_engine::JsSyncWatch {
	fn into_wire(self) -> JsSyncWatch {
		JsSyncWatch(self)
	}
}

/// Installs the SDK's tracing subscriber (stdout on desktop) unless the host already installed
/// one. Idempotent.
#[napi(js_name = "init", ts_args_type = "", ts_return_type = "void")]
pub fn init() {
	crate::obs::try_init(LogLevel::Info);
}

/// Set the SDK's global log level at runtime (e.g. a "verbose logging" toggle on a device).
/// No-op until the subscriber has been installed via [`init`] or the cache's init.
#[napi(
	js_name = "setLogLevel",
	ts_args_type = "level: LogLevel",
	ts_return_type = "void"
)]
pub fn set_log_level(env: &napi::Env, level: <LogLevel as Wire>::Wire) -> napi::Result<()> {
	let level = LogLevel::from_wire(level).map_err(|e| throw(env, e))?;
	crate::obs::set_log_level(level);
	Ok(())
}

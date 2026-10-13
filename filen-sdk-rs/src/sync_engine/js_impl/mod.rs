//! The sync engine for the JS bindings: the mobile one through UniFFI today, a desktop one through
//! napi-rs later, both handing TypeScript the same surface.
//!
//! Everything outside `uniffi_impl` is plain Rust that names no binding type, so another binding
//! can put its own attributes on the same items. The engine's own types that need no conversion
//! carry `#[js_type]` where they are defined; [`types`] holds the mirrors of the rest.
//!
//! Pair ids cross as `u64` (a `bigint` in TS on both bindings, where an `i64` would be a `number`
//! on napi): an engine id, a positive sqlite rowid, always fits.

mod types;
#[cfg(feature = "uniffi")]
mod uniffi_impl;

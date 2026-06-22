#![feature(
	min_specialization,
	try_with_capacity,
	ascii_char,
	// only not stable because it broke crates relying on itertools (https://github.com/rust-lang/rust/issues/79524)
	// is virtually guaranteed to be stabilized as is once https://github.com/rust-lang/rust/issues/89151 is resolved
	iter_intersperse,
	// stable in 1.95
	cfg_select
)]
#![allow(type_alias_bounds)]

pub(crate) mod api;
pub mod auth;
// Compiles on native AND wasm32-unknown-unknown (rusqlite ≥0.38 bundles a wasm SQLite). On wasm
// the cache additionally requires `wasm-full` (worker/socket hosting) and the DB is the wasm
// VFS's named in-memory store — per-session, repopulated by the startup resync; OPFS persistence
// is a future step.
#[cfg(feature = "cache")]
pub mod cache;
pub mod chats;
pub mod connect;
pub mod consts;
pub mod crypto;
pub mod error;
pub mod fs;
#[cfg(feature = "http-provider")]
pub mod http_provider;
pub mod io;
#[cfg(any(all(target_family = "wasm", target_os = "unknown"), feature = "uniffi"))]
pub mod js;
pub mod notes;
pub mod runtime;
pub mod search;
pub(crate) mod serde;
#[cfg(any(
	not(all(target_family = "wasm", target_os = "unknown")),
	feature = "wasm-full"
))]
pub mod socket;
pub mod sync;
pub mod thumbnail;
pub mod user;
pub mod util;

pub use error::{Error, ErrorKind};

#[cfg(feature = "uniffi")]
uniffi::setup_scaffolding!();

#![feature(min_specialization, try_with_capacity, ascii_char, iter_intersperse)]
#![allow(dead_code)]
#![allow(type_alias_bounds)]

pub(crate) mod api;
pub mod auth;
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

// stabilized in Rust 1.92
#![feature(min_specialization)]
#![feature(unsigned_nonzero_div_ceil)]
#![allow(dead_code)]

pub(crate) mod api;
pub mod auth;
pub mod chats;
pub mod connect;
pub mod consts;
pub mod crypto;
pub mod error;
pub mod fs;
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

pub(crate) async fn require_send<F: Send + Future>(fut: F) -> F::Output {
	fut.await
}

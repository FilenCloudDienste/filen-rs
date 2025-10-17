#![allow(dead_code)]
#![allow(incomplete_features)]
// #![feature(async_drop)]

pub(crate) mod api;
pub mod auth;
pub mod chats;
pub mod connect;
pub mod consts;
pub mod crypto;
pub mod error;
pub mod fs;
pub mod io;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub mod js;
pub mod notes;
pub mod runtime;
pub mod search;
pub(crate) mod serde;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
pub mod sockets;
pub mod sync;
pub mod thumbnail;
pub mod user;
pub mod util;

pub use error::{Error, ErrorKind};

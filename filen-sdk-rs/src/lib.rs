#![allow(dead_code)]

pub(crate) mod api;
pub mod auth;
pub mod connect;
pub mod consts;
pub mod crypto;
pub mod error;
pub mod fs;
pub mod io;
#[cfg(any(feature = "node", all(target_arch = "wasm32", target_os = "unknown")))]
pub mod js;
pub mod search;
pub mod sync;
pub mod thumbnail;
pub mod user;
pub mod util;

pub use error::{Error, ErrorKind};

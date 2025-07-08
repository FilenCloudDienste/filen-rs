uniffi::setup_scaffolding!();

pub mod env;
mod error;
pub mod ffi;
pub mod io;
pub(crate) mod sql;
pub(crate) mod sync;
pub use error::CacheError;
pub mod auth;
pub mod local;
pub mod remote;
pub mod thumbnail;
pub mod traits;

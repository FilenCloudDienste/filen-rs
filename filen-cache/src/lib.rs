#![warn(unreachable_pub, unused_qualifications)]

mod error;
mod handle;
mod sql;
mod state;

pub(crate) use state::CacheState;
pub use {
	error::CacheError,
	handle::{CacheHandle, CacheMessage},
	state::CacheControlMessage,
};

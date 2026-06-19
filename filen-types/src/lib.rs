#![feature(min_specialization)]
pub mod api;
pub mod auth;
pub mod conversions;
pub mod crypto;
pub mod error;
pub mod fs;
pub mod rkyv;
pub mod serde;
pub mod traits;
#[cfg(feature = "uniffi")]
pub mod uniffi_impls;

#[cfg(feature = "uniffi")]
uniffi::setup_scaffolding!();

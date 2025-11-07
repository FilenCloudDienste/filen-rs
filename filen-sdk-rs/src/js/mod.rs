mod dir;
mod file;
mod item;
#[cfg(all(target_family = "wasm", target_os = "unknown",))]
mod managed_futures;
#[cfg(feature = "wasm-full")]
mod params;
#[cfg(feature = "wasm-full")]
mod returned_types;
mod service_worker;
mod shared;
#[cfg(all(test, feature = "wasm-full"))]
mod test;
#[cfg(feature = "uniffi")]
mod uniffi;
#[cfg(feature = "wasm-full")]
mod wasm;

#[cfg(feature = "uniffi")]
use std::{borrow::Cow, str::FromStr};

pub use dir::*;
pub use file::*;
#[cfg(feature = "uniffi")]
use filen_types::serde::rsa::RsaDerPublicKey;
pub use item::*;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
pub use managed_futures::*;
#[cfg(feature = "wasm-full")]
pub use params::*;
#[cfg(feature = "wasm-full")]
pub use returned_types::*;
#[cfg(feature = "uniffi")]
use rsa::RsaPublicKey;
#[cfg(feature = "wasm-full")]
pub(crate) use service_worker::shared::*;
use shared::*;

const HIDDEN_META_KEY: &str = "__hiddenMeta";

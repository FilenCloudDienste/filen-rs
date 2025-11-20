#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub(crate) mod native;
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
mod shared;
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub use shared::DecryptedSocketEvent;
#[cfg(all(
	feature = "uniffi",
	not(all(target_family = "wasm", target_os = "unknown"))
))]
mod uniffi;
#[cfg(feature = "wasm-full")]
// todo rework wasm to use shared module
pub(crate) mod wasm;

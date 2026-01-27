#[cfg(feature = "wasm-full")]
mod multi_threaded;
#[cfg(feature = "service-worker")]
mod service_worker;
#[cfg(feature = "uniffi")]
mod uniffi;

#[cfg(feature = "wasm-full")]
pub use multi_threaded::{ManagedFuture, PauseSignal, PauseSignalRust};
#[cfg(feature = "service-worker")]
pub use service_worker::{ManagedFuture, PauseSignal, PauseSignalRust};
#[cfg(feature = "uniffi")]
pub use uniffi::{ManagedFuture, PauseSignal};

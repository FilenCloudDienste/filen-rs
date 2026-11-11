#[cfg(feature = "wasm-full")]
mod multi_threaded;
#[cfg(any(feature = "uniffi", feature = "napi"))]
mod native;
#[cfg(feature = "service-worker")]
mod service_worker;

#[cfg(feature = "wasm-full")]
pub use multi_threaded::{ManagedFuture, PauseSignal, PauseSignalRust};
#[cfg(any(feature = "uniffi", feature = "napi"))]
pub use native::{ManagedAbortController, ManagedAbortSignal, ManagedFuture, PauseSignal};
#[cfg(feature = "service-worker")]
pub use service_worker::{ManagedFuture, PauseSignal, PauseSignalRust};

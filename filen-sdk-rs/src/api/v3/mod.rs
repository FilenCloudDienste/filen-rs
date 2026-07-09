pub(crate) mod auth;
pub(crate) mod chat;
pub(crate) mod confirmation;
pub(crate) mod contacts;
pub(crate) mod dir;
pub(crate) mod file;
pub(crate) mod health;
pub(crate) mod item;
pub(crate) mod login;
// Only used by the `socket` module (which has the same cfg); never on the bare wasm
// service-worker build.
#[cfg(any(
	not(all(target_family = "wasm", target_os = "unknown")),
	feature = "wasm-full"
))]
pub(crate) mod message_ids;
pub(crate) mod notes;
pub(crate) mod register;
pub(crate) mod shared;
pub(crate) mod trash;
pub(crate) mod upload;
pub(crate) mod user;

//! The platform-independent core of Filen's Windows files-on-demand provider, built on the Windows
//! Cloud Files API (cfapi): what a placeholder's file identity holds, which remote names Windows
//! can hold, the values a placeholder is created with, the seam through which the OS layer hands
//! the core its requests and the core answers them, and the dispatcher that answers them from the
//! cache: populating directories and hydrating byte ranges, and the store that seals the session
//! the desktop app pushes into the auth.json the cache reads. Only the Windows layer, built on
//! Windows alone, calls Windows; the `test-seams` feature exposes a fake OS and an in-memory key
//! store the tests drive the core with.

#![warn(unreachable_pub, unused_qualifications)]
// Stdout carries the helper's protocol alone.
#![deny(clippy::print_stdout)]

mod drive;
mod failure;
mod identity;
mod names;
mod os;
mod placeholder;
mod root_path;
#[cfg(any(test, feature = "test-seams"))]
pub mod test_support;

/// Seals the OS-facing traits: only this crate implements them, so the core can rely on what each
/// implementation does.
mod sealed {
	pub trait Sealed {}
}

pub use drive::Drive;
pub use failure::Failure;
pub use identity::{IdentityBlob, IdentityError, MAX_IDENTITY_LEN, PlaceholderIdentity};
pub use names::{NameRejection, PlaceholderName};
// Exported in tests too, where `os::cfapi` is built on every host.
#[cfg(any(windows, test))]
pub use os::cfapi::{CfCall, Hresult, ProviderInfoError, SyncRootPathError, WinError};
pub use placeholder::{
	FileSize, FileTime, NegativeSize, PlaceholderSpec, SpecKind, UnrepresentableTime,
};
pub use root_path::{CaseKey, RootPath, RootPathError};

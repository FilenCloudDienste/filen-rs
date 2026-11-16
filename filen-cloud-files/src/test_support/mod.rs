//! Test doubles for the drive, the OS seam, the sync root and the key store, for this crate's
//! tests and, with the `test-seams` feature, for integration tests. Never in a shipping build.

mod fake_drive;
mod fake_local;
mod fake_os;
mod fake_platform;
mod memory_key_store;

#[cfg(feature = "test-seams")]
pub use crate::local::relocate_for_test;
#[cfg(all(windows, feature = "test-seams"))]
pub use crate::os::win::user_dehydrate;
#[cfg(test)]
pub(crate) use fake_drive::place_files;
pub use fake_drive::{DriveCall, DriveOp, FakeDrive, TIMESTAMP_MILLIS, ffi_dir, ffi_file, pattern};
pub use fake_local::{
	FakeDehydrate, FakeDelete, FakeEntry, FakeLocal, FakeRename, FakeSettled, FakeWriter, LocalOp,
	ProviderAct, UserChange, name, root_path,
};
pub use fake_os::{FakeData, FakeEvent, FakeOs, FakePopulate};
pub use fake_platform::{
	FakeConnected, FakeKeys, FakeOpener, FakePlatform, FakeRegistered, FakeStep, FakeWatcher,
	KillPoint, PlatformCall, WatchGate,
};
pub use memory_key_store::MemoryKeyStore;

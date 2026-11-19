//! The OS seam on Windows, over the Cloud Files API (cfapi) through raw `windows-sys`: registering
//! a sync root, connecting it to a dispatcher, and answering the OS's requests through the
//! responders. Every `unsafe` block of the crate lives in `ffi`, and the COM layer's in `com`;
//! everything else here is safe.
//!
//! A callback runs on an OS thread: `ffi` copies it into owned values, `callbacks` builds the
//! responder and hands the request to the dispatcher, which spawns the work on its runtime and
//! returns, so the callback returns within microseconds. The answers go through `CfExecute` from
//! the runtime's tasks.

mod callbacks;
mod com;
mod explorer;
mod ffi;
mod local;
mod platform;
mod responder;
mod sync_root;
mod system;
#[cfg(test)]
mod tests;

use std::mem::offset_of;

use tokio::{
	runtime::{Handle, RuntimeFlavor},
	task::block_in_place,
};
use windows_sys::Win32::{
	Foundation,
	Storage::{CloudFilters, FileSystem},
};

pub use self::com::ThumbnailServer;
#[cfg(feature = "test-seams")]
pub use self::local::user_dehydrate;
pub use self::local::{WinLocalFs, WinSettled};
pub use self::platform::WinPlatform;
pub use self::sync_root::{
	ConnectError, ConnectFailed, ConnectedRoot, DisconnectFailed, PlatformInfo, ProviderInfo,
	RegisterFailed, RegisteredRoot, SyncRootPath, UnregisterFailed, platform_info, register,
	take_over, unregister_path,
};
pub use self::system::{is_elevated, volume_file_system};
use super::cfapi;

/// The target every event of the Windows layer is logged under.
const LOG_TARGET: &str = "filen_cloud_files::os::win";

/// Runs `f`, a synchronous cfapi call of unbounded duration, without pinning a runtime worker:
/// through `block_in_place` on a multi-thread runtime's worker, which hands the worker's other
/// tasks to a new one first, and inline anywhere else. Measured: a transfer of up to 4 MiB blocks
/// for under 10 ms, a population of 1,000 placeholders for about 230 ms.
fn run_blocking<T>(f: impl FnOnce() -> T) -> T {
	match Handle::try_current() {
		Ok(handle) if handle.runtime_flavor() == RuntimeFlavor::MultiThread => block_in_place(f),
		_ => f(),
	}
}

// `cfapi` compiles on every host, so it spells the codes out; these check each against
// `windows-sys`.
const _: () = {
	assert!(cfapi::E_INVALIDARG == Foundation::E_INVALIDARG);
	assert!(cfapi::E_FAIL == Foundation::E_FAIL);
	assert!(cfapi::STATUS_CLOUD_FILE_UNSUCCESSFUL == Foundation::STATUS_CLOUD_FILE_UNSUCCESSFUL);
	assert!(
		cfapi::STATUS_CLOUD_FILE_AUTHENTICATION_FAILED
			== Foundation::STATUS_CLOUD_FILE_AUTHENTICATION_FAILED
	);
	assert!(cfapi::STATUS_CLOUD_FILE_NOT_IN_SYNC == Foundation::STATUS_CLOUD_FILE_NOT_IN_SYNC);
	assert!(
		cfapi::STATUS_CLOUD_FILE_NETWORK_UNAVAILABLE
			== Foundation::STATUS_CLOUD_FILE_NETWORK_UNAVAILABLE
	);
	assert!(cfapi::STATUS_CLOUD_FILE_NOT_SUPPORTED == Foundation::STATUS_CLOUD_FILE_NOT_SUPPORTED);
	assert!(
		cfapi::STATUS_CLOUD_FILE_VALIDATION_FAILED
			== Foundation::STATUS_CLOUD_FILE_VALIDATION_FAILED
	);
	assert!(
		cfapi::STATUS_CLOUD_FILE_INVALID_REQUEST == Foundation::STATUS_CLOUD_FILE_INVALID_REQUEST
	);
	assert!(
		cfapi::ERROR_CLOUD_FILE_REQUEST_CANCELED == Foundation::ERROR_CLOUD_FILE_REQUEST_CANCELED
	);
	assert!(
		cfapi::ERROR_CLOUD_FILE_REQUEST_ABORTED == Foundation::ERROR_CLOUD_FILE_REQUEST_ABORTED
	);
	assert!(cfapi::ERROR_NOT_A_CLOUD_FILE == Foundation::ERROR_NOT_A_CLOUD_FILE);
	assert!(
		cfapi::ERROR_CLOUD_FILE_INVALID_REQUEST == Foundation::ERROR_CLOUD_FILE_INVALID_REQUEST
	);
	assert!(cfapi::ERROR_CLOUD_FILE_IN_USE == Foundation::ERROR_CLOUD_FILE_IN_USE);
	assert!(cfapi::ERROR_FILE_NOT_FOUND == Foundation::ERROR_FILE_NOT_FOUND);
	assert!(cfapi::ERROR_PATH_NOT_FOUND == Foundation::ERROR_PATH_NOT_FOUND);
	assert!(cfapi::ERROR_SHARING_VIOLATION == Foundation::ERROR_SHARING_VIOLATION);
	assert!(cfapi::ERROR_FILE_EXISTS == Foundation::ERROR_FILE_EXISTS);
	assert!(cfapi::ERROR_ALREADY_EXISTS == Foundation::ERROR_ALREADY_EXISTS);
	assert!(cfapi::ERROR_DIR_NOT_EMPTY == Foundation::ERROR_DIR_NOT_EMPTY);
	assert!(cfapi::ERROR_CLOUD_FILE_NOT_IN_SYNC == Foundation::ERROR_CLOUD_FILE_NOT_IN_SYNC);
	assert!(cfapi::FILE_ATTRIBUTE_REPARSE_POINT == FileSystem::FILE_ATTRIBUTE_REPARSE_POINT);
	assert!(cfapi::DELETE_FLAG_IS_DIRECTORY == CloudFilters::CF_CALLBACK_DELETE_FLAG_IS_DIRECTORY);
	assert!(cfapi::DELETE_FLAG_IS_UNDELETE == CloudFilters::CF_CALLBACK_DELETE_FLAG_IS_UNDELETE);
	assert!(cfapi::RENAME_FLAG_IS_DIRECTORY == CloudFilters::CF_CALLBACK_RENAME_FLAG_IS_DIRECTORY);
	assert!(
		cfapi::RENAME_FLAG_TARGET_IN_SCOPE == CloudFilters::CF_CALLBACK_RENAME_FLAG_TARGET_IN_SCOPE
	);
	assert!(
		cfapi::CLOSE_COMPLETION_FLAG_DELETED
			== CloudFilters::CF_CALLBACK_CLOSE_COMPLETION_FLAG_DELETED
	);
	assert!(
		cfapi::DEHYDRATE_FLAG_BACKGROUND == CloudFilters::CF_CALLBACK_DEHYDRATE_FLAG_BACKGROUND
	);
	assert!(cfapi::DEHYDRATION_REASON_NONE == CloudFilters::CF_CALLBACK_DEHYDRATION_REASON_NONE);
	assert!(
		cfapi::DEHYDRATION_REASON_USER_MANUAL
			== CloudFilters::CF_CALLBACK_DEHYDRATION_REASON_USER_MANUAL
	);
	assert!(
		cfapi::DEHYDRATION_REASON_SYSTEM_LOW_SPACE
			== CloudFilters::CF_CALLBACK_DEHYDRATION_REASON_SYSTEM_LOW_SPACE
	);
	assert!(
		cfapi::DEHYDRATION_REASON_SYSTEM_INACTIVITY
			== CloudFilters::CF_CALLBACK_DEHYDRATION_REASON_SYSTEM_INACTIVITY
	);
	assert!(
		cfapi::DEHYDRATION_REASON_SYSTEM_OS_UPGRADE
			== CloudFilters::CF_CALLBACK_DEHYDRATION_REASON_SYSTEM_OS_UPGRADE
	);
	assert!(cfapi::STATUS_CLOUD_FILE_ACCESS_DENIED == Foundation::STATUS_CLOUD_FILE_ACCESS_DENIED);
	assert!(cfapi::FILE_ATTRIBUTE_NORMAL == FileSystem::FILE_ATTRIBUTE_NORMAL);
	assert!(cfapi::FILE_ATTRIBUTE_PINNED == FileSystem::FILE_ATTRIBUTE_PINNED);
	assert!(cfapi::FILE_ATTRIBUTE_UNPINNED == FileSystem::FILE_ATTRIBUTE_UNPINNED);
	assert!(cfapi::FILE_ATTRIBUTE_DIRECTORY == FileSystem::FILE_ATTRIBUTE_DIRECTORY);
	assert!(cfapi::FILE_ATTRIBUTE_OFFLINE == FileSystem::FILE_ATTRIBUTE_OFFLINE);
	assert!(cfapi::FILE_ATTRIBUTE_RECALL_ON_OPEN == FileSystem::FILE_ATTRIBUTE_RECALL_ON_OPEN);
	assert!(
		cfapi::FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
			== FileSystem::FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
	);
	assert!(
		cfapi::PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC
			== CloudFilters::CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC
	);
	assert!(cfapi::MAX_PROVIDER_NAME_UNITS == CloudFilters::CF_MAX_PROVIDER_NAME_LENGTH as usize);
	assert!(
		cfapi::MAX_PROVIDER_VERSION_UNITS == CloudFilters::CF_MAX_PROVIDER_VERSION_LENGTH as usize
	);
	assert!(
		crate::identity::MAX_IDENTITY_LEN
			== CloudFilters::CF_PLACEHOLDER_MAX_FILE_IDENTITY_LENGTH as usize
	);
	assert!(
		cfapi::ERROR_CLOUD_FILE_NOT_UNDER_SYNC_ROOT
			== Foundation::ERROR_CLOUD_FILE_NOT_UNDER_SYNC_ROOT
	);
	assert!(cfapi::ERROR_NOT_A_CLOUD_SYNC_ROOT == Foundation::ERROR_NOT_A_CLOUD_SYNC_ROOT);
	assert!(
		cfapi::SYNC_ROOT_FILE_ID_OFFSET
			== offset_of!(CloudFilters::CF_SYNC_ROOT_STANDARD_INFO, SyncRootFileId)
	);
	assert!(
		cfapi::SYNC_ROOT_PROVIDER_NAME_OFFSET
			== offset_of!(CloudFilters::CF_SYNC_ROOT_STANDARD_INFO, ProviderName)
	);
	assert!(
		cfapi::SYNC_ROOT_PROVIDER_NAME_UNITS
			== CloudFilters::CF_MAX_PROVIDER_NAME_LENGTH as usize + 1
	);
	assert!(
		cfapi::SYNC_ROOT_IDENTITY_LEN_OFFSET
			== offset_of!(
				CloudFilters::CF_SYNC_ROOT_STANDARD_INFO,
				SyncRootIdentityLength
			)
	);
	assert!(
		cfapi::SYNC_ROOT_IDENTITY_OFFSET
			== offset_of!(CloudFilters::CF_SYNC_ROOT_STANDARD_INFO, SyncRootIdentity)
	);
};

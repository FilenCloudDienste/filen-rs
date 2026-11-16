//! The struct reads of `ffi` against structs a test builds, with each size cfapi might report: run
//! on Windows only, as the cfapi types exist only there.

use std::mem::{offset_of, size_of};

use windows_sys::Win32::Storage::CloudFilters::{
	CF_CALLBACK_INFO, CF_CALLBACK_PARAMETERS, CF_CALLBACK_PARAMETERS_0, CF_CALLBACK_PARAMETERS_0_0,
	CF_CALLBACK_PARAMETERS_0_0_0, CF_CALLBACK_PARAMETERS_0_0_0_0, CF_CALLBACK_PARAMETERS_0_1,
	CF_CALLBACK_PARAMETERS_0_6,
};

use super::{callbacks::Cancel, ffi};
use crate::os::{OptionalRange, RequestedRange};

/// `size` as the `u32` a cfapi struct's size field takes.
fn size(size: usize) -> u32 {
	u32::try_from(size).unwrap()
}

/// A callback info reporting `struct_size` bytes, with keys 7, 9 and 3 and a file of 10,000 bytes.
fn info(struct_size: usize) -> CF_CALLBACK_INFO {
	CF_CALLBACK_INFO {
		StructSize: size(struct_size),
		ConnectionKey: 7,
		TransferKey: 9,
		RequestKey: 3,
		FileSize: 10_000,
		..CF_CALLBACK_INFO::default()
	}
}

/// A `FETCH_DATA` callback's parameters reporting `param_size` bytes.
fn fetch(param_size: usize) -> CF_CALLBACK_PARAMETERS {
	CF_CALLBACK_PARAMETERS {
		ParamSize: size(param_size),
		Anonymous: CF_CALLBACK_PARAMETERS_0 {
			FetchData: CF_CALLBACK_PARAMETERS_0_1 {
				Flags: 0,
				RequiredFileOffset: 4096,
				RequiredLength: 8192,
				OptionalFileOffset: 12_288,
				OptionalLength: 1000,
				LastDehydrationTime: 0,
				LastDehydrationReason: 0,
			},
		},
	}
}

/// A `CANCEL_FETCH_DATA` callback's parameters reporting `param_size` bytes.
fn cancel(param_size: usize) -> CF_CALLBACK_PARAMETERS {
	CF_CALLBACK_PARAMETERS {
		ParamSize: size(param_size),
		Anonymous: CF_CALLBACK_PARAMETERS_0 {
			Cancel: CF_CALLBACK_PARAMETERS_0_0 {
				Flags: 2,
				Anonymous: CF_CALLBACK_PARAMETERS_0_0_0 {
					FetchData: CF_CALLBACK_PARAMETERS_0_0_0_0 {
						FileOffset: 4096,
						Length: 8192,
					},
				},
			},
		},
	}
}

/// Where the union of the parameters starts.
const PARAMS: usize = offset_of!(CF_CALLBACK_PARAMETERS, Anonymous);

#[test]
fn a_full_callback_info_gives_its_keys_and_size() {
	let (keys, file_size) = ffi::keys_of(&info(size_of::<CF_CALLBACK_INFO>())).unwrap();
	assert_eq!((keys.all(), file_size), ([7, 9, 3], 10_000));
}

#[test]
fn a_callback_info_short_of_its_request_key_takes_the_default_one() {
	let (keys, _) = ffi::keys_of(&info(offset_of!(CF_CALLBACK_INFO, RequestKey))).unwrap();
	assert_eq!(keys.all(), [7, 9, 0]);
}

#[test]
fn a_callback_info_short_of_its_transfer_key_cannot_be_answered() {
	let struct_size = offset_of!(CF_CALLBACK_INFO, TransferKey) + 4;
	assert!(ffi::keys_of(&info(struct_size)).is_none());
}

#[test]
fn fetch_parameters_reaching_the_optional_length_give_the_range() {
	let wanted = Some(RequestedRange::new(
		10_000,
		4096,
		8192,
		OptionalRange::new(12_288, 1000),
	));
	let through_optional = PARAMS + offset_of!(CF_CALLBACK_PARAMETERS_0_1, OptionalLength) + 8;
	assert_eq!(
		ffi::fetch_params_of(&fetch(through_optional), 10_000),
		wanted
	);
	assert_eq!(
		ffi::fetch_params_of(&fetch(size_of::<CF_CALLBACK_PARAMETERS>()), 10_000),
		wanted
	);
}

#[test]
fn fetch_parameters_short_of_the_optional_length_give_no_range() {
	let short = PARAMS + offset_of!(CF_CALLBACK_PARAMETERS_0_1, OptionalLength) + 7;
	assert_eq!(ffi::fetch_params_of(&fetch(short), 10_000), None);
	assert_eq!(ffi::fetch_params_of(&fetch(4), 10_000), None);
}

#[test]
fn cancel_parameters_reaching_the_length_give_the_range() {
	let through_length = PARAMS + 8 + 16;
	assert_eq!(
		ffi::cancel_params_of(&cancel(through_length)),
		Some(Cancel {
			flags: 2,
			offset: 4096,
			length: 8192
		})
	);
	assert_eq!(ffi::cancel_params_of(&cancel(through_length - 1)), None);
}

/// A `NOTIFY_DEHYDRATE` callback's parameters reporting `param_size` bytes: background, low space.
fn dehydrate(param_size: usize) -> CF_CALLBACK_PARAMETERS {
	CF_CALLBACK_PARAMETERS {
		ParamSize: size(param_size),
		Anonymous: CF_CALLBACK_PARAMETERS_0 {
			Dehydrate: CF_CALLBACK_PARAMETERS_0_6 {
				Flags: 1,
				Reason: 2,
			},
		},
	}
}

#[test]
fn dehydrate_params_read_only_when_the_struct_holds_flags_and_reason() {
	let base = offset_of!(CF_CALLBACK_PARAMETERS, Anonymous);
	let whole = base + size_of::<CF_CALLBACK_PARAMETERS_0_6>();
	assert_eq!(ffi::dehydrate_params_of(&dehydrate(whole)), Some((1, 2)));
	assert_eq!(ffi::dehydrate_params_of(&dehydrate(whole - 1)), None);
	assert_eq!(ffi::dehydrate_params_of(&dehydrate(base + 4)), None);
}

#[test]
fn a_directory_in_no_sync_root_has_no_registration() {
	let temp = tempfile::tempdir().unwrap();
	let path = super::SyncRootPath::from_path(temp.path()).unwrap();
	assert_eq!(super::sync_root::sync_root_info(&path).unwrap(), None);
}

#[test]
fn a_missing_directory_is_read_on_its_nearest_existing_ancestor() {
	let temp = tempfile::tempdir().unwrap();
	let missing = temp.path().join("a").join("b");
	let (file_system, at) = super::volume_file_system(&missing).unwrap();
	assert_eq!(at, temp.path());
	// The temporary directory of every Windows this runs on is on the system volume.
	assert_eq!(file_system, "NTFS");
}

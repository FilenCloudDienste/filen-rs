//! Every `unsafe` block of the crate: the cfapi calls, and the Win32 calls around them (the
//! process token, volumes, files by handle, the registry keys File Explorer reads and the shell's
//! change notification), each behind a safe function whose arguments make the call sound.
//! Nothing else lives here.

use std::{
	ffi::{OsString, c_void},
	fs::File,
	io,
	mem::{offset_of, size_of, size_of_val},
	os::windows::{
		ffi::OsStringExt,
		io::{AsRawHandle, FromRawHandle, OwnedHandle},
	},
	panic::{self, AssertUnwindSafe},
	path::PathBuf,
	ptr, slice,
};

use tracing::error;
#[cfg(test)]
use windows_sys::Win32::{
	Security::{
		ACL, ACL_REVISION, DACL_SECURITY_INFORMATION, InitializeAcl, InitializeSecurityDescriptor,
		SECURITY_DESCRIPTOR, SetSecurityDescriptorDacl,
	},
	Storage::FileSystem::WRITE_DAC,
	System::Registry::RegSetKeySecurity,
};
// The bindings module is the cfapi surface this file exists to call.
use windows_sys::{
	Win32::{
		Foundation::{
			ERROR_FILE_NOT_FOUND, ERROR_INVALID_PARAMETER, ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS,
			ERROR_NOT_SUPPORTED, ERROR_SUCCESS, HANDLE, LocalFree, STATUS_SUCCESS, WIN32_ERROR,
		},
		Security::{
			Authorization::{ConvertSidToStringSidW, GetSecurityInfo, SE_REGISTRY_KEY},
			GetTokenInformation, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
			TOKEN_ELEVATION, TOKEN_QUERY, TOKEN_USER, TokenElevation, TokenUser,
		},
		Storage::{
			CloudFilters::*,
			FileSystem::{
				FILE_ATTRIBUTE_TAG_INFO, FILE_BASIC_INFO, FILE_DISPOSITION_FLAG_DELETE,
				FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
				FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO_EX, FILE_ID_INFO,
				FILE_INFO_BY_HANDLE_CLASS, FILE_RENAME_INFO, FILE_RENAME_INFO_0,
				FileAttributeTagInfo, FileBasicInfo, FileDispositionInfoEx, FileIdInfo,
				FileRenameInfo, FileRenameInfoEx, GetFileInformationByHandleEx,
				GetVolumeInformationByHandleW, MoveFileExW, SetFileInformationByHandle,
			},
		},
		System::{
			Registry::{
				HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE, REG_DWORD,
				REG_EXPAND_SZ, REG_OPTION_NON_VOLATILE, REG_SZ, REG_VALUE_TYPE, RegCloseKey,
				RegCreateKeyExW, RegDeleteKeyValueW, RegDeleteTreeW, RegEnumKeyExW, RegEnumValueW,
				RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
			},
			Threading::{GetCurrentProcess, OpenProcessToken},
		},
		UI::Shell::{SHCNE_UPDATEDIR, SHCNF_PATHW, SHChangeNotify},
	},
	core::{GUID, PWSTR},
};

use super::{
	LOG_TARGET,
	callbacks::{self, CallbackInfo},
	sync_root::ContextSlot,
};
use crate::{
	identity::{IdentityBlob, MAX_IDENTITY_LEN},
	os::{
		OptionalRange, RequestedRange,
		cfapi::{
			CreateValues, E_FAIL, E_INVALIDARG, Hresult, MAX_SYNC_ROOT_IDENTITY_LEN,
			SyncRootIdentity, WideNul, create_values, field_present, identity_len, take_until_nul,
			volume_path,
		},
	},
	placeholder::PlaceholderSpec,
};

/// The most UTF-16 units a path a callback passes is read for, its NUL included: the longest
/// Win32 path, 32,767 units, and its NUL.
const MAX_PATH_UNITS: usize = 32_768;

/// `size_of::<T>()` as the `u32` a cfapi struct's `StructSize` takes.
fn struct_size<T>() -> u32 {
	u32::try_from(size_of::<T>())
		.expect("a cfapi struct is a few hundred bytes (should be impossible)")
}

/// What one sync root is registered with.
pub(super) struct Registration<'a> {
	/// The provider's name, shown by Windows.
	pub(super) name: &'a WideNul,
	/// The provider's version.
	pub(super) version: &'a WideNul,
	/// The provider's GUID, the same on every registration.
	pub(super) provider_id: u128,
	/// The sync root's identity, which `CfGetSyncRootInfoByPath` reports back.
	pub(super) sync_root_identity: &'a SyncRootIdentity,
	/// The root directory's file identity, which the root's own callbacks carry.
	pub(super) root_identity: &'a IdentityBlob,
}

/// `CfRegisterSyncRoot` of `path`: hydration PROGRESSIVE and population PARTIAL, both without
/// modifiers, no in-sync tracking, no hard links and the default placeholder management: the
/// registration measured to work on Windows 11. `CF_REGISTER_FLAG_NONE`, so registering a
/// registered root fails rather than updating it.
#[allow(unsafe_code)] // cfapi FFI: registers a sync root.
pub(super) fn register(path: &WideNul, registration: &Registration<'_>) -> Hresult {
	let sync_root_identity = registration.sync_root_identity.as_bytes();
	let root_identity = registration.root_identity.as_bytes();
	let (Ok(sync_root_identity_len), Ok(root_identity_len)) = (
		u32::try_from(sync_root_identity.len()),
		u32::try_from(root_identity.len()),
	) else {
		return Hresult::new(E_INVALIDARG);
	};
	let raw = CF_SYNC_REGISTRATION {
		StructSize: struct_size::<CF_SYNC_REGISTRATION>(),
		ProviderName: registration.name.with_nul().as_ptr(),
		ProviderVersion: registration.version.with_nul().as_ptr(),
		SyncRootIdentity: sync_root_identity.as_ptr().cast(),
		SyncRootIdentityLength: sync_root_identity_len,
		FileIdentity: root_identity.as_ptr().cast(),
		FileIdentityLength: root_identity_len,
		ProviderId: GUID::from_u128(registration.provider_id),
	};
	let policies = CF_SYNC_POLICIES {
		StructSize: struct_size::<CF_SYNC_POLICIES>(),
		Hydration: CF_HYDRATION_POLICY {
			Primary: CF_HYDRATION_POLICY_PROGRESSIVE,
			Modifier: CF_HYDRATION_POLICY_MODIFIER_NONE,
		},
		Population: CF_POPULATION_POLICY {
			Primary: CF_POPULATION_POLICY_PARTIAL,
			Modifier: CF_POPULATION_POLICY_MODIFIER_NONE,
		},
		InSync: CF_INSYNC_POLICY_NONE,
		HardLink: CF_HARDLINK_POLICY_NONE,
		PlaceholderManagement: CF_PLACEHOLDER_MANAGEMENT_POLICY_DEFAULT,
	};
	// SAFETY: `path`, the name and the version are NUL-terminated by `WideNul`'s construction, the
	// two identity pointers are valid for their lengths, and all of them, `raw` and `policies` are
	// borrowed for the whole call. cfapi copies what it keeps and holds no pointer after
	// returning.
	Hresult::new(unsafe {
		CfRegisterSyncRoot(
			path.with_nul().as_ptr(),
			&raw,
			&policies,
			CF_REGISTER_FLAG_NONE,
		)
	})
}

/// `CfUnregisterSyncRoot` of `path`.
#[allow(unsafe_code)] // cfapi FFI: unregisters a sync root.
pub(super) fn unregister(path: &WideNul) -> Hresult {
	// SAFETY: `path` is NUL-terminated by `WideNul`'s construction and borrowed for the call.
	Hresult::new(unsafe { CfUnregisterSyncRoot(path.with_nul().as_ptr()) })
}

/// `CfGetPlatformInfo`.
#[allow(unsafe_code)] // cfapi FFI: reads the platform's version.
pub(super) fn platform_info() -> Result<CF_PLATFORM_INFO, Hresult> {
	let mut info = CF_PLATFORM_INFO::default();
	// SAFETY: `info` is a live, exclusively borrowed `CF_PLATFORM_INFO` cfapi writes into.
	let hr = Hresult::new(unsafe { CfGetPlatformInfo(&mut info) });
	if hr.is_ok() { Ok(info) } else { Err(hr) }
}

/// `CfGetSyncRootInfoByPath(CF_SYNC_ROOT_INFO_STANDARD)` of `path`, a path at or below a sync
/// root: the bytes cfapi wrote, with room for an identity of [`MAX_SYNC_ROOT_IDENTITY_LEN`] bytes.
#[allow(unsafe_code)] // cfapi FFI: reads the registration of the sync root a path is in.
pub(super) fn sync_root_info(path: &WideNul) -> Result<Vec<u8>, Hresult> {
	let bytes_len = size_of::<CF_SYNC_ROOT_STANDARD_INFO>() + MAX_SYNC_ROOT_IDENTITY_LEN;
	// `u64`s, so the buffer is aligned for the struct's `i64` field.
	let mut buffer = vec![0_u64; bytes_len.div_ceil(size_of::<u64>())];
	let Ok(buffer_len) = u32::try_from(buffer.len() * size_of::<u64>()) else {
		return Err(Hresult::new(E_INVALIDARG));
	};
	let mut returned = 0_u32;
	// SAFETY: `path` is NUL-terminated by `WideNul`'s construction and borrowed for the call; the
	// buffer is exclusively borrowed, aligned for the struct and `buffer_len` bytes long;
	// `returned` is a live out-param.
	let hr = Hresult::new(unsafe {
		CfGetSyncRootInfoByPath(
			path.with_nul().as_ptr(),
			CF_SYNC_ROOT_INFO_STANDARD,
			buffer.as_mut_ptr().cast(),
			buffer_len,
			&mut returned,
		)
	});
	if !hr.is_ok() {
		return Err(hr);
	}
	let bytes: Vec<u8> = buffer.iter().flat_map(|word| word.to_ne_bytes()).collect();
	let returned = usize::try_from(returned).map_or(bytes.len(), |len| len.min(bytes.len()));
	Ok(bytes.get(..returned).unwrap_or_default().to_vec())
}

/// Whether this process's token is elevated (`TokenElevation`): an administrator's process run
/// as administrator, and the SYSTEM account.
#[allow(unsafe_code)] // Win32 FFI: reads the process token's elevation.
pub(super) fn token_elevated() -> io::Result<bool> {
	let mut token: HANDLE = ptr::null_mut();
	// SAFETY: `GetCurrentProcess` returns a pseudo-handle that needs no closing; `token` is a live,
	// exclusively borrowed out-param.
	if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
		return Err(io::Error::last_os_error());
	}
	// SAFETY: `OpenProcessToken` succeeded, so `token` is an open handle this function owns; the
	// `OwnedHandle` closes it once, on every path below.
	let token = unsafe { OwnedHandle::from_raw_handle(token) };
	let mut elevation = TOKEN_ELEVATION::default();
	let mut returned = 0_u32;
	// SAFETY: the token handle is open for the call; `elevation` is an exclusively borrowed
	// `TOKEN_ELEVATION`, the struct `TokenElevation` fills, of the size passed; `returned` is a
	// live out-param.
	let ok = unsafe {
		GetTokenInformation(
			token.as_raw_handle(),
			TokenElevation,
			ptr::from_mut(&mut elevation).cast(),
			struct_size::<TOKEN_ELEVATION>(),
			&mut returned,
		)
	};
	if ok == 0 {
		Err(io::Error::last_os_error())
	} else {
		Ok(elevation.TokenIsElevated != 0)
	}
}

/// The name of the file system of the volume `file` is on (`NTFS`, `ReFS`, `FAT32`), from
/// `GetVolumeInformationByHandleW`.
#[allow(unsafe_code)] // Win32 FFI: reads the file system of a handle's volume.
pub(super) fn volume_file_system(file: &File) -> io::Result<Vec<u16>> {
	// MAX_PATH + 1, the size the call's documentation asks for.
	let mut name = [0_u16; 261];
	let Ok(name_len) = u32::try_from(name.len()) else {
		return Err(io::Error::from(io::ErrorKind::InvalidInput));
	};
	// SAFETY: the handle is borrowed from a live `File` for the call; `name` is an exclusively
	// borrowed buffer of `name_len` units; every other buffer and out-param is null, which the
	// call accepts.
	let ok = unsafe {
		GetVolumeInformationByHandleW(
			file.as_raw_handle(),
			ptr::null_mut(),
			0,
			ptr::null_mut(),
			ptr::null_mut(),
			ptr::null_mut(),
			name.as_mut_ptr(),
			name_len,
		)
	};
	if ok == 0 {
		return Err(io::Error::last_os_error());
	}
	take_until_nul(name.iter().copied(), name.len())
		.ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))
}

/// `CfRevertPlaceholder` of `file`, synchronously: the placeholder becomes an ordinary file or
/// directory, its data and attributes kept. Fails for a file whose data is not all on disk.
#[allow(unsafe_code)] // cfapi FFI: makes a placeholder an ordinary item.
pub(super) fn revert(file: &File) -> Hresult {
	// SAFETY: the handle is borrowed from a live `File` for the call, and there is no OVERLAPPED,
	// so the call completes before it returns.
	Hresult::new(unsafe {
		CfRevertPlaceholder(file.as_raw_handle(), CF_REVERT_FLAG_NONE, ptr::null_mut())
	})
}

/// A registry hive the provider writes under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Hive {
	/// `HKEY_CURRENT_USER`: the user this process runs as.
	CurrentUser,
	/// `HKEY_LOCAL_MACHINE`.
	LocalMachine,
}

impl Hive {
	fn key(self) -> HKEY {
		match self {
			Self::CurrentUser => HKEY_CURRENT_USER,
			Self::LocalMachine => HKEY_LOCAL_MACHINE,
		}
	}
}

/// A registry call's result as an I/O result.
fn win32(code: WIN32_ERROR) -> io::Result<()> {
	if code == ERROR_SUCCESS {
		Ok(())
	} else {
		Err(io::Error::from_raw_os_error(code.cast_signed()))
	}
}

/// An open registry key, closed when dropped.
pub(super) struct RegKey(HKEY);

impl Drop for RegKey {
	#[allow(unsafe_code)] // Win32 FFI: closes a registry key.
	fn drop(&mut self) {
		// SAFETY: `self.0` is a key `reg_create` or `reg_open` opened, owned by this value alone,
		// closed once, here.
		unsafe { RegCloseKey(self.0) };
	}
}

/// The key `path` below `hive`, created when missing, open to read and write.
#[allow(unsafe_code)] // Win32 FFI: creates or opens a registry key.
pub(super) fn reg_create(hive: Hive, path: &WideNul) -> io::Result<RegKey> {
	let mut key: HKEY = ptr::null_mut();
	// SAFETY: `path` is NUL-terminated and borrowed for the call; no class or security
	// attributes are passed, which the call accepts as null; `key` is a live, exclusively
	// borrowed out-param; the disposition is not asked for.
	win32(unsafe {
		RegCreateKeyExW(
			hive.key(),
			path.with_nul().as_ptr(),
			0,
			ptr::null(),
			REG_OPTION_NON_VOLATILE,
			KEY_READ | KEY_WRITE,
			ptr::null(),
			&mut key,
			ptr::null_mut(),
		)
	})?;
	Ok(RegKey(key))
}

/// The key `path` below the open key or hive `parent`, open to read (`KEY_READ`); `None` when it
/// is not there.
///
/// # Safety
///
/// `parent` is a predefined hive or a key open for the call.
#[allow(unsafe_code)] // Win32 FFI: opens a registry key.
unsafe fn open_below(parent: HKEY, path: &WideNul) -> io::Result<Option<RegKey>> {
	let mut key: HKEY = ptr::null_mut();
	// SAFETY: `parent` is valid for the call (the caller's promise); `path` is NUL-terminated and
	// borrowed for it; `key` is a live, exclusively borrowed out-param.
	let code = unsafe { RegOpenKeyExW(parent, path.with_nul().as_ptr(), 0, KEY_READ, &mut key) };
	match code {
		ERROR_SUCCESS => Ok(Some(RegKey(key))),
		ERROR_FILE_NOT_FOUND => Ok(None),
		code => Err(io::Error::from_raw_os_error(code.cast_signed())),
	}
}

/// The key `path` below `hive`, open to read; `None` when it is not there.
#[allow(unsafe_code)] // Win32 FFI: the hive entry to `open_below`.
pub(super) fn reg_open(hive: Hive, path: &WideNul) -> io::Result<Option<RegKey>> {
	// SAFETY: a predefined hive.
	unsafe { open_below(hive.key(), path) }
}

/// The key `path` below the open key `key`, open to read; `None` when it is not there.
#[allow(unsafe_code)] // Win32 FFI: the subkey entry to `open_below`.
pub(super) fn reg_open_subkey(key: &RegKey, path: &WideNul) -> io::Result<Option<RegKey>> {
	// SAFETY: `key` is open while borrowed.
	unsafe { open_below(key.0, path) }
}

/// Sets the value `name` of `key` (its default value when `None`) to `bytes`, of type `kind`.
#[allow(unsafe_code)] // Win32 FFI: writes a registry value.
fn reg_set(
	key: &RegKey,
	name: Option<&WideNul>,
	kind: REG_VALUE_TYPE,
	bytes: &[u8],
) -> io::Result<()> {
	let len =
		u32::try_from(bytes.len()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
	let name = name.map_or(ptr::null(), |name| name.with_nul().as_ptr());
	// SAFETY: the key is open for the call; `name` is null or NUL-terminated and borrowed for it;
	// `bytes` is valid for `len` bytes, which the call copies before it returns.
	win32(unsafe { RegSetValueExW(key.0, name, 0, kind, bytes.as_ptr(), len) })
}

/// Sets the string value `name` of `key` (its default value when `None`) to `value`: a
/// `REG_EXPAND_SZ` when `expand`, else a `REG_SZ`.
pub(super) fn reg_set_string(
	key: &RegKey,
	name: Option<&WideNul>,
	value: &WideNul,
	expand: bool,
) -> io::Result<()> {
	let bytes: Vec<u8> = value
		.with_nul()
		.iter()
		.flat_map(|unit| unit.to_le_bytes())
		.collect();
	reg_set(
		key,
		name,
		if expand { REG_EXPAND_SZ } else { REG_SZ },
		&bytes,
	)
}

/// Sets the `REG_DWORD` value `name` of `key` to `value`.
pub(super) fn reg_set_dword(key: &RegKey, name: &WideNul, value: u32) -> io::Result<()> {
	reg_set(key, Some(name), REG_DWORD, &value.to_le_bytes())
}

/// Deletes the key `path` below `hive` with every key and value below it; a key that is not there
/// is gone already.
#[allow(unsafe_code)] // Win32 FFI: deletes a registry key and its subkeys.
pub(super) fn reg_delete_tree(hive: Hive, path: &WideNul) -> io::Result<()> {
	// SAFETY: `path` is NUL-terminated and borrowed for the call.
	match unsafe { RegDeleteTreeW(hive.key(), path.with_nul().as_ptr()) } {
		ERROR_SUCCESS | ERROR_FILE_NOT_FOUND => Ok(()),
		code => Err(io::Error::from_raw_os_error(code.cast_signed())),
	}
}

/// Deletes the value `name` of the key `path` below `hive`; one that is not there is gone already.
#[allow(unsafe_code)] // Win32 FFI: deletes a registry value.
pub(super) fn reg_delete_value(hive: Hive, path: &WideNul, name: &WideNul) -> io::Result<()> {
	// SAFETY: `path` and `name` are NUL-terminated and borrowed for the call.
	match unsafe {
		RegDeleteKeyValueW(
			hive.key(),
			path.with_nul().as_ptr(),
			name.with_nul().as_ptr(),
		)
	} {
		ERROR_SUCCESS | ERROR_FILE_NOT_FOUND => Ok(()),
		code => Err(io::Error::from_raw_os_error(code.cast_signed())),
	}
}

/// The longest key name the registry holds, its NUL included.
const MAX_KEY_NAME_UNITS: usize = 256;

/// The names of `key`'s subkeys.
#[allow(unsafe_code)] // Win32 FFI: lists a registry key's subkeys.
pub(super) fn reg_subkeys(key: &RegKey) -> io::Result<Vec<Vec<u16>>> {
	let mut names = Vec::new();
	for index in 0_u32.. {
		let mut name = [0_u16; MAX_KEY_NAME_UNITS];
		let mut len =
			u32::try_from(name.len()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
		// SAFETY: the key is open for the call; `name` is an exclusively borrowed buffer of `len`
		// units, which the call sets to the units written; no class or time is asked for.
		let code = unsafe {
			RegEnumKeyExW(
				key.0,
				index,
				name.as_mut_ptr(),
				&mut len,
				ptr::null(),
				ptr::null_mut(),
				ptr::null_mut(),
				ptr::null_mut(),
			)
		};
		match code {
			ERROR_SUCCESS => {}
			ERROR_NO_MORE_ITEMS => break,
			code => return Err(io::Error::from_raw_os_error(code.cast_signed())),
		}
		let len = usize::try_from(len).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
		names.push(
			name.get(..len)
				.ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?
				.to_vec(),
		);
	}
	Ok(names)
}

/// The most bytes a string value is read for: a 32,767-unit path and its NUL, with room to spare.
const MAX_STRING_VALUE_BYTES: usize = 65_540;

/// The units of a string value's bytes, without the NULs that end it.
fn string_units(bytes: &[u8]) -> Vec<u16> {
	let mut units: Vec<u16> = bytes
		.chunks_exact(2)
		.filter_map(|pair| pair.first_chunk().copied().map(u16::from_le_bytes))
		.collect();
	while units.last() == Some(&0) {
		units.pop();
	}
	units
}

/// The value `name` of `key` (its default value when `None`): `Ok(None)` when it is not there.
#[allow(unsafe_code)] // Win32 FFI: reads a registry value.
fn reg_query(
	key: &RegKey,
	name: Option<&WideNul>,
) -> io::Result<Option<(REG_VALUE_TYPE, Vec<u8>)>> {
	let mut data = vec![0_u8; MAX_STRING_VALUE_BYTES];
	let mut len =
		u32::try_from(data.len()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
	let mut kind: REG_VALUE_TYPE = 0;
	let name = name.map_or(ptr::null(), |name| name.with_nul().as_ptr());
	// SAFETY: the key is open for the call; `name` is null or NUL-terminated and borrowed for it;
	// `data` is an exclusively borrowed buffer of `len` bytes, which the call sets to the bytes
	// written; `kind` is a live out-param.
	let code = unsafe {
		RegQueryValueExW(
			key.0,
			name,
			ptr::null(),
			&mut kind,
			data.as_mut_ptr(),
			&mut len,
		)
	};
	match code {
		ERROR_SUCCESS => {}
		ERROR_FILE_NOT_FOUND => return Ok(None),
		code => return Err(io::Error::from_raw_os_error(code.cast_signed())),
	}
	let len = usize::try_from(len).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
	data.truncate(len);
	Ok(Some((kind, data)))
}

/// The string value `name` of `key` (`REG_SZ` or `REG_EXPAND_SZ`, unexpanded), without its NUL;
/// `None` when it is not there or not a string.
pub(super) fn reg_query_string(
	key: &RegKey,
	name: Option<&WideNul>,
) -> io::Result<Option<Vec<u16>>> {
	Ok(reg_query(key, name)?
		.filter(|(kind, _)| matches!(*kind, REG_SZ | REG_EXPAND_SZ))
		.map(|(_, bytes)| string_units(&bytes)))
}

/// The `REG_DWORD` value `name` of `key`; `None` when it is not there or not a `REG_DWORD`.
pub(super) fn reg_query_dword(key: &RegKey, name: &WideNul) -> io::Result<Option<u32>> {
	Ok(reg_query(key, Some(name))?
		.filter(|(kind, _)| *kind == REG_DWORD)
		.and_then(|(_, bytes)| bytes.first_chunk().copied().map(u32::from_le_bytes)))
}

/// The longest value name the registry holds, its NUL included.
const MAX_VALUE_NAME_UNITS: usize = 16_384;

/// Every string value of `key` (`REG_SZ` or `REG_EXPAND_SZ`, unexpanded) by name, without NULs;
/// values of other types are left out.
#[allow(unsafe_code)] // Win32 FFI: lists a registry key's values.
pub(super) fn reg_string_values(key: &RegKey) -> io::Result<Vec<(Vec<u16>, Vec<u16>)>> {
	let mut values = Vec::new();
	let mut name = vec![0_u16; MAX_VALUE_NAME_UNITS];
	let mut data = vec![0_u8; MAX_STRING_VALUE_BYTES];
	for index in 0_u32.. {
		let mut name_len =
			u32::try_from(name.len()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
		let mut data_len =
			u32::try_from(data.len()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
		let mut kind: u32 = 0;
		// SAFETY: the key is open for the call; `name` and `data` are exclusively borrowed buffers
		// of `name_len` units and `data_len` bytes, which the call sets to what it wrote; `kind`
		// is a live out-param.
		let code = unsafe {
			RegEnumValueW(
				key.0,
				index,
				name.as_mut_ptr(),
				&mut name_len,
				ptr::null(),
				&mut kind,
				data.as_mut_ptr(),
				&mut data_len,
			)
		};
		match code {
			ERROR_SUCCESS => {}
			ERROR_NO_MORE_ITEMS => break,
			// A value too long to be a path is no root of this provider's.
			ERROR_MORE_DATA => continue,
			code => return Err(io::Error::from_raw_os_error(code.cast_signed())),
		}
		if !matches!(kind, REG_SZ | REG_EXPAND_SZ) {
			continue;
		}
		let name_len =
			usize::try_from(name_len).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
		let data_len =
			usize::try_from(data_len).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
		let invalid = || io::Error::from(io::ErrorKind::InvalidData);
		values.push((
			name.get(..name_len).ok_or_else(invalid)?.to_vec(),
			string_units(data.get(..data_len).ok_or_else(invalid)?),
		));
	}
	Ok(values)
}

/// The string SID of the user this process runs as (`S-1-5-21-...`), from its token.
#[allow(unsafe_code)] // Win32 FFI: reads the process token's user and spells its SID.
pub(super) fn current_user_sid() -> io::Result<Vec<u16>> {
	let mut token: HANDLE = ptr::null_mut();
	// SAFETY: as in `token_elevated`.
	if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
		return Err(io::Error::last_os_error());
	}
	// SAFETY: as in `token_elevated`: the handle is open and owned here, closed once.
	let token = unsafe { OwnedHandle::from_raw_handle(token) };
	// A `TOKEN_USER` and the SID it points into, which the call writes after it; u64s, so the
	// struct's pointer is aligned. 128 bytes hold the 16 of a `TOKEN_USER` and the 68 of a SID of
	// the 15 sub-authorities a SID can have.
	let mut buffer = [0_u64; 16];
	let mut returned = 0_u32;
	let size = u32::try_from(size_of_val(&buffer))
		.map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
	// SAFETY: the token handle is open for the call; `buffer` is exclusively borrowed, `size`
	// bytes long and aligned for `TOKEN_USER`; `returned` is a live out-param.
	let ok = unsafe {
		GetTokenInformation(
			token.as_raw_handle(),
			TokenUser,
			buffer.as_mut_ptr().cast(),
			size,
			&mut returned,
		)
	};
	if ok == 0 {
		return Err(io::Error::last_os_error());
	}
	// SAFETY: the call succeeded, so the buffer starts with a `TOKEN_USER` whose `Sid` points at
	// a valid SID inside the same buffer, which lives until the end of this function.
	let sid = unsafe { buffer.as_ptr().cast::<TOKEN_USER>().read() }
		.User
		.Sid;
	// SAFETY: as above, `sid` is valid while `buffer` lives, past this call.
	unsafe { sid_string(sid) }
}

/// The string form of `sid` (`S-1-5-21-...`).
///
/// # Safety
///
/// `sid` points at a valid SID for the call.
#[allow(unsafe_code)] // Win32 FFI: spells a SID.
unsafe fn sid_string(sid: PSID) -> io::Result<Vec<u16>> {
	let mut text: PWSTR = ptr::null_mut();
	// SAFETY: `sid` is a valid SID (the caller's promise); `text` is a live out-param, which the
	// call sets to a string it allocated with `LocalAlloc`.
	if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
		return Err(io::Error::last_os_error());
	}
	// SAFETY: the call succeeded, so `text` is a NUL-terminated string, read before it is freed.
	let units = unsafe { read_wide(text) };
	// SAFETY: `text` was allocated by `ConvertSidToStringSidW` with `LocalAlloc`, freed once, here.
	unsafe { LocalFree(text.cast()) };
	units.ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))
}

/// The string SID of the owner of the open key `key`.
#[allow(unsafe_code)] // Win32 FFI: reads a registry key's owner.
pub(super) fn reg_owner_sid(key: &RegKey) -> io::Result<Vec<u16>> {
	let mut owner: PSID = ptr::null_mut();
	let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
	// SAFETY: the key is open for the call, with `READ_CONTROL` (in `KEY_READ`, which every key of
	// this module is opened with), and a key handle is the handle `SE_REGISTRY_KEY` takes; `owner`
	// and `descriptor` are live out-params; the group and the ACLs are not asked for, which the
	// call accepts as null. On success `owner` points into `descriptor`, which the call allocated
	// with `LocalAlloc`.
	win32(unsafe {
		GetSecurityInfo(
			key.0,
			SE_REGISTRY_KEY,
			OWNER_SECURITY_INFORMATION,
			&mut owner,
			ptr::null_mut(),
			ptr::null_mut(),
			ptr::null_mut(),
			&mut descriptor,
		)
	})?;
	// SAFETY: `owner` points into `descriptor`, freed only after this call.
	let units = unsafe { sid_string(owner) };
	// SAFETY: `descriptor` was allocated by `GetSecurityInfo` with `LocalAlloc`, freed once, here,
	// after the last use of `owner`.
	unsafe { LocalFree(descriptor.cast()) };
	units
}

/// Tells the shell the directory `path` changed (`SHCNE_UPDATEDIR`), so the windows showing it
/// read it again.
#[allow(unsafe_code)] // Win32 FFI: notifies the shell of a changed directory.
pub(super) fn notify_dir_updated(path: &WideNul) {
	// SAFETY: `path` is NUL-terminated and borrowed for the call, which reads it before it
	// returns, as `SHCNF_PATHW` says; the second item is unused by `SHCNE_UPDATEDIR`.
	unsafe {
		SHChangeNotify(
			SHCNE_UPDATEDIR.cast_signed(),
			SHCNF_PATHW,
			path.with_nul().as_ptr().cast(),
			ptr::null(),
		);
	}
}

/// `SECURITY_DESCRIPTOR_REVISION`.
#[cfg(test)]
const SECURITY_DESCRIPTOR_REVISION: u32 = 1;

/// Replaces the DACL of the key `path` below `hive`, which this process's user owns: an empty one
/// when `deny_all`, so no one may open it, else none, so everyone may. For tests of a key that
/// cannot be read.
#[cfg(test)]
#[allow(unsafe_code)] // Win32 FFI: replaces a registry key's DACL, for tests.
pub(super) fn reg_set_dacl_for_test(hive: Hive, path: &WideNul, deny_all: bool) -> io::Result<()> {
	let mut key: HKEY = ptr::null_mut();
	// SAFETY: `path` is NUL-terminated and borrowed for the call; `key` is a live, exclusively
	// borrowed out-param. An owner may always open its key for `WRITE_DAC`.
	win32(unsafe { RegOpenKeyExW(hive.key(), path.with_nul().as_ptr(), 0, WRITE_DAC, &mut key) })?;
	let key = RegKey(key);
	let mut acl = ACL::default();
	let acl_size = u32::try_from(size_of::<ACL>())
		.map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
	// SAFETY: `acl` is a live, exclusively borrowed buffer of `acl_size` bytes, which the call
	// makes an ACL with no entry.
	if unsafe { InitializeAcl(&raw mut acl, acl_size, ACL_REVISION) } == 0 {
		return Err(io::Error::last_os_error());
	}
	let mut descriptor = SECURITY_DESCRIPTOR::default();
	let descriptor_ptr: PSECURITY_DESCRIPTOR = (&raw mut descriptor).cast();
	// SAFETY: `descriptor_ptr` points at a live, exclusively borrowed `SECURITY_DESCRIPTOR`.
	if unsafe { InitializeSecurityDescriptor(descriptor_ptr, SECURITY_DESCRIPTOR_REVISION) } == 0 {
		return Err(io::Error::last_os_error());
	}
	let dacl: *const ACL = if deny_all {
		&raw const acl
	} else {
		ptr::null()
	};
	// SAFETY: the descriptor was initialised above; `dacl` is null (no DACL) or the ACL above,
	// which outlives every use of the descriptor in this function.
	if unsafe { SetSecurityDescriptorDacl(descriptor_ptr, 1, dacl, 0) } == 0 {
		return Err(io::Error::last_os_error());
	}
	// SAFETY: the key is open with `WRITE_DAC`; the descriptor is a valid absolute descriptor whose
	// DACL lives for the call.
	win32(unsafe { RegSetKeySecurity(key.0, DACL_SECURITY_INFORMATION, descriptor_ptr) })
}

/// The keys a callback's answers are sent under, copied out of its `CF_CALLBACK_INFO` before the
/// callback returns. `RequestKey` was measured to be 0 on every data callback; it is passed back
/// as received.
#[derive(Debug, Clone, Copy)]
pub(super) struct Keys {
	connection: i64,
	transfer: i64,
	request: i64,
}

impl Keys {
	/// The transfer key the OS handed over.
	pub(super) fn transfer(self) -> i64 {
		self.transfer
	}
}

#[cfg(test)]
impl Keys {
	/// The connection, transfer and request keys, for tests.
	pub(super) fn all(self) -> [i64; 3] {
		[self.connection, self.transfer, self.request]
	}
}

/// The callbacks every connection registers, ended by the `CF_CALLBACK_TYPE_NONE` entry cfapi
/// expects: the data and population requests, the delete and rename notifications the platform
/// waits on an acknowledgement of, the rename and close completions, and the dehydration requests
/// the platform waits on an acknowledgement of. `'static`, so whatever cfapi keeps of it stays
/// valid.
static CALLBACKS: [CF_CALLBACK_REGISTRATION; 9] = [
	CF_CALLBACK_REGISTRATION {
		Type: CF_CALLBACK_TYPE_FETCH_DATA,
		Callback: Some(on_fetch_data),
	},
	CF_CALLBACK_REGISTRATION {
		Type: CF_CALLBACK_TYPE_CANCEL_FETCH_DATA,
		Callback: Some(on_cancel_fetch_data),
	},
	CF_CALLBACK_REGISTRATION {
		Type: CF_CALLBACK_TYPE_FETCH_PLACEHOLDERS,
		Callback: Some(on_fetch_placeholders),
	},
	CF_CALLBACK_REGISTRATION {
		Type: CF_CALLBACK_TYPE_NOTIFY_DELETE,
		Callback: Some(on_notify_delete),
	},
	CF_CALLBACK_REGISTRATION {
		Type: CF_CALLBACK_TYPE_NOTIFY_RENAME,
		Callback: Some(on_notify_rename),
	},
	CF_CALLBACK_REGISTRATION {
		Type: CF_CALLBACK_TYPE_NOTIFY_RENAME_COMPLETION,
		Callback: Some(on_rename_completion),
	},
	CF_CALLBACK_REGISTRATION {
		Type: CF_CALLBACK_TYPE_NOTIFY_FILE_CLOSE_COMPLETION,
		Callback: Some(on_close_completion),
	},
	CF_CALLBACK_REGISTRATION {
		Type: CF_CALLBACK_TYPE_NOTIFY_DEHYDRATE,
		Callback: Some(on_notify_dehydrate),
	},
	CF_CALLBACK_REGISTRATION {
		Type: CF_CALLBACK_TYPE_NONE,
		Callback: None,
	},
];

/// `CfConnectSyncRoot` of `path`, with this module's callbacks and `slot` as their context, asking
/// for each callback's process and full path. The connection key on success.
#[allow(unsafe_code)] // cfapi FFI: connects a sync root with this module's callbacks.
pub(super) fn connect(path: &WideNul, slot: &'static ContextSlot) -> Result<i64, Hresult> {
	let mut key: CF_CONNECTION_KEY = 0;
	// SAFETY: `path` is NUL-terminated by `WideNul`'s construction and borrowed for the call. The
	// callback table, ended by its NONE entry, and the context are `'static`, so whatever cfapi
	// keeps of them stays valid for the connection's life and after it. `key` is a live,
	// exclusively borrowed out-param.
	let hr = Hresult::new(unsafe {
		CfConnectSyncRoot(
			path.with_nul().as_ptr(),
			CALLBACKS.as_ptr(),
			ptr::from_ref(slot).cast(),
			CF_CONNECT_FLAG_REQUIRE_PROCESS_INFO | CF_CONNECT_FLAG_REQUIRE_FULL_FILE_PATH,
			&mut key,
		)
	});
	if hr.is_ok() { Ok(key) } else { Err(hr) }
}

/// `CfDisconnectSyncRoot` of the connection `key`.
#[allow(unsafe_code)] // cfapi FFI: disconnects a sync root.
pub(super) fn disconnect(key: i64) -> Hresult {
	// SAFETY: takes a plain key; cfapi checks it.
	Hresult::new(unsafe { CfDisconnectSyncRoot(key) })
}

/// The `ParamSize` of a `CF_OPERATION_PARAMETERS` carrying the variant `T`.
fn param_size<T>() -> u32 {
	u32::try_from(offset_of!(CF_OPERATION_PARAMETERS, Anonymous) + size_of::<T>())
		.expect("a cfapi struct is a few hundred bytes (should be impossible)")
}

/// `CfExecute` of `operation` under `keys`, with no correlation vector (measured to be accepted
/// from outside the callback).
///
/// # Safety
///
/// Every pointer in `params` is valid, for the whole call, for what `operation` reads through it
/// and writes through it, and the variant of `params` written is the one `operation` reads.
#[allow(unsafe_code)] // cfapi FFI: the one CfExecute call every answer goes through.
unsafe fn execute(
	keys: &Keys,
	operation: CF_OPERATION_TYPE,
	params: &mut CF_OPERATION_PARAMETERS,
) -> Hresult {
	let info = CF_OPERATION_INFO {
		StructSize: struct_size::<CF_OPERATION_INFO>(),
		Type: operation,
		ConnectionKey: keys.connection,
		TransferKey: keys.transfer,
		CorrelationVector: ptr::null(),
		SyncStatus: ptr::null(),
		RequestKey: keys.request,
	};
	// SAFETY: `info` is live for the call and holds no pointer; `params` is as the caller
	// guarantees.
	Hresult::new(unsafe { CfExecute(&info, params) })
}

/// `TRANSFER_DATA` of `bytes` at `offset`, answering part of a `FETCH_DATA` request.
#[allow(unsafe_code)] // cfapi FFI: hands the OS a piece of a file.
pub(super) fn transfer_data(keys: &Keys, offset: i64, bytes: &[u8]) -> Hresult {
	let Ok(length) = i64::try_from(bytes.len()) else {
		return Hresult::new(E_INVALIDARG);
	};
	let mut params = CF_OPERATION_PARAMETERS {
		ParamSize: param_size::<CF_OPERATION_PARAMETERS_0_0>(),
		Anonymous: CF_OPERATION_PARAMETERS_0 {
			TransferData: CF_OPERATION_PARAMETERS_0_0 {
				Flags: CF_OPERATION_TRANSFER_DATA_FLAG_NONE,
				CompletionStatus: STATUS_SUCCESS,
				Buffer: bytes.as_ptr().cast(),
				Offset: offset,
				Length: length,
			},
		},
	};
	// SAFETY: `Buffer` points at `bytes`, valid for `Length` bytes and borrowed for the call, and
	// cfapi copies them into the file before returning. The variant written is the one
	// TRANSFER_DATA reads.
	unsafe { execute(keys, CF_OPERATION_TYPE_TRANSFER_DATA, &mut params) }
}

/// `TRANSFER_DATA` failing `len` bytes at `offset` with `status`, a failure `NTSTATUS`: no buffer.
#[allow(unsafe_code)] // cfapi FFI: fails a FETCH_DATA request.
pub(super) fn fail_data(keys: &Keys, status: i32, offset: i64, len: i64) -> Hresult {
	let mut params = CF_OPERATION_PARAMETERS {
		ParamSize: param_size::<CF_OPERATION_PARAMETERS_0_0>(),
		Anonymous: CF_OPERATION_PARAMETERS_0 {
			TransferData: CF_OPERATION_PARAMETERS_0_0 {
				Flags: CF_OPERATION_TRANSFER_DATA_FLAG_NONE,
				CompletionStatus: status,
				Buffer: ptr::null(),
				Offset: offset,
				Length: len,
			},
		},
	};
	// SAFETY: the only pointer is a null buffer, which a failure status takes (measured). The
	// variant written is the one TRANSFER_DATA reads.
	unsafe { execute(keys, CF_OPERATION_TYPE_TRANSFER_DATA, &mut params) }
}

/// What a `TRANSFER_PLACEHOLDERS` call returned.
pub(super) struct PlaceholderTransfer {
	/// The call's result.
	pub(super) hr: Hresult,
	/// Each placeholder's own result, in order.
	pub(super) results: Vec<i32>,
	/// How many placeholders the OS processed.
	pub(super) processed: u32,
}

/// The cfapi entries `specs` are created with, each identity pointing into the matching blob of
/// `blobs` (`specs` encoded, in order) and each name into its spec: both must outlive every use of
/// the entries. `E_INVALIDARG` for an identity longer than a `u32` counts.
fn placeholder_entries(
	specs: &[PlaceholderSpec],
	blobs: &[IdentityBlob],
) -> Result<Vec<CF_PLACEHOLDER_CREATE_INFO>, Hresult> {
	specs
		.iter()
		.zip(blobs)
		.map(|(spec, blob)| {
			let identity_len =
				u32::try_from(blob.as_bytes().len()).map_err(|_| Hresult::new(E_INVALIDARG))?;
			let values = create_values(spec);
			Ok(CF_PLACEHOLDER_CREATE_INFO {
				RelativeFileName: spec.name().as_wide_nul().as_ptr(),
				FsMetadata: CF_FS_METADATA {
					BasicInfo: FILE_BASIC_INFO {
						CreationTime: values.created,
						LastAccessTime: values.modified,
						LastWriteTime: values.modified,
						ChangeTime: values.modified,
						FileAttributes: values.attributes,
					},
					FileSize: values.size,
				},
				FileIdentity: blob.as_bytes().as_ptr().cast(),
				FileIdentityLength: identity_len,
				Flags: values.flags,
				Result: 0,
				CreateUsn: 0,
			})
		})
		.collect()
}

/// `TRANSFER_PLACEHOLDERS` of `specs`, answering a `FETCH_PLACEHOLDERS` request, with on-demand
/// population turned off for the directory, so it is not asked for again. An empty `specs` sends
/// a null array.
#[allow(unsafe_code)] // cfapi FFI: creates a directory's placeholders.
pub(super) fn transfer_placeholders(keys: &Keys, specs: &[PlaceholderSpec]) -> PlaceholderTransfer {
	let refused = |hr| PlaceholderTransfer {
		hr,
		results: Vec::new(),
		processed: 0,
	};
	// Owned here, so every identity pointer below outlives the call.
	let blobs: Vec<IdentityBlob> = specs.iter().map(|spec| spec.identity().encode()).collect();
	let mut entries = match placeholder_entries(specs, &blobs) {
		Ok(entries) => entries,
		Err(hr) => return refused(hr),
	};
	let (Ok(total), Ok(count)) = (i64::try_from(entries.len()), u32::try_from(entries.len()))
	else {
		return refused(Hresult::new(E_INVALIDARG));
	};
	let array = if entries.is_empty() {
		ptr::null_mut()
	} else {
		entries.as_mut_ptr()
	};
	let mut params = CF_OPERATION_PARAMETERS {
		ParamSize: param_size::<CF_OPERATION_PARAMETERS_0_4>(),
		Anonymous: CF_OPERATION_PARAMETERS_0 {
			TransferPlaceholders: CF_OPERATION_PARAMETERS_0_4 {
				Flags: CF_OPERATION_TRANSFER_PLACEHOLDERS_FLAG_DISABLE_ON_DEMAND_POPULATION,
				CompletionStatus: STATUS_SUCCESS,
				PlaceholderTotalCount: total,
				PlaceholderArray: array,
				PlaceholderCount: count,
				EntriesProcessed: 0,
			},
		},
	};
	// SAFETY: the array is `entries`, exclusively borrowed for the call, which cfapi writes each
	// entry's `Result` and `CreateUsn` into. Each entry's name points into a `PlaceholderName`'s
	// NUL-terminated wide form and its identity into `blobs`, valid for `FileIdentityLength`
	// bytes; `specs` and `blobs` outlive the call. The variant written is the one
	// TRANSFER_PLACEHOLDERS reads.
	let hr = unsafe { execute(keys, CF_OPERATION_TYPE_TRANSFER_PLACEHOLDERS, &mut params) };
	// SAFETY: the variant read is the one written above, which cfapi updates in place.
	let processed = unsafe { params.Anonymous.TransferPlaceholders.EntriesProcessed };
	PlaceholderTransfer {
		hr,
		results: entries.iter().map(|entry| entry.Result).collect(),
		processed,
	}
}

/// `TRANSFER_PLACEHOLDERS` failing a `FETCH_PLACEHOLDERS` request with `status`, a failure
/// `NTSTATUS`: no placeholders, and on-demand population left on, so the OS asks again.
#[allow(unsafe_code)] // cfapi FFI: fails a FETCH_PLACEHOLDERS request.
pub(super) fn fail_placeholders(keys: &Keys, status: i32) -> Hresult {
	let mut params = CF_OPERATION_PARAMETERS {
		ParamSize: param_size::<CF_OPERATION_PARAMETERS_0_4>(),
		Anonymous: CF_OPERATION_PARAMETERS_0 {
			TransferPlaceholders: CF_OPERATION_PARAMETERS_0_4 {
				Flags: CF_OPERATION_TRANSFER_PLACEHOLDERS_FLAG_NONE,
				CompletionStatus: status,
				PlaceholderTotalCount: 0,
				PlaceholderArray: ptr::null_mut(),
				PlaceholderCount: 0,
				EntriesProcessed: 0,
			},
		},
	};
	// SAFETY: the only pointer is a null array with a count of 0. The variant written is the one
	// TRANSFER_PLACEHOLDERS reads.
	unsafe { execute(keys, CF_OPERATION_TYPE_TRANSFER_PLACEHOLDERS, &mut params) }
}

/// `CfDehydratePlaceholder` of the whole of `file`, synchronously. Measured to block until every
/// transfer of the file in flight is answered, then to fail with `ERROR_CLOUD_FILE_IN_USE`.
#[allow(unsafe_code)] // cfapi FFI: dehydrates a placeholder.
pub(super) fn dehydrate(file: &File) -> Hresult {
	// SAFETY: the handle is borrowed from a live `File` for the call, and there is no OVERLAPPED,
	// so the call completes before it returns.
	Hresult::new(unsafe {
		CfDehydratePlaceholder(
			file.as_raw_handle(),
			0,
			-1,
			CF_DEHYDRATE_FLAG_NONE,
			ptr::null_mut(),
		)
	})
}

/// `CfSetPinState` of `state` on `file` alone, synchronously. Measured (Windows 11, in the VM): it
/// hydrates and dehydrates nothing, raises no callback, works through a handle opened for reading
/// attributes only, and leaves the in-sync state and the modified bytes alone.
#[allow(unsafe_code)] // cfapi FFI: sets a placeholder's pin state.
pub(super) fn set_pin_state(file: &File, state: CF_PIN_STATE) -> Hresult {
	// SAFETY: the handle is borrowed from a live `File` for the call, and there is no OVERLAPPED,
	// so the call completes before it returns.
	Hresult::new(unsafe {
		CfSetPinState(
			file.as_raw_handle(),
			state,
			CF_SET_PIN_FLAG_NONE,
			ptr::null_mut(),
		)
	})
}

/// `ACK_DELETE` with `status`: `STATUS_SUCCESS` lets the delete go ahead, a failure status vetoes
/// it. Measured to work from another thread after the callback returned.
#[allow(unsafe_code)] // cfapi FFI: answers a NOTIFY_DELETE.
pub(super) fn ack_delete(keys: &Keys, status: i32) -> Hresult {
	let mut params = CF_OPERATION_PARAMETERS {
		ParamSize: param_size::<CF_OPERATION_PARAMETERS_0_7>(),
		Anonymous: CF_OPERATION_PARAMETERS_0 {
			AckDelete: CF_OPERATION_PARAMETERS_0_7 {
				Flags: CF_OPERATION_ACK_DELETE_FLAG_NONE,
				CompletionStatus: status,
			},
		},
	};
	// SAFETY: the parameters hold no pointer. The variant written is the one ACK_DELETE reads.
	unsafe { execute(keys, CF_OPERATION_TYPE_ACK_DELETE, &mut params) }
}

/// `ACK_RENAME` with `status`: `STATUS_SUCCESS` lets the rename go ahead, a failure status vetoes
/// it. Measured to work from another thread after the callback returned.
#[allow(unsafe_code)] // cfapi FFI: answers a NOTIFY_RENAME.
pub(super) fn ack_rename(keys: &Keys, status: i32) -> Hresult {
	let mut params = CF_OPERATION_PARAMETERS {
		ParamSize: param_size::<CF_OPERATION_PARAMETERS_0_6>(),
		Anonymous: CF_OPERATION_PARAMETERS_0 {
			AckRename: CF_OPERATION_PARAMETERS_0_6 {
				Flags: CF_OPERATION_ACK_RENAME_FLAG_NONE,
				CompletionStatus: status,
			},
		},
	};
	// SAFETY: the parameters hold no pointer. The variant written is the one ACK_RENAME reads.
	unsafe { execute(keys, CF_OPERATION_TYPE_ACK_RENAME, &mut params) }
}

/// `ACK_DEHYDRATE` with `status` for the placeholder `identity`, or with none when the request
/// carried none the core could read: `STATUS_SUCCESS` lets the dehydration go ahead, a failure
/// status vetoes it. Measured (Windows 11, in the VM): accepted from another thread
/// after the callback returned, with or without the identity.
#[allow(unsafe_code)] // cfapi FFI: answers a NOTIFY_DEHYDRATE.
pub(super) fn ack_dehydrate(keys: &Keys, status: i32, identity: Option<&IdentityBlob>) -> Hresult {
	let bytes = identity.map_or(&[][..], IdentityBlob::as_bytes);
	let Ok(len) = u32::try_from(bytes.len()) else {
		return Hresult::new(E_INVALIDARG);
	};
	let mut params = CF_OPERATION_PARAMETERS {
		ParamSize: param_size::<CF_OPERATION_PARAMETERS_0_5>(),
		Anonymous: CF_OPERATION_PARAMETERS_0 {
			AckDehydrate: CF_OPERATION_PARAMETERS_0_5 {
				Flags: CF_OPERATION_ACK_DEHYDRATE_FLAG_NONE,
				CompletionStatus: status,
				FileIdentity: if bytes.is_empty() {
					ptr::null()
				} else {
					bytes.as_ptr().cast()
				},
				FileIdentityLength: len,
			},
		},
	};
	// SAFETY: the identity pointer is null with a length of 0, or valid for `len` bytes and
	// borrowed for the call, which cfapi copies what it keeps of before returning. The variant
	// written is the one ACK_DEHYDRATE reads.
	unsafe { execute(keys, CF_OPERATION_TYPE_ACK_DEHYDRATE, &mut params) }
}

/// What `CfGetPlaceholderInfo` reports of a placeholder (`CF_PLACEHOLDER_STANDARD_INFO`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StandardInfo {
	/// `OnDiskDataSize`.
	pub(super) on_disk: i64,
	/// `ModifiedDataSize`.
	pub(super) modified: i64,
	/// Whether `InSyncState` is `CF_IN_SYNC_STATE_IN_SYNC`.
	pub(super) in_sync: bool,
	/// The file identity's bytes.
	pub(super) identity: Vec<u8>,
}

/// The native-endian `i64` at `offset` of `bytes`, if they hold it.
fn i64_at(bytes: &[u8], offset: usize) -> Option<i64> {
	let end = offset.checked_add(size_of::<i64>())?;
	Some(i64::from_ne_bytes(bytes.get(offset..end)?.try_into().ok()?))
}

/// The native-endian `u32` at `offset` of `bytes`, if they hold it.
fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
	let end = offset.checked_add(size_of::<u32>())?;
	Some(u32::from_ne_bytes(bytes.get(offset..end)?.try_into().ok()?))
}

/// The standard info of `bytes`, the `returned` bytes `CfGetPlaceholderInfo` wrote: `None` when
/// they are too short for a field read or for the identity they say follows.
fn parse_standard_info(bytes: &[u8]) -> Option<StandardInfo> {
	let on_disk = i64_at(
		bytes,
		offset_of!(CF_PLACEHOLDER_STANDARD_INFO, OnDiskDataSize),
	)?;
	let modified = i64_at(
		bytes,
		offset_of!(CF_PLACEHOLDER_STANDARD_INFO, ModifiedDataSize),
	)?;
	let in_sync = u32_at(bytes, offset_of!(CF_PLACEHOLDER_STANDARD_INFO, InSyncState))?;
	let identity_len = u32_at(
		bytes,
		offset_of!(CF_PLACEHOLDER_STANDARD_INFO, FileIdentityLength),
	)?;
	let start = offset_of!(CF_PLACEHOLDER_STANDARD_INFO, FileIdentity);
	let end = start.checked_add(usize::try_from(identity_len).ok()?)?;
	Some(StandardInfo {
		on_disk,
		modified,
		in_sync: i32::try_from(in_sync).ok() == Some(CF_IN_SYNC_STATE_IN_SYNC),
		identity: bytes.get(start..end)?.to_vec(),
	})
}

/// `CfGetPlaceholderInfo(CF_PLACEHOLDER_INFO_STANDARD)` of `file`, with room for the longest
/// identity cfapi stores. `ERROR_NOT_A_CLOUD_FILE` for an ordinary file.
#[allow(unsafe_code)] // cfapi FFI: reads a placeholder's state.
pub(super) fn placeholder_info(file: &File) -> Result<StandardInfo, Hresult> {
	let bytes_len = size_of::<CF_PLACEHOLDER_STANDARD_INFO>() + MAX_IDENTITY_LEN;
	// `u64`s, so the buffer is aligned for the struct's `i64` fields.
	let mut buffer = vec![0_u64; bytes_len.div_ceil(size_of::<u64>())];
	let Ok(buffer_len) = u32::try_from(buffer.len() * size_of::<u64>()) else {
		return Err(Hresult::new(E_INVALIDARG));
	};
	let mut returned = 0_u32;
	// SAFETY: the handle is borrowed from a live `File` for the call; the buffer is exclusively
	// borrowed, aligned for the struct and `buffer_len` bytes long; `returned` is a live out-param.
	let hr = Hresult::new(unsafe {
		CfGetPlaceholderInfo(
			file.as_raw_handle(),
			CF_PLACEHOLDER_INFO_STANDARD,
			buffer.as_mut_ptr().cast(),
			buffer_len,
			&mut returned,
		)
	});
	if !hr.is_ok() {
		return Err(hr);
	}
	let bytes: Vec<u8> = buffer.iter().flat_map(|word| word.to_ne_bytes()).collect();
	let returned = usize::try_from(returned).map_or(bytes.len(), |len| len.min(bytes.len()));
	parse_standard_info(bytes.get(..returned).unwrap_or_default()).ok_or(Hresult::new(E_FAIL))
}

/// What `GetFileInformationByHandleEx` reports of a local item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct HandleInfo {
	/// `FileAttributes`.
	pub(super) attributes: u32,
	/// `ReparseTag`; meaningful only with `FILE_ATTRIBUTE_REPARSE_POINT`.
	pub(super) reparse_tag: u32,
	/// The 128-bit file id (`FILE_ID_INFO.FileId`), unique on its volume.
	pub(super) file_id: u128,
	/// `CreationTime`, in FILETIME ticks.
	pub(super) created: i64,
	/// `LastWriteTime`, in FILETIME ticks.
	pub(super) modified: i64,
	/// `ChangeTime`, in FILETIME ticks.
	pub(super) changed: i64,
}

/// `GetFileInformationByHandleEx` of `file` for `class` into a `T`.
///
/// # Safety
///
/// `T` is the struct `class` fills.
#[allow(unsafe_code)] // Win32 FFI: reads a file's information through its handle.
unsafe fn handle_info<T: Default>(file: &File, class: FILE_INFO_BY_HANDLE_CLASS) -> io::Result<T> {
	let mut info = T::default();
	let Ok(size) = u32::try_from(size_of::<T>()) else {
		return Err(io::Error::from(io::ErrorKind::InvalidInput));
	};
	// SAFETY: the handle is borrowed from a live `File` for the call, and `info` is an exclusively
	// borrowed `T` of `size` bytes, the struct `class` fills, as the caller guarantees.
	let ok = unsafe {
		GetFileInformationByHandleEx(
			file.as_raw_handle(),
			class,
			ptr::from_mut(&mut info).cast(),
			size,
		)
	};
	if ok == 0 {
		Err(io::Error::last_os_error())
	} else {
		Ok(info)
	}
}

/// The attributes, reparse tag, file id and times of the item `file` holds, read through its
/// handle: neither hydrates a placeholder.
#[allow(unsafe_code)] // Win32 FFI: the typed entries to `handle_info`.
pub(super) fn item_info(file: &File) -> io::Result<HandleInfo> {
	// SAFETY: each class is read into the struct it fills.
	let (tag, id, basic) = unsafe {
		(
			handle_info::<FILE_ATTRIBUTE_TAG_INFO>(file, FileAttributeTagInfo)?,
			handle_info::<FILE_ID_INFO>(file, FileIdInfo)?,
			handle_info::<FILE_BASIC_INFO>(file, FileBasicInfo)?,
		)
	};
	Ok(HandleInfo {
		attributes: tag.FileAttributes,
		reparse_tag: tag.ReparseTag,
		file_id: u128::from_le_bytes(id.FileId.Identifier),
		created: basic.CreationTime,
		modified: basic.LastWriteTime,
		changed: basic.ChangeTime,
	})
}

/// `CfConvertToPlaceholder` of the ordinary file or directory `file` with `identity` and `flags`
/// (`CF_CONVERT_FLAGS`), synchronously. Measured to succeed through a handle with
/// `FILE_WRITE_ATTRIBUTES` whatever share mode another handle holds.
#[allow(unsafe_code)] // cfapi FFI: makes an ordinary file a placeholder.
pub(super) fn convert(file: &File, identity: &IdentityBlob, flags: CF_CONVERT_FLAGS) -> Hresult {
	let bytes = identity.as_bytes();
	let Ok(len) = u32::try_from(bytes.len()) else {
		return Hresult::new(E_INVALIDARG);
	};
	// SAFETY: the handle is borrowed from a live `File` and the identity is valid for `len` bytes,
	// both for the call; there is no USN out-param and no OVERLAPPED, so the call completes before
	// it returns.
	Hresult::new(unsafe {
		CfConvertToPlaceholder(
			file.as_raw_handle(),
			bytes.as_ptr().cast(),
			len,
			flags,
			ptr::null_mut(),
			ptr::null_mut(),
		)
	})
}

/// `CfUpdatePlaceholder` of `file`, giving it `identity`, with `flags` (`CF_UPDATE_FLAGS`): no
/// metadata change and no dehydrated range, so the file's size and bytes stay as they are.
#[allow(unsafe_code)] // cfapi FFI: changes a placeholder's identity and in-sync state.
pub(super) fn update(file: &File, identity: &IdentityBlob, flags: CF_UPDATE_FLAGS) -> Hresult {
	let bytes = identity.as_bytes();
	let Ok(len) = u32::try_from(bytes.len()) else {
		return Hresult::new(E_INVALIDARG);
	};
	// SAFETY: the handle is borrowed from a live `File` and the identity is valid for `len` bytes,
	// both for the call; a null metadata pointer and an empty range array change neither, and there
	// is no USN out-param and no OVERLAPPED.
	Hresult::new(unsafe {
		CfUpdatePlaceholder(
			file.as_raw_handle(),
			ptr::null(),
			bytes.as_ptr().cast(),
			len,
			ptr::null(),
			0,
			flags,
			ptr::null_mut(),
			ptr::null_mut(),
		)
	})
}

/// `CfUpdatePlaceholder` of `file`, giving it `identity` and the size and times of `values`, with
/// `flags` (`CF_UPDATE_FLAGS`). With `CF_UPDATE_FLAG_DEHYDRATE` and no range array the whole file is
/// dehydrated. `FileAttributes` 0 leaves the attributes as they are.
#[allow(unsafe_code)] // cfapi FFI: changes a placeholder to a newer remote version.
pub(super) fn update_with_metadata(
	file: &File,
	identity: &IdentityBlob,
	values: &CreateValues,
	flags: CF_UPDATE_FLAGS,
) -> Hresult {
	let bytes = identity.as_bytes();
	let Ok(len) = u32::try_from(bytes.len()) else {
		return Hresult::new(E_INVALIDARG);
	};
	let metadata = CF_FS_METADATA {
		BasicInfo: FILE_BASIC_INFO {
			CreationTime: values.created,
			LastAccessTime: values.modified,
			LastWriteTime: values.modified,
			ChangeTime: values.modified,
			FileAttributes: 0,
		},
		FileSize: values.size,
	};
	// SAFETY: the handle is borrowed from a live `File`, and the metadata and the identity (valid
	// for `len` bytes) are locals borrowed for the call; an empty range array dehydrates the whole
	// file, and there is no USN out-param and no OVERLAPPED, so the call completes before it
	// returns.
	Hresult::new(unsafe {
		CfUpdatePlaceholder(
			file.as_raw_handle(),
			&raw const metadata,
			bytes.as_ptr().cast(),
			len,
			ptr::null(),
			0,
			flags,
			ptr::null_mut(),
			ptr::null_mut(),
		)
	})
}

/// `CfSetInSyncState` of `file` to in sync.
#[allow(unsafe_code)] // cfapi FFI: marks a placeholder in sync.
pub(super) fn set_in_sync(file: &File) -> Hresult {
	// SAFETY: the handle is borrowed from a live `File` for the call, and there is no USN
	// out-param.
	Hresult::new(unsafe {
		CfSetInSyncState(
			file.as_raw_handle(),
			CF_IN_SYNC_STATE_IN_SYNC,
			CF_SET_IN_SYNC_FLAG_NONE,
			ptr::null_mut(),
		)
	})
}

/// `CfHydratePlaceholder` of the whole of `file`, synchronously: the platform asks the provider
/// for the bytes it lacks (`FETCH_DATA`, measured from the provider's own process too), so this
/// must never run while holding anything a fetch needs.
#[allow(unsafe_code)] // cfapi FFI: hydrates a placeholder.
pub(super) fn hydrate(file: &File) -> Hresult {
	// SAFETY: the handle is borrowed from a live `File` for the call, and there is no OVERLAPPED,
	// so the call completes before it returns.
	Hresult::new(unsafe {
		CfHydratePlaceholder(
			file.as_raw_handle(),
			0,
			-1,
			CF_HYDRATE_FLAG_NONE,
			ptr::null_mut(),
		)
	})
}

/// `CfCreatePlaceholders` of `specs` in the directory `base`, each in sync and not hydrated.
#[allow(unsafe_code)] // cfapi FFI: creates placeholders outside a callback.
pub(super) fn create_placeholders(
	base: &WideNul,
	specs: &[PlaceholderSpec],
) -> PlaceholderTransfer {
	let refused = |hr| PlaceholderTransfer {
		hr,
		results: Vec::new(),
		processed: 0,
	};
	// Owned here, so every identity pointer below outlives the call.
	let blobs: Vec<IdentityBlob> = specs.iter().map(|spec| spec.identity().encode()).collect();
	let mut entries = match placeholder_entries(specs, &blobs) {
		Ok(entries) => entries,
		Err(hr) => return refused(hr),
	};
	let Ok(count) = u32::try_from(entries.len()) else {
		return refused(Hresult::new(E_INVALIDARG));
	};
	if count == 0 {
		return refused(Hresult::new(0));
	}
	let mut processed = 0_u32;
	// SAFETY: `base` is NUL-terminated by `WideNul`'s construction; the array is `entries`,
	// exclusively borrowed for the call, which cfapi writes each entry's `Result` and `CreateUsn`
	// into; each entry's name and identity point into `specs` and `blobs`, which outlive the call;
	// `processed` is a live out-param.
	let hr = Hresult::new(unsafe {
		CfCreatePlaceholders(
			base.with_nul().as_ptr(),
			entries.as_mut_ptr(),
			count,
			CF_CREATE_FLAG_NONE,
			&mut processed,
		)
	});
	PlaceholderTransfer {
		hr,
		results: entries.iter().map(|entry| entry.Result).collect(),
		processed,
	}
}

/// `MoveFileExW` of `from` to `to` with no flag: refused when `to` exists, unlike `std::fs::rename`,
/// which replaces it.
#[allow(unsafe_code)] // Win32 FFI: renames without replacing.
pub(super) fn move_no_replace(from: &WideNul, to: &WideNul) -> io::Result<()> {
	// SAFETY: both paths are NUL-terminated by `WideNul`'s construction and borrowed for the call.
	let ok = unsafe { MoveFileExW(from.with_nul().as_ptr(), to.with_nul().as_ptr(), 0) };
	if ok == 0 {
		Err(io::Error::last_os_error())
	} else {
		Ok(())
	}
}

/// `SetFileInformationByHandle` of `file` for `class` with the `size` bytes at `info`.
///
/// # Safety
///
/// `info` points at `size` readable bytes holding the struct `class` takes.
#[allow(unsafe_code)] // Win32 FFI: changes a file through its handle.
unsafe fn set_handle_info(
	file: &File,
	class: FILE_INFO_BY_HANDLE_CLASS,
	info: *const c_void,
	size: u32,
) -> io::Result<()> {
	// SAFETY: the handle is borrowed from a live `File` for the call, and `info` holds `size` bytes
	// of the struct `class` takes, as the caller guarantees.
	let ok = unsafe { SetFileInformationByHandle(file.as_raw_handle(), class, info, size) };
	if ok == 0 {
		Err(io::Error::last_os_error())
	} else {
		Ok(())
	}
}

/// Deletes the file or empty directory `file` holds, through its handle, which must have `DELETE`
/// access (`FileDispositionInfoEx` with POSIX semantics): the item deleted is the one the handle
/// was opened on, whatever a rename did to its path since, and its name goes when the last handle
/// on it closes (measured: listed until the deleting handle closed). A read-only item is deleted
/// too, as the user's attribute does not keep what the drive removed. `ERROR_DIR_NOT_EMPTY` for a
/// directory holding anything.
#[allow(unsafe_code)] // Win32 FFI: the typed entry to `set_handle_info` for a delete.
pub(super) fn delete_by_handle(file: &File) -> io::Result<()> {
	let info = FILE_DISPOSITION_INFO_EX {
		Flags: FILE_DISPOSITION_FLAG_DELETE
			| FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
			| FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
	};
	// SAFETY: `info` is a local `FILE_DISPOSITION_INFO_EX`, the struct the class takes, borrowed
	// for the call.
	unsafe {
		set_handle_info(
			file,
			FileDispositionInfoEx,
			(&raw const info).cast(),
			struct_size::<FILE_DISPOSITION_INFO_EX>(),
		)
	}
}

/// Renames the item `file` holds to `to`, a full path, through its handle, which must have
/// `DELETE` access, refusing with `ERROR_ALREADY_EXISTS` when `to` is taken: the item renamed is
/// the one the handle was opened on, whatever happened at its old path since.
/// `FileRenameInfoEx` with no flag, or `FileRenameInfo` without replacing where the volume does
/// not take the former, as std's rename falls back.
#[allow(unsafe_code)] // Win32 FFI: the typed entry to `set_handle_info` for a rename.
pub(super) fn rename_by_handle(file: &File, to: &WideNul) -> io::Result<()> {
	let name = to.with_nul();
	let name_offset = offset_of!(FILE_RENAME_INFO, FileName);
	let size = name_offset + size_of_val(name);
	let (Ok(size_u32), Ok(name_bytes)) = (
		u32::try_from(size),
		u32::try_from(size_of_val(name) - size_of::<u16>()),
	) else {
		return Err(io::Error::from(io::ErrorKind::InvalidInput));
	};
	// u64 words, so the struct's handle field is aligned.
	let mut buffer = vec![0_u64; size.div_ceil(size_of::<u64>())];
	let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
	let rename = |class: FILE_INFO_BY_HANDLE_CLASS| {
		// SAFETY: `buffer` holds at least `size` bytes, aligned for the struct: its header fields
		// are written in place, and the name, NUL included, from the `FileName` field on, which
		// ends at `size`. The buffer outlives the call, which reads `size` bytes of it; a null
		// root directory makes the name a full path, and no flag means no replacing.
		unsafe {
			(&raw mut (*info).Anonymous).write(FILE_RENAME_INFO_0 { Flags: 0 });
			(&raw mut (*info).RootDirectory).write(ptr::null_mut());
			(&raw mut (*info).FileNameLength).write(name_bytes);
			name.as_ptr().copy_to_nonoverlapping(
				info.cast::<u8>().add(name_offset).cast::<u16>(),
				name.len(),
			);
			set_handle_info(file, class, info.cast_const().cast(), size_u32)
		}
	};
	match rename(FileRenameInfoEx) {
		Err(error)
			if matches!(
				error
					.raw_os_error()
					.and_then(|code| u32::try_from(code).ok()),
				Some(ERROR_INVALID_PARAMETER | ERROR_NOT_SUPPORTED)
			) =>
		{
			rename(FileRenameInfo)
		}
		renamed => renamed,
	}
}

/// Reads the field `$field`, of type `$ty`, of the `$struct` lying `$base` bytes into the struct
/// at `$ptr` (at its start when no base is given), which cfapi says is `$size` bytes long: `Some`
/// only when the struct reaches the field's end, as an older Windows may pass a shorter struct
/// than this build declares. Reads that field alone, never the bytes after it, and never forms a
/// reference to the struct.
macro_rules! read_field {
	($ptr:expr, $size:expr, $struct:ty, $field:ident: $ty:ty) => {
		read_field!($ptr, $size, 0, $struct, $field: $ty)
	};
	($ptr:expr, $size:expr, $base:expr, $struct:ty, $field:ident: $ty:ty) => {{
		// Checks at compile time that `$ty` is the field's type.
		let _: fn(&$struct) -> $ty = |s| s.$field;
		let offset = $base + offset_of!($struct, $field);
		if field_present($size, offset, size_of::<$ty>()) {
			// SAFETY: `$ptr` points at a struct cfapi passed, at least `$size` bytes long, holding
			// a `$struct` `$base` bytes in, and the field lies within those bytes.
			Some(unsafe { $ptr.cast::<u8>().add(offset).cast::<$ty>().read_unaligned() })
		} else {
			None
		}
	}};
}

/// Where the union of a `CF_CALLBACK_PARAMETERS` starts.
const PARAMS: usize = offset_of!(CF_CALLBACK_PARAMETERS, Anonymous);
/// Where the cancelled range of a `CANCEL_FETCH_DATA`'s parameters starts.
const CANCEL_RANGE: usize = PARAMS + offset_of!(CF_CALLBACK_PARAMETERS_0_0, Anonymous);

/// The NUL-terminated string at `string`, read for at most [`MAX_PATH_UNITS`] units; `None` when
/// it is null or has no NUL within them.
///
/// # Safety
///
/// `string` is null or points at a NUL-terminated UTF-16 string cfapi passed, valid for the
/// callback.
#[allow(unsafe_code)] // cfapi FFI: reads a string a callback passed.
unsafe fn read_wide(string: *const u16) -> Option<Vec<u16>> {
	if string.is_null() {
		return None;
	}
	// SAFETY: the reads stop at the first NUL, which the string holds, so each unit read lies
	// within it.
	take_until_nul(
		(0..).map(|index| unsafe { string.add(index).read_unaligned() }),
		MAX_PATH_UNITS,
	)
}

/// The keys of the callback `info` and its file's size, `None` when its struct is too short to
/// hold the connection and transfer keys and the size: such a callback cannot be answered. A
/// struct too short for `RequestKey`, a later field, takes `CF_REQUEST_KEY_DEFAULT`, the value
/// every data callback was measured to carry.
///
/// # Safety
///
/// `info` is non-null and points at the `CF_CALLBACK_INFO` cfapi passed, valid for the callback.
#[allow(unsafe_code)] // cfapi FFI: copies a callback's keys out of OS memory.
unsafe fn callback_keys(info: *const CF_CALLBACK_INFO) -> Option<(Keys, i64)> {
	// SAFETY: every struct cfapi passes starts with its `u32` size.
	let size = unsafe { info.cast::<u32>().read_unaligned() };
	let keys = Keys {
		connection: read_field!(info, size, CF_CALLBACK_INFO, ConnectionKey: i64)?,
		transfer: read_field!(info, size, CF_CALLBACK_INFO, TransferKey: i64)?,
		request: read_field!(info, size, CF_CALLBACK_INFO, RequestKey: i64)
			.unwrap_or(i64::from(CF_REQUEST_KEY_DEFAULT)),
	};
	let file_size = read_field!(info, size, CF_CALLBACK_INFO, FileSize: i64)?;
	Some((keys, file_size))
}

/// Copies what every callback needs out of `info` into owned values; `None`, logged, when its
/// struct is too short to hold its keys and its file's size.
///
/// # Safety
///
/// As [`callback_keys`].
#[allow(unsafe_code)] // cfapi FFI: copies a callback's info out of OS memory.
unsafe fn callback_info(
	info: *const CF_CALLBACK_INFO,
	callback: &'static str,
) -> Option<CallbackInfo> {
	// SAFETY: as this function's own contract.
	let Some((keys, file_size)) = (unsafe { callback_keys(info) }) else {
		error!(target: LOG_TARGET, callback, "a callback's info is too short to answer it");
		return None;
	};
	// SAFETY: every struct cfapi passes starts with its `u32` size.
	let size = unsafe { info.cast::<u32>().read_unaligned() };
	let identity_ptr = read_field!(info, size, CF_CALLBACK_INFO, FileIdentity: *const c_void)
		.unwrap_or(ptr::null());
	let identity_bytes =
		read_field!(info, size, CF_CALLBACK_INFO, FileIdentityLength: u32).unwrap_or(0);
	let identity = identity_len(identity_ptr.is_null(), identity_bytes).and_then(|len| {
		// SAFETY: cfapi passes the identity valid for `FileIdentityLength` bytes, which
		// `identity_len` bounded before this slice exists; it is copied at once.
		let bytes = unsafe { slice::from_raw_parts(identity_ptr.cast::<u8>(), len) };
		IdentityBlob::try_from(bytes)
	});
	let process = read_field!(info, size, CF_CALLBACK_INFO, ProcessInfo: *mut CF_PROCESS_INFO)
		.filter(|process| !process.is_null());
	let pid = process.and_then(|process| {
		// SAFETY: a non-null `ProcessInfo` points at a `CF_PROCESS_INFO` valid for the callback,
		// which starts with its `u32` size.
		let process_size = unsafe { process.cast::<u32>().read_unaligned() };
		read_field!(process, process_size, CF_PROCESS_INFO, ProcessId: u32)
	});
	let context = read_field!(info, size, CF_CALLBACK_INFO, CallbackContext: *mut c_void)
		.unwrap_or(ptr::null_mut());
	// SAFETY: the only context this module connects with is a `&'static ContextSlot`.
	let slot = unsafe { context.cast::<ContextSlot>().cast_const().as_ref() };
	Some(CallbackInfo {
		keys,
		file_size,
		identity,
		pid,
		slot,
	})
}

/// The full path of the callback's file, from `VolumeDosName` and `NormalizedPath`.
///
/// # Safety
///
/// As [`callback_info`].
#[allow(unsafe_code)] // cfapi FFI: reads a callback's path strings.
unsafe fn callback_path(info: *const CF_CALLBACK_INFO) -> Option<PathBuf> {
	// SAFETY: as this function's own contract.
	let path = unsafe { callback_units(info) }?;
	Some(PathBuf::from(OsString::from_wide(&path)))
}

/// The callback's `VolumeDosName` (`X:`), when it carries one.
///
/// # Safety
///
/// As [`callback_info`].
#[allow(unsafe_code)] // cfapi FFI: reads a callback's volume name.
unsafe fn volume_dos(info: *const CF_CALLBACK_INFO) -> Option<Vec<u16>> {
	// SAFETY: every struct cfapi passes starts with its `u32` size.
	let size = unsafe { info.cast::<u32>().read_unaligned() };
	let dos =
		read_field!(info, size, CF_CALLBACK_INFO, VolumeDosName: *const u16).unwrap_or(ptr::null());
	// SAFETY: null or a NUL-terminated string cfapi passed, valid for the callback.
	unsafe { read_wide(dos) }
}

/// The full path of the callback's file in UTF-16, from `VolumeDosName` and `NormalizedPath`.
///
/// # Safety
///
/// As [`callback_info`].
#[allow(unsafe_code)] // cfapi FFI: reads a callback's path strings.
unsafe fn callback_units(info: *const CF_CALLBACK_INFO) -> Option<Vec<u16>> {
	// SAFETY: every struct cfapi passes starts with its `u32` size.
	let size = unsafe { info.cast::<u32>().read_unaligned() };
	let normalized = read_field!(info, size, CF_CALLBACK_INFO, NormalizedPath: *const u16)
		.unwrap_or(ptr::null());
	// SAFETY: as this function's own contract; `normalized` is null or a NUL-terminated string
	// cfapi passed, valid for the callback.
	let (dos, normalized) = unsafe { (volume_dos(info), read_wide(normalized)?) };
	volume_path(dos.as_deref(), &normalized)
}

/// The full path of a path a callback's parameters carry, measured volume-relative
/// (`\cfroot\exp\a.txt`), on the callback's volume.
///
/// # Safety
///
/// As [`callback_info`], and `path` is null or a NUL-terminated string cfapi passed, valid for the
/// callback.
#[allow(unsafe_code)] // cfapi FFI: reads a path a callback's parameters carry.
unsafe fn param_path(info: *const CF_CALLBACK_INFO, path: *const u16) -> Option<Vec<u16>> {
	// SAFETY: as this function's own contract.
	let (dos, path) = unsafe { (volume_dos(info), read_wide(path)?) };
	volume_path(dos.as_deref(), &path)
}

/// The `Flags` of a notification's parameters, the first field of its variant (`$variant`):
/// `None` when `params` is null or too short to hold it.
macro_rules! notify_flags {
	($params:expr, $variant:ty) => {{
		let params: *const CF_CALLBACK_PARAMETERS = $params;
		if params.is_null() {
			None
		} else {
			// SAFETY: the struct starts with its `u32` size.
			let size = unsafe { params.cast::<u32>().read_unaligned() };
			read_field!(params, size, PARAMS, $variant, Flags: i32)
		}
	}};
}

/// The flags of a `NOTIFY_DELETE` callback's parameters.
///
/// # Safety
///
/// `params` is null or points at the `CF_CALLBACK_PARAMETERS` of a `NOTIFY_DELETE` callback cfapi
/// passed, valid for the callback.
#[allow(unsafe_code)] // cfapi FFI: copies a callback's parameters out of OS memory.
unsafe fn delete_flags(params: *const CF_CALLBACK_PARAMETERS) -> Option<i32> {
	notify_flags!(params, CF_CALLBACK_PARAMETERS_0_8)
}

/// The flags of a `NOTIFY_FILE_CLOSE_COMPLETION` callback's parameters.
///
/// # Safety
///
/// `params` is null or points at the `CF_CALLBACK_PARAMETERS` of a `NOTIFY_FILE_CLOSE_COMPLETION`
/// callback cfapi passed, valid for the callback.
#[allow(unsafe_code)] // cfapi FFI: copies a callback's parameters out of OS memory.
unsafe fn close_flags(params: *const CF_CALLBACK_PARAMETERS) -> Option<i32> {
	notify_flags!(params, CF_CALLBACK_PARAMETERS_0_5)
}

/// The flags and the reason of a `NOTIFY_DEHYDRATE` callback's parameters: `None` when `params` is
/// null or too short to hold both.
///
/// # Safety
///
/// `params` is null or points at the `CF_CALLBACK_PARAMETERS` of a `NOTIFY_DEHYDRATE` callback
/// cfapi passed, valid for the callback.
#[allow(unsafe_code)] // cfapi FFI: copies a callback's parameters out of OS memory.
unsafe fn dehydrate_params(params: *const CF_CALLBACK_PARAMETERS) -> Option<(i32, i32)> {
	let flags = notify_flags!(params, CF_CALLBACK_PARAMETERS_0_6)?;
	// SAFETY: `params` is non-null, as the flags were read; the struct starts with its size.
	let size = unsafe { params.cast::<u32>().read_unaligned() };
	let reason = read_field!(params, size, PARAMS, CF_CALLBACK_PARAMETERS_0_6, Reason: i32)?;
	Some((flags, reason))
}

/// The flags and the full target path of a `NOTIFY_RENAME` callback's parameters: `None` when
/// `params` is null or too short to hold the flags; the path `None` when it holds none.
///
/// # Safety
///
/// As [`callback_info`], and `params` is null or points at the `CF_CALLBACK_PARAMETERS` of a
/// `NOTIFY_RENAME` callback cfapi passed, valid for the callback.
#[allow(unsafe_code)] // cfapi FFI: copies a callback's parameters out of OS memory.
unsafe fn rename_params(
	info: *const CF_CALLBACK_INFO,
	params: *const CF_CALLBACK_PARAMETERS,
) -> Option<(i32, Option<Vec<u16>>)> {
	let flags = notify_flags!(params, CF_CALLBACK_PARAMETERS_0_10)?;
	// SAFETY: `params` is non-null, as the flags were read; the struct starts with its size.
	let size = unsafe { params.cast::<u32>().read_unaligned() };
	let target =
		read_field!(params, size, PARAMS, CF_CALLBACK_PARAMETERS_0_10, TargetPath: *const u16)
			.unwrap_or(ptr::null());
	// SAFETY: the target is null or a NUL-terminated string cfapi passed, valid for the callback.
	Some((flags, unsafe { param_path(info, target) }))
}

/// The full source path of a `NOTIFY_RENAME_COMPLETION` callback's parameters.
///
/// # Safety
///
/// As [`callback_info`], and `params` is null or points at the `CF_CALLBACK_PARAMETERS` of a
/// `NOTIFY_RENAME_COMPLETION` callback cfapi passed, valid for the callback.
#[allow(unsafe_code)] // cfapi FFI: copies a callback's parameters out of OS memory.
unsafe fn rename_completion_source(
	info: *const CF_CALLBACK_INFO,
	params: *const CF_CALLBACK_PARAMETERS,
) -> Option<Vec<u16>> {
	if params.is_null() {
		return None;
	}
	// SAFETY: the struct starts with its `u32` size.
	let size = unsafe { params.cast::<u32>().read_unaligned() };
	let source =
		read_field!(params, size, PARAMS, CF_CALLBACK_PARAMETERS_0_11, SourcePath: *const u16)?;
	// SAFETY: the source is null or a NUL-terminated string cfapi passed, valid for the callback.
	unsafe { param_path(info, source) }
}

/// The range of a `FETCH_DATA` callback's parameters, for a file of `file_size` bytes: `None`
/// when `params` is null or `ParamSize` does not reach the end of `OptionalLength`, the last field
/// read. The fields after it (the last dehydration's time and reason) are not needed.
///
/// # Safety
///
/// `params` is null or points at the `CF_CALLBACK_PARAMETERS` of a `FETCH_DATA` callback cfapi
/// passed, valid for the callback.
#[allow(unsafe_code)] // cfapi FFI: copies a callback's parameters out of OS memory.
unsafe fn fetch_params(
	params: *const CF_CALLBACK_PARAMETERS,
	file_size: i64,
) -> Option<RequestedRange> {
	if params.is_null() {
		return None;
	}
	// SAFETY: the struct starts with its `u32` size.
	let size = unsafe { params.cast::<u32>().read_unaligned() };
	let required_offset =
		read_field!(params, size, PARAMS, CF_CALLBACK_PARAMETERS_0_1, RequiredFileOffset: i64)?;
	let required_len =
		read_field!(params, size, PARAMS, CF_CALLBACK_PARAMETERS_0_1, RequiredLength: i64)?;
	let optional_offset =
		read_field!(params, size, PARAMS, CF_CALLBACK_PARAMETERS_0_1, OptionalFileOffset: i64)?;
	let optional_len =
		read_field!(params, size, PARAMS, CF_CALLBACK_PARAMETERS_0_1, OptionalLength: i64)?;
	Some(RequestedRange::new(
		file_size,
		required_offset,
		required_len,
		OptionalRange::new(optional_offset, optional_len),
	))
}

/// The flags and range of a `CANCEL_FETCH_DATA` callback's parameters: `None` when `params` is
/// null or `ParamSize` does not reach the end of the range's `Length`.
///
/// # Safety
///
/// `params` is null or points at the `CF_CALLBACK_PARAMETERS` of a `CANCEL_FETCH_DATA` callback
/// cfapi passed, valid for the callback.
#[allow(unsafe_code)] // cfapi FFI: copies a callback's parameters out of OS memory.
unsafe fn cancel_params(params: *const CF_CALLBACK_PARAMETERS) -> Option<callbacks::Cancel> {
	if params.is_null() {
		return None;
	}
	// SAFETY: the struct starts with its `u32` size.
	let size = unsafe { params.cast::<u32>().read_unaligned() };
	Some(callbacks::Cancel {
		flags: read_field!(params, size, PARAMS, CF_CALLBACK_PARAMETERS_0_0, Flags: i32)?,
		offset: read_field!(params, size, CANCEL_RANGE, CF_CALLBACK_PARAMETERS_0_0_0_0, FileOffset: i64)?,
		length: read_field!(params, size, CANCEL_RANGE, CF_CALLBACK_PARAMETERS_0_0_0_0, Length: i64)?,
	})
}

/// [`callback_keys`] of `info`, for tests.
#[cfg(test)]
#[allow(unsafe_code)] // cfapi FFI: the tests' entry to the struct reads.
pub(super) fn keys_of(info: &CF_CALLBACK_INFO) -> Option<(Keys, i64)> {
	// SAFETY: a reference is valid for the whole struct, which holds every field read whatever
	// its `StructSize` says, and only plain numbers are read.
	unsafe { callback_keys(info) }
}

/// [`fetch_params`] of `params`, for tests.
#[cfg(test)]
#[allow(unsafe_code)] // cfapi FFI: the tests' entry to the struct reads.
pub(super) fn fetch_params_of(
	params: &CF_CALLBACK_PARAMETERS,
	file_size: i64,
) -> Option<RequestedRange> {
	// SAFETY: a reference is valid for the whole struct, which holds every field read whatever
	// its `ParamSize` says, and only plain numbers are read.
	unsafe { fetch_params(params, file_size) }
}

/// [`cancel_params`] of `params`, for tests.
#[cfg(test)]
#[allow(unsafe_code)] // cfapi FFI: the tests' entry to the struct reads.
pub(super) fn cancel_params_of(params: &CF_CALLBACK_PARAMETERS) -> Option<callbacks::Cancel> {
	// SAFETY: as `fetch_params_of`.
	unsafe { cancel_params(params) }
}

/// [`dehydrate_params`] of `params`, for tests.
#[cfg(test)]
#[allow(unsafe_code)] // cfapi FFI: the tests' entry to the struct reads.
pub(super) fn dehydrate_params_of(params: &CF_CALLBACK_PARAMETERS) -> Option<(i32, i32)> {
	// SAFETY: as `fetch_params_of`.
	unsafe { dehydrate_params(params) }
}

/// Runs a callback's body, logging a panic rather than unwinding into cfapi, which would abort
/// the process. A responder built inside `body` fails its request as it drops during the unwind.
fn guard(callback: &'static str, body: impl FnOnce()) {
	if panic::catch_unwind(AssertUnwindSafe(body)).is_err() {
		error!(target: LOG_TARGET, callback, "a cfapi callback panicked");
	}
}

/// The `FETCH_DATA` callback.
///
/// # Safety
///
/// Called only by cfapi, through the callback table [`connect`] registers: `info` is null or
/// points to a `CF_CALLBACK_INFO`, and `params` to a `CF_CALLBACK_PARAMETERS` holding the
/// `FetchData` variant, both valid for the duration of the call; neither is kept past it.
#[allow(unsafe_code)] // cfapi FFI: the FETCH_DATA trampoline.
unsafe extern "system" fn on_fetch_data(
	info: *const CF_CALLBACK_INFO,
	params: *const CF_CALLBACK_PARAMETERS,
) {
	guard("fetch_data", || {
		if info.is_null() {
			return;
		}
		// SAFETY: cfapi passes `info` and `params` valid for the callback, and a FETCH_DATA
		// callback carries the `FetchData` variant.
		let Some(common) = (unsafe { callback_info(info, "fetch_data") }) else {
			return;
		};
		// SAFETY: as above.
		let (range, path) =
			unsafe { (fetch_params(params, common.file_size), callback_path(info)) };
		callbacks::fetch_data(common, range, path);
	});
}

/// The `CANCEL_FETCH_DATA` callback.
///
/// # Safety
///
/// Called only by cfapi, through the callback table [`connect`] registers: `info` is null or
/// points to a `CF_CALLBACK_INFO`, and `params` to a `CF_CALLBACK_PARAMETERS` holding the
/// `Cancel` variant, both valid for the duration of the call; neither is kept past it.
#[allow(unsafe_code)] // cfapi FFI: the CANCEL_FETCH_DATA trampoline.
unsafe extern "system" fn on_cancel_fetch_data(
	info: *const CF_CALLBACK_INFO,
	params: *const CF_CALLBACK_PARAMETERS,
) {
	guard("cancel_fetch_data", || {
		if info.is_null() {
			return;
		}
		// SAFETY: cfapi passes `info` and `params` valid for the callback, and a
		// CANCEL_FETCH_DATA callback carries the `Cancel` variant, whose union holds `FetchData`.
		let Some(common) = (unsafe { callback_info(info, "cancel_fetch_data") }) else {
			return;
		};
		// SAFETY: as above.
		let cancel = unsafe { cancel_params(params) };
		callbacks::cancel_fetch_data(common, cancel);
	});
}

/// The `FETCH_PLACEHOLDERS` callback. Its search pattern is ignored: the whole directory is sent.
///
/// # Safety
///
/// Called only by cfapi, through the callback table [`connect`] registers: `info` is null or
/// points to a `CF_CALLBACK_INFO`, and `params` to a `CF_CALLBACK_PARAMETERS` holding the
/// `FetchPlaceholders` variant, both valid for the duration of the call; neither is kept past it.
#[allow(unsafe_code)] // cfapi FFI: the FETCH_PLACEHOLDERS trampoline.
unsafe extern "system" fn on_fetch_placeholders(
	info: *const CF_CALLBACK_INFO,
	_params: *const CF_CALLBACK_PARAMETERS,
) {
	guard("fetch_placeholders", || {
		if info.is_null() {
			return;
		}
		// SAFETY: cfapi passes `info` valid for the callback.
		let Some(common) = (unsafe { callback_info(info, "fetch_placeholders") }) else {
			return;
		};
		callbacks::fetch_placeholders(common);
	});
}

/// The `NOTIFY_DELETE` callback: a placeholder is about to be deleted, and waits for its
/// `ACK_DELETE`. Info too short to read carries no keys to answer with: it is left unanswered, and
/// the platform fails the delete after its 60 s limit.
///
/// # Safety
///
/// Called only by cfapi, through the callback table [`connect`] registers: `info` is null or
/// points to a `CF_CALLBACK_INFO`, and `params` to a `CF_CALLBACK_PARAMETERS` holding the
/// `Delete` variant, both valid for the duration of the call; neither is kept past it.
#[allow(unsafe_code)] // cfapi FFI: the NOTIFY_DELETE trampoline.
unsafe extern "system" fn on_notify_delete(
	info: *const CF_CALLBACK_INFO,
	params: *const CF_CALLBACK_PARAMETERS,
) {
	guard("notify_delete", || {
		if info.is_null() {
			return;
		}
		// SAFETY: cfapi passes `info` and `params` valid for the callback, and a NOTIFY_DELETE
		// callback carries the `Delete` variant.
		let Some(common) = (unsafe { callback_info(info, "notify_delete") }) else {
			return;
		};
		// SAFETY: as above.
		let (flags, path) = unsafe { (delete_flags(params), callback_units(info)) };
		callbacks::notify_delete(common, flags, path);
	});
}

/// The `NOTIFY_RENAME` callback: a placeholder is about to be renamed or moved, and waits for its
/// `ACK_RENAME`. Info too short to read is left unanswered, as for a delete.
///
/// # Safety
///
/// Called only by cfapi, through the callback table [`connect`] registers: `info` is null or
/// points to a `CF_CALLBACK_INFO`, and `params` to a `CF_CALLBACK_PARAMETERS` holding the
/// `Rename` variant, both valid for the duration of the call; neither is kept past it.
#[allow(unsafe_code)] // cfapi FFI: the NOTIFY_RENAME trampoline.
unsafe extern "system" fn on_notify_rename(
	info: *const CF_CALLBACK_INFO,
	params: *const CF_CALLBACK_PARAMETERS,
) {
	guard("notify_rename", || {
		if info.is_null() {
			return;
		}
		// SAFETY: cfapi passes `info` and `params` valid for the callback, and a NOTIFY_RENAME
		// callback carries the `Rename` variant.
		let Some(common) = (unsafe { callback_info(info, "notify_rename") }) else {
			return;
		};
		// SAFETY: as above.
		let (rename, source) = unsafe { (rename_params(info, params), callback_units(info)) };
		callbacks::notify_rename(common, rename, source);
	});
}

/// The `NOTIFY_DEHYDRATE` callback: another process asks to dehydrate a placeholder, and waits for
/// its `ACK_DEHYDRATE`. Info too short to read is left unanswered: measured, the caller then fails
/// after about 121 s with `ERROR_CLOUD_FILE_REQUEST_TIMEOUT`.
///
/// # Safety
///
/// Called only by cfapi, through the callback table [`connect`] registers: `info` is null or
/// points to a `CF_CALLBACK_INFO`, and `params` to a `CF_CALLBACK_PARAMETERS` holding the
/// `Dehydrate` variant, both valid for the duration of the call; neither is kept past it.
#[allow(unsafe_code)] // cfapi FFI: the NOTIFY_DEHYDRATE trampoline.
unsafe extern "system" fn on_notify_dehydrate(
	info: *const CF_CALLBACK_INFO,
	params: *const CF_CALLBACK_PARAMETERS,
) {
	guard("notify_dehydrate", || {
		if info.is_null() {
			return;
		}
		// SAFETY: cfapi passes `info` and `params` valid for the callback, and a NOTIFY_DEHYDRATE
		// callback carries the `Dehydrate` variant.
		let Some(common) = (unsafe { callback_info(info, "notify_dehydrate") }) else {
			return;
		};
		// SAFETY: as above.
		let (request, path) = unsafe { (dehydrate_params(params), callback_units(info)) };
		callbacks::notify_dehydrate(common, request, path);
	});
}

/// The `NOTIFY_RENAME_COMPLETION` callback: a placeholder was renamed or moved within the root.
///
/// # Safety
///
/// Called only by cfapi, through the callback table [`connect`] registers: `info` is null or
/// points to a `CF_CALLBACK_INFO`, and `params` to a `CF_CALLBACK_PARAMETERS` holding the
/// `RenameCompletion` variant, both valid for the duration of the call; neither is kept past it.
#[allow(unsafe_code)] // cfapi FFI: the NOTIFY_RENAME_COMPLETION trampoline.
unsafe extern "system" fn on_rename_completion(
	info: *const CF_CALLBACK_INFO,
	params: *const CF_CALLBACK_PARAMETERS,
) {
	guard("rename_completion", || {
		if info.is_null() {
			return;
		}
		// SAFETY: cfapi passes `info` and `params` valid for the callback, and a
		// NOTIFY_RENAME_COMPLETION callback carries the `RenameCompletion` variant.
		let Some(common) = (unsafe { callback_info(info, "rename_completion") }) else {
			return;
		};
		// SAFETY: as above.
		let (source, path) =
			unsafe { (rename_completion_source(info, params), callback_units(info)) };
		callbacks::rename_completion(&common, source, path);
	});
}

/// The `NOTIFY_FILE_CLOSE_COMPLETION` callback: the last handle on a placeholder was closed.
///
/// # Safety
///
/// Called only by cfapi, through the callback table [`connect`] registers: `info` is null or
/// points to a `CF_CALLBACK_INFO`, and `params` to a `CF_CALLBACK_PARAMETERS` holding the
/// `CloseCompletion` variant, both valid for the duration of the call; neither is kept past it.
#[allow(unsafe_code)] // cfapi FFI: the NOTIFY_FILE_CLOSE_COMPLETION trampoline.
unsafe extern "system" fn on_close_completion(
	info: *const CF_CALLBACK_INFO,
	params: *const CF_CALLBACK_PARAMETERS,
) {
	guard("close_completion", || {
		if info.is_null() {
			return;
		}
		// SAFETY: cfapi passes `info` and `params` valid for the callback, and a
		// NOTIFY_FILE_CLOSE_COMPLETION callback carries the `CloseCompletion` variant.
		let Some(common) = (unsafe { callback_info(info, "close_completion") }) else {
			return;
		};
		// SAFETY: as above.
		let (flags, path) = unsafe { (close_flags(params), callback_units(info)) };
		callbacks::close_completion(&common, flags, path);
	});
}

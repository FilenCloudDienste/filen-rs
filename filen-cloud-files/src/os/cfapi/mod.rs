//! The rules the Windows layer follows around the cfapi calls themselves: the codes cfapi answers
//! with, and the NUL-terminated UTF-16 strings it takes. No Windows API and no `unsafe` here, so
//! this compiles, and is tested, on every host; `os::win` checks each code against `windows-sys`.

// Read only by the Windows layer; on other hosts only its tests build it.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) mod explorer;

use std::{fmt, iter};

use filen_types::fs::Uuid;

use crate::{
	failure::Failure,
	identity::{IdentityError, MAX_IDENTITY_LEN},
	os::{DehydrateReason, EntryOutcome, LocalError, OsError, PinAttr, UnsupportedKind},
	placeholder::{PlaceholderSpec, SpecKind},
	root_path::{RootPath, RootPathError},
};

/// `E_INVALIDARG`: an argument cfapi refused, as it refuses an identity over 4096 bytes.
pub(crate) const E_INVALIDARG: i32 = 0x8007_0057_u32.cast_signed();
/// `E_FAIL`: a failure that carries no code of its own.
pub(crate) const E_FAIL: i32 = 0x8000_4005_u32.cast_signed();

/// `STATUS_CLOUD_FILE_UNSUCCESSFUL`: the reader sees error 389.
pub(crate) const STATUS_CLOUD_FILE_UNSUCCESSFUL: i32 = 0xC000_CF12_u32.cast_signed();
/// `STATUS_CLOUD_FILE_AUTHENTICATION_FAILED`: the reader sees error 386.
pub(crate) const STATUS_CLOUD_FILE_AUTHENTICATION_FAILED: i32 = 0xC000_CF0F_u32.cast_signed();
/// `STATUS_CLOUD_FILE_NOT_IN_SYNC` (unmeasured).
pub(crate) const STATUS_CLOUD_FILE_NOT_IN_SYNC: i32 = 0xC000_CF08_u32.cast_signed();
/// `STATUS_CLOUD_FILE_NETWORK_UNAVAILABLE`: the reader sees error 388.
pub(crate) const STATUS_CLOUD_FILE_NETWORK_UNAVAILABLE: i32 = 0xC000_CF11_u32.cast_signed();
/// `STATUS_CLOUD_FILE_NOT_SUPPORTED` (unmeasured).
pub(crate) const STATUS_CLOUD_FILE_NOT_SUPPORTED: i32 = 0xC000_CF0A_u32.cast_signed();
/// `STATUS_CLOUD_FILE_VALIDATION_FAILED`: the reader sees error 383.
pub(crate) const STATUS_CLOUD_FILE_VALIDATION_FAILED: i32 = 0xC000_CF0E_u32.cast_signed();
/// `STATUS_CLOUD_FILE_INVALID_REQUEST` (unmeasured).
pub(crate) const STATUS_CLOUD_FILE_INVALID_REQUEST: i32 = 0xC000_CF0B_u32.cast_signed();

/// `ERROR_CLOUD_FILE_REQUEST_CANCELED`: a late `TRANSFER_DATA` for a cancelled request.
pub(crate) const ERROR_CLOUD_FILE_REQUEST_CANCELED: u32 = 398;
/// `ERROR_CLOUD_FILE_REQUEST_ABORTED`.
pub(crate) const ERROR_CLOUD_FILE_REQUEST_ABORTED: u32 = 393;
/// `ERROR_NOT_A_CLOUD_FILE`: measured from `TRANSFER_DATA` when the reader closed its handle as
/// the transfer completed, the reader holding its bytes already. Cause not established.
pub(crate) const ERROR_NOT_A_CLOUD_FILE: u32 = 376;
/// `ERROR_CLOUD_FILE_NOT_IN_SYNC`: measured from `CfUpdatePlaceholder` with `VERIFY_IN_SYNC` on a
/// file written since it was marked in sync, and from `DEHYDRATE` on a file not in sync.
pub(crate) const ERROR_CLOUD_FILE_NOT_IN_SYNC: u32 = 377;
/// `ERROR_CLOUD_FILE_INVALID_REQUEST`: a placeholder refused for an empty identity.
pub(crate) const ERROR_CLOUD_FILE_INVALID_REQUEST: u32 = 380;
/// `ERROR_CLOUD_FILE_IN_USE`: a dehydrate while a transfer of the file was in flight; a call on a
/// placeholder another handle has in use, which a later try may pass.
pub(crate) const ERROR_CLOUD_FILE_IN_USE: u32 = 391;
/// `ERROR_CLOUD_FILE_PINNED`: measured from `CfUpdatePlaceholder` with `DEHYDRATE` on a pinned file,
/// which it left unchanged.
pub(crate) const ERROR_CLOUD_FILE_PINNED: u32 = 392;

/// `ERROR_FILE_NOT_FOUND`.
pub(crate) const ERROR_FILE_NOT_FOUND: u32 = 2;
/// `ERROR_PATH_NOT_FOUND`.
pub(crate) const ERROR_PATH_NOT_FOUND: u32 = 3;
/// `ERROR_SHARING_VIOLATION`: another handle's share mode refuses the open; measured for an open
/// shared for reading only while a writer holds the file.
pub(crate) const ERROR_SHARING_VIOLATION: u32 = 32;
/// `ERROR_FILE_EXISTS`.
pub(crate) const ERROR_FILE_EXISTS: u32 = 80;
/// `ERROR_ALREADY_EXISTS`: also a placeholder entry whose name is taken.
pub(crate) const ERROR_ALREADY_EXISTS: u32 = 183;
/// `ERROR_DIR_NOT_EMPTY`: measured from `RemoveDirectoryW` on a directory that holds anything.
pub(crate) const ERROR_DIR_NOT_EMPTY: u32 = 145;
/// `ERROR_REPARSE_TAG_MISMATCH`: seen (0x8007112a) by an unregister preparing a root in which an
/// append through a handle opened for appending only had failed with 1450 on a placeholder
/// dehydrated just before; which call returned it, and on which item, was not logged.
pub(crate) const ERROR_REPARSE_TAG_MISMATCH: u32 = 4394;

/// `FILE_ATTRIBUTE_NORMAL`.
pub(crate) const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
/// `FILE_ATTRIBUTE_DIRECTORY`.
pub(crate) const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
/// `FILE_ATTRIBUTE_REPARSE_POINT`.
pub(crate) const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
/// `FILE_ATTRIBUTE_OFFLINE`.
pub(crate) const FILE_ATTRIBUTE_OFFLINE: u32 = 0x1000;
/// `FILE_ATTRIBUTE_RECALL_ON_OPEN`.
pub(crate) const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;
/// `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`.
pub(crate) const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
/// `CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC`.
pub(crate) const PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC: i32 = 2;

/// `IO_REPARSE_TAG_SYMLINK`.
pub(crate) const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;
/// `IO_REPARSE_TAG_MOUNT_POINT`: a junction or a volume mount point.
pub(crate) const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
/// `IO_REPARSE_TAG_CLOUD`: a placeholder's tag with its flag bits ([`IO_REPARSE_TAG_CLOUD_MASK`])
/// cleared. Measured: `0x9000401A`, `0x9000601A`, `0x9000201A`, `0x9000001A`.
pub(crate) const IO_REPARSE_TAG_CLOUD: u32 = 0x9000_001A;
/// `IO_REPARSE_TAG_CLOUD_MASK`: the flag bits of a placeholder's tag.
pub(crate) const IO_REPARSE_TAG_CLOUD_MASK: u32 = 0x0000_F000;

/// `CF_CALLBACK_DELETE_FLAG_IS_DIRECTORY`.
pub(crate) const DELETE_FLAG_IS_DIRECTORY: i32 = 1;
/// `CF_CALLBACK_DELETE_FLAG_IS_UNDELETE`: the platform takes a delete back (measured from the
/// recycle bin, before it moves the item out of the root).
pub(crate) const DELETE_FLAG_IS_UNDELETE: i32 = 2;
/// `CF_CALLBACK_RENAME_FLAG_IS_DIRECTORY`.
pub(crate) const RENAME_FLAG_IS_DIRECTORY: i32 = 1;
/// `CF_CALLBACK_RENAME_FLAG_TARGET_IN_SCOPE`: measured clear for a move to another folder, another
/// volume's folder or the recycle bin.
pub(crate) const RENAME_FLAG_TARGET_IN_SCOPE: i32 = 4;
/// `CF_CALLBACK_CLOSE_COMPLETION_FLAG_DELETED`: the closed handle deleted the file, which proves a
/// delete as neither `NOTIFY_DELETE` nor `NOTIFY_DELETE_COMPLETION` does.
pub(crate) const CLOSE_COMPLETION_FLAG_DELETED: i32 = 1;
/// `CF_CALLBACK_DEHYDRATE_FLAG_BACKGROUND`: the platform asks on its own, for no waiting caller.
pub(crate) const DEHYDRATE_FLAG_BACKGROUND: i32 = 1;
/// `CF_CALLBACK_DEHYDRATION_REASON_NONE`.
pub(crate) const DEHYDRATION_REASON_NONE: i32 = 0;
/// `CF_CALLBACK_DEHYDRATION_REASON_USER_MANUAL`: measured for `CfDehydratePlaceholder` from another
/// process.
pub(crate) const DEHYDRATION_REASON_USER_MANUAL: i32 = 1;
/// `CF_CALLBACK_DEHYDRATION_REASON_SYSTEM_LOW_SPACE`.
pub(crate) const DEHYDRATION_REASON_SYSTEM_LOW_SPACE: i32 = 2;
/// `CF_CALLBACK_DEHYDRATION_REASON_SYSTEM_INACTIVITY`.
pub(crate) const DEHYDRATION_REASON_SYSTEM_INACTIVITY: i32 = 3;
/// `CF_CALLBACK_DEHYDRATION_REASON_SYSTEM_OS_UPGRADE`.
pub(crate) const DEHYDRATION_REASON_SYSTEM_OS_UPGRADE: i32 = 4;
/// `STATUS_CLOUD_FILE_ACCESS_DENIED`: the status a dehydration is vetoed with. Measured
/// (Windows 11, in the VM): the caller's `CfDehydratePlaceholder` fails at once with
/// `ERROR_CLOUD_FILE_ACCESS_DENIED` (395), and the file keeps its bytes and its pin state.
pub(crate) const STATUS_CLOUD_FILE_ACCESS_DENIED: i32 = 0xC000_CF18_u32.cast_signed();

/// cfapi's transfer granularity, in bytes.
const ALIGN: i64 = 4096;
/// The prefix of a verbatim path, `\\?\`, in UTF-16.
const VERBATIM: [u16; 4] = [0x5C, 0x5C, 0x3F, 0x5C];

/// The most UTF-16 units of a provider's name (`CF_MAX_PROVIDER_NAME_LENGTH`).
pub(crate) const MAX_PROVIDER_NAME_UNITS: usize = 255;
/// The most UTF-16 units of a provider's version (`CF_MAX_PROVIDER_VERSION_LENGTH`).
pub(crate) const MAX_PROVIDER_VERSION_UNITS: usize = 255;

/// An `HRESULT` a cfapi call returned. `Display` prints it in hex, with its name when this layer
/// knows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hresult(i32);

impl Hresult {
	/// The `HRESULT` `raw`, as cfapi returned it.
	pub(crate) fn new(raw: i32) -> Self {
		Self(raw)
	}

	/// The `HRESULT` for the Win32 error `code` (`HRESULT_FROM_WIN32`): 0 stays 0, anything else
	/// becomes `0x8007xxxx` from its low 16 bits.
	pub(crate) fn from_win32(code: u32) -> Self {
		if code == 0 {
			return Self(0);
		}
		Self((0x8007_0000 | (code & 0xFFFF)).cast_signed())
	}

	/// The raw value.
	pub fn get(self) -> i32 {
		self.0
	}

	/// Whether the call succeeded: the severity bit is clear.
	pub fn is_ok(self) -> bool {
		self.0 >= 0
	}

	/// The name of a code this layer meets, if it is one.
	fn name(self) -> Option<&'static str> {
		const WIN32: [(u32, &str); 6] = [
			(
				ERROR_CLOUD_FILE_REQUEST_CANCELED,
				"ERROR_CLOUD_FILE_REQUEST_CANCELED",
			),
			(
				ERROR_CLOUD_FILE_REQUEST_ABORTED,
				"ERROR_CLOUD_FILE_REQUEST_ABORTED",
			),
			(ERROR_NOT_A_CLOUD_FILE, "ERROR_NOT_A_CLOUD_FILE"),
			(
				ERROR_CLOUD_FILE_INVALID_REQUEST,
				"ERROR_CLOUD_FILE_INVALID_REQUEST",
			),
			(ERROR_CLOUD_FILE_IN_USE, "ERROR_CLOUD_FILE_IN_USE"),
			(ERROR_REPARSE_TAG_MISMATCH, "ERROR_REPARSE_TAG_MISMATCH"),
		];
		match self.0 {
			E_INVALIDARG => Some("E_INVALIDARG"),
			E_FAIL => Some("E_FAIL"),
			_ => WIN32
				.iter()
				.find_map(|(code, name)| (Self::from_win32(*code) == self).then_some(*name)),
		}
	}
}

impl fmt::Display for Hresult {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "{:#010x}", self.0.cast_unsigned())?;
		if let Some(name) = self.name() {
			write!(f, " ({name})")?;
		}
		Ok(())
	}
}

/// The cfapi call that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CfCall {
	/// `CfRegisterSyncRoot`.
	Register,
	/// `CfUnregisterSyncRoot`.
	Unregister,
	/// `CfGetPlatformInfo`.
	PlatformInfo,
	/// `CfConnectSyncRoot`.
	Connect,
	/// `CfDisconnectSyncRoot`.
	Disconnect,
	/// `CfGetSyncRootInfoByPath`.
	SyncRootInfo,
}

/// `ERROR_CLOUD_FILE_NOT_UNDER_SYNC_ROOT`: the path is in no sync root.
// Only the Windows layer's calls answer with or need it.
#[cfg(windows)]
pub(crate) const ERROR_CLOUD_FILE_NOT_UNDER_SYNC_ROOT: u32 = 390;
/// `ERROR_NOT_A_CLOUD_SYNC_ROOT`: the path is not a sync root.
// Only the Windows layer's calls answer with or need it.
#[cfg(windows)]
pub(crate) const ERROR_NOT_A_CLOUD_SYNC_ROOT: u32 = 405;

/// The longest sync root identity cfapi stores, in bytes (`CfRegisterSyncRoot`: 64 KiB).
// Only the Windows layer's calls answer with or need it.
#[cfg(windows)]
pub(crate) const MAX_SYNC_ROOT_IDENTITY_LEN: usize = 64 * 1024;
/// Where `CF_SYNC_ROOT_STANDARD_INFO.SyncRootFileId` (an `i64`) sits, in bytes.
pub(crate) const SYNC_ROOT_FILE_ID_OFFSET: usize = 0;
/// Where `CF_SYNC_ROOT_STANDARD_INFO.ProviderName` sits, in bytes.
pub(crate) const SYNC_ROOT_PROVIDER_NAME_OFFSET: usize = 28;
/// `CF_SYNC_ROOT_STANDARD_INFO.ProviderName`'s length, in UTF-16 units, its NUL included.
pub(crate) const SYNC_ROOT_PROVIDER_NAME_UNITS: usize = 256;
/// Where `CF_SYNC_ROOT_STANDARD_INFO.SyncRootIdentityLength` (a `u32`) sits, in bytes.
pub(crate) const SYNC_ROOT_IDENTITY_LEN_OFFSET: usize = 1052;
/// Where `CF_SYNC_ROOT_STANDARD_INFO.SyncRootIdentity` starts, in bytes.
pub(crate) const SYNC_ROOT_IDENTITY_OFFSET: usize = 1056;

/// The layout of the sync root identity this build registers, and the only one it reads.
const SYNC_ROOT_IDENTITY_V1: u8 = 1;
/// The layout's byte, the provider's GUID and the account root's uuid.
const SYNC_ROOT_IDENTITY_LEN: usize = 1 + 16 + 16;

/// The sync root identity a root of the provider `provider_id` is registered with for the account
/// whose root directory is `account_root`: `[SYNC_ROOT_IDENTITY_V1][GUID, big-endian][uuid]`.
/// `CfGetSyncRootInfoByPath` reports a registration's name, root file id and identity but not its
/// provider GUID, so the GUID is carried in the identity, where the provider's own check finds it.
/// [`SyncRootIdentity::account`] reads the layout [`SyncRootIdentity::new`] writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SyncRootIdentity([u8; SYNC_ROOT_IDENTITY_LEN]);

impl SyncRootIdentity {
	pub(crate) fn new(provider_id: u128, account_root: Uuid) -> Self {
		let mut bytes = [0_u8; SYNC_ROOT_IDENTITY_LEN];
		let (format, rest) = bytes.split_at_mut(1);
		let (guid, uuid) = rest.split_at_mut(16);
		format[0] = SYNC_ROOT_IDENTITY_V1;
		guid.copy_from_slice(&provider_id.to_be_bytes());
		uuid.copy_from_slice(account_root.as_bytes());
		Self(bytes)
	}

	/// The bytes registered.
	pub(crate) fn as_bytes(&self) -> &[u8] {
		&self.0
	}

	/// The account root the identity `identity`, as a registration reports it back, names for the
	/// provider `provider_id`; `None` for any other bytes, another provider's GUID included.
	pub(crate) fn account(identity: &[u8], provider_id: u128) -> Option<Uuid> {
		let (&format, rest) = identity.split_first()?;
		let (guid, uuid) = rest.split_first_chunk::<16>()?;
		let uuid = <[u8; 16]>::try_from(uuid).ok()?;
		(format == SYNC_ROOT_IDENTITY_V1 && u128::from_be_bytes(*guid) == provider_id)
			.then(|| Uuid::from_bytes(uuid))
	}
}

/// What `CfGetSyncRootInfoByPath` reports of the sync root a path is at or below.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SyncRootStandard {
	/// The root directory's 64-bit file id.
	pub(crate) file_id: i64,
	/// The name its provider registered, without the NUL.
	pub(crate) provider_name: Vec<u16>,
	/// The sync root's identity.
	pub(crate) identity: Vec<u8>,
}

/// The standard info of `bytes`, the bytes `CfGetSyncRootInfoByPath` wrote: `None` when they are
/// too short for a field read or for the identity they say follows, or the name has no NUL.
pub(crate) fn parse_sync_root_info(bytes: &[u8]) -> Option<SyncRootStandard> {
	let file_id_end = SYNC_ROOT_FILE_ID_OFFSET.checked_add(8)?;
	let file_id = i64::from_ne_bytes(
		bytes
			.get(SYNC_ROOT_FILE_ID_OFFSET..file_id_end)?
			.try_into()
			.ok()?,
	);
	let name_end = SYNC_ROOT_PROVIDER_NAME_OFFSET
		.checked_add(SYNC_ROOT_PROVIDER_NAME_UNITS.checked_mul(2)?)?;
	let name = bytes.get(SYNC_ROOT_PROVIDER_NAME_OFFSET..name_end)?;
	let provider_name = take_until_nul(
		name.as_chunks::<2>()
			.0
			.iter()
			.map(|pair| u16::from_ne_bytes(*pair)),
		SYNC_ROOT_PROVIDER_NAME_UNITS,
	)?;
	let len_end = SYNC_ROOT_IDENTITY_LEN_OFFSET.checked_add(4)?;
	let identity_len = u32::from_ne_bytes(
		bytes
			.get(SYNC_ROOT_IDENTITY_LEN_OFFSET..len_end)?
			.try_into()
			.ok()?,
	);
	let identity_end =
		SYNC_ROOT_IDENTITY_OFFSET.checked_add(usize::try_from(identity_len).ok()?)?;
	Some(SyncRootStandard {
		file_id,
		provider_name,
		identity: bytes.get(SYNC_ROOT_IDENTITY_OFFSET..identity_end)?.to_vec(),
	})
}

/// Who holds the sync root a path was looked up in, as far as this provider is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyncRootOwner {
	/// The path is this provider's sync root, registered for the account whose root directory is
	/// `account_root`.
	Ours {
		/// The account root its identity names.
		account_root: Uuid,
	},
	/// The path is another provider's sync root, or lies inside a sync root without being one.
	Other,
}

/// Who holds the path `path_file_id` names, from the `info` of the sync root it is at or below:
/// ours only when the root directory is that path, its provider name is `our_name` and its
/// identity is a [`sync_root_identity`] carrying `our_id`, this provider's GUID. A root that
/// shows the name but carries any other identity is another provider's, which nothing here
/// unregisters or takes over. `path_file_id` is the path's 128-bit `FILE_ID_INFO` id, whose low
/// 64 bits are the 64-bit id on NTFS.
pub(crate) fn sync_root_owner(
	info: &SyncRootStandard,
	our_name: &[u16],
	our_id: u128,
	path_file_id: u128,
) -> SyncRootOwner {
	let low = u64::try_from(path_file_id & u128::from(u64::MAX))
		.expect("masked to its low 64 bits (should be impossible)");
	if low.cast_signed() != info.file_id || info.provider_name != our_name {
		return SyncRootOwner::Other;
	}
	match SyncRootIdentity::account(&info.identity, our_id) {
		Some(account_root) => SyncRootOwner::Ours { account_root },
		None => SyncRootOwner::Other,
	}
}

/// What a local item's attributes and reparse tag make it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReparseKind {
	/// No reparse point: an ordinary file or directory.
	Plain,
	/// A Cloud Files placeholder, whatever its flag bits.
	Placeholder,
	/// A reparse point the core never follows.
	Unsupported(UnsupportedKind),
}

/// What the item with `attributes` and the reparse tag `tag` (`FILE_ATTRIBUTE_TAG_INFO`) is.
pub(crate) fn reparse_kind(attributes: u32, tag: u32) -> ReparseKind {
	if attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
		return ReparseKind::Plain;
	}
	match tag {
		IO_REPARSE_TAG_SYMLINK => ReparseKind::Unsupported(UnsupportedKind::Symlink),
		IO_REPARSE_TAG_MOUNT_POINT => ReparseKind::Unsupported(UnsupportedKind::Junction),
		tag if tag & !IO_REPARSE_TAG_CLOUD_MASK == IO_REPARSE_TAG_CLOUD => ReparseKind::Placeholder,
		_ => ReparseKind::Unsupported(UnsupportedKind::ForeignReparsePoint),
	}
}

/// `FILE_ATTRIBUTE_PINNED`: the user keeps the item on this device.
pub(crate) const FILE_ATTRIBUTE_PINNED: u32 = 0x0008_0000;
/// `FILE_ATTRIBUTE_UNPINNED`: the user freed the item's space.
pub(crate) const FILE_ATTRIBUTE_UNPINNED: u32 = 0x0010_0000;

/// The pin an item with `attributes` has of its own. Both bits set count as pinned: measured, a
/// call setting both changes nothing, so such an item was pinned before, and keeping data is the
/// side that never loses any.
pub(crate) fn pin_attr(attributes: u32) -> PinAttr {
	if attributes & FILE_ATTRIBUTE_PINNED != 0 {
		PinAttr::Pinned
	} else if attributes & FILE_ATTRIBUTE_UNPINNED != 0 {
		PinAttr::Unpinned
	} else {
		PinAttr::Unspecified
	}
}

/// Whether a `NOTIFY_DEHYDRATE` with `flags` comes from the platform on its own, and why it asks,
/// from its `reason`.
pub(crate) fn dehydrate_request(flags: i32, reason: i32) -> (bool, DehydrateReason) {
	let reason = match reason {
		DEHYDRATION_REASON_NONE => DehydrateReason::None,
		DEHYDRATION_REASON_USER_MANUAL => DehydrateReason::UserManual,
		DEHYDRATION_REASON_SYSTEM_LOW_SPACE => DehydrateReason::SystemLowSpace,
		DEHYDRATION_REASON_SYSTEM_INACTIVITY => DehydrateReason::SystemInactivity,
		DEHYDRATION_REASON_SYSTEM_OS_UPGRADE => DehydrateReason::SystemOsUpgrade,
		other => DehydrateReason::Other(other),
	};
	(flags & DEHYDRATE_FLAG_BACKGROUND != 0, reason)
}

/// Whether a placeholder with `attributes` is partial: a file whose data is not all on disk, or a
/// directory the platform has not populated yet. Measured (Windows 11, in the VM): an
/// on-demand directory not yet populated reads `0x401410` and a populated one in sync `0x410`.
pub(crate) fn is_partial(attributes: u32) -> bool {
	attributes
		& (FILE_ATTRIBUTE_OFFLINE
			| FILE_ATTRIBUTE_RECALL_ON_OPEN
			| FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS)
		!= 0
}

/// What the Win32 error `code` of a local operation tells the core.
pub(crate) fn win32_local_error(code: u32) -> LocalError {
	match code {
		ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => LocalError::NotFound,
		ERROR_SHARING_VIOLATION | ERROR_CLOUD_FILE_IN_USE => LocalError::Busy,
		ERROR_FILE_EXISTS | ERROR_ALREADY_EXISTS | ERROR_DIR_NOT_EMPTY => LocalError::Exists,
		ERROR_CLOUD_FILE_NOT_IN_SYNC => LocalError::Modified,
		ERROR_CLOUD_FILE_PINNED => LocalError::Pinned,
		code => LocalError::Os(OsError::Rejected {
			code: Hresult::from_win32(code).get(),
		}),
	}
}

/// What a failed cfapi call on a local item tells the core: a Win32 error wrapped in an `HRESULT`
/// (`0x8007xxxx`) as [`win32_local_error`], anything else as [`os_error`].
pub(crate) fn local_error(hr: Hresult) -> LocalError {
	let raw = hr.get().cast_unsigned();
	if raw & 0xFFFF_0000 == 0x8007_0000 {
		win32_local_error(raw & 0xFFFF)
	} else {
		LocalError::Os(os_error(hr))
	}
}

/// `true` for a UTF-16 path separator, `\` or `/`.
fn is_separator(unit: u16) -> bool {
	unit == u16::from(b'\\') || unit == u16::from(b'/')
}

/// Whether two path components name the same NTFS item: equal when lowercased, or unit for unit
/// when either is not valid UTF-16.
fn same_component(a: &[u16], b: &[u16]) -> bool {
	match (String::from_utf16(a), String::from_utf16(b)) {
		(Ok(a), Ok(b)) => a.to_lowercase() == b.to_lowercase(),
		_ => a == b,
	}
}

/// The components of `full`, a full path, below the sync root `root`, compared case-insensitively
/// and either with or without the verbatim prefix; empty for the root itself. `None` when `full`
/// is not at or below `root`.
pub(crate) fn root_relative(root: &[u16], full: &[u16]) -> Option<Vec<Vec<u16>>> {
	let split = |units: &[u16]| -> Vec<Vec<u16>> {
		units
			.strip_prefix(&VERBATIM)
			.unwrap_or(units)
			.split(|unit| is_separator(*unit))
			.filter(|component| !component.is_empty())
			.map(<[u16]>::to_vec)
			.collect()
	};
	let root = split(root);
	let full = split(full);
	if root.is_empty() || full.len() < root.len() {
		return None;
	}
	let (head, rest) = full.split_at(root.len());
	head.iter()
		.zip(&root)
		.all(|(ours, theirs)| same_component(ours, theirs))
		.then(|| rest.to_vec())
}

/// The root path of `components`, each checked as [`RootPath::parse`] does.
pub(crate) fn root_path_of(components: &[Vec<u16>]) -> Result<RootPath, RootPathError> {
	let names = components
		.iter()
		.map(|component| String::from_utf16(component).map_err(|_| RootPathError::NotUnicode))
		.collect::<Result<Vec<_>, _>>()?;
	RootPath::parse(names.iter().map(String::as_str))
}

/// The item a callback's full path `full` names in the sync root `root`: `None` when it is
/// outside the root or not a path the core can name.
pub(crate) fn callback_root_path(root: &[u16], full: &[u16]) -> Option<RootPath> {
	root_path_of(&root_relative(root, full)?).ok()
}

/// What a `NOTIFY_DELETE`'s `flags` say: whether the item is a directory, and whether the
/// platform takes a delete back rather than announcing one.
pub(crate) fn delete_flags(flags: i32) -> (bool, bool) {
	(
		flags & DELETE_FLAG_IS_DIRECTORY != 0,
		flags & DELETE_FLAG_IS_UNDELETE != 0,
	)
}

/// Whether a `NOTIFY_RENAME`'s `flags` say the item is a directory.
pub(crate) fn rename_is_directory(flags: i32) -> bool {
	flags & RENAME_FLAG_IS_DIRECTORY != 0
}

/// Whether a `NOTIFY_FILE_CLOSE_COMPLETION`'s `flags` say the closed handle deleted the file.
pub(crate) fn close_deleted(flags: Option<i32>) -> bool {
	flags.is_some_and(|flags| flags & CLOSE_COMPLETION_FLAG_DELETED != 0)
}

/// Where a `NOTIFY_RENAME` sends its placeholder, as the core is asked about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RenameScope {
	/// Into the sync root, at a path the core can name.
	InRoot(RootPath),
	/// Out of the sync root: a delete of the item, as far as the drive goes.
	OutOfRoot,
	/// A rename the core cannot be asked about, vetoed with the failure: its target is in scope
	/// by its flags but not a path below the root (never taken for a move out, which trashes the
	/// remote item), or names an item the core cannot name.
	Refused(Failure),
}

/// Where a `NOTIFY_RENAME` with `flags` and the full target path `target` sends its placeholder,
/// in the sync root `root`. The flags say whether the target is in the root; the path says where.
pub(crate) fn rename_scope(flags: i32, root: &[u16], target: Option<&[u16]>) -> RenameScope {
	if flags & RENAME_FLAG_TARGET_IN_SCOPE == 0 {
		return RenameScope::OutOfRoot;
	}
	let Some(components) = target.and_then(|target| root_relative(root, target)) else {
		return RenameScope::Refused(Failure::Internal);
	};
	if components.is_empty() {
		return RenameScope::Refused(Failure::InvalidRequest);
	}
	match root_path_of(&components) {
		Ok(path) => RenameScope::InRoot(path),
		Err(_) => RenameScope::Refused(Failure::InvalidRequest),
	}
}

/// The units of the path `units` as the long-path form takes them: a drive-absolute path of at
/// least `MAX_PATH - 12` units, separated by `\` only, gets the verbatim prefix, so a Win32 call
/// taking it is not cut at `MAX_PATH`; anything else is unchanged, as a shorter path was measured
/// in its plain form.
pub(crate) fn long_path(units: &[u16]) -> Vec<u16> {
	/// `MAX_PATH` less the 8.3 name a directory must leave room for.
	const SHORT_LIMIT: usize = 248;
	if units.len() < SHORT_LIMIT
		|| units.starts_with(&VERBATIM)
		|| !is_drive_absolute(units)
		|| units.contains(&u16::from(b'/'))
	{
		return units.to_vec();
	}
	VERBATIM.iter().chain(units).copied().collect()
}

/// `units` with a NUL appended, as a Win32 call takes a path; `None` when they hold a NUL.
pub(crate) fn path_nul(units: &[u16]) -> Option<WideNul> {
	WideNul::new(units)
}

/// A cfapi call that failed: `call` returned the failure `code`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{call:?} failed: {code}")]
pub struct WinError {
	/// The call.
	pub call: CfCall,
	/// What it returned.
	pub code: Hresult,
}

/// The most UTF-16 units of an image's file name logged: a file name's longest.
const MAX_IMAGE_NAME_UNITS: usize = 255;

/// The file name of the image a callback's process runs (`CF_PROCESS_INFO.ImagePath`), for the
/// log: its last component only, so no user's directory or other part of the path is logged;
/// `None` for an empty path, one ending in a separator, or a name longer than a file name holds.
pub(crate) fn image_file_name(path: &[u16]) -> Option<String> {
	let name = path.rsplit(|unit| is_separator(*unit)).next()?;
	if name.is_empty() || name.len() > MAX_IMAGE_NAME_UNITS {
		return None;
	}
	Some(String::from_utf16_lossy(name))
}

/// A string as cfapi takes it: UTF-16 with exactly one NUL, its last unit. Only the parsers in
/// this module build one, which is what makes passing its pointer to cfapi sound.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct WideNul(Box<[u16]>);

impl WideNul {
	/// `units` with a NUL appended, or `None` when they hold a NUL already.
	fn new(units: &[u16]) -> Option<Self> {
		if units.contains(&0) {
			return None;
		}
		Some(Self(units.iter().copied().chain(iter::once(0)).collect()))
	}

	/// The units, the trailing NUL included.
	pub(crate) fn with_nul(&self) -> &[u16] {
		&self.0
	}

	/// The units without the trailing NUL.
	pub(crate) fn without_nul(&self) -> &[u16] {
		self.0.split_last().map_or(&[], |(_, units)| units)
	}
}

impl fmt::Debug for WideNul {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		fmt::Debug::fmt(&String::from_utf16_lossy(self.without_nul()), f)
	}
}

/// Why a path cannot be a sync root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SyncRootPathError {
	/// The path is empty.
	#[error("the sync root path is empty")]
	Empty,
	/// The path holds a NUL.
	#[error("the sync root path holds a NUL")]
	InteriorNul,
	/// Not an absolute path on a local drive (`X:\...` or `\\?\X:\...`): a relative, UNC or device
	/// path.
	#[error("the sync root must be an absolute path on a local drive")]
	NotLocalAbsolute,
}

/// Whether `units` start with a drive letter, a colon and a backslash.
fn is_drive_absolute(units: &[u16]) -> bool {
	starts_with_drive(units) && units.get(2) == Some(&u16::from(b'\\'))
}

/// The sync root path `units` as cfapi takes it: `X:\...` or `\\?\X:\...`, refused when empty,
/// holding a NUL, relative, UNC (`\\server\...`) or a device path (`\\.\...`).
pub(crate) fn sync_root_units(units: &[u16]) -> Result<WideNul, SyncRootPathError> {
	if units.is_empty() {
		return Err(SyncRootPathError::Empty);
	}
	let local = units.strip_prefix(&VERBATIM).unwrap_or(units);
	if !is_drive_absolute(local) {
		return Err(SyncRootPathError::NotLocalAbsolute);
	}
	WideNul::new(units).ok_or(SyncRootPathError::InteriorNul)
}

/// Why a provider name or version cannot be registered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ProviderInfoError {
	/// The string is empty.
	#[error("the provider string is empty")]
	Empty,
	/// The string holds a NUL.
	#[error("the provider string holds a NUL")]
	InteriorNul,
	/// `units` UTF-16 units, more than cfapi takes.
	#[error("the provider string is {units} UTF-16 units, more than cfapi takes")]
	TooLong { units: usize },
}

/// `s` as cfapi takes a provider name or version: 1 to `max` UTF-16 units, no NUL.
pub(crate) fn provider_units(s: &str, max: usize) -> Result<WideNul, ProviderInfoError> {
	let units: Vec<u16> = s.encode_utf16().collect();
	if units.is_empty() {
		return Err(ProviderInfoError::Empty);
	}
	if units.len() > max {
		return Err(ProviderInfoError::TooLong { units: units.len() });
	}
	WideNul::new(&units).ok_or(ProviderInfoError::InteriorNul)
}

/// The `NTSTATUS` a request failed with `failure` is answered with. Every one is a
/// `STATUS_CLOUD_FILE_*` code: cfapi hands the reader any other status as error 389, "the cloud
/// operation was unsuccessful" (measured). Each arm is a recorded default, to be checked against
/// what Explorer shows.
pub(crate) fn failure_status(failure: Failure) -> i32 {
	match failure {
		Failure::NotAuthenticated => STATUS_CLOUD_FILE_AUTHENTICATION_FAILED,
		Failure::NotFound | Failure::StalePlaceholder => STATUS_CLOUD_FILE_NOT_IN_SYNC,
		Failure::Unreachable => STATUS_CLOUD_FILE_NETWORK_UNAVAILABLE,
		Failure::Unsupported => STATUS_CLOUD_FILE_NOT_SUPPORTED,
		Failure::ContentDamaged => STATUS_CLOUD_FILE_VALIDATION_FAILED,
		Failure::InvalidRequest => STATUS_CLOUD_FILE_INVALID_REQUEST,
		Failure::Undecryptable | Failure::Transient | Failure::Internal => {
			STATUS_CLOUD_FILE_UNSUCCESSFUL
		}
	}
}

/// What a failed cfapi call tells the core: the request is gone (cancelled or aborted), or the OS
/// refused the call with `hr`.
pub(crate) fn os_error(hr: Hresult) -> OsError {
	if hr == Hresult::from_win32(ERROR_CLOUD_FILE_REQUEST_CANCELED)
		|| hr == Hresult::from_win32(ERROR_CLOUD_FILE_REQUEST_ABORTED)
	{
		OsError::Cancelled
	} else {
		OsError::Rejected { code: hr.get() }
	}
}

/// What a failed `TRANSFER_DATA` tells the core: as [`os_error`], and `ERROR_NOT_A_CLOUD_FILE`
/// means the request is gone too. Measured only when the reader had closed its handle, holding
/// its bytes already or having cancelled, so it ends the request without a retry.
pub(crate) fn transfer_error(hr: Hresult) -> OsError {
	if hr == Hresult::from_win32(ERROR_NOT_A_CLOUD_FILE) {
		OsError::Cancelled
	} else {
		os_error(hr)
	}
}

/// Whether a `TRANSFER_DATA` refused with `hr`, which [`transfer_error`] ends as cancelled, is
/// still answered with a failure: `ERROR_NOT_A_CLOUD_FILE`, whose request was gone each time it
/// was measured but whose cause is not established, so a request still live is not left to wait
/// out the OS's 60 s timeout. A failure answer to a request gone is refused, as one to a cancelled
/// request was measured to be. A refusal as cancelled or aborted is the OS's own cancel.
pub(crate) fn transfer_refusal_unconfirmed(hr: Hresult) -> bool {
	hr == Hresult::from_win32(ERROR_NOT_A_CLOUD_FILE)
}

/// Whether a struct cfapi passed, `struct_size` bytes long, holds the field of `field_size` bytes
/// at `field_offset`: an older Windows may pass a shorter struct than this build declares.
pub(crate) fn field_present(struct_size: u32, field_offset: usize, field_size: usize) -> bool {
	match (
		usize::try_from(struct_size),
		field_offset.checked_add(field_size),
	) {
		(Ok(size), Some(end)) => end <= size,
		_ => false,
	}
}

/// The length of the file identity a callback carries, checked before any slice of it exists:
/// none (a null pointer or 0 bytes) and more than [`MAX_IDENTITY_LEN`] are refused.
pub(crate) fn identity_len(is_null: bool, len: u32) -> Result<usize, IdentityError> {
	if is_null || len == 0 {
		return Err(IdentityError::Empty);
	}
	let len = usize::try_from(len).map_err(|_| IdentityError::TooLong { len: usize::MAX })?;
	if len > MAX_IDENTITY_LEN {
		return Err(IdentityError::TooLong { len });
	}
	Ok(len)
}

/// The units before the first NUL of `units`, or `None` when no NUL comes within `max` units. The
/// iterator is not advanced past the NUL, so it may read the string's memory lazily.
pub(crate) fn take_until_nul(units: impl Iterator<Item = u16>, max: usize) -> Option<Vec<u16>> {
	let mut out = Vec::new();
	for unit in units.take(max) {
		if unit == 0 {
			return Some(out);
		}
		out.push(unit);
	}
	None
}

/// Whether `units` start with a drive letter and a colon.
fn starts_with_drive(units: &[u16]) -> bool {
	match units {
		[letter, colon, ..] => {
			u8::try_from(*letter).is_ok_and(|letter| letter.is_ascii_alphabetic())
				&& *colon == u16::from(b':')
		}
		_ => false,
	}
}

/// The full path of a callback's file: `normalized` (`CF_CALLBACK_INFO.NormalizedPath`, from the
/// volume's root) after `dos` (`VolumeDosName`, `X:`), or `normalized` alone when it is absolute
/// already. `None` when neither gives an absolute path.
pub(crate) fn volume_path(dos: Option<&[u16]>, normalized: &[u16]) -> Option<Vec<u16>> {
	if starts_with_drive(normalized) || normalized.starts_with(&VERBATIM) {
		return Some(normalized.to_vec());
	}
	let dos = dos.filter(|dos| dos.len() == 2 && starts_with_drive(dos))?;
	if normalized.first() != Some(&u16::from(b'\\')) {
		return None;
	}
	Some(dos.iter().chain(normalized).copied().collect())
}

/// `len` bytes at `offset` of a `file_size`-byte file, as `(start, end)`: clamped to the file,
/// its start aligned down to 4096 bytes and its end up to 4096 or the end of the file. `None` when
/// nothing of it lies in the file.
fn aligned_in_file(offset: i64, len: i64, file_size: i64) -> Option<(i64, i64)> {
	let end = offset.checked_add(len)?.min(file_size);
	let start = offset.max(0);
	if start >= end {
		return None;
	}
	let start = start - start % ALIGN;
	let end = match end % ALIGN {
		0 => end,
		rest => end.checked_add(ALIGN - rest)?.min(file_size),
	};
	Some((start, end))
}

/// The range a failure answer names for a request that required `required` (offset, length) of
/// a `file_size`-byte file: the required range clamped to the file and aligned as
/// [`aligned_in_file`] does. For the aligned, in-file ranges Windows sends that is the range as
/// sent, which a failure was measured to be accepted with. The range as sent when nothing of it
/// lies in the file, and the whole file when the callback's parameters held no range.
pub(crate) fn failure_range(required: Option<(i64, i64)>, file_size: i64) -> (i64, i64) {
	let (offset, len) = required.unwrap_or((0, file_size));
	aligned_in_file(offset, len, file_size)
		.map_or((offset, len), |(start, end)| (start, end - start))
}

/// Whether bytes transferred over `transferred` (`[start, end)`, `None` when nothing was) cover
/// what a request that required `required` (offset, length) of a `file_size`-byte file needs: its
/// required range in the file, aligned as [`aligned_in_file`] does. A required range holding
/// nothing of the file needs nothing.
pub(crate) fn covers_required(
	transferred: Option<(i64, i64)>,
	required: (i64, i64),
	file_size: i64,
) -> bool {
	match aligned_in_file(required.0, required.1, file_size) {
		None => true,
		Some((start, end)) => transferred.is_some_and(|(from, to)| from <= start && end <= to),
	}
}

/// The outcome of each placeholder of a `TRANSFER_PLACEHOLDERS` call that returned success, from
/// its per-entry `results` and the count the OS processed. An entry past that count was not
/// processed, and is reported as refused with `ERROR_CLOUD_FILE_INVALID_REQUEST`. An entry whose
/// name a local item holds (`ERROR_ALREADY_EXISTS`, measured for a file the user moved into a
/// directory before it was populated) is [`EntryOutcome::Exists`].
pub(crate) fn entry_outcomes(results: &[i32], processed: u32) -> Vec<EntryOutcome> {
	let processed = usize::try_from(processed).unwrap_or(usize::MAX);
	let unprocessed = OsError::Rejected {
		code: Hresult::from_win32(ERROR_CLOUD_FILE_INVALID_REQUEST).get(),
	};
	results
		.iter()
		.enumerate()
		.map(|(index, &result)| {
			if index >= processed {
				EntryOutcome::Rejected(unprocessed)
			} else if Hresult::new(result).is_ok() {
				EntryOutcome::Created
			} else if [ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS]
				.map(|code| Hresult::from_win32(code).get())
				.contains(&result)
			{
				EntryOutcome::Exists
			} else {
				EntryOutcome::Rejected(os_error(Hresult::new(result)))
			}
		})
		.collect()
}

/// The values one placeholder is created with (`CF_PLACEHOLDER_CREATE_INFO`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CreateValues {
	/// `FileAttributes`.
	pub(crate) attributes: u32,
	/// `FileSize`, in bytes.
	pub(crate) size: i64,
	/// `CreationTime`, in FILETIME ticks.
	pub(crate) created: i64,
	/// `LastWriteTime`, `LastAccessTime` and `ChangeTime`, in FILETIME ticks.
	pub(crate) modified: i64,
	/// `Flags`.
	pub(crate) flags: i32,
}

/// The values `spec` is created with: a file with normal attributes and its size, a directory
/// with the directory attribute and size 0, both marked in sync. A directory is created with
/// on-demand population on, so its children are asked for when it is first opened.
pub(crate) fn create_values(spec: &PlaceholderSpec) -> CreateValues {
	let (attributes, size) = match spec.kind() {
		SpecKind::File { size } => (FILE_ATTRIBUTE_NORMAL, size.as_i64()),
		SpecKind::Dir => (FILE_ATTRIBUTE_DIRECTORY, 0),
	};
	CreateValues {
		attributes,
		size,
		created: spec.created().ticks(),
		modified: spec.modified().ticks(),
		flags: PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC,
	}
}

#[cfg(test)]
mod tests;

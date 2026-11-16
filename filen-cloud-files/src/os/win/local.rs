//! The sync root's items as the core reads and changes them on Windows: [`LocalFs`] over std's file
//! handles and the cfapi calls in `ffi`. Every handle is opened with the least access its call was
//! measured to need, so the user's own handles are refused as little as possible: the state reads
//! open for reading attributes only, which never hydrates a placeholder, and the identity and
//! in-sync changes open for writing attributes only, which any share mode admits.

use std::{
	fs::{self, File, OpenOptions},
	io::{self, Read, Seek, SeekFrom},
	os::windows::{ffi::OsStrExt, fs::OpenOptionsExt},
	path::{Path, PathBuf},
	slice,
	sync::atomic::{AtomicBool, Ordering},
};

use windows_sys::Win32::{
	Foundation::{ERROR_ACCESS_DENIED, GENERIC_READ},
	Storage::{
		CloudFilters::{
			CF_CONVERT_FLAG_ENABLE_ON_DEMAND_POPULATION, CF_CONVERT_FLAG_MARK_IN_SYNC,
			CF_CONVERT_FLAG_NONE, CF_PIN_STATE, CF_PIN_STATE_PINNED, CF_PIN_STATE_UNPINNED,
			CF_PIN_STATE_UNSPECIFIED, CF_UPDATE_FLAG_CLEAR_IN_SYNC, CF_UPDATE_FLAG_DEHYDRATE,
			CF_UPDATE_FLAG_MARK_IN_SYNC, CF_UPDATE_FLAG_VERIFY_IN_SYNC,
		},
		FileSystem::{
			DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_FLAG_BACKUP_SEMANTICS,
			FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
			FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES,
		},
	},
};

use tracing::{debug, warn};

use super::{ffi, sync_root::SyncRootPath};
use crate::{
	identity::{IdentityBlob, PlaceholderIdentity},
	names::PlaceholderName,
	os::{
		CheckedHash, EntryOutcome, FileSnapshot, FileStamp, IfPinned, LocalEntry, LocalError,
		LocalFileId, LocalFs, LocalKind, OsError, PinAttr, PlaceholderState, Updated,
		cfapi::{
			CreateValues, E_FAIL, E_INVALIDARG, ERROR_NOT_A_CLOUD_FILE, Hresult, ReparseKind,
			WideNul, create_values, entry_outcomes, is_partial, local_error, long_path, path_nul,
			pin_attr, reparse_kind, win32_local_error,
		},
	},
	placeholder::{PlaceholderSpec, SpecKind},
	root_path::{RootPath, RootPathError},
	sealed,
};

/// Every share mode: an open that refuses no other handle.
const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
/// How many bytes a hash reads at a time.
const HASH_CHUNK: usize = 1 << 20;

/// The sync root's items, read and changed through Windows.
#[derive(Debug, Clone)]
pub struct WinLocalFs {
	root: PathBuf,
}

/// A file opened for reading and writing its attributes, shared for reading and deleting only: no
/// other handle can write it while this one is open, and opening it fails with
/// `ERROR_SHARING_VIOLATION` while one already does (measured, for placeholders and ordinary files).
#[derive(Debug)]
pub struct WinSettled {
	file: File,
}

/// A directory opened for writing its attributes, shared with every other handle
/// ([`LocalFs::Dir`]).
#[derive(Debug)]
pub struct WinDir {
	file: File,
}

impl WinLocalFs {
	/// The items of the sync root at `root`.
	pub fn new(root: &SyncRootPath) -> Self {
		Self {
			root: root.as_path(),
		}
	}

	/// The full path of `path`.
	fn full(&self, path: &RootPath) -> PathBuf {
		let mut full = self.root.clone();
		for name in path.components() {
			full.push(name.as_str());
		}
		full
	}

	/// The full path of `path` as a Win32 call takes it, in its long form when it needs one.
	fn wide(&self, path: &RootPath) -> Result<WideNul, LocalError> {
		let units: Vec<u16> = self.full(path).as_os_str().encode_wide().collect();
		path_nul(&long_path(&units)).ok_or(LocalError::Os(OsError::Rejected { code: E_INVALIDARG }))
	}
}

/// What an I/O error of a local operation tells the core.
fn io_error(error: &io::Error) -> LocalError {
	match error
		.raw_os_error()
		.and_then(|code| u32::try_from(code).ok())
	{
		Some(code) => win32_local_error(code),
		None if error.kind() == io::ErrorKind::NotFound => LocalError::NotFound,
		None => LocalError::Os(OsError::Rejected { code: E_FAIL }),
	}
}

/// What a cfapi call's `hr` tells the core.
fn checked(hr: Hresult) -> Result<(), LocalError> {
	if hr.is_ok() {
		Ok(())
	} else {
		Err(local_error(hr))
	}
}

/// Opens `path` with `access` and `share`, and `flags` (`FILE_FLAG_*`).
fn open(path: &Path, access: u32, share: u32, flags: u32) -> Result<File, LocalError> {
	OpenOptions::new()
		.access_mode(access)
		.share_mode(share)
		.custom_flags(flags)
		.open(path)
		.map_err(|error| io_error(&error))
}

/// Opens the file or directory `path` for writing its attributes, refusing no other handle, and
/// checks it is still the item `expect`: [`LocalError::NotFound`] when another item took its place.
fn open_expected(path: &Path, expect: LocalFileId, access: u32) -> Result<File, LocalError> {
	let file = open(path, access, SHARE_ALL, FILE_FLAG_BACKUP_SEMANTICS)?;
	let info = ffi::item_info(&file).map_err(|error| io_error(&error))?;
	if info.file_id == expect.get() {
		Ok(file)
	} else {
		Err(LocalError::NotFound)
	}
}

/// The kind of the item `file` holds, from `info`, its size read from the handle.
fn kind_of(file: &File, info: &ffi::HandleInfo) -> Result<LocalKind, LocalError> {
	if info.attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
		return Ok(LocalKind::Dir {
			created: info.created,
		});
	}
	let size = file.metadata().map_err(|error| io_error(&error))?.len();
	Ok(LocalKind::File(FileStamp {
		size,
		created: info.created,
		modified: info.modified,
		changed: info.changed,
	}))
}

/// Checks the placeholder `file` holds is the clean placeholder `expect`: in sync, with no byte
/// written since. [`LocalError::NotFound`] when another item is there, one whose identity does not
/// decode included (logged), [`LocalError::Modified`] when it is not clean. Returns how many bytes
/// of its data are on disk.
fn check_clean(file: &File, expect: PlaceholderIdentity) -> Result<u64, LocalError> {
	let standard = match ffi::placeholder_info(file) {
		Ok(standard) => standard,
		Err(hr) if hr == Hresult::from_win32(ERROR_NOT_A_CLOUD_FILE) => {
			return Err(LocalError::NotFound);
		}
		Err(hr) => return Err(local_error(hr)),
	};
	match IdentityBlob::try_from(standard.identity.as_slice())
		.and_then(|blob| PlaceholderIdentity::decode(&blob))
	{
		Ok(identity) if identity == expect => {}
		Ok(_) => return Err(LocalError::NotFound),
		Err(error) => {
			debug!(%error, "the placeholder to change holds an identity that does not decode");
			return Err(LocalError::NotFound);
		}
	}
	if !standard.in_sync || standard.modified > 0 {
		return Err(LocalError::Modified);
	}
	Ok(byte_count(standard.on_disk))
}

/// Whether the file placeholder `file` holds `identity`, is not all on disk and holds no byte
/// written since it was last in sync. [`LocalError::NotFound`] for a directory, an item that is no
/// placeholder, or one holding another identity or one that does not decode.
fn unwritten_partial(file: &File, identity: PlaceholderIdentity) -> Result<bool, LocalError> {
	let info = ffi::item_info(file).map_err(|error| io_error(&error))?;
	let LocalKind::File(stamp) = kind_of(file, &info)? else {
		return Err(LocalError::NotFound);
	};
	let standard = match ffi::placeholder_info(file) {
		Ok(standard) => standard,
		Err(hr) if hr == Hresult::from_win32(ERROR_NOT_A_CLOUD_FILE) => {
			return Err(LocalError::NotFound);
		}
		Err(hr) => return Err(local_error(hr)),
	};
	match IdentityBlob::try_from(standard.identity.as_slice())
		.and_then(|blob| PlaceholderIdentity::decode(&blob))
	{
		Ok(held) if held == identity => {}
		_ => return Err(LocalError::NotFound),
	}
	Ok(standard.modified == 0 && byte_count(standard.on_disk) < stamp.size)
}

/// Opens `path` to delete, rename, dehydrate or update it while no other handle may write it, the
/// provider's clean-placeholder check and its change made through the one handle: `DELETE` takes
/// part in share checks, so the open fails with `ERROR_SHARING_VIOLATION` while another handle
/// writes the item or does not share deleting, and none may open it for writing until this one
/// closes. It reads no data, so it fetches nothing of a placeholder not all on disk (measured,
/// Windows 11 ARM64 in the VM, where an open for reading data fetched all of it first). The
/// change goes through this handle, so it changes the item checked even when a user renamed it or
/// put another item at its path meanwhile.
fn open_guarded(path: &Path) -> Result<File, LocalError> {
	open(
		path,
		DELETE | FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES,
		FILE_SHARE_READ | FILE_SHARE_DELETE,
		FILE_FLAG_BACKUP_SEMANTICS,
	)
}

/// The BLAKE3 hash of `file`'s bytes, read from its start; [`LocalError::Busy`] once `stop` is set.
fn hash_file(file: &mut File, stop: Option<&AtomicBool>) -> Result<[u8; 32], LocalError> {
	file.seek(SeekFrom::Start(0))
		.map_err(|error| io_error(&error))?;
	let mut hasher = blake3::Hasher::new();
	let mut buffer = vec![0; HASH_CHUNK];
	loop {
		if stop.is_some_and(|stop| stop.load(Ordering::Relaxed)) {
			return Err(LocalError::Busy);
		}
		match file.read(&mut buffer) {
			Ok(0) => return Ok(*hasher.finalize().as_bytes()),
			Ok(read) => {
				hasher.update(buffer.get(..read).unwrap_or_default());
			}
			Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
			Err(error) => return Err(io_error(&error)),
		}
	}
}

/// The file placeholder `file` holds, as it is now; [`LocalError::NotFound`] for a directory or an
/// item that is no placeholder.
fn snapshot(file: &File) -> Result<FileSnapshot, LocalError> {
	let info = ffi::item_info(file).map_err(|error| io_error(&error))?;
	let LocalKind::File(stamp) = kind_of(file, &info)? else {
		return Err(LocalError::NotFound);
	};
	let standard = match ffi::placeholder_info(file) {
		Ok(standard) => standard,
		Err(hr) if hr == Hresult::from_win32(ERROR_NOT_A_CLOUD_FILE) => {
			return Err(LocalError::NotFound);
		}
		Err(hr) => return Err(local_error(hr)),
	};
	Ok(FileSnapshot {
		id: LocalFileId::new(info.file_id),
		stamp,
		state: PlaceholderState::new(
			standard.in_sync,
			is_partial(info.attributes),
			byte_count(standard.on_disk),
			byte_count(standard.modified),
			pin_attr(info.attributes),
		),
	})
}

/// The `CF_PIN_STATE` that sets `pin`.
fn pin_state(pin: PinAttr) -> CF_PIN_STATE {
	match pin {
		PinAttr::Unspecified => CF_PIN_STATE_UNSPECIFIED,
		PinAttr::Pinned => CF_PIN_STATE_PINNED,
		PinAttr::Unpinned => CF_PIN_STATE_UNPINNED,
	}
}

/// Runs `change`, which the platform refuses on a pinned file, on the placeholder `file`: when it
/// is pinned, unpinned for the change and pinned again after it, whether the change went through
/// or not. Measured (Windows 11, in the VM): a pinned file refuses a dehydration with
/// `ERROR_CLOUD_FILE_PINNED`, and one no longer pinned takes it. Says whether it was pinned.
fn unpinned_for<T>(
	file: &File,
	change: impl FnOnce() -> Result<T, LocalError>,
) -> Result<(T, bool), LocalError> {
	let info = ffi::item_info(file).map_err(|error| io_error(&error))?;
	if pin_attr(info.attributes) != PinAttr::Pinned {
		return change().map(|done| (done, false));
	}
	checked(ffi::set_pin_state(file, CF_PIN_STATE_UNSPECIFIED))?;
	let changed = change();
	let repinned = checked(ffi::set_pin_state(file, CF_PIN_STATE_PINNED));
	if let Err(error) = &repinned {
		warn!(%error, "pinning a file again after changing it failed");
	}
	let done = changed?;
	repinned.map(|()| (done, true))
}

/// `CfDehydratePlaceholder` of the file at `path` through a handle for writing its attributes,
/// as an application other than the provider asks the platform to dehydrate a file: for the VM
/// harness's user processes. Measured (Windows 11, in the VM): the platform asks the
/// provider (`NOTIFY_DEHYDRATE`) and the call returns once the provider dehydrated the file, or at
/// once with the veto's error.
#[cfg(feature = "test-seams")]
pub fn user_dehydrate(path: &Path) -> Hresult {
	match OpenOptions::new()
		.access_mode(FILE_WRITE_ATTRIBUTES)
		.share_mode(SHARE_ALL)
		.open(path)
	{
		Ok(file) => ffi::dehydrate(&file),
		Err(error) => error
			.raw_os_error()
			.and_then(|code| u32::try_from(code).ok())
			.map_or(Hresult::new(E_FAIL), Hresult::from_win32),
	}
}

/// A byte count cfapi reports as an `i64`, a negative one read as 0.
fn byte_count(count: i64) -> u64 {
	u64::try_from(count).unwrap_or(0)
}

impl sealed::Sealed for WinLocalFs {}

impl LocalFs for WinLocalFs {
	/// `CfRevertPlaceholder` through a handle checked to be `expect`, opened for writing attributes,
	/// as the conversion is.
	fn revert_to_plain(&self, path: &RootPath, expect: LocalFileId) -> Result<(), LocalError> {
		let file = open_expected(&self.full(path), expect, FILE_WRITE_ATTRIBUTES)?;
		checked(ffi::revert(&file))
	}

	/// Read through a handle for reading shared with every other, as [`LocalFs::hash_checked`]
	/// reads (measured: reading a placeholder all on disk so raises no `FETCH_DATA`), after its id
	/// and, for a placeholder, the bytes on disk were checked through it; written to a file created
	/// new (`CREATE_NEW`), synced before this returns.
	fn copy_to_plain(
		&self,
		path: &RootPath,
		expect: LocalFileId,
		to: &RootPath,
	) -> Result<(), LocalError> {
		let mut file = open(&self.full(path), GENERIC_READ, SHARE_ALL, 0)?;
		let info = ffi::item_info(&file).map_err(|error| io_error(&error))?;
		let LocalKind::File(stamp) = kind_of(&file, &info)? else {
			return Err(LocalError::NotFound);
		};
		let on_disk = match ffi::placeholder_info(&file) {
			Ok(standard) => byte_count(standard.on_disk),
			Err(hr) if hr == Hresult::from_win32(ERROR_NOT_A_CLOUD_FILE) => stamp.size,
			Err(hr) => return Err(local_error(hr)),
		};
		if info.file_id != expect.get() || on_disk < stamp.size {
			return Err(LocalError::NotFound);
		}
		let target = self.full(to);
		let mut copy = OpenOptions::new()
			.write(true)
			.create_new(true)
			.open(&target)
			.map_err(|error| io_error(&error))?;
		let copied = io::copy(&mut file, &mut copy).and_then(|_| copy.sync_all());
		if let Err(error) = copied {
			drop(copy);
			if let Err(removing) = fs::remove_file(&target) {
				warn!(%removing, "removing a copy cut short failed");
			}
			return Err(io_error(&error));
		}
		Ok(())
	}

	type Settled = WinSettled;

	type Dir = WinDir;

	/// Opens the item for reading its attributes only, without following a reparse point, to tell
	/// a placeholder from a link; then a placeholder again, following it, for its cfapi state, as
	/// the state reads were measured through such handles. Neither open hydrates.
	fn inspect(&self, path: &RootPath) -> Result<LocalEntry, LocalError> {
		let full = self.full(path);
		let file = match open(
			&full,
			FILE_READ_ATTRIBUTES,
			SHARE_ALL,
			FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
		) {
			Ok(file) => file,
			Err(LocalError::NotFound) => return Ok(LocalEntry::Missing),
			Err(error) => return Err(error),
		};
		let info = ffi::item_info(&file).map_err(|error| io_error(&error))?;
		let id = LocalFileId::new(info.file_id);
		match reparse_kind(info.attributes, info.reparse_tag) {
			ReparseKind::Unsupported(kind) => return Ok(LocalEntry::Unsupported(kind)),
			ReparseKind::Plain => {
				return Ok(LocalEntry::Plain {
					kind: kind_of(&file, &info)?,
					id,
				});
			}
			ReparseKind::Placeholder => {}
		}
		drop(file);
		let file = open(
			&full,
			FILE_READ_ATTRIBUTES,
			SHARE_ALL,
			FILE_FLAG_BACKUP_SEMANTICS,
		)?;
		let info = ffi::item_info(&file).map_err(|error| io_error(&error))?;
		if info.file_id != id.get() {
			// Replaced between the two opens: the caller looks again on the event that raised.
			return Err(LocalError::NotFound);
		}
		let kind = kind_of(&file, &info)?;
		let standard = match ffi::placeholder_info(&file) {
			Ok(standard) => standard,
			Err(hr) if hr == Hresult::from_win32(ERROR_NOT_A_CLOUD_FILE) => {
				return Ok(LocalEntry::Plain { kind, id });
			}
			Err(hr) => return Err(local_error(hr)),
		};
		Ok(LocalEntry::Placeholder {
			kind,
			id,
			identity: IdentityBlob::try_from(standard.identity.as_slice())
				.and_then(|blob| PlaceholderIdentity::decode(&blob)),
			state: PlaceholderState::new(
				standard.in_sync,
				is_partial(info.attributes),
				byte_count(standard.on_disk),
				byte_count(standard.modified),
				pin_attr(info.attributes),
			),
		})
	}

	/// Lists the directory through `FindFirstFileW` (std's `read_dir`): measured to return no entry
	/// and raise no population for an on-demand directory not yet populated.
	fn list_dir(
		&self,
		path: &RootPath,
	) -> Result<Vec<Result<RootPath, RootPathError>>, LocalError> {
		let entries = fs::read_dir(self.full(path)).map_err(|error| io_error(&error))?;
		entries
			.map(|entry| {
				let name = entry.map_err(|error| io_error(&error))?.file_name();
				Ok(match name.to_str() {
					None => Err(RootPathError::NotUnicode),
					Some(name) => PlaceholderName::parse(name)
						.map(|name| path.join(name))
						.map_err(|rejection| RootPathError::Name {
							component: name.to_owned(),
							rejection,
						}),
				})
			})
			.collect()
	}

	fn open_settled(&self, path: &RootPath) -> Result<WinSettled, LocalError> {
		let file = open(
			&self.full(path),
			GENERIC_READ | FILE_WRITE_ATTRIBUTES,
			FILE_SHARE_READ | FILE_SHARE_DELETE,
			0,
		)?;
		Ok(WinSettled { file })
	}

	fn settled_stamp(&self, file: &WinSettled) -> Result<(LocalFileId, FileStamp), LocalError> {
		let info = ffi::item_info(&file.file).map_err(|error| io_error(&error))?;
		match kind_of(&file.file, &info)? {
			LocalKind::File(stamp) => Ok((LocalFileId::new(info.file_id), stamp)),
			LocalKind::Dir { .. } => Err(LocalError::NotFound),
		}
	}

	/// Reads the file from its start through the settled handle. A placeholder's data the file
	/// does not hold is fetched from this provider (measured: its own reads raise `FETCH_DATA`), so
	/// this runs on the blocking pool, holding nothing a fetch needs.
	fn hash(&self, file: &mut WinSettled) -> Result<[u8; 32], LocalError> {
		hash_file(&mut file.file, None)
	}

	fn upload_path(&self, path: &RootPath) -> PathBuf {
		self.full(path)
	}

	/// Measured: a conversion through any handle succeeds, and one without `MARK_IN_SYNC` leaves a
	/// placeholder not in sync whose every byte counts as modified.
	fn convert_file(
		&self,
		file: &WinSettled,
		identity: PlaceholderIdentity,
		in_sync: bool,
	) -> Result<(), LocalError> {
		let flags = if in_sync {
			CF_CONVERT_FLAG_MARK_IN_SYNC
		} else {
			CF_CONVERT_FLAG_NONE
		};
		checked(ffi::convert(&file.file, &identity.encode(), flags))
	}

	/// Through the settled handle, which holds `FILE_WRITE_ATTRIBUTES` as the handle
	/// [`LocalFs::revert_to_plain`] reverts through does.
	fn revert_settled(&self, file: &WinSettled) -> Result<(), LocalError> {
		checked(ffi::revert(&file.file))
	}

	/// Not read through the handle here: the core walks the root instead.
	fn settled_path(&self, _file: &WinSettled) -> Result<Option<RootPath>, LocalError> {
		Err(LocalError::Os(OsError::Rejected { code: E_FAIL }))
	}

	/// `MARK_IN_SYNC` marks the file in sync whatever was written to it (measured: it hides a
	/// user's change), so the core calls it only for the bytes it just hashed under this handle.
	/// Marked in sync, the placeholder is given the file's size and times as they are: the platform
	/// counts every byte past the size it last gave the placeholder as modified, in sync or not
	/// (measured in the VM's saves loop: a file grown from 7,003 to 7,006 and then 7,010 bytes by
	/// overwrites in place, each uploaded and marked in sync, kept 3 and then 7 modified bytes).
	fn update_file(
		&self,
		file: &WinSettled,
		identity: PlaceholderIdentity,
		in_sync: bool,
	) -> Result<(), LocalError> {
		if !in_sync {
			return checked(ffi::update(
				&file.file,
				&identity.encode(),
				CF_UPDATE_FLAG_CLEAR_IN_SYNC,
			));
		}
		let info = ffi::item_info(&file.file).map_err(|error| io_error(&error))?;
		let size = file
			.file
			.metadata()
			.map_err(|error| io_error(&error))?
			.len();
		let values = CreateValues {
			// 0 leaves the attributes as they are.
			attributes: 0,
			size: i64::try_from(size)
				.map_err(|_| LocalError::Os(OsError::Rejected { code: E_INVALIDARG }))?,
			created: info.created,
			modified: info.modified,
			flags: 0,
		};
		checked(ffi::update_with_metadata(
			&file.file,
			&identity.encode(),
			&values,
			CF_UPDATE_FLAG_MARK_IN_SYNC,
		))
	}

	/// Checked, marked and checked again through one handle for reading and writing attributes,
	/// which fetches nothing: measured (Windows 11 ARM64, in the VM), opening a dehydrated
	/// placeholder from the provider's own process with `GENERIC_READ | FILE_WRITE_ATTRIBUTES`,
	/// shared for reading and deleting, as [`LocalFs::open_settled`] opens a file, raised a
	/// `FETCH_DATA` for all of it before the open returned. Marked with the identity alone, as
	/// [`LocalFs::rename_clean`] marks a renamed placeholder: its size and times stay as the
	/// platform holds them, which a file no byte was written to keeps.
	///
	/// An attribute handle keeps no writer out, so the file is read again after the mark: a write
	/// hydrates the whole file first, and one that came all on disk by then may hold a write the
	/// mark hid. It is marked not in sync again and refused with [`LocalError::Modified`], and the
	/// core's next look compares its bytes with the drive's.
	fn mark_partial_in_sync(
		&self,
		path: &RootPath,
		expect: LocalFileId,
		identity: PlaceholderIdentity,
	) -> Result<(), LocalError> {
		let file = open_expected(
			&self.full(path),
			expect,
			FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES,
		)?;
		if !unwritten_partial(&file, identity)? {
			return Err(LocalError::Modified);
		}
		checked(ffi::update(
			&file,
			&identity.encode(),
			CF_UPDATE_FLAG_MARK_IN_SYNC,
		))?;
		if unwritten_partial(&file, identity)? {
			return Ok(());
		}
		checked(ffi::update(
			&file,
			&identity.encode(),
			CF_UPDATE_FLAG_CLEAR_IN_SYNC,
		))?;
		Err(LocalError::Modified)
	}

	/// Through a handle for writing attributes, which a user's handle of any share mode admits
	/// (measured for `CfUpdatePlaceholder` against four kinds of holder).
	fn retag_unsynced(
		&self,
		path: &RootPath,
		expect: LocalFileId,
		identity: PlaceholderIdentity,
	) -> Result<(), LocalError> {
		let file = open_expected(&self.full(path), expect, FILE_WRITE_ATTRIBUTES)?;
		checked(ffi::update(
			&file,
			&identity.encode(),
			CF_UPDATE_FLAG_CLEAR_IN_SYNC,
		))
	}

	/// Through a handle for writing attributes, as the conversion was measured through.
	fn open_dir(&self, path: &RootPath, expect: LocalFileId) -> Result<WinDir, LocalError> {
		open_expected(&self.full(path), expect, FILE_WRITE_ATTRIBUTES).map(|file| WinDir { file })
	}

	/// Measured: a converted directory's ordinary children stay ordinary, and with on-demand
	/// population on, its next listing asks the provider for its children.
	fn convert_dir(
		&self,
		dir: &WinDir,
		identity: PlaceholderIdentity,
		populated: bool,
	) -> Result<(), LocalError> {
		let flags = if populated {
			CF_CONVERT_FLAG_MARK_IN_SYNC
		} else {
			CF_CONVERT_FLAG_MARK_IN_SYNC | CF_CONVERT_FLAG_ENABLE_ON_DEMAND_POPULATION
		};
		checked(ffi::convert(&dir.file, &identity.encode(), flags))
	}

	/// Through the handle the directory was converted through, which holds `FILE_WRITE_ATTRIBUTES`
	/// as the handle [`LocalFs::revert_to_plain`] reverts a directory through does.
	fn revert_dir(&self, dir: &WinDir) -> Result<(), LocalError> {
		checked(ffi::revert(&dir.file))
	}

	/// Not read through the handle here: the core walks the root instead.
	fn dir_path(&self, _dir: &WinDir) -> Result<Option<RootPath>, LocalError> {
		Err(LocalError::Os(OsError::Rejected { code: E_FAIL }))
	}

	/// The update [`LocalFs::update_dir`] makes, through the handle the directory was converted
	/// through.
	fn update_open_dir(
		&self,
		dir: &WinDir,
		identity: PlaceholderIdentity,
	) -> Result<(), LocalError> {
		checked(ffi::update(
			&dir.file,
			&identity.encode(),
			CF_UPDATE_FLAG_MARK_IN_SYNC,
		))
	}

	fn set_dir_in_sync(&self, path: &RootPath, expect: LocalFileId) -> Result<(), LocalError> {
		let file = open_expected(&self.full(path), expect, FILE_WRITE_ATTRIBUTES)?;
		checked(ffi::set_in_sync(&file))
	}

	/// Through a handle for reading attributes only, which refuses no other handle: measured
	/// (Windows 11, in the VM), `CfHydratePlaceholder` works through it and asks only for the
	/// ranges the file lacks, where a handle for reading and writing was refused while another
	/// handle refused writers.
	fn hydrate(&self, path: &RootPath, expect: LocalFileId) -> Result<(), LocalError> {
		let file = open_expected(&self.full(path), expect, FILE_READ_ATTRIBUTES)?;
		checked(ffi::hydrate(&file))
	}

	/// Through a handle for reading shared with every other: measured (Windows 11, in the VM),
	/// reading a full placeholder so raises no `FETCH_DATA` and changes neither its state, its USN
	/// nor its times, and a user may write, rename or delete it meanwhile (a change the snapshots
	/// after the hash then show: the in-sync state, the modified bytes or the times).
	fn hash_checked(
		&self,
		path: &RootPath,
		expect: LocalFileId,
		stop: &AtomicBool,
	) -> Result<CheckedHash, LocalError> {
		let mut file = open(&self.full(path), GENERIC_READ, SHARE_ALL, 0)?;
		let before = snapshot(&file)?;
		// Another file, or one dehydrated since the job looked: reading it would fetch it again.
		if before.id != expect || before.state.on_disk() < before.stamp.size {
			return Err(LocalError::NotFound);
		}
		let hash = hash_file(&mut file, Some(stop))?;
		let after = snapshot(&file)?;
		Ok(CheckedHash {
			before,
			hash,
			after,
		})
	}

	/// Checked and dehydrated (`CfDehydratePlaceholder`) through [`open_guarded`]'s handle, which
	/// no writer can hold at the same time and which reads no data: measured (Windows 11 ARM64, in
	/// the VM), an open for reading data fetches all of a placeholder's data the file lacks before
	/// it returns, so a file partly on disk would be downloaded whole only to be dehydrated, where the
	/// guard's open fetched nothing (remote renames and trashes of dehydrated files). The call
	/// refuses a file written since (`ERROR_CLOUD_FILE_NOT_IN_SYNC`), a pinned one
	/// (`ERROR_CLOUD_FILE_PINNED`) and one another process holds open for its data
	/// (`ERROR_CLOUD_FILE_IN_USE`), where `CfUpdatePlaceholder(DEHYDRATE)` dehydrates files held open
	/// for reading or mapped. A handle that refuses deleting, which the guard's open fails on as
	/// busy, is one such holder. The platform's refusal of a pinned file stands unless `if_pinned`
	/// overrides it.
	fn dehydrate_clean(
		&self,
		path: &RootPath,
		expect: PlaceholderIdentity,
		if_pinned: IfPinned,
	) -> Result<(), LocalError> {
		let file = open_guarded(&self.full(path))?;
		check_clean(&file, expect)?;
		match if_pinned {
			IfPinned::Refuse => checked(ffi::dehydrate(&file)),
			IfPinned::Override => {
				unpinned_for(&file, || checked(ffi::dehydrate(&file))).map(|((), _)| ())
			}
		}
	}

	/// Through a handle for reading attributes only, which refuses no other handle: its own pin
	/// read through it, then set when still `from`.
	fn swap_pin(
		&self,
		path: &RootPath,
		expect: LocalFileId,
		from: PinAttr,
		to: PinAttr,
	) -> Result<bool, LocalError> {
		let file = open_expected(&self.full(path), expect, FILE_READ_ATTRIBUTES)?;
		let info = ffi::item_info(&file).map_err(|error| io_error(&error))?;
		if pin_attr(info.attributes) != from {
			return Ok(false);
		}
		checked(ffi::set_pin_state(&file, pin_state(to))).map(|()| true)
	}

	fn create_placeholder(
		&self,
		parent: &RootPath,
		spec: &PlaceholderSpec,
	) -> Result<(), LocalError> {
		let base = self.wide(parent)?;
		let sent = ffi::create_placeholders(&base, slice::from_ref(spec));
		match entry_outcomes(&sent.results, sent.processed).first() {
			Some(EntryOutcome::Created) => Ok(()),
			Some(EntryOutcome::Exists) => Err(LocalError::Exists),
			Some(EntryOutcome::Rejected(OsError::Rejected { code })) => {
				Err(local_error(Hresult::new(*code)))
			}
			Some(EntryOutcome::Rejected(error)) => Err(LocalError::Os(*error)),
			None => checked(sent.hr).and(Err(LocalError::Os(OsError::Rejected { code: E_FAIL }))),
		}
	}

	fn rename_no_replace(&self, from: &RootPath, to: &RootPath) -> Result<(), LocalError> {
		ffi::move_no_replace(&self.wide(from)?, &self.wide(to)?).map_err(|error| io_error(&error))
	}

	/// Checked and updated through [`open_guarded`]'s handle, which refuses a writer holding the
	/// file and keeps any from opening it meanwhile (measured: a writer that opened the file before
	/// an update writes into the new version unseen), and which reads no data. Measured (Windows 11
	/// ARM64, in the VM): an open for reading data, as this once opened, raised a `FETCH_DATA` for
	/// all of a dehydrated file's old version from the provider's own process before it returned,
	/// which the dispatcher refused as stale, the cache's head being the new version already; a
	/// file partly on disk would have been fetched whole before its dehydration. The guard's open
	/// fetched nothing (remote renames and trashes of dehydrated files). `VERIFY_IN_SYNC` makes the
	/// call refuse a file written since it was marked in sync (measured
	/// `ERROR_CLOUD_FILE_NOT_IN_SYNC`); without it the update was measured to discard the edit.
	/// Refused as busy while another handle writes the file or refuses its delete.
	///
	/// A file with bytes on disk is first dehydrated through the same handle with
	/// `CfDehydratePlaceholder`, which refuses one another process holds open, readers and mapped
	/// views included (measured `ERROR_CLOUD_FILE_IN_USE`, [`LocalError::Busy`]), where
	/// `CfUpdatePlaceholder(DEHYDRATE)` alone was measured to dehydrate it under them, whose next
	/// reads then return the new version's bytes after the old one's. A file with no byte on disk
	/// goes straight to the update: no handle on it reads data, as every open for its data fetches
	/// it first. Once dehydrated, a reader opening the file before the update asks for the old
	/// version, which the cache no longer holds as the head, and is refused as stale with nothing
	/// transferred.
	fn update_from_remote(
		&self,
		path: &RootPath,
		expect: PlaceholderIdentity,
		to: &PlaceholderSpec,
	) -> Result<Updated, LocalError> {
		if !matches!(to.kind(), SpecKind::File { .. }) {
			return Err(LocalError::Os(OsError::Rejected { code: E_INVALIDARG }));
		}
		let file = open_guarded(&self.full(path))?;
		let on_disk = check_clean(&file, expect)?;
		let ((), was_pinned) = unpinned_for(&file, || {
			if on_disk > 0 {
				checked(ffi::dehydrate(&file))?;
			}
			checked(ffi::update_with_metadata(
				&file,
				&to.identity().encode(),
				&create_values(to),
				CF_UPDATE_FLAG_VERIFY_IN_SYNC
					| CF_UPDATE_FLAG_DEHYDRATE
					| CF_UPDATE_FLAG_MARK_IN_SYNC,
			))
		})?;
		Ok(Updated { was_pinned })
	}

	/// Checked and deleted through [`open_guarded`]'s handle; a directory with anything in it is
	/// refused (`ERROR_DIR_NOT_EMPTY`). Measured: the provider's own deletes raise no delete
	/// notification, and a delete through a `DELETE` handle with POSIX semantics drops the name
	/// once that handle closes, which it does before this returns.
	fn remove_clean(&self, path: &RootPath, expect: PlaceholderIdentity) -> Result<(), LocalError> {
		let guard = open_guarded(&self.full(path))?;
		check_clean(&guard, expect)?;
		ffi::delete_by_handle(&guard).map_err(|error| io_error(&error))
	}

	/// Checked, renamed and marked in sync again through [`open_guarded`]'s handle: measured, the
	/// provider's own rename leaves a placeholder not in sync, and the guard kept every writer out
	/// since the check. Measured: a directory with a child held open refuses the move with
	/// `ERROR_ACCESS_DENIED`, which here means busy.
	fn rename_clean(
		&self,
		from: &RootPath,
		to: &RootPath,
		expect: PlaceholderIdentity,
	) -> Result<(), LocalError> {
		let guard = open_guarded(&self.full(from))?;
		check_clean(&guard, expect)?;
		let is_dir = matches!(expect, PlaceholderIdentity::Dir { .. });
		ffi::rename_by_handle(&guard, &self.wide(to)?).map_err(|error| {
			match error.raw_os_error() {
				Some(code) if is_dir && u32::try_from(code).ok() == Some(ERROR_ACCESS_DENIED) => {
					LocalError::Busy
				}
				_ => io_error(&error),
			}
		})?;
		checked(ffi::update(
			&guard,
			&expect.encode(),
			CF_UPDATE_FLAG_MARK_IN_SYNC,
		))
	}

	fn update_dir(
		&self,
		path: &RootPath,
		expect: LocalFileId,
		identity: PlaceholderIdentity,
	) -> Result<(), LocalError> {
		let file = open_expected(&self.full(path), expect, FILE_WRITE_ATTRIBUTES)?;
		checked(ffi::update(
			&file,
			&identity.encode(),
			CF_UPDATE_FLAG_MARK_IN_SYNC,
		))
	}
}

//! What each cfapi callback does once `ffi` has copied it into owned values: builds its responder
//! first, so a request that goes no further is still answered, then hands the request to the
//! dispatcher the connection was made with. Runs on the OS's callback thread and returns at once:
//! it never waits, and takes no lock but the context slot's read lock and the dispatcher's short
//! ones.

use std::{path::PathBuf, process};

use tracing::{debug, trace, warn};

use super::{
	LOG_TARGET,
	ffi::Keys,
	responder::{WinData, WinDehydrate, WinDelete, WinPopulate, WinRename},
	sync_root::ContextSlot,
};
use crate::{
	failure::Failure,
	identity::{IdentityBlob, IdentityError},
	log_safe::path_tag,
	os::{
		CancelRequest, Completed, DataResponder, DehydrateRequest, DehydrateResponder,
		DeleteRequest, DeleteResponder, FetchDataRequest, LocalEvent, OptionalRange,
		PopulateRequest, PopulateResponder, RenameRequest, RenameResponder, RenameTarget,
		RequestedRange, TransferKey,
		cfapi::{
			RenameScope, callback_root_path, close_deleted, dehydrate_request, delete_flags,
			rename_is_directory, rename_scope,
		},
	},
};

/// What every callback carries, copied out of its `CF_CALLBACK_INFO`.
pub(super) struct CallbackInfo {
	/// The keys its answers are sent under.
	pub(super) keys: Keys,
	/// The placeholder's size, in bytes, as the OS knows it.
	pub(super) file_size: i64,
	/// The placeholder's identity, or why it has none this build reads.
	pub(super) identity: Result<IdentityBlob, IdentityError>,
	/// The process whose access raised the callback, when the OS names one.
	pub(super) pid: Option<u32>,
	/// That process's image's file name, never its path, when the OS names one.
	pub(super) image: Option<String>,
	/// The connection's context; `None` only if cfapi passed none.
	pub(super) slot: Option<&'static ContextSlot>,
}

/// A `CANCEL_FETCH_DATA` callback's parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Cancel {
	/// `CF_CALLBACK_CANCEL_FLAGS`: measured 1 for the 60 s timeout, 2 for an aborted read-ahead.
	pub(super) flags: i32,
	/// Where the cancelled range starts, in bytes.
	pub(super) offset: i64,
	/// How many bytes are cancelled.
	pub(super) length: i64,
}

/// A `FETCH_DATA` callback, its range `None` when its parameters were too short to hold one (the
/// request is then failed over the whole file), and `path` the file's full path, which the
/// responder holds open while it transfers.
pub(super) fn fetch_data(info: CallbackInfo, range: Option<RequestedRange>, path: Option<PathBuf>) {
	let transfer = info.keys.transfer();
	let optional = range.and_then(RequestedRange::optional);
	debug!(
		target: LOG_TARGET,
		callback = "fetch_data",
		transfer,
		file_size = info.file_size,
		required_offset = range.map(RequestedRange::required_offset),
		required_len = range.map(RequestedRange::required_len),
		optional_offset = optional.map(OptionalRange::offset),
		optional_len = optional.map(OptionalRange::length),
		pid = info.pid,
		reader = info.image.as_deref(),
		"cf callback"
	);
	let own = own_process(&info);
	let required = range.map(|range| (range.required_offset(), range.required_len()));
	let responder = WinData::new(info.keys, required, info.file_size, path);
	let Some(range) = range else {
		warn!(target: LOG_TARGET, transfer, "a FETCH_DATA callback carried no range");
		responder.fail(Failure::InvalidRequest);
		return;
	};
	let identity = match info.identity {
		Ok(identity) => identity,
		Err(error) => {
			warn!(target: LOG_TARGET, transfer, %error, "a FETCH_DATA callback carried no usable identity");
			responder.fail(Failure::InvalidRequest);
			return;
		}
	};
	// Disconnected, or no context: the responder fails the request as it drops.
	let Some(target) = info.slot.and_then(ContextSlot::target) else {
		return;
	};
	target.fetch_data(
		FetchDataRequest::new(identity, TransferKey::new(transfer), range, own),
		responder,
	);
}

/// A `CANCEL_FETCH_DATA` callback, `cancel` `None` when its parameters were too short to hold a
/// range: the dispatcher then gives up every read under its transfer key.
pub(super) fn cancel_fetch_data(info: CallbackInfo, cancel: Option<Cancel>) {
	let transfer = info.keys.transfer();
	debug!(
		target: LOG_TARGET,
		callback = "cancel_fetch_data",
		transfer,
		offset = cancel.map(|cancel| cancel.offset),
		length = cancel.map(|cancel| cancel.length),
		flags = cancel.map(|cancel| cancel.flags),
		pid = info.pid,
		"cf callback"
	);
	let Some(target) = info.slot.and_then(ContextSlot::target) else {
		return;
	};
	let key = TransferKey::new(transfer);
	target.cancel_fetch_data(match cancel {
		Some(cancel) => CancelRequest::new(key, cancel.offset, cancel.length),
		None => CancelRequest::unranged(key),
	});
}

/// A `FETCH_PLACEHOLDERS` callback.
pub(super) fn fetch_placeholders(info: CallbackInfo) {
	debug!(
		target: LOG_TARGET,
		callback = "fetch_placeholders",
		identity = ?info.identity,
		pid = info.pid,
		"cf callback"
	);
	let responder = WinPopulate::new(info.keys);
	let identity = match info.identity {
		Ok(identity) => identity,
		Err(error) => {
			warn!(target: LOG_TARGET, %error, "a FETCH_PLACEHOLDERS callback carried no usable identity");
			responder.fail(Failure::InvalidRequest);
			return;
		}
	};
	// Disconnected, or no context: the responder fails the request as it drops.
	let Some(target) = info.slot.and_then(ContextSlot::target) else {
		return;
	};
	target.fetch_placeholders(PopulateRequest::new(identity), responder);
}

/// Whether the callback was raised by this process: the provider's own renames and deletes (an
/// aside rename of a conflict's loser) are the core's doing, so they are allowed without asking
/// it, and its own closes are not looked at.
fn own_process(info: &CallbackInfo) -> bool {
	info.pid == Some(process::id())
}

/// The identity a delete or rename notification carries, or `None`, logged, when it carries none
/// this build reads: such a placeholder names no remote item, so its delete or rename touches
/// nothing remote and is allowed, as the core allows one whose identity does not decode.
fn notify_identity(info: CallbackInfo, callback: &'static str) -> Option<IdentityBlob> {
	match info.identity {
		Ok(identity) => Some(identity),
		Err(error) => {
			warn!(target: LOG_TARGET, callback, %error, "a notification carried no usable identity; allowed");
			None
		}
	}
}

/// A `NOTIFY_DELETE` callback, `flags` `None` when its parameters were too short to hold them and
/// `path` the placeholder's full path. Vetoed when the request cannot be read whole; the core is
/// never asked about a placeholder it cannot name.
pub(super) fn notify_delete(info: CallbackInfo, flags: Option<i32>, path: Option<Vec<u16>>) {
	debug!(
		target: LOG_TARGET,
		callback = "notify_delete",
		flags,
		pid = info.pid,
		path = path.as_deref().map(String::from_utf16_lossy),
		"cf callback"
	);
	let responder = WinDelete::new(info.keys);
	if own_process(&info) {
		responder.allow();
		return;
	}
	let Some(flags) = flags else {
		warn!(target: LOG_TARGET, "a NOTIFY_DELETE callback carried no flags");
		responder.veto(Failure::InvalidRequest);
		return;
	};
	// Disconnected, or no context: the responder vetoes the delete as it drops.
	let Some((slot, target)) = info.slot.and_then(|slot| Some((slot, slot.target()?))) else {
		return;
	};
	let Some(path) = path.and_then(|path| callback_root_path(slot.root(), &path)) else {
		warn!(target: LOG_TARGET, "a NOTIFY_DELETE callback named no path in the sync root");
		responder.veto(Failure::Internal);
		return;
	};
	let Some(identity) = notify_identity(info, "notify_delete") else {
		responder.allow();
		return;
	};
	let (is_directory, undelete) = delete_flags(flags);
	target.notify_delete(
		DeleteRequest::new(identity, path, is_directory, undelete),
		responder,
	);
}

/// A `NOTIFY_RENAME` callback, `rename` its flags and full target path (`None` when its parameters
/// were too short to hold the flags) and `source` the placeholder's full path. Vetoed when the
/// request cannot be read whole, or its target is in the root by its flags but not by its path:
/// a move out of the root trashes the remote item, so it is never assumed.
pub(super) fn notify_rename(
	info: CallbackInfo,
	rename: Option<(i32, Option<Vec<u16>>)>,
	source: Option<Vec<u16>>,
) {
	debug!(
		target: LOG_TARGET,
		callback = "notify_rename",
		flags = rename.as_ref().map(|(flags, _)| *flags),
		pid = info.pid,
		source = source.as_deref().map(String::from_utf16_lossy),
		target = rename
			.as_ref()
			.and_then(|(_, target)| target.as_deref())
			.map(String::from_utf16_lossy),
		"cf callback"
	);
	let responder = WinRename::new(info.keys);
	if own_process(&info) {
		responder.allow();
		return;
	}
	let Some((flags, target_path)) = rename else {
		warn!(target: LOG_TARGET, "a NOTIFY_RENAME callback carried no flags");
		responder.veto(Failure::InvalidRequest);
		return;
	};
	// Disconnected, or no context: the responder vetoes the rename as it drops.
	let Some((slot, requests)) = info.slot.and_then(|slot| Some((slot, slot.target()?))) else {
		return;
	};
	let Some(from) = source.and_then(|source| callback_root_path(slot.root(), &source)) else {
		warn!(target: LOG_TARGET, "a NOTIFY_RENAME callback named no source in the sync root");
		responder.veto(Failure::Internal);
		return;
	};
	let target = match rename_scope(flags, slot.root(), target_path.as_deref()) {
		RenameScope::InRoot(to) => RenameTarget::InRoot(to),
		RenameScope::OutOfRoot => RenameTarget::OutOfRoot,
		RenameScope::Refused(failure) => {
			warn!(target: LOG_TARGET, from = %path_tag(&from), flags, ?failure, "vetoed a rename to a target the core cannot name");
			responder.veto(failure);
			return;
		}
	};
	let Some(identity) = notify_identity(info, "notify_rename") else {
		responder.allow();
		return;
	};
	requests.notify_rename(
		RenameRequest::new(identity, from, target, rename_is_directory(flags)),
		responder,
	);
}

/// A `NOTIFY_DEHYDRATE` callback, `request` its flags and reason (`None` when its parameters were
/// too short to hold them) and `path` the placeholder's full path. Vetoed when the request cannot
/// be read whole or names no placeholder the core can name, an unreadable identity included,
/// acknowledged without one: when in doubt, the bytes stay, and every request is answered. The
/// provider's own dehydrations were measured to raise none; one arriving anyway is allowed, as the
/// provider dehydrates the file itself.
pub(super) fn notify_dehydrate(
	info: CallbackInfo,
	request: Option<(i32, i32)>,
	path: Option<Vec<u16>>,
) {
	debug!(
		target: LOG_TARGET,
		callback = "notify_dehydrate",
		flags = request.map(|(flags, _)| flags),
		reason = request.map(|(_, reason)| reason),
		pid = info.pid,
		path = path.as_deref().map(String::from_utf16_lossy),
		"cf callback"
	);
	let identity = match &info.identity {
		Ok(identity) => identity.clone(),
		Err(error) => {
			warn!(target: LOG_TARGET, %error, "a NOTIFY_DEHYDRATE callback carried no usable identity");
			WinDehydrate::new(info.keys, None).veto();
			return;
		}
	};
	let responder = WinDehydrate::new(info.keys, Some(identity.clone()));
	if own_process(&info) {
		responder.allow();
		return;
	}
	let Some((flags, reason)) = request else {
		warn!(target: LOG_TARGET, "a NOTIFY_DEHYDRATE callback carried no flags");
		responder.veto();
		return;
	};
	// Disconnected, or no context: the responder vetoes the dehydration as it drops.
	let Some((slot, target)) = info.slot.and_then(|slot| Some((slot, slot.target()?))) else {
		return;
	};
	let Some(path) = path.and_then(|path| callback_root_path(slot.root(), &path)) else {
		warn!(target: LOG_TARGET, "a NOTIFY_DEHYDRATE callback named no path in the sync root");
		responder.veto();
		return;
	};
	let (background, reason) = dehydrate_request(flags, reason);
	target.notify_dehydrate(
		DehydrateRequest::new(identity, path, background, reason),
		responder,
	);
}

/// A `NOTIFY_RENAME_COMPLETION` callback, `source` the full path the placeholder had and `path`
/// the one it has. Measured to come only for a target in the root.
pub(super) fn rename_completion(
	info: &CallbackInfo,
	source: Option<Vec<u16>>,
	path: Option<Vec<u16>>,
) {
	debug!(
		target: LOG_TARGET,
		callback = "rename_completion",
		pid = info.pid,
		source = source.as_deref().map(String::from_utf16_lossy),
		path = path.as_deref().map(String::from_utf16_lossy),
		"cf callback"
	);
	if own_process(info) {
		return;
	}
	let Some((slot, target)) = info.slot.and_then(|slot| Some((slot, slot.target()?))) else {
		return;
	};
	let from = source.and_then(|source| callback_root_path(slot.root(), &source));
	let to = path.and_then(|path| callback_root_path(slot.root(), &path));
	if let (Some(from), Some(to)) = (from, to) {
		target.completed(Completed::Renamed { from, to });
	}
}

/// A `NOTIFY_FILE_CLOSE_COMPLETION` callback, `path` the placeholder's full path: a delete when
/// its flags say the closed handle deleted it, which nothing else proves, and otherwise a writer
/// done with the file, looked at shortly. A path outside the root (measured on ARM64 after a
/// delete: `\$Extend\$Deleted\...`) is dropped; the watcher reports what it names.
pub(super) fn close_completion(info: &CallbackInfo, flags: Option<i32>, path: Option<Vec<u16>>) {
	trace!(
		target: LOG_TARGET,
		callback = "close_completion",
		flags,
		pid = info.pid,
		path = path.as_deref().map(String::from_utf16_lossy),
		"cf callback"
	);
	if own_process(info) {
		return;
	}
	let Some((slot, target)) = info.slot.and_then(|slot| Some((slot, slot.target()?))) else {
		return;
	};
	let Some(path) = path.and_then(|path| callback_root_path(slot.root(), &path)) else {
		return;
	};
	if close_deleted(flags) {
		target.completed(Completed::Deleted(path));
	} else {
		target.local_event(LocalEvent::Closed(path));
	}
}

//! Dehydrations another process asks for (`NOTIFY_DEHYDRATE`): Explorer's "Free up space", or an
//! application calling `CfDehydratePlaceholder`. The platform refuses by itself a file written since
//! it was marked in sync, a pinned one and one held open; what reaches the core is allowed only for
//! a placeholder that holds nothing the drive lacks and that nothing in the core is working on.
//!
//! Measured (Windows 11, in the VM): an allowed request waits until the provider
//! dehydrates the file itself (a provider that never does fails the caller after about 121 s), so
//! the core dehydrates it right after allowing it, through its checked handle. A file appended to
//! through an append-only handle stays marked in sync with modified bytes, which the platform does
//! not refuse; the core vetoes it.

use std::{sync::Arc, time::Duration};

use filen_mobile_native_cache::ffi::FfiObject;
use tokio::time::sleep;
use tracing::{debug, warn};

use super::{
	FileVersion, Inner, hydration, lock,
	remote::{ReadInFlight, upload_pending},
};
use crate::{
	drive::Drive,
	identity::{PlaceholderIdentity, stable_id},
	log_safe::{self, path_tag},
	os::{
		DehydrateRequest, DehydrateResponder, IfPinned, LocalEntry, LocalError, LocalFs, LocalKind,
		PinAttr,
	},
	root_path::RootPath,
};

/// How many times an allowed dehydration refused as busy is tried in all, and how long apart: a
/// retry of its own rather than the queue's, as the platform's caller waits for it (and fails
/// after about 121 s), where the queue's backoff runs to minutes.
const ATTEMPTS: u32 = 5;
const RETRY: Duration = Duration::from_millis(200);

/// Why a dehydration request is vetoed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Veto {
	/// No file placeholder of the identity asked about is at the path.
	NotThePlaceholder,
	/// It holds bytes written since it was marked in sync, or is not marked in sync.
	Modified,
	/// It is pinned to this device.
	Pinned,
	/// A read of it is in flight.
	Reading,
	/// A job of the core's runs or waits on it.
	Queued,
	/// It is being uploaded, or the cache holds an edit of it not yet on the server.
	Uploading,
	/// Its state or its cache row could not be read: when in doubt, the bytes stay.
	Unknown,
}

/// Answers `request` through `responder`, and dehydrates the file once allowed. Allowed, the
/// platform waits for the dehydration; a stop between the two leaves the file's bytes, and the
/// caller fails after the platform's time limit.
pub(super) async fn answer<D: Drive, L: LocalFs, R: DehydrateResponder>(
	inner: &Arc<Inner<D, L>>,
	request: DehydrateRequest,
	responder: R,
) {
	let path = request.path().clone();
	let (identity, held) = match check(inner, &request).await {
		Ok(allowed) => allowed,
		Err(veto) => {
			debug!(%path, ?veto, reason = ?request.reason(), "vetoed a dehydration");
			responder.veto();
			return;
		}
	};
	responder.allow();
	dehydrate(inner, &path, identity).await;
	drop(held);
}

/// The placeholder the request names, when it may be dehydrated, with a read in flight of its own
/// held from before the queue was checked until the dehydration ends: a hydration job that starts
/// meanwhile waits for it, rather than hash a file being dehydrated, which would fetch it again.
async fn check<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	request: &DehydrateRequest,
) -> Result<(PlaceholderIdentity, ReadInFlight), Veto> {
	let identity =
		PlaceholderIdentity::decode(request.identity()).map_err(|_| Veto::NotThePlaceholder)?;
	let Some(version) = FileVersion::of(identity) else {
		return Err(Veto::NotThePlaceholder);
	};
	let path = request.path();
	let state = match inner.inspect(path).await {
		Ok(LocalEntry::Placeholder {
			kind: LocalKind::File(_),
			identity: Ok(held),
			state,
			..
		}) if held == identity => state,
		Ok(_) => return Err(Veto::NotThePlaceholder),
		Err(error) => {
			warn!(item = %path_tag(path), %error, "reading a placeholder's state for a dehydration failed");
			return Err(Veto::Unknown);
		}
	};
	if !state.in_sync() || state.modified_bytes() > 0 {
		return Err(Veto::Modified);
	}
	let inherited = inner.inherited_pin(path).await.map_err(|error| {
		warn!(item = %path_tag(path), %error, "reading the pin above a placeholder for a dehydration failed");
		Veto::Unknown
	})?;
	if hydration::effective(state.pin(), inherited) == PinAttr::Pinned {
		return Err(Veto::Pinned);
	}
	if inner.hints.reads_in_flight(version.stable) > 0 {
		return Err(Veto::Reading);
	}
	let held = inner.hints.reading(version, true);
	{
		let state = lock(&inner.state);
		if state.queue.holds(path) {
			return Err(Veto::Queued);
		}
		if state
			.uploading
			.values()
			.any(|at| at.case_key() == path.case_key())
		{
			return Err(Veto::Uploading);
		}
	}
	match inner.drive.query_item(&stable_id(version.stable)).await {
		Ok(Some(head @ FfiObject::File(_))) if upload_pending(&head) => Err(Veto::Uploading),
		Ok(_) => Ok((identity, held)),
		Err(error) => {
			warn!(item = %path_tag(path), error = %log_safe::error(&error), "reading a file's cache row for a dehydration failed");
			Err(Veto::Unknown)
		}
	}
}

/// Dehydrates the clean placeholder `identity` at `path`, which the platform's caller waits for,
/// trying again shortly while it is refused as busy. A file the user pinned since the check stays
/// pinned: the platform refuses it, and the caller fails.
async fn dehydrate<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	identity: PlaceholderIdentity,
) {
	for attempt in 1..=ATTEMPTS {
		match inner
			.dehydrate_clean(path, identity, IfPinned::Refuse)
			.await
		{
			Ok(()) => {
				debug!(%path, "dehydrated a file another process asked to");
				return;
			}
			Err(LocalError::Busy) if attempt < ATTEMPTS => sleep(RETRY).await,
			Err(error) => {
				warn!(item = %path_tag(path), %error, "dehydrating an allowed file failed; its caller times out");
				return;
			}
		}
	}
}

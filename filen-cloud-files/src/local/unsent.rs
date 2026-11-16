//! The edits a previous run left marked in the cache: each upload of an edit goes through the
//! cache's copy of the file and marks it while it runs, and a run that stopped mid-upload leaves
//! the mark. The cache's own drain would upload that copy as the file's new version with no check of
//! the version the edit was made from, over whatever another device uploaded since; on Windows the
//! local file is the edit's real copy, so the engine never runs the drain and takes each mark over
//! once its first walk has seen the sync root:
//!
//! | The file the mark is on | What happens |
//! |---|---|
//! | its placeholder holds changes | the mark goes: the placeholder's own job uploads them, checking the version they were made from |
//! | its edit is the drive's content, or the copy is gone | the mark goes, nothing is written |
//! | anything else (deleted or put back while the provider was stopped) | the copy is written into the sync root beside the file under a name the engine leaves alone, then renamed to a free `.old` name, never over a file, and uploaded from there as a new file; then the mark goes |
//!
//! Everything here is the cache's own, with no server round trip, so it works offline. A copy that
//! fails keeps the mark and is tried again, backing off, for as long as the engine runs, and the
//! file's edits are not uploaded until it is kept: their copy would go through the same slot.
//! Nothing here deletes an edit's only copy.

use std::{collections::HashSet, sync::Arc, time::Duration};

use filen_mobile_native_cache::{
	ffi::{FfiId, FfiObject},
	remote::PendingCopy,
};
use filen_sdk_rs::sync_engine::watch_backoff;
use filen_types::fs::Uuid;
use tokio::{sync::watch, time::sleep};
use tracing::{debug, warn};

use super::{
	Inner, jobs, lock,
	queue::Delay,
	report::{KeptReason, LocalReport, RemoteApplied},
};
use crate::{
	drive::Drive,
	identity::{stable_id, uuid_of_stable_id},
	log_safe::{self},
	names::{PlaceholderName, copy_name},
	os::LocalFs,
	root_path::RootPath,
};

/// The longest the take-over waits between tries to list the marked edits, or to keep an edit it
/// could not.
const KEEP_AGAIN_MAX: Duration = Duration::from_secs(5 * 60);

/// Says the take-over is done when dropped, however it ended: an edit's upload waits for it.
struct Done<'a>(&'a watch::Sender<bool>);

impl Drop for Done<'_> {
	fn drop(&mut self) {
		self.0.send_replace(true);
	}
}

/// Takes over every edit the cache holds marked, once a walk of the sync root has listed the file
/// placeholders holding changes; edits' uploads wait until every mark was looked at once, and an
/// edit that could not be kept is tried again until it is.
///
/// Edits wait for as long as the cache cannot list its marks: one not listed could be overwritten.
pub(super) async fn take_over<D: Drive, L: LocalFs>(inner: Arc<Inner<D, L>>) {
	let done = Done(&inner.taken_over);
	let mut walks = inner.walks_ended.subscribe();
	let dirty = loop {
		if let Some(dirty) = lock(&inner.state).walked_dirty.take() {
			break dirty;
		}
		if walks.changed().await.is_err() {
			return;
		}
	};
	let mut attempts = 0_u32;
	let marked = loop {
		match inner.drive.pending_uploads().await {
			Ok(marked) => break marked,
			Err(error) => {
				attempts = attempts.saturating_add(1);
				warn!(error = %log_safe::error(&error), "listing the edits the cache holds failed; asking again");
				sleep(watch_backoff(attempts).unwrap_or(KEEP_AGAIN_MAX)).await;
			}
		}
	};
	for id in marked {
		let Some(stable) = uuid_of_stable_id(&id.0) else {
			// Left marked: it may be an edit's only copy, and no file of the sync root claims it.
			// The id may be of a path's form, so it shows at debug alone.
			debug!(id = %id.0, "an edit marked under an id that is not a file's");
			warn!("the cache holds an edit marked under an id that is not a file's");
			continue;
		};
		if dirty.contains(&stable) {
			// The placeholder holds the edit, or a later one, and uploads it itself.
			if let Err(error) = inner.drive.drop_pending_upload(id).await {
				warn!(%stable, error = %log_safe::error(&error), "giving up the cache's copy of an edit failed");
			}
			continue;
		}
		if !keep_copy(&inner, stable, id).await {
			lock(&inner.state).unkept.insert(stable);
		}
	}
	drop(done);
	keep_the_rest(&inner).await;
}

/// Tries again, backing off, to keep each edit the take-over could not; once one is kept, a walk
/// looks at its file again, whose upload waited for it.
async fn keep_the_rest<D: Drive, L: LocalFs>(inner: &Arc<Inner<D, L>>) {
	let mut attempts = 0_u32;
	loop {
		let unkept: Vec<Uuid> = lock(&inner.state).unkept.iter().copied().collect();
		if unkept.is_empty() {
			return;
		}
		attempts = attempts.saturating_add(1);
		sleep(watch_backoff(attempts).unwrap_or(KEEP_AGAIN_MAX)).await;
		let mut kept_any = false;
		for stable in unkept {
			if keep_copy(inner, stable, stable_id(stable)).await {
				lock(&inner.state).unkept.remove(&stable);
				kept_any = true;
			}
		}
		if kept_any {
			Inner::request_walk(inner);
		}
	}
}

/// Keeps the cache's copy of an edit of the file `stable`, which no placeholder holds: written into
/// the sync root beside the file under a free `.old` name, where it is taken in as a new file.
/// Whether the cache holds no edit of it any more; `false` leaves the mark, to be tried again.
async fn keep_copy<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	stable: Uuid,
	id: FfiId,
) -> bool {
	let Some((dir, remote_dir)) = home_of(inner, stable).await else {
		return false;
	};
	let name = match inner.drive.query_item(&id).await {
		Ok(Some(FfiObject::File(file))) => file.meta.map(|meta| meta.name),
		Ok(_) => None,
		Err(error) => {
			warn!(%stable, error = %log_safe::error(&error), "reading a marked file from the cache failed");
			return false;
		}
	};
	let Some(target) = name
		.as_deref()
		.and_then(|name| PlaceholderName::parse(name).ok())
		.or_else(|| PlaceholderName::parse(&stable.to_string()).ok())
		.map(|name| dir.join(name))
	else {
		warn!(%stable, "no name to keep an unsent edit under; trying again");
		return false;
	};
	let Ok(copy) = PlaceholderName::parse(&copy_name()).map(|name| dir.join(name)) else {
		warn!(%stable, "no name to copy an unsent edit under; trying again");
		return false;
	};
	// The cache's listing, with no server round trip: a name taken on the server since keeps both
	// when the copy is uploaded.
	let listed = match inner.drive.cached_children(&remote_dir).await {
		Ok(Some(listing)) => listing.objects,
		Ok(None) => Vec::new(),
		Err(error) => {
			warn!(%stable, error = %log_safe::error(&error), "listing where to keep an unsent edit failed; trying again");
			return false;
		}
	};
	let Some(aside) = jobs::choose_aside(inner, &target, &listed).await else {
		warn!(%stable, "no free name to keep an unsent edit; trying again");
		return false;
	};
	let dest = inner.local.upload_path(&copy);
	match inner.drive.copy_pending_upload(id.clone(), dest).await {
		Ok(PendingCopy::Copied) => {
			// Whole on disk under a name of its own, it takes its name, never over a file put there
			// since; one that does not stays where it is, never uploaded, and the edit is copied
			// again.
			let (from, to) = (copy.clone(), aside.clone());
			if let Err(error) = inner
				.fs(move |local| local.rename_no_replace(&from, &to))
				.await
			{
				warn!(%stable, %error, "naming the copy of an unsent edit failed; trying again");
				return false;
			}
			// The sync root holds the edit now.
			if let Err(error) = inner.drive.drop_pending_upload(id).await {
				warn!(%stable, error = %log_safe::error(&error), "giving up the cache's copy of a kept edit failed");
			}
			inner.report(LocalReport::Remote(RemoteApplied::KeptLocal {
				path: aside.clone(),
				reason: KeptReason::UnsentEdit,
			}));
			inner.push(&aside, Delay::Now);
			true
		}
		Ok(PendingCopy::Nothing) => {
			debug!(%stable, "a marked edit had nothing left to keep");
			true
		}
		Err(error) => {
			warn!(%stable, error = %log_safe::error(&error), "copying an unsent edit out of the cache failed; trying again");
			false
		}
	}
}

/// Where a kept copy of the file `stable` goes: the placeholder directory of the remote directory
/// it is in, with that directory's cache id, or the sync root when that directory has none.
async fn home_of<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	stable: Uuid,
) -> Option<(RootPath, FfiId)> {
	let root = match inner.drive.root_uuid().await {
		Ok(root) => root,
		Err(error) => {
			warn!(%stable, error = %log_safe::error(&error), "reading the drive's root failed; keeping an unsent edit is tried again");
			return None;
		}
	};
	let at_root = (RootPath::root(), FfiId(root));
	let parent = match inner.drive.query_item(&stable_id(stable)).await {
		Ok(Some(object)) => {
			jobs::parent_of(&object).and_then(|parent| Uuid::parse_str(parent).ok())
		}
		Ok(None) => None,
		Err(error) => {
			warn!(%stable, error = %log_safe::error(&error), "reading a marked file from the cache failed");
			return None;
		}
	};
	let placed = parent.and_then(|parent| {
		let state = lock(&inner.state);
		state
			.index
			.path_of(parent)
			.filter(|dir| !dir.is_root())
			.map(|dir| (dir.clone(), stable_id(parent)))
	});
	Some(placed.unwrap_or(at_root))
}

/// The stables of the file placeholders a walk found holding changes.
pub(super) type Dirty = HashSet<Uuid>;

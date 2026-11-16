//! The feed: reads the cache's changes after an anchor held in memory, and turns each into hints.
//!
//! The cache signals the live updates it applied; the provider's own listings and writes raise no
//! signal, so the feed also reads on a tick. Reads are coalesced on the leading edge: a signal reads
//! at once, and the signals during the next [`COALESCE`] cause exactly one more read after it. The
//! anchor is read before the first walk, so a change between the two is read twice at worst, never
//! lost; it is not persisted, as every start reconciles every populated directory from the cache.

use std::{sync::Arc, time::Duration};

use filen_mobile_native_cache::{
	CacheError, ffi::FfiChanges, ffi::FfiObject, traits::WorkingSetUpdateListener,
};
use filen_sdk_rs::sync_engine::watch_backoff;
use filen_types::fs::Uuid;
use tokio::{select, sync::Notify, time::sleep};
use tracing::{debug, warn};

use crate::{
	drive::Drive,
	identity::uuid_of_stable_id,
	local::{
		Inner, lock,
		queue::{Delay, Kind},
	},
	log_safe::{self},
	os::LocalFs,
	root_path::RootPath,
};

/// How many changes a page holds.
const PAGE: u32 = 500;
/// How often the feed reads with no signal, to see the provider's own changes.
pub(in crate::local) const TICK: Duration = Duration::from_secs(30);
/// How long after a read further signals wait for one more read.
pub(in crate::local) const COALESCE: Duration = Duration::from_millis(500);

/// The cache's listener: wakes the feed and never blocks.
struct Signal(Arc<Notify>);

impl WorkingSetUpdateListener for Signal {
	fn working_set_changed(&self) {
		self.0.notify_one();
	}
}

/// Registers the feed's listener and reads the anchor it starts from; `None` when the cache could
/// not give one, which the feed retries.
pub(in crate::local) async fn start_anchor<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
) -> Option<Vec<u8>> {
	inner
		.drive
		.watch_changes(Some(Arc::new(Signal(Arc::clone(&inner.feed_wake)))))
		.await;
	match inner.drive.change_anchor().await {
		Ok(anchor) => Some(anchor),
		Err(error) => {
			warn!(error = %log_safe::error(&error), "reading the change feed's anchor failed");
			None
		}
	}
}

/// Reads the feed from `anchor` for as long as the engine runs; a failed read is tried again after
/// the sync engine's backoff, not on the next signal or tick.
pub(in crate::local) async fn run_feed<D: Drive, L: LocalFs>(
	inner: Arc<Inner<D, L>>,
	mut anchor: Option<Vec<u8>>,
) {
	let mut failures = 0u32;
	loop {
		if failures == 0 {
			select! {
				() = inner.feed_wake.notified() => {}
				() = sleep(TICK) => {}
			}
		}
		let read = match anchor.take() {
			Some(from) => read_pages(&inner, from).await,
			None => restart(&inner).await,
		};
		match read {
			Ok(reached) => {
				anchor = Some(reached);
				failures = 0;
			}
			Err((kept, error)) => {
				anchor = kept;
				failures = failures.saturating_add(1);
				warn!(error = %log_safe::error(&error), failures, "reading the cache's changes failed");
				sleep(watch_backoff(failures).unwrap_or(TICK)).await;
				continue;
			}
		}
		sleep(COALESCE).await;
	}
}

/// A new anchor, read when the old one is lost, then every populated directory reconciled: what
/// changed meanwhile is not in the feed.
async fn restart<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
) -> Result<Vec<u8>, (Option<Vec<u8>>, CacheError)> {
	let anchor = inner
		.drive
		.change_anchor()
		.await
		.map_err(|error| (None, error))?;
	Inner::request_walk(inner);
	Ok(anchor)
}

/// Reads every page after `from`, hinting each, and returns the anchor reached. An expired anchor
/// restarts the feed; another failure keeps the anchor of the last page read.
async fn read_pages<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	mut from: Vec<u8>,
) -> Result<Vec<u8>, (Option<Vec<u8>>, CacheError)> {
	loop {
		let page = match inner.drive.changes(Some(from.clone()), PAGE).await {
			Ok(page) => page,
			Err(CacheError::SyncAnchorExpired(_)) => {
				debug!("the change feed's anchor expired; reconciling every populated directory");
				return restart(inner).await;
			}
			Err(error) => return Err((Some(from), error)),
		};
		inner.hint_changes(&page);
		if !page.more {
			return Ok(page.anchor);
		}
		from = page.anchor;
	}
}

/// The uuid of a changed item and of the directory it is in: its original parent when trashed.
fn item_and_parent(item: &FfiObject) -> Option<(Uuid, Option<Uuid>)> {
	let (uuid, parent, original) = match item {
		FfiObject::File(file) => (&file.stable_uuid, &file.parent, &file.original_parent),
		FfiObject::Dir(dir) => (&dir.uuid, &dir.parent, &dir.original_parent),
		FfiObject::Root(_) => return None,
	};
	let parent = original.as_deref().unwrap_or(parent);
	Some((Uuid::parse_str(uuid).ok()?, Uuid::parse_str(parent).ok()))
}

impl<D: Drive, L: LocalFs> Inner<D, L> {
	/// Queues the jobs a page of changes hints at: a remote job on each changed item's placeholder,
	/// a reconcile of each populated directory a changed item is in, and a remote job on each
	/// removed item's placeholder. An item with no placeholder and no populated directory is none
	/// of the provider's business.
	fn hint_changes(self: &Arc<Self>, page: &FfiChanges) {
		let mut pushes: Vec<(RootPath, Kind)> = Vec::new();
		{
			let state = lock(&self.state);
			let index = &state.index;
			for item in &page.updated {
				let Some((uuid, parent)) = item_and_parent(item) else {
					debug!("a changed item's id is not a uuid");
					continue;
				};
				if let Some(path) = index.path_of(uuid) {
					pushes.push((path.clone(), Kind::Remote));
				}
				if let Some(parent) = parent
					&& index.is_populated(parent)
					&& let Some(path) = index.path_of(parent)
				{
					pushes.push((path.clone(), Kind::Children));
				}
			}
			for id in &page.deleted_ids {
				let Some(uuid) = uuid_of_stable_id(id) else {
					debug!(%id, "a removed item's id is not a stable id");
					continue;
				};
				if let Some(path) = index.path_of(uuid) {
					pushes.push((path.clone(), Kind::Remote));
				}
			}
		}
		for (path, kind) in pushes {
			self.push_kind(&path, Delay::Now, kind);
		}
	}
}

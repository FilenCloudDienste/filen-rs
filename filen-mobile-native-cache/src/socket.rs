//! Dedicated File Provider socket listener (replicated-extension Phase 5 liveness).
//!
//! The iOS File Provider extension owns its OWN filen-sdk-rs socket connection — a deliberate
//! choice: a second socket alongside the main app's is a minor cost that keeps the dependency
//! tree simple (the extension never has to coordinate a shared connection with the RN app). On
//! each remote drive event we poke the system via [`SocketNotificationCallback`] →
//! `signalEnumerator`, so changes made on other devices surface without the user having to back
//! out of and reopen a folder.
//!
//! The socket is a pure *trigger*: it never carries the change into the cache. The receiver's
//! `enumerateChanges(refresh: true)` re-lists the affected container against the backend, so the
//! server remains the source of truth and we don't have to trust socket event ordering or
//! completeness.

use std::sync::Arc;

use filen_sdk_rs::fs::HasParent;
use filen_sdk_rs::socket::{DecryptedDriveEvent, DecryptedSocketEvent};
use filen_types::fs::ParentUuid;

use crate::{auth::FilenMobileCacheState, error::CacheError, traits::SocketNotificationCallback};

/// Map a container's `ParentUuid` to a change hint: a concrete parent uuid to re-enumerate, or a
/// trash marker, or neither (working-set refresh only).
fn hint_from_parent(parent: &ParentUuid) -> (Option<String>, bool) {
	match parent {
		ParentUuid::Uuid(u) => (Some(u.to_string()), false),
		ParentUuid::Trash(_) => (None, true),
		_ => (None, false),
	}
}

/// Extract the affected-container hint from a decrypted drive event: the parent container uuid
/// whose child listing may have changed (when the event cheaply carries it) plus whether trash is
/// affected. Events that don't name a concrete parent return `(None, ...)` — the receiver still
/// refreshes the working set, which covers materialized / favorited / recent items.
//
// ponytail: precise for the common "item appeared/moved in folder X" events (the ones where a live
// refresh matters most); metadata/rename/favorite events fall back to the working-set refresh
// rather than doing a uuid->parent DB lookup off the websocket thread. Upgrade to a lookup only if
// stale rename/favorite listings become a real problem.
fn drive_event_hint(ev: &DecryptedDriveEvent<'_>) -> (Option<String>, bool) {
	use DecryptedDriveEvent as D;
	match ev {
		// File/folder create/restore/move all name the containing folder via HasParent::parent().
		// (Distinct newtypes, so they can't share one or-pattern binding.)
		D::FileNew(e) => hint_from_parent(e.0.parent()),
		D::FileRestore(e) => hint_from_parent(e.0.parent()),
		D::FileMove(e) => hint_from_parent(e.0.parent()),
		D::FolderMove(e) => hint_from_parent(e.0.parent()),
		D::FolderSubCreated(e) => hint_from_parent(e.0.parent()),
		D::FolderRestore(e) => hint_from_parent(e.0.parent()),
		// Item moved to / emptied from / permanently removed from trash: refresh trash (+ working set).
		D::FileTrash(_)
		| D::FolderTrash(_)
		| D::FileDeletedPermanent(_)
		| D::FolderDeletedPermanent(_)
		| D::TrashEmpty
		| D::DeleteAll
		| D::DeleteVersioned => (None, true),
		// Metadata / favorite / color / archive changes: no container-membership change; the
		// unconditional working-set refresh surfaces them where materialized.
		D::FileArchiveRestored(_)
		| D::FileArchived(_)
		| D::ItemFavorite(_)
		| D::FolderColorChanged(_)
		| D::FolderMetadataChanged(_)
		| D::FileMetadataChanged(_) => (None, false),
	}
}

#[uniffi::export]
impl FilenMobileCacheState {
	/// Start (or replace) the File Provider's dedicated socket listener. On each remote drive event
	/// it invokes `callback.on_drive_change(...)`. The listener handle is stored on the authed state
	/// and is dropped — unsubscribing and, once it's the last handle, tearing down the socket
	/// thread — on re-auth / logout or via [`Self::stop_socket_notifications`]. Calling this again
	/// replaces the previous listener.
	pub async fn start_socket_notifications(
		&self,
		callback: Arc<dyn SocketNotificationCallback>,
	) -> Result<(), CacheError> {
		self.async_execute_authed_owned(async move |auth_state| {
			let cb = callback;
			let handle = auth_state
				.client
				.add_event_listener(
					Box::new(move |event: &DecryptedSocketEvent<'_>| match event {
						DecryptedSocketEvent::Drive { inner, .. } => {
							let (parent, affects_trash) = drive_event_hint(inner);
							cb.on_drive_change(parent.into_iter().collect(), affects_trash);
						}
						// Couldn't decrypt the event payload — refresh broadly rather than miss a change.
						DecryptedSocketEvent::DriveMalformed { .. } => {
							cb.on_drive_change(Vec::new(), true);
						}
						// Every (re)connect: catch up on anything that changed while disconnected. The
						// extension is short-lived, so a remote change during a socket-down window is
						// otherwise lost with no recovery (reopen won't re-list — the anchor is local).
						DecryptedSocketEvent::AuthSuccess => cb.on_reconnect(),
						_ => {}
					}),
					None,
				)
				.await?;
			*auth_state.socket_listener.lock().await = Some(handle);
			Ok(())
		})
		.await
	}

	/// Stop the File Provider socket listener (drops the handle → unsubscribes).
	pub async fn stop_socket_notifications(&self) -> Result<(), CacheError> {
		self.async_execute_authed_owned(async move |auth_state| {
			*auth_state.socket_listener.lock().await = None;
			Ok(())
		})
		.await
	}
}

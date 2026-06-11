//! UniFFI exposure of the cache's configuration surface. UniFFI-ONLY for now: the cache cannot
//! compile to wasm at all yet (a deliberate compile error — see the `cache` module note in
//! `lib.rs`), so the wasm twin of this module arrives with the wasm port rather than as
//! unbuildable dead code today.
//!
//! Follows the house FFI shape: owned MIRROR types converted from the core enums (the core
//! [`CacheMessage`] carries payloads that cannot cross the boundary — raw `rusqlite::Error`s and
//! full `RemoteFile`/`RemoteDirectory` conversion-failure records — so errors are flattened to
//! display strings), a `with_foreign` listener trait whose every invocation is `spawn_blocking`'d
//! off the runtime, and a [`JsClient`] method whose body ships to the commander thread via
//! `do_on_commander`.

use std::{path::PathBuf, sync::Arc};

use filen_types::fs::UuidStr;

use crate::{
	Error,
	auth::JsClient,
	cache::{CacheMessage, ResyncProgress},
	runtime::do_on_commander,
};

/// Receives every status-message batch the cache worker emits (see [`CacheStatusMessage`]).
/// Registered ONCE via [`JsClient::configure_cache`] and reused across worker restarts.
/// Invocations are dispatched with `spawn_blocking`, so a slow foreign implementation cannot
/// stall the cache — but delivery is BEST-EFFORT (messages can drop under load), so treat each
/// message as a fresh snapshot, never as a delta to accumulate.
#[uniffi::export(with_foreign)]
pub trait CacheStatusListener: Send + Sync {
	fn on_messages(&self, messages: Vec<CacheStatusMessage>);
}

/// FFI mirror of [`CacheMessage`] (see its docs for the full semantics of each variant).
#[derive(Debug, Clone, uniffi::Enum)]
pub enum CacheStatusMessage {
	/// Mirror of [`CacheMessage::Error`]: non-fatal worker errors, flattened to display strings
	/// (the structured payloads cannot cross the FFI boundary; definitive failures still arrive
	/// structurally as `Result`s on the calls that caused them).
	Errors { errors: Vec<String> },
	/// Mirror of [`CacheMessage::SyncRootsDeleted`]: these roots were deleted server-side and
	/// dropped from the active set — re-add them to resume syncing.
	SyncRootsDeleted { roots: Vec<UuidStr> },
	/// Mirror of [`CacheMessage::ResyncProgress`].
	ResyncProgress { progress: ResyncProgressMessage },
}

/// FFI mirror of [`ResyncProgress`] (see its docs for the resync lifecycle and the
/// worker-global attribution caveats). `root_index`/`root_count` are widened from `usize`.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum ResyncProgressMessage {
	Started {
		roots: Vec<UuidStr>,
	},
	Listing {
		root: UuidStr,
		root_index: u64,
		root_count: u64,
		bytes_downloaded: u64,
		total_bytes: Option<u64>,
	},
	Applying,
	Finished {
		converged: bool,
	},
}

impl From<CacheMessage> for CacheStatusMessage {
	fn from(message: CacheMessage) -> Self {
		match message {
			CacheMessage::Error(errors) => Self::Errors {
				errors: errors.iter().map(ToString::to_string).collect(),
			},
			CacheMessage::SyncRootsDeleted(roots) => Self::SyncRootsDeleted {
				roots: roots.iter().map(UuidStr::from).collect(),
			},
			CacheMessage::ResyncProgress(progress) => Self::ResyncProgress {
				progress: progress.into(),
			},
		}
	}
}

impl From<ResyncProgress> for ResyncProgressMessage {
	fn from(progress: ResyncProgress) -> Self {
		match progress {
			ResyncProgress::Started { roots } => Self::Started {
				roots: roots.iter().map(UuidStr::from).collect(),
			},
			ResyncProgress::Listing {
				root,
				root_index,
				root_count,
				bytes_downloaded,
				total_bytes,
			} => Self::Listing {
				root: UuidStr::from(&root),
				root_index: root_index as u64,
				root_count: root_count as u64,
				bytes_downloaded,
				total_bytes,
			},
			ResyncProgress::Applying => Self::Applying,
			ResyncProgress::Finished { converged } => Self::Finished { converged },
		}
	}
}

#[uniffi::export]
impl JsClient {
	/// Pre-warm the cache: store the SQLite DB path and the global status listener on the
	/// client. PURE STORAGE — no worker spawns, no file is opened and no network I/O happens
	/// until the first sync root is added — and the configuration survives worker restarts, so
	/// call this once early (e.g. at app startup right after login).
	///
	/// Errors with `InvalidState` if a cache worker is currently live; reconfiguring is allowed
	/// once every sync-root handle is dropped or the cache is flushed.
	pub async fn configure_cache(
		&self,
		cache_path: String,
		status_listener: Arc<dyn CacheStatusListener>,
	) -> Result<(), Error> {
		let this = self.inner();
		do_on_commander(move || async move {
			this.configure_cache(PathBuf::from(cache_path), move |messages| {
				let listener = Arc::clone(&status_listener);
				let messages: Vec<CacheStatusMessage> =
					messages.into_iter().map(Into::into).collect();
				// House callback discipline: the foreign call runs on a blocking thread, never
				// on the runtime delivering the messages.
				tokio::task::spawn_blocking(move || {
					listener.on_messages(messages);
				});
			})
			.await
		})
		.await
	}
}

#[cfg(test)]
mod tests {
	use uuid::Uuid;

	use super::*;
	use crate::cache::CacheError;

	#[test]
	fn cache_messages_map_to_their_ffi_mirrors() {
		let uuid = Uuid::new_v4();

		let errors =
			CacheStatusMessage::from(CacheMessage::Error(vec![CacheError::Serialization(
				"boom".to_string(),
			)]));
		let CacheStatusMessage::Errors { errors } = errors else {
			panic!("expected Errors, got {errors:?}");
		};
		assert_eq!(errors.len(), 1);
		assert!(errors[0].contains("boom"), "got {:?}", errors[0]);

		let deleted = CacheStatusMessage::from(CacheMessage::SyncRootsDeleted(vec![uuid]));
		let CacheStatusMessage::SyncRootsDeleted { roots } = deleted else {
			panic!("expected SyncRootsDeleted, got {deleted:?}");
		};
		assert_eq!(roots, vec![UuidStr::from(&uuid)]);
	}

	#[test]
	fn resync_progress_maps_field_for_field() {
		let uuid = Uuid::new_v4();

		let started = ResyncProgressMessage::from(ResyncProgress::Started { roots: vec![uuid] });
		assert!(
			matches!(started, ResyncProgressMessage::Started { ref roots } if *roots == vec![UuidStr::from(&uuid)])
		);

		let listing = ResyncProgressMessage::from(ResyncProgress::Listing {
			root: uuid,
			root_index: 3,
			root_count: 7,
			bytes_downloaded: 42,
			total_bytes: Some(100),
		});
		let ResyncProgressMessage::Listing {
			root,
			root_index,
			root_count,
			bytes_downloaded,
			total_bytes,
		} = listing
		else {
			panic!("expected Listing, got {listing:?}");
		};
		assert_eq!(root, UuidStr::from(&uuid));
		assert_eq!(root_index, 3);
		assert_eq!(root_count, 7);
		assert_eq!(bytes_downloaded, 42);
		assert_eq!(total_bytes, Some(100));

		assert!(matches!(
			ResyncProgressMessage::from(ResyncProgress::Applying),
			ResyncProgressMessage::Applying
		));
		assert!(matches!(
			ResyncProgressMessage::from(ResyncProgress::Finished { converged: true }),
			ResyncProgressMessage::Finished { converged: true }
		));
	}
}

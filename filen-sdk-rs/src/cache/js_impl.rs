//! FFI exposure of the cache's configuration surface, for BOTH UniFFI (mobile) and wasm (web).
//!
//! Owned MIRROR types converted from the core enums (the core
//! [`CacheMessage`] carries payloads that cannot cross the boundary — raw `rusqlite::Error`s and
//! full `RemoteFile`/`RemoteDirectory` conversion-failure records — so errors are flattened to
//! display strings). On UniFFI the listener is a `with_foreign` trait whose every invocation is
//! `spawn_blocking`'d off the runtime; on wasm it is a `js_sys::Function` fed by a channel pump
//! on the calling thread (the socket-listener pattern), since a JS function can never leave its
//! thread. Method bodies ship to the commander thread via `do_on_commander`.

use std::path::PathBuf;
#[cfg(feature = "uniffi")]
use std::sync::Arc;

use filen_macros::js_type;
use filen_types::fs::UuidStr;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasm_bindgen::JsValue;

use crate::{
	Error,
	auth::JsClient,
	cache::{CacheMessage, ResyncProgress},
	runtime::do_on_commander,
};

/// FFI mirror of [`CacheMessage`] (see its docs for the full semantics of each variant).
/// Delivery is BEST-EFFORT on every platform (messages can drop under load): treat each message
/// as a fresh snapshot, never as a delta to accumulate.
#[js_type(export, no_deser, tagged)]
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
#[js_type(export, no_deser, tagged)]
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

/// Receives every status-message batch the cache worker emits (see [`CacheStatusMessage`]).
/// Registered ONCE via [`JsClient::configure_cache`] and reused across worker restarts.
/// Invocations are dispatched with `spawn_blocking`, so a slow foreign implementation cannot
/// stall the cache.
#[cfg(feature = "uniffi")]
#[uniffi::export(with_foreign)]
pub trait CacheStatusListener: Send + Sync {
	fn on_messages(&self, messages: Vec<CacheStatusMessage>);
}

#[cfg(feature = "uniffi")]
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

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
#[wasm_bindgen::prelude::wasm_bindgen(js_class = "Client")]
impl JsClient {
	/// Pre-warm the cache: store the DB name and the global status listener on the client.
	/// PURE STORAGE — the worker (and its in-memory SQLite store: the wasm cache is per-session
	/// and repopulated by the startup resync) starts with the first sync root.
	#[wasm_bindgen::prelude::wasm_bindgen(js_name = "configureCache")]
	pub async fn configure_cache(
		&self,
		cache_path: String,
		#[wasm_bindgen(unchecked_param_type = "(messages: CacheStatusMessage[]) => void")]
		status_listener: web_sys::js_sys::Function,
	) -> Result<(), Error> {
		// The JS function can never leave this thread: the Send closure stored in the cache
		// config only forwards mirrors over a channel, and this pump (owning the function)
		// serializes + invokes on the calling thread — the socket-listener pattern.
		let (sender, mut receiver) =
			tokio::sync::mpsc::unbounded_channel::<Vec<CacheStatusMessage>>();
		crate::runtime::spawn_local(async move {
			while let Some(messages) = receiver.recv().await {
				let serializer = serde_wasm_bindgen::Serializer::new()
					.serialize_maps_as_objects(true)
					.serialize_large_number_types_as_bigints(true);
				let _ = status_listener.call1(
					&JsValue::UNDEFINED,
					&serde::Serialize::serialize(&messages, &serializer)
						.expect("failed to serialize cache status messages (should be impossible)"),
				);
			}
		});

		let this = self.inner();
		do_on_commander(move || async move {
			this.configure_cache(PathBuf::from(cache_path), move |messages| {
				let _ = sender.send(messages.into_iter().map(Into::into).collect());
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

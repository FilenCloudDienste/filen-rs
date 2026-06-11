//! UniFFI exposure of the cache-backed search. UniFFI-ONLY for now, like the parent cache FFI
//! module (`cache::js_impl`) — the wasm twin arrives with the cache's wasm port.
//!
//! Results cross as the SAME `Dir`/`File` types the rest of the FFI API returns (via the
//! lossless `Cacheable* → Remote*` conversions), so a search hit can be fed straight back into
//! download/move/share calls without a second lookup. The core [`Search`] lives inside
//! [`CacheSearch`] behind a `Mutex<Option<_>>` because [`Search::close`] consumes it, which an
//! FFI object (always held behind an `Arc`) can only express by interior take.

use std::sync::Arc;

use filen_types::fs::UuidStr;
use uuid::Uuid;

use super::{
	Search, SearchConfig, SearchItemType, SearchResult, SearchSnapshot, SearchWindowHandle,
};
use crate::{
	Error, ErrorKind,
	auth::JsClient,
	io::{RemoteDirectory, RemoteFile},
	js::{Dir, File},
	runtime::do_on_commander,
};

/// Receives fresh snapshots for ONE window whenever its contents or the total change (the
/// initial snapshot is returned by [`CacheSearch::get_range`], never delivered here). Dispatched
/// via `spawn_blocking`, so a slow foreign implementation cannot stall the search engine. A
/// snapshot with `live: false` is TERMINAL for the whole search and fires at most once per
/// window.
#[uniffi::export(with_foreign)]
pub trait CacheSearchWindowListener: Send + Sync {
	fn on_snapshot(&self, snapshot: CacheSearchSnapshot);
}

/// FFI mirror of [`SearchConfig`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct CacheSearchConfig {
	/// Substring match on item names (trimmed + NFC-normalized; matched case-insensitively with
	/// Unicode simple case folding unless `case_sensitive`). `None` matches everything.
	#[uniffi(default = None)]
	pub name: Option<String>,
	/// `None` means [`SearchItemType::All`] (UniFFI cannot express an enum-variant default).
	#[uniffi(default = None)]
	pub item_type: Option<SearchItemType>,
	/// `true`: match the whole subtree; `false`: direct children only (a live, sorted
	/// directory listing).
	#[uniffi(default = true)]
	pub recursive: bool,
	#[uniffi(default = false)]
	pub case_sensitive: bool,
}

impl From<CacheSearchConfig> for SearchConfig {
	fn from(config: CacheSearchConfig) -> Self {
		let mut out = Self::new()
			.with_item_type(config.item_type.unwrap_or_default())
			.with_recursive(config.recursive)
			.with_case_sensitive(config.case_sensitive);
		out.name = config.name;
		out
	}
}

/// FFI mirror of [`SearchResult`]: the same `Dir`/`File` payloads the rest of the API uses,
/// directly actionable without a second lookup.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum CacheSearchResult {
	Dir { dir: Dir },
	File { file: File },
}

impl From<SearchResult> for CacheSearchResult {
	fn from(result: SearchResult) -> Self {
		match result {
			SearchResult::Dir(dir) => Self::Dir {
				dir: Dir::from(RemoteDirectory::from(dir)),
			},
			SearchResult::File(file) => Self::File {
				file: File::from(RemoteFile::from(file)),
			},
		}
	}
}

/// FFI mirror of [`SearchSnapshot`]: one window's FULL fresh contents plus the total match
/// count — never a delta. Treat each delivery as the window's new truth.
#[derive(Debug, Clone, uniffi::Record)]
pub struct CacheSearchSnapshot {
	/// The window's current contents (name-ascending, directories first).
	pub results: Vec<CacheSearchResult>,
	/// Total matches across the WHOLE result set, not just this window.
	pub total: u64,
	/// `false` is TERMINAL: the searched directory was deleted server-side or the cache worker
	/// stopped. Fired at most once per window, carrying the window's last-delivered results.
	pub live: bool,
}

impl From<SearchSnapshot> for CacheSearchSnapshot {
	fn from(snapshot: SearchSnapshot) -> Self {
		Self {
			results: snapshot.results.into_iter().map(Into::into).collect(),
			total: snapshot.total as u64,
			live: snapshot.live,
		}
	}
}

/// One registered window: the window's initial snapshot plus the RAII handle keeping the
/// subscription alive.
#[derive(uniffi::Record)]
pub struct CacheSearchWindow {
	pub snapshot: CacheSearchSnapshot,
	pub handle: Arc<CacheSearchWindowHandle>,
}

/// Keeps one window subscription alive: releasing the foreign handle unsubscribes the window
/// (its listener never fires again). Holds only a weak engine reference, so an outliving handle
/// never keeps a closed search alive.
#[derive(uniffi::Object)]
pub struct CacheSearchWindowHandle {
	_handle: SearchWindowHandle,
}

/// FFI handle to a live cache-backed search (see the cache search module docs for the
/// consistency model and costs). Releasing the foreign object shuts the engine down best-effort
/// — like dropping the Rust [`Search`] — and releases the search's sync-root registration;
/// [`close`](Self::close) does the same deterministically.
#[derive(uniffi::Object)]
pub struct CacheSearch {
	root: UuidStr,
	/// `Some` until [`close`](Self::close) takes it — the core close consumes the [`Search`],
	/// which an `Arc`-held FFI object can only express by interior take.
	inner: tokio::sync::Mutex<Option<Search>>,
}

#[uniffi::export]
impl CacheSearch {
	/// The searched directory's uuid — correlate with the `ResyncProgress` /
	/// `SyncRootsDeleted` messages on the cache status listener.
	pub fn root_uuid(&self) -> UuidStr {
		self.root
	}

	/// Total matches currently in the result set. Advisory — each snapshot carries its own
	/// coherent total. `0` after [`close`](Self::close).
	pub async fn total(&self) -> u64 {
		self.inner
			.lock()
			.await
			.as_ref()
			.map_or(0, |search| search.total() as u64)
	}

	/// `false` once the search went terminal — or after [`close`](Self::close).
	pub async fn is_live(&self) -> bool {
		self.inner
			.lock()
			.await
			.as_ref()
			.is_some_and(|search| search.is_live())
	}

	/// Subscribe a window over `start..end` of the sorted result set (CLAMPED to the available
	/// results — never an out-of-bounds error). Returns the window's current snapshot plus its
	/// RAII handle; from then on `listener` fires with a fresh snapshot whenever the window's
	/// contents or the total change. The window keeps its REQUESTED range, refilling as the
	/// result set grows.
	pub async fn get_range(
		&self,
		start: u64,
		end: u64,
		listener: Arc<dyn CacheSearchWindowListener>,
	) -> Result<CacheSearchWindow, Error> {
		let guard = self.inner.lock().await;
		let search = guard.as_ref().ok_or_else(search_closed)?;
		let range = usize::try_from(start).unwrap_or(usize::MAX)
			..usize::try_from(end).unwrap_or(usize::MAX);
		let (snapshot, handle) = search
			.get_range(
				range,
				Box::new(move |snapshot| {
					let listener = Arc::clone(&listener);
					let snapshot = CacheSearchSnapshot::from(snapshot);
					// House callback discipline: the foreign call runs on a blocking thread,
					// never on the engine task delivering the snapshot.
					tokio::task::spawn_blocking(move || {
						listener.on_snapshot(snapshot);
					});
				}),
			)
			.await?;
		Ok(CacheSearchWindow {
			snapshot: snapshot.into(),
			handle: Arc::new(CacheSearchWindowHandle { _handle: handle }),
		})
	}

	/// Replace the filter configuration: an engine-local refilter — no re-registration, no
	/// network. THE way to change what the search matches; never close + recreate per filter
	/// change.
	pub async fn set_config(&self, config: CacheSearchConfig) -> Result<(), Error> {
		let guard = self.inner.lock().await;
		let search = guard.as_ref().ok_or_else(search_closed)?;
		search.set_config(config.into()).await
	}

	/// Deterministic teardown: resolves once the engine has exited with its DB connection
	/// closed. Idempotent; outstanding window handles become inert. Releasing the object
	/// WITHOUT calling this is equally correct (best-effort shutdown), just not awaitable.
	pub async fn close(&self) {
		let search = self.inner.lock().await.take();
		if let Some(search) = search {
			search.close().await;
		}
	}
}

fn search_closed() -> Error {
	Error::custom(ErrorKind::InvalidState, "the search has been closed")
}

#[uniffi::export]
impl JsClient {
	/// Create a live, windowed search over the subtree rooted at `uuid`, served from the local
	/// cache ([`configure_cache`](JsClient::configure_cache) must have run first). CHEAP (zero
	/// network, zero resync) when `uuid` is already an active sync root or is covered by one;
	/// otherwise the worker validates it remotely and runs a convergence resync — observable as
	/// `ResyncProgress` messages keyed by this uuid on the cache status listener. Change
	/// filters with [`CacheSearch::set_config`], not by recreating the search.
	pub async fn create_search(
		&self,
		uuid: UuidStr,
		config: CacheSearchConfig,
	) -> Result<CacheSearch, Error> {
		let this = self.inner();
		do_on_commander(move || async move {
			// The commander runtime is the long-lived multi-thread runtime anchoring every FFI
			// call — it satisfies create_search's "app Tokio runtime" requirement (the cache
			// worker's resync `block_on` handle binds to it).
			let search = this.create_search(Uuid::from(&uuid), config.into()).await?;
			Ok(CacheSearch {
				root: uuid,
				inner: tokio::sync::Mutex::new(Some(search)),
			})
		})
		.await
	}
}

#[cfg(test)]
mod tests {
	use std::borrow::Cow;

	use filen_types::{api::v3::dir::color::DirColor, auth::FileEncryptionVersion};

	use super::*;
	use crate::{
		crypto::file::FileKey,
		fs::{dir::cache::CacheableDir, file::cache::CacheableFile},
	};

	fn ms(millis: i64) -> chrono::DateTime<chrono::Utc> {
		chrono::DateTime::from_timestamp_millis(millis).unwrap()
	}

	#[test]
	fn config_mirror_maps_defaults_and_fields() {
		let config = SearchConfig::from(CacheSearchConfig {
			name: None,
			item_type: None,
			recursive: true,
			case_sensitive: false,
		});
		assert_eq!(config, SearchConfig::new(), "FFI defaults == core defaults");

		let config = SearchConfig::from(CacheSearchConfig {
			name: Some("report".to_string()),
			item_type: Some(SearchItemType::File),
			recursive: false,
			case_sensitive: true,
		});
		assert_eq!(config.name.as_deref(), Some("report"));
		assert_eq!(config.item_type, SearchItemType::File);
		assert!(!config.recursive);
		assert!(config.case_sensitive);
	}

	#[test]
	fn snapshot_mirror_carries_results_total_and_live() {
		let dir = CacheableDir {
			uuid: Uuid::new_v4(),
			parent: Uuid::new_v4(),
			color: DirColor::Default,
			favorited: false,
			timestamp: ms(1_700_000_000_000),
			name: Cow::Borrowed("docs"),
			created: Some(ms(1_700_000_000_001)),
		};
		let file = CacheableFile {
			uuid: Uuid::new_v4(),
			parent: dir.uuid,
			chunks_size: 1,
			chunks: 1,
			favorited: true,
			region: Cow::Borrowed("region"),
			bucket: Cow::Borrowed("bucket"),
			timestamp: ms(1_700_000_000_002),
			name: Cow::Borrowed("notes.txt"),
			size: 5,
			mime: Cow::Borrowed("text/plain"),
			key: FileKey::from_str_with_version(&"a".repeat(64), FileEncryptionVersion::V3)
				.unwrap(),
			last_modified: ms(1_700_000_000_003),
			created: None,
			hash: None,
		};

		let snapshot = CacheSearchSnapshot::from(SearchSnapshot {
			results: vec![
				SearchResult::Dir(dir.clone()),
				SearchResult::File(file.clone()),
			],
			total: 9,
			live: true,
		});
		assert_eq!(snapshot.total, 9);
		assert!(snapshot.live);
		assert_eq!(snapshot.results.len(), 2);
		// The mirrors are the SAME types the rest of the FFI API returns, built through the
		// lossless Cacheable → Remote conversions.
		assert!(matches!(
			&snapshot.results[0],
			CacheSearchResult::Dir { dir: converted } if *converted == Dir::from(RemoteDirectory::from(dir.clone()))
		));
		assert!(matches!(
			&snapshot.results[1],
			CacheSearchResult::File { file: converted } if *converted == File::from(RemoteFile::from(file.clone()))
		));
	}
}

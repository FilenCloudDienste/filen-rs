//! FFI exposure of the cache-backed search, for BOTH UniFFI (mobile) and wasm (web).
//!
//! Results cross as the SAME `Dir`/`File` types the rest of the FFI API returns (via the
//! lossless `Cacheable* → Remote*` conversions), so a search hit can be fed straight back into
//! download/move/share calls without a second lookup. The core [`Search`] lives inside
//! [`CacheSearch`] behind a `Mutex<Option<_>>` because [`Search::close`] consumes it, which an
//! FFI object (always held behind a shared handle) can only express by interior take. Window
//! listeners follow each platform's callback discipline: `spawn_blocking` per invocation on
//! UniFFI, a channel-pumped `js_sys::Function` on wasm.

#[cfg(feature = "uniffi")]
use std::sync::Arc;

use filen_macros::js_type;
use filen_types::fs::UuidStr;
use uuid::Uuid;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasm_bindgen::JsValue;

use super::{
	Search, SearchConfig, SearchHit, SearchItemType, SearchResult, SearchSnapshot,
	SearchWindowHandle,
};
use crate::{
	Error, ErrorKind,
	auth::JsClient,
	io::{RemoteDirectory, RemoteFile},
	js::{Dir, File},
	runtime::do_on_commander,
};

/// FFI mirror of [`SearchItemType`]. Hand-rolled derives (not `js_type`): the macro's tagged
/// twin generation has no use for a fieldless enum, and the config struct's serde derives need
/// serde on every platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(from_wasm_abi, into_wasm_abi)
)]
pub enum CacheSearchItemType {
	All,
	File,
	Dir,
}

impl From<CacheSearchItemType> for SearchItemType {
	fn from(item_type: CacheSearchItemType) -> Self {
		match item_type {
			CacheSearchItemType::All => Self::All,
			CacheSearchItemType::File => Self::File,
			CacheSearchItemType::Dir => Self::Dir,
		}
	}
}

/// FFI mirror of [`SearchConfig`].
#[js_type(import)]
pub struct CacheSearchConfig {
	/// Substring match on item names (trimmed + NFC-normalized; matched case-insensitively with
	/// Unicode simple case folding unless `case_sensitive`). `None` matches everything.
	#[cfg_attr(feature = "uniffi", uniffi(default = None))]
	pub name: Option<String>,
	/// `None` means [`CacheSearchItemType::All`] (UniFFI cannot express an enum-variant
	/// default).
	#[cfg_attr(feature = "uniffi", uniffi(default = None))]
	pub item_type: Option<CacheSearchItemType>,
	/// `true`: match the whole subtree; `false`: direct children only (a live, sorted
	/// directory listing).
	#[cfg_attr(feature = "uniffi", uniffi(default = true))]
	pub recursive: bool,
	#[cfg_attr(feature = "uniffi", uniffi(default = false))]
	pub case_sensitive: bool,
}

impl From<CacheSearchConfig> for SearchConfig {
	fn from(config: CacheSearchConfig) -> Self {
		let mut out = Self::new()
			.with_item_type(config.item_type.map(Into::into).unwrap_or_default())
			.with_recursive(config.recursive)
			.with_case_sensitive(config.case_sensitive);
		out.name = config.name;
		out
	}
}

/// FFI mirror of [`SearchResult`]: the same `Dir`/`File` payloads the rest of the API uses,
/// directly actionable without a second lookup.
#[js_type(export, no_deser, tagged)]
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

/// FFI mirror of [`SearchHit`]: a [`CacheSearchResult`] plus its `parent_path` relative to the
/// search root — the `/`-joined chain of ancestor directory names from the search root
/// (exclusive) down to the item's parent (inclusive); empty for a direct child of the root. A
/// wrapper struct (mirroring the `*WithPath` shape used elsewhere in the FFI) rather than extra
/// fields on the result enum's variants.
#[js_type(export, no_deser)]
pub struct CacheSearchHit {
	/// The item's parent path relative to the search root; empty for a direct child of the root.
	pub parent_path: String,
	/// The matched item.
	pub result: CacheSearchResult,
}

impl From<SearchHit> for CacheSearchHit {
	fn from(hit: SearchHit) -> Self {
		Self {
			parent_path: String::from(hit.parent_path),
			result: hit.result.into(),
		}
	}
}

/// FFI mirror of [`SearchSnapshot`]: one window's FULL fresh contents plus the total match
/// count — never a delta. Treat each delivery as the window's new truth.
#[js_type(export, no_deser)]
pub struct CacheSearchSnapshot {
	/// The window's current contents (name-ascending, directories first), each paired with its
	/// `parent_path` relative to the search root.
	pub results: Vec<CacheSearchHit>,
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

/// Keeps one window subscription alive: releasing the foreign handle unsubscribes the window
/// (its listener never fires again). Holds only a weak engine reference, so an outliving handle
/// never keeps a closed search alive. UniFFI-only: the wasm window type embeds the core handle
/// directly, so exporting this to the web bundle would only add a dead, unconstructible class.
#[cfg(feature = "uniffi")]
#[derive(uniffi::Object)]
pub struct CacheSearchWindowHandle {
	_handle: SearchWindowHandle,
}

/// One registered window: the window's initial snapshot plus the RAII handle keeping the
/// subscription alive.
#[cfg(feature = "uniffi")]
#[derive(uniffi::Record)]
pub struct CacheSearchWindow {
	pub snapshot: CacheSearchSnapshot,
	pub handle: Arc<CacheSearchWindowHandle>,
}

/// FFI handle to a live cache-backed search (see the cache search module docs for the
/// consistency model and costs). Releasing the foreign object shuts the engine down best-effort
/// — like dropping the Rust [`Search`] — and releases the search's sync-root registration;
/// [`close`](Self::close) does the same deterministically.
#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	wasm_bindgen::prelude::wasm_bindgen
)]
pub struct CacheSearch {
	root: UuidStr,
	/// `Some` until [`close`](Self::close) takes it — the core close consumes the [`Search`],
	/// which a shared FFI handle can only express by interior take.
	inner: tokio::sync::Mutex<Option<Search>>,
}

impl CacheSearch {
	async fn total_inner(&self) -> u64 {
		self.inner
			.lock()
			.await
			.as_ref()
			.map_or(0, |search| search.total() as u64)
	}

	async fn is_live_inner(&self) -> bool {
		self.inner
			.lock()
			.await
			.as_ref()
			.is_some_and(|search| search.is_live())
	}

	async fn get_range_inner(
		&self,
		start: u64,
		end: u64,
		callback: super::SearchWindowCallback,
	) -> Result<(CacheSearchSnapshot, SearchWindowHandle), Error> {
		let guard = self.inner.lock().await;
		let search = guard.as_ref().ok_or_else(search_closed)?;
		let range = usize::try_from(start).unwrap_or(usize::MAX)
			..usize::try_from(end).unwrap_or(usize::MAX);
		let (snapshot, handle) = search.get_range(range, callback).await?;
		Ok((snapshot.into(), handle))
	}

	async fn set_config_inner(&self, config: CacheSearchConfig) -> Result<(), Error> {
		let guard = self.inner.lock().await;
		let search = guard.as_ref().ok_or_else(search_closed)?;
		search.set_config(config.into()).await
	}

	async fn close_inner(&self) {
		let search = self.inner.lock().await.take();
		if let Some(search) = search {
			search.close().await;
		}
	}
}

/// Receives fresh snapshots for ONE window whenever its contents or the total change (the
/// initial snapshot is returned by [`CacheSearch::get_range`], never delivered here). Dispatched
/// via `spawn_blocking`, so a slow foreign implementation cannot stall the search engine. A
/// snapshot with `live: false` is TERMINAL for the whole search and fires at most once per
/// window.
#[cfg(feature = "uniffi")]
#[uniffi::export(with_foreign)]
pub trait CacheSearchWindowListener: Send + Sync {
	fn on_snapshot(&self, snapshot: CacheSearchSnapshot);
}

#[cfg(feature = "uniffi")]
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
		self.total_inner().await
	}

	/// `false` once the search went terminal — or after [`close`](Self::close).
	pub async fn is_live(&self) -> bool {
		self.is_live_inner().await
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
		let (snapshot, handle) = self
			.get_range_inner(
				start,
				end,
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
			snapshot,
			handle: Arc::new(CacheSearchWindowHandle { _handle: handle }),
		})
	}

	/// Replace the filter configuration: an engine-local refilter — no re-registration, no
	/// network. THE way to change what the search matches; never close + recreate per filter
	/// change.
	pub async fn set_config(&self, config: CacheSearchConfig) -> Result<(), Error> {
		self.set_config_inner(config).await
	}

	/// Deterministic teardown: resolves once the engine has exited. Idempotent; outstanding
	/// window handles become inert. Releasing the object WITHOUT calling this is equally
	/// correct (best-effort shutdown), just not awaitable.
	pub async fn close(&self) {
		self.close_inner().await;
	}
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
#[wasm_bindgen::prelude::wasm_bindgen]
impl CacheSearch {
	/// The searched directory's uuid — correlate with the `ResyncProgress` /
	/// `SyncRootsDeleted` messages on the cache status listener.
	#[wasm_bindgen::prelude::wasm_bindgen(js_name = "rootUuid")]
	pub fn root_uuid(&self) -> UuidStr {
		self.root
	}

	/// Total matches currently in the result set. Advisory — each snapshot carries its own
	/// coherent total. `0` after `close`.
	pub async fn total(&self) -> u64 {
		self.total_inner().await
	}

	/// `false` once the search went terminal — or after `close`.
	#[wasm_bindgen::prelude::wasm_bindgen(js_name = "isLive")]
	pub async fn is_live(&self) -> bool {
		self.is_live_inner().await
	}

	/// Subscribe a window over `start..end` of the sorted result set (CLAMPED — never an
	/// out-of-bounds error). Returns the window's current snapshot plus its RAII handle (free
	/// the handle to unsubscribe); from then on `listener` fires with a fresh snapshot whenever
	/// the window's contents or the total change.
	#[wasm_bindgen::prelude::wasm_bindgen(js_name = "getRange")]
	pub async fn get_range(
		&self,
		start: u64,
		end: u64,
		#[wasm_bindgen(unchecked_param_type = "(snapshot: CacheSearchSnapshot) => void")]
		listener: web_sys::js_sys::Function,
	) -> Result<CacheSearchWindow, Error> {
		// The JS function stays on this thread; the engine-side Send callback only forwards
		// mirrors over a channel (the socket-listener pattern).
		let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<CacheSearchSnapshot>();
		crate::runtime::spawn_local(async move {
			while let Some(snapshot) = receiver.recv().await {
				let serializer = serde_wasm_bindgen::Serializer::new()
					.serialize_maps_as_objects(true)
					.serialize_large_number_types_as_bigints(true);
				let _ = listener.call1(
					&JsValue::UNDEFINED,
					&serde::Serialize::serialize(&snapshot, &serializer)
						.expect("failed to serialize search snapshot (should be impossible)"),
				);
			}
		});

		let (snapshot, handle) = self
			.get_range_inner(
				start,
				end,
				Box::new(move |snapshot| {
					let _ = sender.send(snapshot.into());
				}),
			)
			.await?;
		Ok(CacheSearchWindow {
			snapshot: Some(snapshot),
			_handle: handle,
		})
	}

	/// Replace the filter configuration: an engine-local refilter — no re-registration, no
	/// network. THE way to change what the search matches; never close + recreate per filter
	/// change.
	#[wasm_bindgen::prelude::wasm_bindgen(js_name = "setConfig")]
	pub async fn set_config(&self, config: CacheSearchConfig) -> Result<(), Error> {
		self.set_config_inner(config).await
	}

	/// Deterministic teardown: resolves once the engine has exited. Idempotent; outstanding
	/// window handles become inert. Freeing the object WITHOUT calling this is equally correct
	/// (best-effort shutdown), just not awaitable.
	pub async fn close(&self) {
		self.close_inner().await;
	}
}

/// One registered window on wasm: the initial snapshot (read it once via
/// [`initial_snapshot`](Self::initial_snapshot)) doubling as the RAII subscription handle —
/// `free()` it to unsubscribe the window.
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
#[wasm_bindgen::prelude::wasm_bindgen]
pub struct CacheSearchWindow {
	snapshot: Option<CacheSearchSnapshot>,
	_handle: SearchWindowHandle,
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
#[wasm_bindgen::prelude::wasm_bindgen]
impl CacheSearchWindow {
	/// The window's snapshot at registration time (later updates arrive via the listener).
	/// Consumed on first call — returns `undefined` afterwards.
	#[wasm_bindgen::prelude::wasm_bindgen(
		js_name = "initialSnapshot",
		unchecked_return_type = "CacheSearchSnapshot | undefined"
	)]
	pub fn initial_snapshot(&mut self) -> Result<JsValue, Error> {
		let Some(snapshot) = self.snapshot.take() else {
			return Ok(JsValue::UNDEFINED);
		};
		let serializer = serde_wasm_bindgen::Serializer::new()
			.serialize_maps_as_objects(true)
			.serialize_large_number_types_as_bigints(true);
		Ok(serde::Serialize::serialize(&snapshot, &serializer)
			.expect("failed to serialize search snapshot (should be impossible)"))
	}
}

fn search_closed() -> Error {
	Error::custom(ErrorKind::InvalidState, "the search has been closed")
}

#[cfg(feature = "uniffi")]
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
			let search = this.create_search(Uuid::from(&uuid), config.into()).await?;
			Ok(CacheSearch {
				root: uuid,
				inner: tokio::sync::Mutex::new(Some(search)),
			})
		})
		.await
	}
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
#[wasm_bindgen::prelude::wasm_bindgen(js_class = "Client")]
impl JsClient {
	/// Create a live, windowed search over the subtree rooted at `uuid`, served from the local
	/// cache (`configureCache` must have run first). CHEAP (zero network, zero resync) when
	/// `uuid` is already an active sync root or is covered by one; otherwise the worker
	/// validates it remotely and runs a convergence resync — observable as `ResyncProgress`
	/// messages on the cache status listener.
	#[wasm_bindgen::prelude::wasm_bindgen(js_name = "createSearch")]
	pub async fn create_search(
		&self,
		uuid: UuidStr,
		config: CacheSearchConfig,
	) -> Result<CacheSearch, Error> {
		let this = self.inner();
		do_on_commander(move || async move {
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
			item_type: Some(CacheSearchItemType::File),
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
				SearchHit {
					result: SearchResult::Dir(dir.clone()),
					parent_path: "".into(),
				},
				SearchHit {
					result: SearchResult::File(file.clone()),
					parent_path: "docs".into(),
				},
			],
			total: 9,
			live: true,
		});
		assert_eq!(snapshot.total, 9);
		assert!(snapshot.live);
		assert_eq!(snapshot.results.len(), 2);
		assert_eq!(snapshot.results[0].parent_path, "");
		assert!(matches!(
			&snapshot.results[0].result,
			CacheSearchResult::Dir { dir: converted } if *converted == Dir::from(RemoteDirectory::from(dir.clone()))
		));
		assert_eq!(snapshot.results[1].parent_path, "docs");
		assert!(matches!(
			&snapshot.results[1].result,
			CacheSearchResult::File { file: converted } if *converted == File::from(RemoteFile::from(file.clone()))
		));
	}
}

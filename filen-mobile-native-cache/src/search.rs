//! Live drive search for the documents provider, backed by the SDK's [`cache::search`] engine.
//!
//! Unlike the rest of this crate (which persists into its own `native_cache.db`), search runs on
//! a SECOND, SDK-owned cache DB ([`AuthCacheState::sdk_cache_path`](crate::auth)): the first query
//! configures it, registers the drive root as a sync root, and resyncs it from the server, then
//! serves live, windowed results. ONE [`Search`] is kept alive on the root and re-filtered per
//! query via [`Search::set_config`] — recreating it per query would re-register the sync root.
//!
//! The engine matches by name + item type only; `mime`/`size`/`modified` from the query are
//! post-filtered here (mirroring the old server-search query semantics). Because that post-filter
//! can drop name+type matches, [`collect_filtered`] pages past the first window until the result
//! cap fills or the matches are exhausted, so a filtered query is not silently truncated at the
//! first window's worth of name-matches.
//!
//! [`cache::search`]: filen_sdk_rs::cache::search

use std::{collections::HashSet, sync::Arc};

use filen_sdk_rs::{
	cache::search::{
		Search, SearchConfig, SearchItemType, SearchResult, SearchSnapshot, SearchWindowCallback,
		SearchWindowHandle,
	},
	fs::{dir::cache::CacheableDir, file::cache::CacheableFile},
};
use uuid::Uuid;

use crate::{
	CacheError,
	auth::AuthCacheState,
	ffi::{
		FfiDir, FfiDirMeta, FfiFile, FfiFileMeta, FfiNonRootObject, ItemType, SearchQueryArgs,
		SearchQueryResponseEntry,
	},
	traits::SearchUpdateCallback,
};

/// Filename of the SDK cache DB backing search, kept at the FILES-DIR ROOT (a sibling of
/// `native_cache.db`, deliberately NOT under `cache_dir` whose cleanup scans for per-file uuid
/// subdirs — see `AuthCacheState` construction). Rebuildable from the server; wiped on deauth by
/// [`crate::io`]'s cache cleanup.
pub(crate) const SDK_CACHE_DB_NAME: &str = "sdk_search_cache.db";

/// Upper bound on results returned per query. The documents provider renders a flat cursor (no
/// pagination), so we read a single wide window.
const SEARCH_RESULT_LIMIT: usize = 1000;

/// The one live search, its root, and the window handle keeping its subscription (and thus the
/// update callback) alive between queries. Dropping the handle unsubscribes.
pub(crate) struct ActiveSearch {
	root: Uuid,
	search: Search,
	/// Held to keep the window subscription (and its update callback) alive; dropped to
	/// unsubscribe.
	window: SearchWindowHandle,
}

impl AuthCacheState {
	/// Search the subtree rooted at `root_id` via the live cache engine (the drive root for the
	/// documents provider; any directory uuid otherwise). Returns the current page immediately
	/// (whatever has synced so far); `on_update` fires as the on-demand resync converges so the
	/// caller can re-query for the fuller set.
	pub(crate) async fn query_search(
		&self,
		root_id: String,
		args: SearchQueryArgs,
		on_update: Arc<dyn SearchUpdateCallback>,
	) -> Result<Vec<SearchQueryResponseEntry>, CacheError> {
		let root_uuid = Uuid::parse_str(&root_id)
			.map_err(|e| CacheError::conversion(format!("invalid search root '{root_id}': {e}")))?;
		let config = config_from_args(&args);

		// Fast path: reuse a live search already rooted here. Take it OUT under the lock, then RELEASE
		// the lock before touching the engine — `set_config` + `get_range` + the (now possibly
		// multi-page) `collect_filtered` are local but no longer O(1), so holding `self.search` across
		// them would serialize every other concurrent query_search (any root) behind this one.
		let existing = {
			let mut guard = self.search.lock().await;
			// Either way drop any stored search: a matching root we take to reuse; a different root
			// (or first use) we discard so the create below starts clean.
			match &*guard {
				Some(active) if active.root == root_uuid => guard.take(),
				_ => {
					guard.take();
					None
				}
			}
		};
		if let Some(ActiveSearch { search, window, .. }) = existing {
			// Unsubscribe the previous window BEFORE refiltering so its stale listener doesn't fire an
			// extra update.
			drop(window);
			search.set_config(config).await?;
			let (snapshot, window) = search
				.get_range(0..SEARCH_RESULT_LIMIT, update_callback(on_update))
				.await?;
			let entries = collect_filtered(&search, snapshot, &root_id, &args).await?;
			// Re-store only on success. On an error above the taken search is dropped (its engine
			// shuts down) and the next query rebuilds it via the create path — a transient failure
			// costs one recreate, never a wrong result.
			*self.search.lock().await = Some(ActiveSearch {
				root: root_uuid,
				search,
				window,
			});
			return Ok(entries);
		}

		// Create WITHOUT holding `self.search` (create_search does a remote validation + registers
		// a sync root that can wait on an in-flight resync).
		// Best-effort: configuring only errors when a worker is already live, in which case
		// create_search reuses it; a genuine misconfiguration surfaces from create_search.
		let _ = self
			.client
			.configure_cache(self.sdk_cache_path.clone(), |messages| {
				tracing::debug!(?messages, "sdk search cache status");
			})
			.await;
		let search = self.client.clone().create_search(root_uuid, config).await?;
		let (snapshot, window) = search
			.get_range(0..SEARCH_RESULT_LIMIT, update_callback(on_update))
			.await?;
		let entries = collect_filtered(&search, snapshot, &root_id, &args).await?;

		// Store it. If another caller created a search for the SAME root while we were creating,
		// ours simply replaces it (the loser's engine shuts down on drop) — a rare, bounded waste,
		// only reachable when two searches race on a not-yet-registered root, never a correctness
		// issue.
		*self.search.lock().await = Some(ActiveSearch {
			root: root_uuid,
			search,
			window,
		});
		Ok(entries)
	}
}

/// Reads the (post-filtered) result page for a query. The engine matches name + item type only, so
/// `mime`/`size`/`modified` are applied here via [`passes_post_filter`]. Since that can drop
/// matches out of the engine's first window, when such a filter is active we page past it (`total`
/// counts pre-post-filter matches) until [`SEARCH_RESULT_LIMIT`] entries are collected or the
/// matches run out — otherwise a filtered query would silently return only what survived the first
/// window. Probe pages use a no-op callback; the live subscription rides on the caller's first
/// window.
async fn collect_filtered(
	search: &Search,
	first: SearchSnapshot,
	root_id: &str,
	args: &SearchQueryArgs,
) -> Result<Vec<SearchQueryResponseEntry>, CacheError> {
	let total = first.total;
	let mut entries = snapshot_to_entries(first, root_id, args);
	if has_post_filter(args) && entries.len() < SEARCH_RESULT_LIMIT && total > SEARCH_RESULT_LIMIT {
		// Dedup across pages by document path (unique per item): each page is a separate read of a
		// live, still-resyncing table, so a row shifting across a page boundary could otherwise land
		// in two pages. The inverse (a newly-inserted earlier row missed by every page) self-heals on
		// the next update-driven query.
		let mut seen: HashSet<String> = entries.iter().map(|e| e.path.clone()).collect();
		let mut offset = SEARCH_RESULT_LIMIT;
		while entries.len() < SEARCH_RESULT_LIMIT && offset < total {
			let (page, window) = search
				.get_range(offset..offset + SEARCH_RESULT_LIMIT, noop_callback())
				.await?;
			drop(window);
			for entry in snapshot_to_entries(page, root_id, args) {
				if seen.insert(entry.path.clone()) {
					entries.push(entry);
				}
			}
			offset += SEARCH_RESULT_LIMIT;
		}
		entries.truncate(SEARCH_RESULT_LIMIT);
	}
	Ok(entries)
}

/// Whether the query carries a constraint the engine can't apply (mime/size/modified), so results
/// must be post-filtered — and thus paged (see [`collect_filtered`]).
fn has_post_filter(args: &SearchQueryArgs) -> bool {
	!args.mime_types.is_empty() || args.file_size_min.is_some() || args.last_modified_min.is_some()
}

/// A window callback that ignores updates, for the transient probe windows [`collect_filtered`]
/// opens while paging — only the caller's first window needs to stay subscribed.
fn noop_callback() -> SearchWindowCallback {
	Box::new(|_: SearchSnapshot| {})
}

/// Window listener that bounces snapshot notifications to the FFI callback off the engine task
/// (per the callback discipline).
fn update_callback(on_update: Arc<dyn SearchUpdateCallback>) -> SearchWindowCallback {
	Box::new(move |_snapshot: SearchSnapshot| {
		let on_update = Arc::clone(&on_update);
		tokio::task::spawn_blocking(move || on_update.on_update());
	})
}

/// Maps a window snapshot to FFI entries: post-filters by mime/size/modified and builds each
/// item's `<search root>/<parent path>/<name>` document id.
fn snapshot_to_entries(
	snapshot: SearchSnapshot,
	root_id: &str,
	args: &SearchQueryArgs,
) -> Vec<SearchQueryResponseEntry> {
	snapshot
		.results
		.into_iter()
		.filter(|hit| passes_post_filter(&hit.result, args))
		.map(|hit| SearchQueryResponseEntry {
			path: format!("{}/{}", root_id, hit.full_path()),
			object: ffi_object(hit.result),
		})
		.collect()
}

fn config_from_args(args: &SearchQueryArgs) -> SearchConfig {
	let item_type = match args.item_type {
		Some(ItemType::File) => SearchItemType::File,
		Some(ItemType::Dir) => SearchItemType::Dir,
		// Root never appears in a search query; treat as unfiltered.
		Some(ItemType::Root) | None => SearchItemType::All,
	};
	let mut config = SearchConfig::new()
		.with_item_type(item_type)
		.with_recursive(true);
	// Empty / whitespace-only needle matches everything (engine treats it as "match all").
	config.name = args.name.clone();
	config
}

/// Post-filter the engine's name+type matches by the query's mime/size/modified constraints,
/// mirroring the old server-search behaviour: files must satisfy all constraints; a directory is
/// excluded outright by any mime or size constraint, and by a `last_modified_min` its `created`
/// predates.
fn passes_post_filter(result: &SearchResult, args: &SearchQueryArgs) -> bool {
	match result {
		SearchResult::File(file) => {
			(args.mime_types.is_empty()
				|| args
					.mime_types
					.iter()
					.any(|pattern| mime_matches(&file.mime, pattern)))
				&& args.file_size_min.is_none_or(|min| file.size >= min)
				&& args
					.last_modified_min
					.is_none_or(|min| millis_at_least(file.last_modified, min))
		}
		SearchResult::Dir(dir) => {
			args.mime_types.is_empty()
				&& args.file_size_min.is_none()
				&& args
					.last_modified_min
					.is_none_or(|min| dir.created.is_some_and(|c| millis_at_least(c, min)))
		}
	}
}

fn millis_at_least(time: chrono::DateTime<chrono::Utc>, min: u64) -> bool {
	u64::try_from(time.timestamp_millis()).is_ok_and(|millis| millis >= min)
}

/// Match a mime against a query pattern that may use a single `*` wildcard (e.g. `image/*`),
/// mirroring the old SQL `LIKE` with `*`→`%`.
fn mime_matches(mime: &str, pattern: &str) -> bool {
	match pattern.split_once('*') {
		None => mime.eq_ignore_ascii_case(pattern),
		Some((prefix, suffix)) => {
			// Compare on BYTES, not char-boundary `str` slices: `mime` is arbitrary uploaded
			// metadata and may contain multi-byte UTF-8, so slicing it at offsets derived from
			// `pattern` would panic on a non-char-boundary. Mime types are ASCII in practice; a
			// non-ASCII byte simply won't ascii-eq the (ASCII) pattern.
			let mime = mime.as_bytes();
			let (prefix, suffix) = (prefix.as_bytes(), suffix.as_bytes());
			mime.len() >= prefix.len() + suffix.len()
				&& mime[..prefix.len()].eq_ignore_ascii_case(prefix)
				&& mime[mime.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
		}
	}
}

fn ffi_object(result: SearchResult) -> FfiNonRootObject {
	match result {
		SearchResult::Dir(dir) => FfiNonRootObject::Dir(ffi_dir(dir)),
		SearchResult::File(file) => FfiNonRootObject::File(ffi_file(file)),
	}
}

fn ffi_file(file: CacheableFile<'_>) -> FfiFile {
	FfiFile {
		uuid: file.uuid.to_string(),
		// Search results come from the SDK search cache, which has no re-mint history, so the stable
		// id is trivially the current uuid, and there is no server timestamp to surface here.
		stable_uuid: file.uuid.to_string(),
		parent: file.parent.to_string(),
		// Search only indexes live items, never trashed ones, so there is no original parent.
		original_parent: None,
		meta: Some(FfiFileMeta {
			name: file.name.into_owned(),
			mime: file.mime.into_owned(),
			created: file
				.created
				.map(|c| c.timestamp_millis())
				.unwrap_or_default(),
			modified: file.last_modified.timestamp_millis(),
			hash: file.hash.map(|h| h.as_ref().to_vec()),
		}),
		size: file.size as i64,
		favorite_rank: i64::from(file.favorited),
		timestamp: 0,
		// Search results carry no per-device local data; the browsing cache owns that.
		local_data: None,
	}
}

fn ffi_dir(dir: CacheableDir<'_>) -> FfiDir {
	FfiDir {
		uuid: dir.uuid.to_string(),
		stable_uuid: dir.uuid.to_string(),
		parent: dir.parent.to_string(),
		// Search only indexes live items, never trashed ones, so there is no original parent.
		original_parent: None,
		meta: Some(FfiDirMeta {
			name: dir.name.into_owned(),
			created: dir.created.map(|c| c.timestamp_millis()),
		}),
		color: dir.color.into(),
		favorite_rank: i64::from(dir.favorited),
		timestamp: 0,
		last_listed: 0,
		local_data: None,
	}
}

#[cfg(test)]
mod tests {
	use super::{has_post_filter, millis_at_least, mime_matches};
	use crate::ffi::SearchQueryArgs;

	fn args() -> SearchQueryArgs {
		SearchQueryArgs {
			name: Some("x".into()),
			item_type: None,
			exclude_media_on_device: false,
			mime_types: Vec::new(),
			file_size_min: None,
			last_modified_min: None,
		}
	}

	#[test]
	fn has_post_filter_only_for_engine_unaware_constraints() {
		// A plain name/type query needs no paging: the engine already applied everything.
		assert!(!has_post_filter(&args()));
		// Each of mime/size/modified is post-filtered, so it can drop name-matches -> must page.
		assert!(has_post_filter(&SearchQueryArgs {
			mime_types: vec!["image/*".into()],
			..args()
		}));
		assert!(has_post_filter(&SearchQueryArgs {
			file_size_min: Some(1),
			..args()
		}));
		assert!(has_post_filter(&SearchQueryArgs {
			last_modified_min: Some(1),
			..args()
		}));
	}

	#[test]
	fn mime_matches_exact_and_wildcard() {
		// exact (case-insensitive)
		assert!(mime_matches("image/png", "image/png"));
		assert!(mime_matches("image/png", "IMAGE/PNG"));
		assert!(!mime_matches("image/png", "image/jpeg"));
		// wildcards mirroring the old SQL LIKE ('*' → '%')
		assert!(mime_matches("image/png", "image/*"));
		assert!(mime_matches("image/png", "*/png"));
		assert!(mime_matches("image/png", "*"));
		assert!(!mime_matches("text/plain", "image/*"));
		// pattern longer than the mime must not panic on the slice
		assert!(!mime_matches("im", "image/*"));
	}

	#[test]
	fn mime_matches_non_ascii_does_not_panic() {
		// `mime` is arbitrary uploaded metadata: a multi-byte char at a byte offset the pattern's
		// prefix/suffix lands inside must NOT panic (regression: byte-boundary str slice).
		assert!(!mime_matches("é/json", "a*json")); // prefix slice lands inside 'é'
		assert!(!mime_matches("é/json", "*/png"));
		assert!(mime_matches("é/json", "*json")); // suffix still matches on bytes
		assert!(mime_matches("💾/x", "*"));
	}

	#[test]
	fn millis_at_least_handles_epoch_and_negative() {
		let t = chrono::DateTime::from_timestamp_millis(1_700_000_000_000).unwrap();
		assert!(millis_at_least(t, 1_699_999_999_999));
		assert!(millis_at_least(t, 1_700_000_000_000));
		assert!(!millis_at_least(t, 1_700_000_000_001));
		// pre-epoch time can never satisfy a u64 minimum
		let pre = chrono::DateTime::from_timestamp_millis(-1).unwrap();
		assert!(!millis_at_least(pre, 0));
	}
}

use uuid::Uuid;

use crate::fs::{dir::cache::CacheableDir, file::cache::CacheableFile};

/// One search hit, carrying the full cached payload — the same types the cache's event dispatch
/// exposes — so a result is directly actionable (a [`CacheableFile`] includes its `FileKey`)
/// without a second lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchResult {
	Dir(CacheableDir<'static>),
	File(CacheableFile<'static>),
}

impl SearchResult {
	pub fn uuid(&self) -> Uuid {
		match self {
			Self::Dir(dir) => dir.uuid,
			Self::File(file) => file.uuid,
		}
	}

	pub fn parent(&self) -> Uuid {
		match self {
			Self::Dir(dir) => dir.parent,
			Self::File(file) => file.parent,
		}
	}

	pub fn name(&self) -> &str {
		match self {
			Self::Dir(dir) => &dir.name,
			Self::File(file) => &file.name,
		}
	}

	pub fn is_dir(&self) -> bool {
		matches!(self, Self::Dir(_))
	}
}

/// A materialized view of one window: the window's FULL fresh contents plus the total match
/// count — never a delta. Snapshots are ephemeral full replacements; treat each delivery as the
/// window's new truth.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SearchSnapshot {
	/// The window's current contents (name-ascending, directories first, ties broken by uuid).
	pub results: Vec<SearchResult>,
	/// Total matches across the WHOLE result set, not just this window. Any total change marks
	/// every window dirty, so a delivered total never goes silently stale.
	pub total: usize,
	/// `false` is TERMINAL (fired at most once per window): the cache stopped feeding this
	/// search — either the search root was deleted server-side, or the cache worker stopped
	/// ([`flush_cache`](crate::auth::Client::flush_cache) or a failure). The final fire carries
	/// the window's LAST-DELIVERED results. Disambiguating the cause via
	/// [`CacheMessage::SyncRootsDeleted`](crate::cache::CacheMessage::SyncRootsDeleted) +
	/// [`Search::root_uuid`](super::Search::root_uuid) is BEST-EFFORT (that message can be
	/// dropped under load); re-creating the search is the definitive probe — a deleted root is
	/// rejected with [`CacheError::InvalidSyncRoot`](crate::cache::CacheError::InvalidSyncRoot).
	pub live: bool,
}

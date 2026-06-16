use uuid::Uuid;

use crate::fs::{dir::cache::CacheableDir, file::cache::CacheableFile};

/// One matched item, carrying the full cached payload — the same types the cache's event
/// dispatch exposes — so a result is directly actionable (a [`CacheableFile`] includes its
/// `FileKey`) without a second lookup. Paired with its parent path relative to the search root
/// in a [`SearchHit`].
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

/// One search hit: the matched [`SearchResult`] plus its [`parent_path`](Self::parent_path)
/// relative to the search root — the `/`-joined chain of ancestor directory names from the search
/// root (EXCLUSIVE) down to the item's parent (INCLUSIVE). A direct child of the search root has
/// an empty parent path; the item's own name is NOT part of it ([`full_path`](Self::full_path)
/// appends it). The parent path is computed from cached ancestry at query time, so it tracks
/// ancestor renames/moves.
///
/// The joined form assumes item names do not themselves contain `/`: cache names are not
/// validated on ingest, but a slash in a name is out of contract and would make the joined parent
/// path ambiguous.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
	/// The matched item.
	pub result: SearchResult,
	/// The item's parent path relative to the search root (see the type docs). Empty for a direct
	/// child of the search root.
	pub parent_path: Box<str>,
}

impl SearchHit {
	/// The item's parent path relative to the search root: ancestor directory names from the root
	/// (exclusive) down to the parent (inclusive), `/`-joined. Empty for a direct child of the
	/// search root.
	pub fn parent_path(&self) -> &str {
		&self.parent_path
	}

	/// The item's full path relative to the search root: [`parent_path`](Self::parent_path) plus
	/// the item's own name (just the name for a direct child of the root).
	pub fn full_path(&self) -> String {
		if self.parent_path.is_empty() {
			self.result.name().to_string()
		} else {
			format!("{}/{}", self.parent_path, self.result.name())
		}
	}
}

/// A materialized view of one window: the window's FULL fresh contents plus the total match
/// count — never a delta. Snapshots are ephemeral full replacements; treat each delivery as the
/// window's new truth.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SearchSnapshot {
	/// The window's current contents (name-ascending, directories first, ties broken by uuid),
	/// each paired with its [`parent_path`](SearchHit::parent_path) relative to the search root.
	pub results: Vec<SearchHit>,
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

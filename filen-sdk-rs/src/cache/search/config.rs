use unicode_normalization::UnicodeNormalization;

/// Filter/shape configuration for a cache-backed search (see
/// [`Client::create_search`](crate::auth::Client::create_search)). Construct via
/// [`SearchConfig::new`] / [`Default`] plus the chainable `with_*` setters; the struct is
/// `#[non_exhaustive]` so future filters (size, mime, date, more sort orders, …) slot in without
/// a breaking change. v1 sorts by name ascending (directories first, ties broken by uuid).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SearchConfig {
	/// Substring match on item names. The needle is Unicode-normalized (trim + NFC) and — unless
	/// [`case_sensitive`](Self::case_sensitive) — matched case-insensitively with full Unicode
	/// case folding. Cached names are ASSUMED to already be NFC-normalized (see the
	/// [module docs](super)). `None` (or a needle that normalizes to the empty string) matches
	/// everything.
	pub name: Option<String>,
	/// Which item kinds appear in the results. Defaults to [`SearchItemType::All`].
	pub item_type: SearchItemType,
	/// `true` (default): match the whole subtree under the search root; `false`: direct
	/// children only — which makes the search double as a live, sorted directory listing.
	pub recursive: bool,
	/// `false` (default): names match case-insensitively (full Unicode case folding). `true`:
	/// byte-exact substring match (after the needle's trim + NFC normalization).
	pub case_sensitive: bool,
}

impl Default for SearchConfig {
	fn default() -> Self {
		Self {
			name: None,
			item_type: SearchItemType::All,
			recursive: true,
			case_sensitive: false,
		}
	}
}

impl SearchConfig {
	pub fn new() -> Self {
		Self::default()
	}

	pub fn with_name(mut self, name: impl Into<String>) -> Self {
		self.name = Some(name.into());
		self
	}

	pub fn with_item_type(mut self, item_type: SearchItemType) -> Self {
		self.item_type = item_type;
		self
	}

	pub fn with_recursive(mut self, recursive: bool) -> Self {
		self.recursive = recursive;
		self
	}

	pub fn with_case_sensitive(mut self, case_sensitive: bool) -> Self {
		self.case_sensitive = case_sensitive;
		self
	}
}

/// Which item kinds a search returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchItemType {
	#[default]
	All,
	File,
	Dir,
}

/// `SearchConfig` compiled into the engine's query-binding form: the needle normalized once
/// (trim + NFC, then lowercased unless case-sensitive — the SQL matcher expects a PRE-FOLDED
/// needle), an empty/absent needle collapsed to `None`.
#[derive(Debug, Clone)]
pub(super) struct CompiledFilter {
	pub(super) needle: Option<String>,
	pub(super) item_type: SearchItemType,
	pub(super) recursive: bool,
	pub(super) case_sensitive: bool,
}

impl CompiledFilter {
	pub(super) fn compile(config: &SearchConfig) -> Self {
		let needle = config
			.name
			.as_deref()
			.map(|name| {
				let normalized: String = name.trim().nfc().collect();
				if config.case_sensitive {
					normalized
				} else {
					normalized.to_lowercase()
				}
			})
			.filter(|needle| !needle.is_empty());
		Self {
			needle,
			item_type: config.item_type,
			recursive: config.recursive,
			case_sensitive: config.case_sensitive,
		}
	}
}

//! Paths inside the sync root, as the core names local items: one validated name per component,
//! compared as NTFS compares them.

use std::{fmt, path::Path};

use crate::names::{NameRejection, PlaceholderName};

/// A path relative to the sync root, empty for the root itself: each component a name Windows
/// holds under its exact spelling. Two paths that differ only in case name one item on NTFS; their
/// [`CaseKey`]s are equal.
#[derive(Clone, PartialEq, Eq)]
pub struct RootPath(Box<[PlaceholderName]>);

/// The key two root paths that name one NTFS item share: each component's case key, joined by a
/// separator no name holds. Opaque: only compared and hashed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CaseKey(String);

/// Why a local path is not one the core can name. Such an item is skipped and reported, never
/// synced (the default for the open decision on names Windows cannot hold).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RootPathError {
	/// A component is not valid Unicode (an unpaired surrogate on NTFS).
	#[error("a path component is not valid Unicode")]
	NotUnicode,
	/// A component is `.`, `..`, a prefix or the root: not a plain name.
	#[error("the path holds a component that is not a name")]
	NotAName,
	/// A component is not a name Windows holds under its spelling.
	#[error("{component:?}: {rejection}")]
	Name {
		component: String,
		rejection: NameRejection,
	},
}

impl RootPath {
	/// The sync root itself.
	pub fn root() -> Self {
		Self(Box::new([]))
	}

	/// The path of `components`, outermost first, each checked as [`PlaceholderName::parse`] does.
	pub fn parse<'a>(components: impl IntoIterator<Item = &'a str>) -> Result<Self, RootPathError> {
		components
			.into_iter()
			.map(|component| {
				PlaceholderName::parse(component).map_err(|rejection| RootPathError::Name {
					component: component.to_owned(),
					rejection,
				})
			})
			.collect::<Result<Vec<_>, _>>()
			.map(|names| Self(names.into_boxed_slice()))
	}

	/// The path of `relative`, a path below the sync root with plain components only.
	pub fn from_relative(relative: &Path) -> Result<Self, RootPathError> {
		let components = relative
			.components()
			.map(|component| match component {
				std::path::Component::Normal(name) => {
					name.to_str().ok_or(RootPathError::NotUnicode)
				}
				_ => Err(RootPathError::NotAName),
			})
			.collect::<Result<Vec<_>, _>>()?;
		Self::parse(components)
	}

	/// Whether this is the sync root.
	pub fn is_root(&self) -> bool {
		self.0.is_empty()
	}

	/// The components, outermost first.
	pub fn components(&self) -> &[PlaceholderName] {
		&self.0
	}

	/// How many components the path has; 0 for the root.
	pub fn depth(&self) -> usize {
		self.0.len()
	}

	/// The last component; `None` for the root.
	pub fn name(&self) -> Option<&PlaceholderName> {
		self.0.last()
	}

	/// The directory holding the item; `None` for the root.
	pub fn parent(&self) -> Option<Self> {
		let (_, parent) = self.0.split_last()?;
		Some(Self(Box::from(parent)))
	}

	/// The child `name` of this path.
	pub fn join(&self, name: PlaceholderName) -> Self {
		let mut components = self.0.to_vec();
		components.push(name);
		Self(components.into_boxed_slice())
	}

	/// The sibling `name` of this path, in the same directory; `None` for the root.
	pub fn with_name(&self, name: PlaceholderName) -> Option<Self> {
		Some(self.parent()?.join(name))
	}

	/// Whether `ancestor` is this path or a directory above it, in NTFS's case folding.
	pub fn starts_with(&self, ancestor: &Self) -> bool {
		ancestor.0.len() <= self.0.len()
			&& ancestor
				.0
				.iter()
				.zip(self.0.iter())
				.all(|(theirs, ours)| theirs.case_key() == ours.case_key())
	}

	/// This path moved from under `from` to under `to`, as a directory rename moves its subtree;
	/// `None` when the path is not at or under `from`.
	pub fn rebase(&self, from: &Self, to: &Self) -> Option<Self> {
		if !self.starts_with(from) {
			return None;
		}
		let rest = self.0.get(from.0.len()..)?;
		let mut components = to.0.to_vec();
		components.extend_from_slice(rest);
		Some(Self(components.into_boxed_slice()))
	}

	/// The path as the default ignore patterns match it: components joined by `/`.
	pub(crate) fn slash_joined(&self) -> String {
		self.join_with('/')
	}

	/// The key this path shares with every path naming the same NTFS item.
	pub fn case_key(&self) -> CaseKey {
		let mut key = String::new();
		for (n, name) in self.0.iter().enumerate() {
			if n > 0 {
				// No name holds a `\`, so the components stay apart.
				key.push('\\');
			}
			key.push_str(&name.case_key());
		}
		CaseKey(key)
	}

	fn join_with(&self, separator: char) -> String {
		let mut joined = String::new();
		for (n, name) in self.0.iter().enumerate() {
			if n > 0 {
				joined.push(separator);
			}
			joined.push_str(name.as_str());
		}
		joined
	}
}

impl fmt::Display for RootPath {
	/// The path as Windows spells it below the root, components joined by `\`.
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(&self.join_with('\\'))
	}
}

impl fmt::Debug for RootPath {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "RootPath({:?})", self.join_with('\\'))
	}
}

#[cfg(test)]
mod tests;

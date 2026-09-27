//! Where each archive entry goes in the drive: the directory it lands in, planned once however
//! many entries name it, and a name that is free there.
//!
//! Archives name directories by path, often only implicitly (a file `a/b/c.txt` with no entry
//! for `a/` or `a/b/`), and may name one directory in several spellings. The rules:
//! - two directory paths that differ only in case are one directory, under the first spelling;
//! - every other collision keeps both: a file and a directory, two files, or an archive entry
//!   and an item the destination already holds (an archive directory is never merged into an
//!   existing one, so extracting never mixes into the user's content).

use crate::{
	fs::name::{
		EntryNameError, ValidatedName,
		keep_both::{NameShape, TakenNames, collision_key},
	},
	util::SeededMap,
};

/// A directory the resolver planned; the root (the directory entries land in) is [`ROOT`].
pub(crate) type DirId = usize;

pub(crate) const ROOT: DirId = 0;

/// A directory planned by [`PathResolver::resolve_dirs`], to be created in `parent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannedDir {
	pub(crate) id: DirId,
	pub(crate) parent: DirId,
	pub(crate) name: ValidatedName,
	/// Whether `name` is a keep-both name rather than the one the archive gave it.
	pub(crate) renamed: bool,
}

struct Node {
	/// The names taken in this directory, by entries planned into it or items already there.
	taken: TakenNames,
	/// Subdirectories by the collision key of the name the archive gave them, which may differ
	/// from the name they are created under.
	children: SeededMap<String, DirId>,
}

pub(crate) struct PathResolver {
	dirs: Vec<Node>,
}

impl PathResolver {
	/// A resolver whose root already holds `existing` (empty for a directory the job creates).
	pub(crate) fn new<'a>(existing: impl IntoIterator<Item = &'a str>) -> Self {
		Self {
			dirs: vec![Node {
				taken: TakenNames::new(existing),
				children: SeededMap::default(),
			}],
		}
	}

	/// The directory the path `segments` (every one a directory) names, planning each one not
	/// seen before into `planned`, parents first.
	pub(crate) fn resolve_dirs(
		&mut self,
		segments: &[ValidatedName],
		planned: &mut Vec<PlannedDir>,
	) -> Result<DirId, EntryNameError> {
		let mut current = ROOT;
		for segment in segments {
			let key = collision_key(segment.as_ref());
			if let Some(&child) = self.dirs[current].children.get(&key) {
				current = child;
				continue;
			}
			let name = self.dirs[current]
				.taken
				.allocate(segment.clone(), NameShape::Dir)?;
			let id = self.dirs.len();
			self.dirs.push(Node {
				taken: TakenNames::default(),
				children: SeededMap::default(),
			});
			self.dirs[current].children.insert(key, id);
			planned.push(PlannedDir {
				id,
				parent: current,
				renamed: name.as_ref() != segment.as_ref(),
				name,
			});
			current = id;
		}
		Ok(current)
	}

	/// A free name in `dir` for a file the archive calls `name`, taken for it.
	pub(crate) fn file_name(
		&mut self,
		dir: DirId,
		name: ValidatedName,
	) -> Result<ValidatedName, EntryNameError> {
		self.dirs[dir].taken.allocate(name, NameShape::File)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn names(path: &str) -> Vec<ValidatedName> {
		path.split('/')
			.map(|s| ValidatedName::try_from(s).unwrap())
			.collect()
	}

	fn resolve(resolver: &mut PathResolver, path: &str) -> (DirId, Vec<(DirId, DirId, String)>) {
		let mut planned = Vec::new();
		let dir = resolver.resolve_dirs(&names(path), &mut planned).unwrap();
		let planned = planned
			.into_iter()
			.map(|p| (p.id, p.parent, p.name.as_ref().to_owned()))
			.collect();
		(dir, planned)
	}

	fn file(resolver: &mut PathResolver, dir: DirId, name: &str) -> String {
		resolver
			.file_name(dir, ValidatedName::try_from(name).unwrap())
			.unwrap()
			.into()
	}

	#[test]
	fn directories_are_planned_once_parents_first() {
		let mut resolver = PathResolver::new([]);
		assert_eq!(
			resolve(&mut resolver, "a/b/c"),
			(
				3,
				vec![
					(1, ROOT, "a".into()),
					(2, 1, "b".into()),
					(3, 2, "c".into())
				]
			)
		);
		assert_eq!(resolve(&mut resolver, "a/b"), (2, vec![]));
		assert_eq!(resolve(&mut resolver, "a/d"), (4, vec![(4, 1, "d".into())]));
	}

	#[test]
	fn case_variants_of_a_directory_are_one_directory() {
		let mut resolver = PathResolver::new([]);
		assert_eq!(
			resolve(&mut resolver, "Docs"),
			(1, vec![(1, ROOT, "Docs".into())])
		);
		assert_eq!(
			resolve(&mut resolver, "DOCS/x"),
			(2, vec![(2, 1, "x".into())])
		);
	}

	#[test]
	fn every_other_collision_keeps_both() {
		let mut resolver = PathResolver::new([]);
		// two files
		assert_eq!(file(&mut resolver, ROOT, "a.txt"), "a.txt");
		assert_eq!(file(&mut resolver, ROOT, "A.txt"), "A (1).txt");
		// a file, then a directory of the same name
		assert_eq!(file(&mut resolver, ROOT, "x"), "x");
		assert_eq!(
			resolve(&mut resolver, "x/y"),
			(2, vec![(1, ROOT, "x (1)".into()), (2, 1, "y".into())])
		);
		// later entries under `x/` keep going to the renamed directory
		assert_eq!(resolve(&mut resolver, "x/y"), (2, vec![]));
		// a directory, then a file of the same name
		assert_eq!(file(&mut resolver, 1, "y"), "y (1)");
	}

	#[test]
	fn items_already_in_the_destination_are_never_merged_into() {
		let mut resolver = PathResolver::new(["photos", "notes.txt"]);
		assert_eq!(
			resolve(&mut resolver, "Photos"),
			(1, vec![(1, ROOT, "Photos (1)".into())])
		);
		assert_eq!(file(&mut resolver, ROOT, "notes.txt"), "notes (1).txt");
		// but a subdirectory of a planned directory is planned as usual
		assert_eq!(
			resolve(&mut resolver, "photos/2024"),
			(2, vec![(2, 1, "2024".into())])
		);
	}

	#[test]
	fn a_rename_is_flagged() {
		let mut resolver = PathResolver::new(["a"]);
		let mut planned = Vec::new();
		resolver.resolve_dirs(&names("a"), &mut planned).unwrap();
		resolver.resolve_dirs(&names("b"), &mut planned).unwrap();
		assert_eq!(
			planned.iter().map(|p| p.renamed).collect::<Vec<_>>(),
			[true, false]
		);
	}
}

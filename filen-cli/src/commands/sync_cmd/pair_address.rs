//! Which registered sync pair a command's PAIR argument names.

use std::{
	fs,
	path::{self, Path, PathBuf},
};

use anyhow::Result;
use filen_sdk_rs::sync_engine::{PairId, PairRecord};

use super::sync_pairs::{self, SyncDirs};
use crate::ui::UI;

/// How a PAIR argument names a registered sync pair.
#[derive(Debug, PartialEq)]
enum PairAddress<'a> {
	/// By the id `list-sync-pairs` prints.
	Id(PairId),
	/// By the alias the default sync pairs file gives it.
	Alias(&'a str),
	/// By its local directory.
	Local(&'a Path),
}

impl<'a> PairAddress<'a> {
	/// How `given` names a pair: a number is an id; a word that is not the name of a file or
	/// directory in the working directory is an alias; anything else is the local directory,
	/// which `find_pair` still reads as an alias when it is a word that no pair syncs.
	fn of(given: &'a str) -> Self {
		if let Ok(id) = given.parse() {
			return Self::Id(id);
		}
		let path = Path::new(given);
		if sync_pairs::is_alias(given) && !path.exists() {
			Self::Alias(given)
		} else {
			Self::Local(path)
		}
	}
}

/// The pair of `registered` that `given` names: by its id, by its alias in the default sync pairs
/// file in `dirs`, or by its local directory, which is compared once the operating system has
/// resolved it, as the sync engine stored it. A word that names an entry of the working directory
/// is tried as that local directory first, then as an alias.
pub(super) fn find_pair(
	mut registered: Vec<PairRecord>,
	given: &str,
	dirs: &SyncDirs<'_>,
) -> Result<PairRecord> {
	match PairAddress::of(given) {
		PairAddress::Id(id) => registered
			.into_iter()
			.find(|pair| pair.id == id)
			.ok_or_else(|| UI::failure(&format!("No sync pair has the id {id}. {LIST_HINT}"))),
		PairAddress::Alias(alias) => {
			let local = sync_pairs::aliased_local(dirs, alias)?;
			pair_at(&registered, &local)
				.map(|index| registered.swap_remove(index))
				.ok_or_else(|| {
					UI::failure(&format!(
						"The sync pair {alias} ({}) is not registered: `filen sync {alias}` registers it.",
						local.display()
					))
				})
		}
		PairAddress::Local(local) => pair_at(&registered, local)
			.or_else(|| {
				// a word that is also the name of an entry here may still be meant as an alias;
				// when it names no registered pair either, the error is about the directory, which
				// is how the word was read first
				let aliased = sync_pairs::is_alias(given)
					.then(|| {
						sync_pairs::aliased_local(dirs, given)
							.inspect_err(|e| log::debug!("{given} is not a sync pair alias: {e:#}"))
							.ok()
					})
					.flatten()?;
				pair_at(&registered, &aliased)
			})
			.map(|index| registered.swap_remove(index))
			.ok_or_else(|| {
				UI::failure(&format!(
					"No sync pair syncs the local directory {}. {LIST_HINT}",
					local.display()
				))
			}),
	}
}

const LIST_HINT: &str = "`filen list-sync-pairs` lists the sync pairs.";

/// The index in `registered` of the pair whose local root is the directory `local`.
fn pair_at(registered: &[PairRecord], local: &Path) -> Option<usize> {
	let local = resolved(local);
	registered
		.iter()
		.position(|pair| resolved(Path::new(&pair.local_root)) == local)
}

/// `path` as the operating system resolves it, or, when it cannot (it no longer exists), made
/// absolute against the working directory.
pub(super) fn resolved(path: &Path) -> PathBuf {
	fs::canonicalize(path)
		.or_else(|_| path::absolute(path))
		.unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
	use std::path::Path;

	use filen_sdk_rs::sync_engine::{DeleteGuard, PairRecord, SyncMode};
	use filen_types::fs::Uuid;

	use super::{PairAddress, find_pair};
	use crate::commands::sync_cmd::sync_pairs::SyncDirs;

	fn record(id: i64, local_root: &Path) -> PairRecord {
		PairRecord {
			id,
			local_root: local_root.canonicalize().unwrap().display().to_string(),
			remote_root: Uuid::from_u128(1),
			mode: SyncMode::TwoWay,
			delete_guard: DeleteGuard::default(),
			paused: false,
			first_pass_completed: true,
		}
	}

	#[test]
	fn a_number_is_an_id_a_word_an_alias_and_a_path_a_local_directory() {
		assert_eq!(PairAddress::of("12"), PairAddress::Id(12));
		assert_eq!(
			PairAddress::of("photos-no-such-dir"),
			PairAddress::Alias("photos-no-such-dir")
		);
		assert_eq!(
			PairAddress::of("/home/me/docs"),
			PairAddress::Local(Path::new("/home/me/docs"))
		);
	}

	/// A word that names an entry of the working directory is read as that path first.
	#[test]
	fn a_word_that_names_an_existing_entry_is_a_path() {
		// the crate's directory, which `cargo test` runs in
		assert_eq!(PairAddress::of("src"), PairAddress::Local(Path::new("src")));
	}

	#[test]
	fn a_pair_is_found_by_its_id() {
		let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
		let config = tempfile::tempdir().unwrap();
		let dirs = SyncDirs::new(config.path(), false, None);

		let found = find_pair(vec![record(1, a.path()), record(2, b.path())], "2", &dirs);

		assert_eq!(found.unwrap().id, 2);
	}

	#[test]
	fn an_unknown_id_says_how_to_list_the_pairs() {
		let a = tempfile::tempdir().unwrap();
		let config = tempfile::tempdir().unwrap();
		let dirs = SyncDirs::new(config.path(), false, None);

		let error = find_pair(vec![record(1, a.path())], "7", &dirs).unwrap_err();

		assert_eq!(
			error.to_string(),
			"No sync pair has the id 7. `filen list-sync-pairs` lists the sync pairs."
		);
	}

	/// The local directory is compared once resolved, so a path through `..` finds the pair.
	#[test]
	fn a_pair_is_found_by_its_local_directory() {
		let parent = tempfile::tempdir().unwrap();
		let a = parent.path().join("a");
		std::fs::create_dir(&a).unwrap();
		let config = tempfile::tempdir().unwrap();
		let dirs = SyncDirs::new(config.path(), false, None);
		let given = a.join("..").join("a");

		let found = find_pair(vec![record(4, &a)], given.to_str().unwrap(), &dirs);

		assert_eq!(found.unwrap().id, 4);
	}

	#[test]
	fn a_directory_no_pair_syncs_is_named() {
		let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
		let config = tempfile::tempdir().unwrap();
		let dirs = SyncDirs::new(config.path(), false, None);
		let given = b.path().to_str().unwrap();

		let error = find_pair(vec![record(1, a.path())], given, &dirs).unwrap_err();

		assert_eq!(
			error.to_string(),
			format!(
				"No sync pair syncs the local directory {given}. `filen list-sync-pairs` lists the \
				 sync pairs."
			)
		);
	}

	#[test]
	fn a_pair_is_found_by_its_alias_in_the_default_sync_pairs_file() {
		let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
		let config = tempfile::tempdir().unwrap();
		std::fs::write(
			config.path().join("syncPairs.yaml"),
			format!(
				"syncPairs:\n  - local: {}\n    remote: /a\n    alias: photos-alias\n  - local: {}\n    \
				 remote: /b\n    alias: docs-alias\n",
				a.path().display(),
				b.path().display()
			),
		)
		.unwrap();
		let dirs = SyncDirs::new(config.path(), false, None);

		let found = find_pair(
			vec![record(1, a.path()), record(2, b.path())],
			"docs-alias",
			&dirs,
		);

		assert_eq!(found.unwrap().id, 2);
	}

	/// A word that names a directory no pair syncs is looked up as an alias next.
	#[test]
	fn a_word_that_names_a_directory_no_pair_syncs_is_found_by_its_alias() {
		let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
		let config = tempfile::tempdir().unwrap();
		std::fs::write(
			config.path().join("syncPairs.yaml"),
			format!(
				"syncPairs:\n  - local: {}\n    remote: /b\n    alias: src\n",
				b.path().display()
			),
		)
		.unwrap();
		let dirs = SyncDirs::new(config.path(), false, None);

		// `src` is the crate's directory, which `cargo test` runs in, and no pair syncs it
		let found = find_pair(vec![record(1, a.path()), record(2, b.path())], "src", &dirs);

		assert_eq!(found.unwrap().id, 2);
	}

	/// A word that names a directory no pair syncs, and no aliased pair either, is reported as
	/// that directory.
	#[test]
	fn a_word_that_names_neither_a_paired_directory_nor_an_alias_names_the_directory() {
		let a = tempfile::tempdir().unwrap();
		let config = tempfile::tempdir().unwrap();
		let dirs = SyncDirs::new(config.path(), false, None);

		let error = find_pair(vec![record(1, a.path())], "src", &dirs).unwrap_err();

		assert_eq!(
			error.to_string(),
			"No sync pair syncs the local directory src. `filen list-sync-pairs` lists the sync \
			 pairs."
		);
	}

	#[test]
	fn an_alias_whose_pair_is_not_registered_says_how_to_register_it() {
		let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
		let config = tempfile::tempdir().unwrap();
		std::fs::write(
			config.path().join("syncPairs.yaml"),
			format!(
				"syncPairs:\n  - local: {}\n    remote: /b\n    alias: docs-alias\n",
				b.path().display()
			),
		)
		.unwrap();
		let dirs = SyncDirs::new(config.path(), false, None);

		let error = find_pair(vec![record(1, a.path())], "docs-alias", &dirs).unwrap_err();

		assert_eq!(
			error.to_string(),
			format!(
				"The sync pair docs-alias ({}) is not registered: `filen sync docs-alias` registers \
				 it.",
				b.path().display()
			)
		);
	}

	#[test]
	fn an_unknown_alias_is_a_usage_error() {
		let a = tempfile::tempdir().unwrap();
		let config = tempfile::tempdir().unwrap();
		std::fs::write(
			config.path().join("syncPairs.yaml"),
			format!(
				"syncPairs:\n  - local: {}\n    remote: /a\n    alias: docs-alias\n",
				a.path().display()
			),
		)
		.unwrap();
		let dirs = SyncDirs::new(config.path(), false, None);

		let error = find_pair(vec![record(1, a.path())], "nope-alias", &dirs).unwrap_err();

		assert_eq!(error.to_string(), "Unknown sync pair alias: nope-alias");
	}
}

use anyhow::Result;
use filen_sdk_rs::sync_engine::ConflictResolution;

use crate::ui::UI;

/// The paths given to `--keep-local`, `--keep-remote` and `--keep-both`, and which side each keeps.
///
/// A path is relative to the root of a sync pair, and applies to every pair of the run. It covers
/// the conflicts at that path and under it as a directory: `dir` covers `dir/f`, while `a.txt` does
/// not cover `a.txt.bak`. Where two paths cover a conflict (`dir` and `dir/f`), the longer one
/// decides. `.` covers the whole pair; an empty path, or any other path that names the pair's root
/// (`/`, `./`), is refused, so an unset shell variable does not resolve every conflict to one side.
pub(crate) struct ConflictResolutions(Vec<(String, ConflictResolution)>);

impl ConflictResolutions {
	/// Fails when a path is empty or names the pair's root other than as `.`, or when the same path
	/// is given to two different flags.
	pub(crate) fn new(
		keep_local: Vec<String>,
		keep_remote: Vec<String>,
		keep_both: Vec<String>,
	) -> Result<Self> {
		let given = keep_local
			.into_iter()
			.map(|path| (path, ConflictResolution::KeepLocal))
			.chain(
				keep_remote
					.into_iter()
					.map(|path| (path, ConflictResolution::KeepRemote)),
			)
			.chain(
				keep_both
					.into_iter()
					.map(|path| (path, ConflictResolution::KeepBoth)),
			);
		let mut resolutions: Vec<(String, ConflictResolution)> = Vec::new();
		for (given_path, resolution) in given {
			let path = normalize(&given_path);
			// Only a literal "." covers the whole pair: "/", "./" or "$DIR/" with `DIR` unset also
			// normalize to the root, and are refused like the empty path.
			if path.is_empty() && given_path != "." {
				return Err(UI::failure(&format!(
					"{} was given an empty path; give \".\" to cover the whole sync pair",
					flag_name(resolution)
				)));
			}
			match resolutions.iter().find(|(known, _)| *known == path) {
				Some((_, known)) if *known == resolution => {}
				Some((_, known)) => {
					return Err(UI::failure(&format!(
						"\"{given_path}\" is given to both {} and {}; give each path to one of them",
						flag_name(*known),
						flag_name(resolution)
					)));
				}
				None => resolutions.push((path, resolution)),
			}
		}
		Ok(Self(resolutions))
	}

	/// How to resolve a conflict at `rel_path`, a path relative to its pair's root as the engine
	/// reports it, if a flag covers it.
	pub(crate) fn for_path(&self, rel_path: &str) -> Option<ConflictResolution> {
		self.0
			.iter()
			.filter(|(path, _)| covers(path, rel_path))
			.max_by_key(|(path, _)| path.len())
			.map(|(_, resolution)| *resolution)
	}
}

/// `path` as the engine writes a relative path: `/`-separated, with no leading `./` or `/` and
/// no trailing `/`. The pair's root itself is the empty path.
pub(super) fn normalize(path: &str) -> String {
	let path = if cfg!(windows) {
		path.replace('\\', "/")
	} else {
		path.to_owned()
	};
	let mut path = path.as_str();
	loop {
		let trimmed = path.trim_start_matches('/');
		let trimmed = trimmed.strip_prefix("./").unwrap_or(trimmed);
		if trimmed == path {
			break;
		}
		path = trimmed;
	}
	let path = path.trim_end_matches('/');
	if path == "." { "" } else { path }.to_owned()
}

/// Whether `path` (normalized) is `rel_path` or one of its parent directories.
fn covers(path: &str, rel_path: &str) -> bool {
	path.is_empty()
		|| rel_path
			.strip_prefix(path)
			.is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

fn flag_name(resolution: ConflictResolution) -> &'static str {
	match resolution {
		ConflictResolution::KeepLocal => "--keep-local",
		ConflictResolution::KeepRemote => "--keep-remote",
		ConflictResolution::KeepBoth => "--keep-both",
	}
}

#[cfg(test)]
mod tests {
	use filen_sdk_rs::sync_engine::ConflictResolution;

	use super::ConflictResolutions;

	fn keep_local(paths: &[&str]) -> ConflictResolutions {
		ConflictResolutions::new(
			paths.iter().map(|path| path.to_string()).collect(),
			Vec::new(),
			Vec::new(),
		)
		.unwrap()
	}

	#[test]
	fn a_directory_covers_the_conflicts_under_it() {
		let resolutions = keep_local(&["dir"]);

		assert_eq!(
			resolutions.for_path("dir/f.txt"),
			Some(ConflictResolution::KeepLocal)
		);
		assert_eq!(
			resolutions.for_path("dir/sub/f.txt"),
			Some(ConflictResolution::KeepLocal)
		);
	}

	#[test]
	fn a_path_covers_a_conflict_at_that_path() {
		assert_eq!(
			keep_local(&["a.txt"]).for_path("a.txt"),
			Some(ConflictResolution::KeepLocal)
		);
	}

	#[test]
	fn a_path_does_not_cover_a_sibling_it_is_a_prefix_of() {
		let resolutions = keep_local(&["a.txt", "dir"]);

		assert_eq!(resolutions.for_path("a.txt.bak"), None);
		assert_eq!(resolutions.for_path("dir2/f.txt"), None);
	}

	#[test]
	fn a_path_does_not_cover_its_parent_directory() {
		assert_eq!(keep_local(&["dir/f.txt"]).for_path("dir"), None);
	}

	#[test]
	fn leading_and_trailing_slashes_and_dot_segments_are_ignored() {
		let resolutions = keep_local(&["./dir/", "/b.txt"]);

		assert_eq!(
			resolutions.for_path("dir/f.txt"),
			Some(ConflictResolution::KeepLocal)
		);
		assert_eq!(
			resolutions.for_path("b.txt"),
			Some(ConflictResolution::KeepLocal)
		);
	}

	#[test]
	fn the_pair_root_covers_every_conflict() {
		assert_eq!(
			keep_local(&["."]).for_path("any/f.txt"),
			Some(ConflictResolution::KeepLocal)
		);
	}

	#[test]
	fn the_longer_of_two_covering_paths_decides() {
		let resolutions = ConflictResolutions::new(
			vec!["dir".to_string()],
			vec!["dir/sub".to_string()],
			Vec::new(),
		)
		.unwrap();

		assert_eq!(
			resolutions.for_path("dir/sub/f.txt"),
			Some(ConflictResolution::KeepRemote)
		);
		assert_eq!(
			resolutions.for_path("dir/f.txt"),
			Some(ConflictResolution::KeepLocal)
		);
	}

	#[test]
	fn a_path_given_to_two_flags_is_an_error() {
		let error = ConflictResolutions::new(
			vec!["dir/f.txt".to_string()],
			Vec::new(),
			vec!["dir/f.txt/".to_string()],
		)
		.err()
		.unwrap();

		assert!(
			error.to_string().ends_with(
				"\"dir/f.txt/\" is given to both --keep-local and --keep-both; give each path \
				 to one of them"
			),
			"{error}"
		);
	}

	#[test]
	fn an_empty_path_is_an_error() {
		let error = ConflictResolutions::new(Vec::new(), vec![String::new()], Vec::new())
			.err()
			.unwrap();

		assert!(
			error.to_string().ends_with(
				"--keep-remote was given an empty path; give \".\" to cover the whole sync pair"
			),
			"{error}"
		);
	}

	#[test]
	fn a_path_that_names_the_pair_root_other_than_as_a_dot_is_an_error() {
		// "/" is also what "$DIR/" gives with `DIR` unset.
		for path in ["/", "./", "/.", "//"] {
			let error = ConflictResolutions::new(vec![path.to_string()], Vec::new(), Vec::new())
				.err()
				.unwrap_or_else(|| panic!("{path:?} was accepted"));

			assert!(
				error.to_string().ends_with(
					"--keep-local was given an empty path; give \".\" to cover the whole sync pair"
				),
				"{path:?}: {error}"
			);
		}
	}

	#[test]
	fn a_path_given_twice_to_the_same_flag_is_not_an_error() {
		assert_eq!(
			keep_local(&["dir", "dir/"]).for_path("dir/f.txt"),
			Some(ConflictResolution::KeepLocal)
		);
	}
}

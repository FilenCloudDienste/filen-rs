use std::{
	cell::RefCell,
	os::unix::fs::{PermissionsExt, symlink},
};

use uuid::Uuid;

use super::*;

/// One yielded item, with every path relative to the base of the tree, as both walks are compared.
#[derive(Debug, PartialEq, Eq)]
enum Seen {
	Entry {
		path: PathBuf,
		depth: usize,
		is_dir: bool,
		is_file: bool,
		is_symlink: bool,
		path_is_symlink: bool,
		metadata_is_dir: Option<bool>,
	},
	Error {
		path: Option<PathBuf>,
		loop_ancestor: Option<PathBuf>,
		kind: Option<io::ErrorKind>,
	},
}

fn relative(root: &Path, path: &Path) -> PathBuf {
	path.strip_prefix(root).unwrap_or(path).to_owned()
}

/// A tree with what the scan meets: nested and deep directories, a link to a directory inside
/// the root and one outside it, a loop, a dangling link, a link to a file, a link to a
/// directory that cannot be read, an unreadable directory, an unreadable rule file, and one
/// directory (`gone`) the walks remove as they reach it. Directories named `skip*` are skipped
/// by the caller, one of them unreadable.
fn build(base: &Path) {
	let root = base.join("root");
	let outside = base.join("outside");
	fs::create_dir_all(outside.join("o")).unwrap();
	fs::write(outside.join("o").join("f.txt"), b"o").unwrap();
	let mut deep = root.join("deep");
	for level in 0..14 {
		deep = deep.join(format!("l{level}"));
	}
	fs::create_dir_all(&deep).unwrap();
	fs::write(deep.join("bottom.txt"), b"b").unwrap();
	for dir in [
		"a/b/c",
		"real/sub",
		"skip_me/x",
		"skip_locked",
		"locked",
		"gone/g",
	] {
		fs::create_dir_all(root.join(dir)).unwrap();
	}
	for (index, file) in [
		"a/1.txt",
		"a/b/2.txt",
		"a/b/c/3.txt",
		"real/sub/r.txt",
		"skip_me/x/s.txt",
		"gone/g/z.txt",
	]
	.iter()
	.enumerate()
	{
		fs::write(root.join(file), index.to_string()).unwrap();
	}
	for wide in 0..40 {
		fs::write(root.join("a").join(format!("w{wide:02}")), b"w").unwrap();
	}
	fs::write(root.join("a").join(".filenignore"), b"*.tmp").unwrap();
	symlink(root.join("real"), root.join("alias")).unwrap();
	symlink("sub", root.join("real").join("inner")).unwrap();
	symlink(&outside, root.join("away")).unwrap();
	symlink(root.join("nowhere"), root.join("dangling")).unwrap();
	symlink(root.join("a").join("1.txt"), root.join("file_link")).unwrap();
	symlink("..", root.join("a").join("b").join("up")).unwrap();
	symlink(root.join("locked"), root.join("to_locked")).unwrap();
	fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
	fs::set_permissions(root.join("skip_locked"), fs::Permissions::from_mode(0o000)).unwrap();
	fs::set_permissions(
		root.join("a").join(".filenignore"),
		fs::Permissions::from_mode(0o000),
	)
	.unwrap();
	// Root reads them anyway, and both walks would then agree without covering them.
	assert!(
		fs::read_dir(root.join("locked")).is_err(),
		"run as non-root: the unreadable cases need permissions enforced"
	);
}

/// A fresh directory under the system temp dir, removed with everything under it when dropped,
/// so a failed assertion leaves nothing behind, unreadable directories included.
struct TempRoot(PathBuf);

impl TempRoot {
	fn new() -> Self {
		let path = std::env::temp_dir().join(format!("filen_walk_test_{}", Uuid::new_v4()));
		fs::create_dir_all(&path).unwrap();
		Self(path)
	}
}

impl std::ops::Deref for TempRoot {
	type Target = Path;

	fn deref(&self) -> &Path {
		&self.0
	}
}

impl Drop for TempRoot {
	fn drop(&mut self) {
		unlock(&self.0);
		fs::remove_dir_all(&self.0).ok();
	}
}

/// Make every directory under `path` readable again, so that it can be removed.
fn unlock(path: &Path) {
	if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir()) {
		fs::set_permissions(path, fs::Permissions::from_mode(0o755)).ok();
		for entry in fs::read_dir(path).into_iter().flatten().flatten() {
			unlock(&entry.path());
		}
	}
}

/// What a walk over `start` yields, skipping `skip*` and removing `gone` once it is handed over.
fn walk_with<E>(
	root: &Path,
	mut next: impl FnMut() -> Option<Result<Seen, E>>,
	mut skip: impl FnMut(),
	error: impl Fn(E) -> Seen,
) -> Vec<Seen> {
	let mut seen = Vec::new();
	while let Some(item) = next() {
		let item = item.unwrap_or_else(&error);
		if let Seen::Entry { path, is_dir, .. } = &item {
			let name = path.file_name().and_then(OsStr::to_str).unwrap_or("");
			if *is_dir && name.starts_with("skip") {
				skip();
			}
			if name == "gone" {
				fs::remove_dir_all(root.join(path)).unwrap();
			}
		}
		seen.push(item);
	}
	seen
}

fn ours(root: &Path, start: &Path) -> Vec<Seen> {
	let walk = RefCell::new(Walk::new(start));
	walk_with(
		root,
		|| {
			walk.borrow_mut().next().map(|item| {
				item.map(|entry| Seen::Entry {
					path: relative(root, entry.path()),
					depth: entry.depth(),
					is_dir: entry.file_type().is_dir(),
					is_file: entry.file_type().is_file(),
					is_symlink: entry.file_type().is_symlink(),
					path_is_symlink: entry.path_is_symlink(),
					metadata_is_dir: entry.metadata().ok().map(|m| m.is_dir()),
				})
			})
		},
		|| walk.borrow_mut().skip_current_dir(),
		|err| Seen::Error {
			path: err.path().map(|path| relative(root, path)),
			loop_ancestor: match &err {
				WalkError::Loop { ancestor, .. } => Some(relative(root, ancestor.as_path())),
				WalkError::Io { .. } => None,
			},
			kind: match err {
				WalkError::Io { source, .. } => Some(source.kind()),
				WalkError::Loop { .. } => None,
			},
		},
	)
}

fn walkdirs(root: &Path, start: &Path) -> Vec<Seen> {
	let walk = RefCell::new(walkdir::WalkDir::new(start).follow_links(true).into_iter());
	walk_with(
		root,
		|| {
			walk.borrow_mut().next().map(|item| {
				item.map(|entry| Seen::Entry {
					path: relative(root, entry.path()),
					depth: entry.depth(),
					is_dir: entry.file_type().is_dir(),
					is_file: entry.file_type().is_file(),
					is_symlink: entry.file_type().is_symlink(),
					path_is_symlink: entry.path_is_symlink(),
					metadata_is_dir: entry.metadata().ok().map(|m| m.is_dir()),
				})
			})
		},
		|| walk.borrow_mut().skip_current_dir(),
		|err| Seen::Error {
			path: err.path().map(|path| relative(root, path)),
			loop_ancestor: err.loop_ancestor().map(|ancestor| relative(root, ancestor)),
			kind: err.into_io_error().map(|source| source.kind()),
		},
	)
}

/// What `walkdir` yields where this walk yields `seen`. The two differ only where `walkdir` decides
/// at hand-over what this walk decides at the descent (see the module docs). A followed
/// directory this walk hands over and then fails the loop check of is, in `walkdir`, the check's
/// error alone in place of the entry; none of the directories that fail it here are skipped, so
/// `walkdir` reports the same error (one that is skipped is where the two disagree outright, see
/// `a_followed_directory_the_caller_skips_is_never_opened`). And `gone`, removed once handed
/// over, was open in `walkdir` already, which then lists nothing from it; here it is opened after
/// its removal, and its listing fails. A handle the check cannot open on a followed directory
/// is an error at its path here and one with no path in `walkdir`.
fn as_walkdir_orders_it(seen: Vec<Seen>) -> Vec<Seen> {
	let mut ordered: Vec<Seen> = Vec::with_capacity(seen.len());
	for item in seen {
		let item = match item {
			Seen::Error {
				path: Some(path),
				loop_ancestor: None,
				kind,
			} if matches!(
				ordered.last(),
				Some(Seen::Entry {
					path: followed,
					is_dir: true,
					path_is_symlink: true,
					..
				}) if *followed == path
			) =>
			{
				Seen::Error {
					path: None,
					loop_ancestor: None,
					kind,
				}
			}
			item => item,
		};
		if let Seen::Error {
			path: Some(path),
			loop_ancestor: None,
			kind: Some(io::ErrorKind::NotFound),
		} = &item && path.file_name() == Some(OsStr::new("gone"))
		{
			continue;
		}
		let failed_check = matches!(
			item,
			Seen::Error { path: None, .. }
				| Seen::Error {
					loop_ancestor: Some(_),
					..
				}
		);
		if failed_check
			&& matches!(
				ordered.last(),
				Some(Seen::Entry {
					is_dir: true,
					path_is_symlink: true,
					..
				})
			) {
			ordered.pop();
		}
		ordered.push(item);
	}
	ordered
}

/// Both walks over a fresh copy of the same tree, from `start` (relative to the copy's base).
/// Each walk lists its own copy, so this holds only where two directories built the same way
/// list in the same order: APFS, ext4 and tmpfs do; a filesystem salting its hash order per
/// directory (ZFS) need not.
fn compare(start: impl Fn(&Path) -> PathBuf) -> Vec<Seen> {
	let base = TempRoot::new();
	let (theirs_base, ours_base) = (base.join("theirs"), base.join("ours"));
	build(&theirs_base);
	build(&ours_base);
	let theirs = walkdirs(&theirs_base, &start(&theirs_base));
	let mine = as_walkdir_orders_it(ours(&ours_base, &start(&ours_base)));
	// The first item the two differ at, rather than two whole walks to compare by eye.
	let first_difference = mine
		.iter()
		.zip(&theirs)
		.position(|(mine, theirs)| mine != theirs);
	assert_eq!(
		(first_difference, mine.len()),
		(None, theirs.len()),
		"ours {:#?} walkdir {:#?}",
		first_difference.map(|at| &mine[at]),
		first_difference.map(|at| &theirs[at]),
	);
	theirs
}

#[test]
fn walks_a_tree_as_walkdir_following_links_does() {
	let seen = compare(|base| base.join("root"));
	assert!(seen.len() > 60, "the tree is walked: {seen:#?}");
	assert!(
		seen.iter().any(|seen| matches!(
			seen,
			Seen::Error {
				loop_ancestor: Some(_),
				..
			}
		)),
		"the loop is met: {seen:#?}"
	);
}

#[test]
fn walks_a_root_reached_through_a_symlink_as_walkdir_does() {
	compare(|base| {
		let link = base.join("root_link");
		symlink(base.join("root"), &link).unwrap();
		link
	});
}

#[test]
fn walks_a_subtree_start_as_walkdir_does() {
	compare(|base| base.join("root").join("deep").join("l0"));
}

#[test]
fn a_followed_directory_the_caller_skips_is_never_opened() {
	let base = TempRoot::new();
	let (root, locked) = (base.join("root"), base.join("locked"));
	fs::create_dir_all(&root).unwrap();
	fs::create_dir_all(&locked).unwrap();
	let link = root.join("skip_link");
	symlink(&locked, &link).unwrap();
	// A stat through the link still answers; opening the directory does not.
	fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
	let unopenable = fs::read_dir(&locked).is_err();

	let theirs = walkdir::WalkDir::new(&root)
		.follow_links(true)
		.into_iter()
		.map(|item| {
			item.map(|entry| entry.path().to_owned()).map_err(|err| {
				(
					err.path().map(Path::to_owned),
					err.io_error().map(io::Error::kind),
				)
			})
		})
		.collect::<Vec<_>>();
	let mut walk = Walk::new(&root);
	let mut mine = Vec::new();
	while let Some(item) = walk.next() {
		let item = item.map(|entry| entry.path().to_owned());
		if item.as_ref().is_ok_and(|path| *path == link) {
			walk.skip_current_dir();
		}
		mine.push(item.map_err(|err| err.path().map(Path::to_owned)));
	}

	assert!(
		unopenable,
		"run as non-root: the case needs permissions enforced"
	);
	// `walkdir` opens the target for its loop check before the caller can skip the link, and
	// fails there: an error in place of the entry, which no skip can take back. This walk is
	// meant to differ here.
	assert_eq!(
		theirs,
		vec![
			Ok(root.clone()),
			Err((None, Some(io::ErrorKind::PermissionDenied)))
		]
	);
	assert_eq!(mine, vec![Ok(root), Ok(link)]);
}

#[test]
fn a_loop_under_an_entered_link_is_reported_at_the_link_after_its_entry() {
	let base = TempRoot::new();
	let root = base.join("root");
	fs::create_dir_all(root.join("a")).unwrap();
	let link = root.join("a").join("up");
	symlink("..", &link).unwrap();

	let theirs = walkdir::WalkDir::new(&root)
		.follow_links(true)
		.into_iter()
		.map(|item| {
			item.map(|entry| entry.path().to_owned()).map_err(|err| {
				(
					err.path().map(Path::to_owned),
					err.loop_ancestor().map(Path::to_owned),
				)
			})
		})
		.collect::<Vec<_>>();
	let loops = |enter: bool| {
		let mut walk = Walk::new(&root);
		let mut seen = Vec::new();
		while let Some(item) = walk.next() {
			match item {
				Ok(entry) => {
					seen.push(Ok(entry.path().to_owned()));
					if enter && entry.path() == link {
						match walk.enter() {
							Err(WalkError::Loop { ancestor, child }) => {
								seen.push(Err((Some(child), Some(ancestor))));
							}
							other => panic!("expected the loop, got {other:?}"),
						}
					}
				}
				Err(WalkError::Loop { ancestor, child }) => {
					seen.push(Err((Some(child), Some(ancestor))));
				}
				Err(err) => panic!("unexpected {err:?}"),
			}
		}
		seen
	};
	let (on_next, on_enter) = (loops(false), loops(true));

	let loop_error = Err((Some(link.clone()), Some(root.clone())));
	// The same error at the same link as `walkdir`'s, which reports it in place of the entry.
	assert_eq!(
		theirs,
		vec![Ok(root.clone()), Ok(root.join("a")), loop_error.clone()]
	);
	let expected = vec![
		Ok(root.clone()),
		Ok(root.join("a")),
		Ok(link.clone()),
		loop_error,
	];
	assert_eq!(on_next, expected, "found as the walk descends");
	assert_eq!(on_enter, expected, "found when the caller enters it");
}

/// Walks a root holding one plain directory, removes that directory once it is handed over, and
/// returns what the walk yields after it: the caller skips it first when `skip` is set.
fn after_removing_a_handed_over_dir(skip: bool) -> (PathBuf, Vec<Listing>) {
	let base = TempRoot::new();
	let dir = base.join("d");
	fs::create_dir_all(&dir).unwrap();
	let mut walk = Walk::new(&base);
	assert_eq!(walk.next().unwrap().unwrap().path(), &*base);
	assert_eq!(walk.next().unwrap().unwrap().path(), dir);
	fs::remove_dir(&dir).unwrap();
	if skip {
		walk.skip_current_dir();
	}
	(dir, walk.collect())
}

#[test]
fn a_plain_directory_is_listed_only_once_the_walk_descends_into_it() {
	let (dir, rest) = after_removing_a_handed_over_dir(false);
	// Listed at hand-over, the removed directory would read as empty, with no error.
	match rest.as_slice() {
		[
			Err(WalkError::Io {
				path: Some(path),
				source,
			}),
		] => {
			assert_eq!(*path, dir);
			assert_eq!(source.kind(), io::ErrorKind::NotFound);
		}
		other => panic!("expected the directory's listing error, got {other:?}"),
	}
}

#[test]
fn a_plain_directory_the_caller_skips_is_never_listed() {
	let (_dir, rest) = after_removing_a_handed_over_dir(true);
	assert!(rest.is_empty(), "{rest:?}");
}

#[test]
fn a_missing_start_is_one_error_at_its_path() {
	let base = TempRoot::new();
	let start = base.join("missing");
	let mut walk = Walk::new(&start);
	match walk.next() {
		Some(Err(WalkError::Io {
			path: Some(path),
			source,
		})) => {
			assert_eq!(path, start);
			assert_eq!(source.kind(), io::ErrorKind::NotFound);
		}
		other => panic!("expected the start's own error, got {other:?}"),
	}
	assert!(walk.next().is_none());
}

#[test]
fn a_walk_deeper_than_max_open_keeps_at_most_max_open_streams_open() {
	let base = TempRoot::new();
	let mut deep = base.to_path_buf();
	for level in 0..3 * MAX_OPEN {
		deep = deep.join(format!("l{level}"));
		fs::create_dir_all(&deep).unwrap();
		fs::write(deep.join("f.txt"), b"f").unwrap();
	}
	let mut walk = Walk::new(&base);
	let (mut items, mut deepest) = (0, 0);
	while let Some(item) = walk.next() {
		item.unwrap();
		items += 1;
		deepest = deepest.max(walk.inside.len());
		let open = walk
			.inside
			.iter()
			.filter(|dir| matches!(dir.remaining, Remaining::Open { .. }))
			.count();
		assert!(
			open <= MAX_OPEN,
			"{open} streams open at depth {}",
			walk.inside.len()
		);
	}
	assert_eq!(items, 1 + 2 * 3 * MAX_OPEN);
	assert_eq!(deepest, 1 + 3 * MAX_OPEN);
}

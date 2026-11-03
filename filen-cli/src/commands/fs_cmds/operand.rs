//! The items commands are given: a path, read from the working path when it is relative, or
//! `uuid:<UUID>`, which names an item wherever it is, even one whose name cannot be decrypted. A
//! path's last segment can be a glob, which names the items in that directory it matches.

use std::{borrow::Cow, collections::HashSet, fmt};

use anyhow::{Context as _, Result};
use filen_sdk_rs::{
	auth::Client,
	fs::{
		HasName as _, HasParent as _, HasUUID as _,
		categories::{DirType, NonRootFileType, NonRootItemType, Normal},
	},
};
use filen_types::fs::{ParentUuid, Uuid};

use super::natural_cmp;
use crate::{
	error::{CliError, find_cli_error},
	ui::UI,
	util::RemotePath,
};

/// What an operand starts with to name an item by its UUID.
const UUID_PREFIX: &str = "uuid:";

/// An item a command was given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ItemOperand {
	/// The item at this path.
	Path(RemotePath),
	/// `uuid:<UUID>`: the item with this UUID.
	Uuid(Uuid),
}

impl ItemOperand {
	/// The item `operand` names: `uuid:<UUID>`, or a path relative to `working_path` unless it
	/// starts with '/'.
	pub(crate) fn parse(working_path: &RemotePath, operand: &str) -> Result<Self> {
		match operand.strip_prefix(UUID_PREFIX) {
			Some(uuid) => Uuid::parse_str(uuid)
				.map(Self::Uuid)
				.map_err(|_| UI::failure(&format!("Not a UUID: {}", uuid))),
			None => Ok(Self::Path(working_path.navigate(operand))),
		}
	}

	/// The failure of a command that found no `what` (such as "file or directory") here.
	pub(crate) fn not_found(&self, what: &str) -> anyhow::Error {
		match self {
			Self::Path(path) => UI::failure(&format!("No such {}: {}", what, path)),
			Self::Uuid(uuid) => UI::failure(&format!("No item with UUID {}", uuid)),
		}
	}
}

impl fmt::Display for ItemOperand {
	/// The operand as it was given, but for a path, which is shown normalized and absolute.
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Path(path) => write!(f, "{}", path),
			Self::Uuid(uuid) => write!(f, "{}{}", UUID_PREFIX, uuid),
		}
	}
}

/// The item `operand` names, if there is one in the drive. An item named by its UUID that is in
/// the trash (as the server reports every item in a trashed directory to be), or is an earlier
/// version of a file, is not in the drive: that fails, saying where it is instead.
pub(crate) async fn find_item<'a>(
	client: &'a Client,
	operand: &ItemOperand,
) -> Result<Option<NonRootFileType<'a, Normal>>> {
	match operand {
		ItemOperand::Path(path) => client
			.find_item_at_path(&path.0)
			.await
			.context("Failed to find file or directory"),
		ItemOperand::Uuid(uuid) => find_item_by_uuid(client, *uuid).await,
	}
}

/// The item `operand` names, or the failure of finding no `what` (such as "file or directory").
pub(crate) async fn resolve_item<'a>(
	client: &'a Client,
	operand: &ItemOperand,
	what: &str,
) -> Result<NonRootFileType<'a, Normal>> {
	find_item(client, operand)
		.await?
		.ok_or_else(|| operand.not_found(what))
}

/// `found`, or `None` if the server found no such file or directory.
fn optional<T>(found: Result<T, filen_sdk_rs::Error>) -> Result<Option<T>, filen_sdk_rs::Error> {
	// called by its path: in scope, its `context` would clash with anyhow's
	filen_sdk_rs::error::ResultExt::optional(found)
}

async fn find_item_by_uuid(
	client: &Client,
	uuid: Uuid,
) -> Result<Option<NonRootFileType<'_, Normal>>> {
	if uuid == client.root().uuid() {
		return Ok(Some(NonRootFileType::Root(Cow::Borrowed(client.root()))));
	}
	let in_trash = || {
		UI::failure(&format!(
			"{UUID_PREFIX}{uuid} is in the trash: restore it with trash-restore {UUID_PREFIX}{uuid}"
		))
	};
	// the server answers either lookup of the other kind's UUID as not found
	if let Some(found) =
		optional(client.get_file_with_info(uuid).await).context("Failed to find file")?
	{
		if found.trash {
			return Err(in_trash());
		}
		if found.versioned {
			return Err(UI::failure(&format!(
				"{UUID_PREFIX}{uuid} is an earlier version of a file, which `versions` lists"
			)));
		}
		return Ok(Some(NonRootFileType::File(Cow::Owned(found.file))));
	}
	match optional(client.get_dir(uuid).await).context("Failed to find directory")? {
		Some(dir) if matches!(dir.parent(), ParentUuid::Trash(_)) => Err(in_trash()),
		Some(dir) => Ok(Some(NonRootFileType::Dir(Cow::Owned(dir)))),
		None => Ok(None),
	}
}

/// The path of `item`, which `operand` names: the operand's own path, or, for a UUID, the path
/// the item is at, which only an item whose name and whose directories' names can be decrypted
/// has.
pub(crate) async fn path_of(
	client: &Client,
	operand: &ItemOperand,
	item: &NonRootFileType<'_, Normal>,
) -> Result<RemotePath> {
	let item = match (operand, item) {
		(ItemOperand::Path(path), _) => return Ok(path.clone()),
		(ItemOperand::Uuid(_), NonRootFileType::Root(_)) => return Ok(RemotePath::new("/")),
		(ItemOperand::Uuid(_), NonRootFileType::Dir(dir)) => {
			NonRootItemType::Dir(Cow::Borrowed(dir.as_ref()))
		}
		(ItemOperand::Uuid(_), NonRootFileType::File(file)) => {
			NonRootItemType::File(Cow::Borrowed(file.as_ref()))
		}
	};
	let (path, _) = client
		.get_item_path(&item)
		.await
		.with_context(|| format!("Failed to find the path of {}", operand))?;
	Ok(RemotePath::new(&path))
}

/// The path `operand` names: its own, or that of the item with its UUID, which must exist.
pub(crate) async fn operand_path(
	client: &Client,
	operand: &ItemOperand,
	what: &str,
) -> Result<RemotePath> {
	match operand {
		ItemOperand::Path(path) => Ok(path.clone()),
		ItemOperand::Uuid(_) => {
			let item = resolve_item(client, operand, what).await?;
			path_of(client, operand, &item).await
		}
	}
}

/// An item a command was given, and the operand naming it: the one given, or for an item a glob
/// matched, its path.
pub(crate) struct GivenItem<'a> {
	pub(crate) operand: ItemOperand,
	pub(crate) item: NonRootFileType<'a, Normal>,
}

/// The operands `operands`, each read as [`ItemOperand::parse`] reads it. One that is no valid
/// operand fails the command before it does anything.
pub(crate) fn parse_operands(
	working_path: &RemotePath,
	operands: &[String],
) -> Result<Vec<ItemOperand>> {
	operands
		.iter()
		.map(|operand| ItemOperand::parse(working_path, operand))
		.collect()
}

/// The glob in the last segment of the path `operand` names, if it has one.
fn glob_of(operand: &ItemOperand) -> Option<Glob> {
	match operand {
		ItemOperand::Path(path) => path.basename().and_then(Glob::of),
		ItemOperand::Uuid(_) => None,
	}
}

/// The items `operand` names: the one it names (`what`, such as "file or directory"), or, if the
/// last segment of its path is a glob, every item in that directory whose name the glob matches,
/// in the order a listing sorts them. A glob that matches nothing fails. An item named exactly
/// like the glob is the one item it names, as a name can hold `[` and `]` (and one from an older
/// app `*` and `?`).
pub(crate) async fn expand<'a>(
	client: &'a Client,
	operand: ItemOperand,
	what: &str,
) -> Result<Vec<GivenItem<'a>>> {
	let Some(glob) = glob_of(&operand) else {
		let item = resolve_item(client, &operand, what).await?;
		return Ok(vec![GivenItem { operand, item }]);
	};
	if let Some(item) = find_item(client, &operand).await? {
		return Ok(vec![GivenItem { operand, item }]);
	}
	let ItemOperand::Path(path) = &operand else {
		// only a path has a glob
		return Err(operand.not_found(what));
	};
	let parent = path.parent();
	let dir = match find_item(client, &ItemOperand::Path(parent.clone())).await? {
		Some(NonRootFileType::Dir(dir)) => DirType::Dir(dir),
		Some(NonRootFileType::Root(root)) => DirType::Root(root),
		Some(NonRootFileType::File(_)) => {
			return Err(UI::failure(&format!("Not a directory: {}", parent)));
		}
		None => return Err(UI::failure(&format!("No such directory: {}", parent))),
	};
	let (dirs, files) = client
		.list_dir::<_, Normal>(&dir, None::<&fn(u64, Option<u64>)>)
		.await
		.context("Failed to list directory")?;
	let named = dirs
		.into_iter()
		.filter_map(|dir| {
			Some((
				dir.name()?.to_owned(),
				NonRootFileType::Dir(Cow::Owned(dir)),
			))
		})
		.chain(files.into_iter().filter_map(|file| {
			Some((
				file.name()?.to_owned(),
				NonRootFileType::File(Cow::Owned(file)),
			))
		}))
		.collect::<Vec<(String, NonRootFileType<'a, Normal>)>>();
	let matched = matching(&glob, named);
	if matched.is_empty() {
		return Err(UI::failure(&format!("No {} matches {}", what, path)));
	}
	Ok(matched
		.into_iter()
		.map(|(name, item)| GivenItem {
			operand: ItemOperand::Path(parent.navigate(&name)),
			item,
		})
		.collect())
}

/// The ones among `named` items whose names `glob` matches, sorted by name as a listing sorts
/// names.
fn matching<T>(glob: &Glob, named: Vec<(String, T)>) -> Vec<(String, T)> {
	let mut matched = named
		.into_iter()
		.filter(|(name, _)| glob.matches(name))
		.collect::<Vec<(String, T)>>();
	matched.sort_by(|(a, _), (b, _)| natural_cmp(a, b));
	matched
}

/// The items several operands name (see [`expand`]), and the failures so far.
pub(crate) struct Given<'a> {
	pub(crate) items: Vec<GivenItem<'a>>,
	pub(crate) failures: Failures,
}

/// The items `operands` name (see [`expand`]), each once, in the order they are first named. An
/// operand that names none is reported, and the command goes on with the others; a command given
/// one operand fails with its failure instead.
pub(crate) async fn expand_all<'a>(
	ui: &mut UI,
	client: &'a Client,
	operands: Vec<ItemOperand>,
	what: &str,
) -> Result<Given<'a>> {
	let one_operand = operands.len() == 1;
	let mut items: Vec<GivenItem<'a>> = Vec::with_capacity(operands.len());
	let mut seen = HashSet::with_capacity(operands.len());
	let mut unresolved = 0usize;
	for operand in operands {
		match expand(client, operand, what).await {
			Ok(expanded) => items.extend(
				expanded
					.into_iter()
					.filter(|given| seen.insert(given.item.uuid())),
			),
			Err(e) if one_operand || is_interruption(&e) => return Err(e),
			Err(e) => {
				ui.print_failure_or_error(&e);
				unresolved = unresolved.saturating_add(1);
			}
		}
	}
	Ok(Given {
		failures: Failures {
			given: items.len().saturating_add(unresolved),
			failed: unresolved,
		},
		items,
	})
}

/// Whether `e` stopped the command rather than failing one item: Ctrl-C, or a closed stdout.
fn is_interruption(e: &anyhow::Error) -> bool {
	matches!(
		find_cli_error(e),
		Some(CliError::Interrupted | CliError::StdoutClosed)
	)
}

/// How many of the items a command was given it failed for. A command given one item fails with
/// its failure; one given several reports each failure, goes on with the rest, and fails at the
/// end.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Failures {
	/// How many items the command was given, counting an operand that named none as one.
	given: usize,
	failed: usize,
}

impl Failures {
	/// How many items the command was given, counting an operand that named none as one.
	pub(crate) fn given(&self) -> usize {
		self.given
	}

	/// Records the `result` of acting on one of the items: a failure is returned for a command
	/// given one item, or to stop it, and otherwise reported.
	pub(crate) fn record(&mut self, ui: &mut UI, result: Result<()>) -> Result<()> {
		match result {
			Ok(()) => Ok(()),
			Err(e) if self.given == 1 || is_interruption(&e) => Err(e),
			Err(e) => {
				ui.print_failure_or_error(&e);
				self.failed = self.failed.saturating_add(1);
				Ok(())
			}
		}
	}

	/// The command's result once it went through every item: a failure saying how many items could
	/// not be `done` (such as "trashed"), if any.
	pub(crate) fn finish(self, done: &str) -> Result<()> {
		if self.failed == 0 {
			return Ok(());
		}
		Err(UI::failure(&format!(
			"{} of {} items could not be {}",
			self.failed, self.given, done
		)))
	}
}

/// A glob in the last segment of a path: `*` matches any run of characters, `?` any one
/// character, and `[...]` any one of the characters in the brackets (`a-z` for a range, `!` or `^`
/// first for any character not in them). Characters are compared as they are, case included. A
/// name that starts with '.' is only matched by a glob that starts with '.' too, as in a shell.
#[derive(Debug, PartialEq, Eq)]
struct Glob(Vec<GlobToken>);

#[derive(Debug, PartialEq, Eq)]
enum GlobToken {
	Char(char),
	/// `*`
	AnyRun,
	/// `?`
	AnyChar,
	/// `[...]`: one of the characters in these inclusive ranges, or, if `negated`, one not in them.
	Class {
		negated: bool,
		ranges: Vec<(char, char)>,
	},
}

impl GlobToken {
	/// Whether the token matches `c`, as one character (`*` matches it as part of a run).
	fn matches(&self, c: char) -> bool {
		match self {
			Self::Char(expected) => *expected == c,
			Self::AnyRun | Self::AnyChar => true,
			Self::Class { negated, ranges } => {
				ranges.iter().any(|(low, high)| (*low..=*high).contains(&c)) != *negated
			}
		}
	}
}

impl Glob {
	/// The glob `segment` is, or `None` if it has no `*`, `?` or `[...]` and so names one item. A
	/// '[' without a ']' after it is a '['.
	fn of(segment: &str) -> Option<Self> {
		let chars = segment.chars().collect::<Vec<char>>();
		let mut rest = chars.as_slice();
		let mut tokens = Vec::new();
		while let [c, tail @ ..] = rest {
			rest = tail;
			tokens.push(match c {
				'*' => GlobToken::AnyRun,
				'?' => GlobToken::AnyChar,
				'[' => match class(tail) {
					Some((class, after)) => {
						rest = after;
						class
					}
					None => GlobToken::Char('['),
				},
				c => GlobToken::Char(*c),
			});
		}
		tokens
			.iter()
			.any(|token| !matches!(token, GlobToken::Char(_)))
			.then_some(Self(tokens))
	}

	/// Whether the glob matches all of `name`.
	fn matches(&self, name: &str) -> bool {
		if name.starts_with('.') && self.0.first() != Some(&GlobToken::Char('.')) {
			return false;
		}
		let name = name.chars().collect::<Vec<char>>();
		let mut token = 0;
		let mut at = 0;
		// the token after the last `*` passed, and where in the name the `*`'s run ends
		let mut last_run: Option<(usize, usize)> = None;
		loop {
			match self.0.get(token) {
				Some(GlobToken::AnyRun) => {
					token += 1;
					last_run = Some((token, at));
					continue;
				}
				Some(next) if name.get(at).is_some_and(|c| next.matches(*c)) => {
					token += 1;
					at += 1;
					continue;
				}
				None if at == name.len() => return true,
				_ => {}
			}
			// no match from here: the last `*` takes one more character, if there is one
			match last_run {
				Some((after_run, run_end)) if run_end < name.len() => {
					last_run = Some((after_run, run_end + 1));
					token = after_run;
					at = run_end + 1;
				}
				_ => return false,
			}
		}
	}
}

/// The `[...]` class whose characters `chars` starts with (after its '['), and what follows its
/// ']', or `None` if no ']' ends it. A ']' first in the brackets (after '!' or '^') is one of its
/// characters.
fn class(chars: &[char]) -> Option<(GlobToken, &[char])> {
	let (negated, mut rest) = match chars {
		['!' | '^', tail @ ..] => (true, tail),
		_ => (false, chars),
	};
	let mut ranges = Vec::new();
	loop {
		match rest {
			[']', tail @ ..] if !ranges.is_empty() => {
				return Some((GlobToken::Class { negated, ranges }, tail));
			}
			[low, '-', high, tail @ ..] if *high != ']' => {
				ranges.push((*low, *high));
				rest = tail;
			}
			[c, tail @ ..] => {
				ranges.push((*c, *c));
				rest = tail;
			}
			[] => return None,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::ui::test_support::Printed;

	fn uuid() -> Uuid {
		Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef)
	}

	#[test]
	fn a_path_operand_is_read_from_the_working_path() {
		let working_path = RemotePath::new("/a/b");

		assert_eq!(
			ItemOperand::parse(&working_path, "c/../d").unwrap(),
			ItemOperand::Path(RemotePath::new("/a/b/d"))
		);
		assert_eq!(
			ItemOperand::parse(&working_path, "/c").unwrap(),
			ItemOperand::Path(RemotePath::new("/c"))
		);
	}

	#[test]
	fn a_uuid_operand_names_the_item_with_that_uuid_wherever_the_working_path_is() {
		let operand = ItemOperand::parse(
			&RemotePath::new("/a"),
			"uuid:01234567-89AB-CDEF-0123-456789ABCDEF",
		)
		.unwrap();

		assert_eq!(operand, ItemOperand::Uuid(uuid()));
		assert_eq!(
			operand.to_string(),
			"uuid:01234567-89ab-cdef-0123-456789abcdef"
		);
	}

	#[test]
	fn a_uuid_operand_that_is_no_uuid_fails() {
		let error = ItemOperand::parse(&RemotePath::new("/"), "uuid:abc").unwrap_err();

		assert_eq!(error.to_string(), "Not a UUID: abc");
	}

	#[test]
	fn a_path_naming_a_uuid_without_the_prefix_is_a_path() {
		let operand = ItemOperand::parse(
			&RemotePath::new("/"),
			"01234567-89ab-cdef-0123-456789abcdef",
		)
		.unwrap();

		assert_eq!(
			operand,
			ItemOperand::Path(RemotePath::new("/01234567-89ab-cdef-0123-456789abcdef"))
		);
	}

	#[test]
	fn a_missing_item_is_named_by_its_path_or_its_uuid() {
		assert_eq!(
			ItemOperand::Path(RemotePath::new("/a"))
				.not_found("file")
				.to_string(),
			"No such file: /a"
		);
		assert_eq!(
			ItemOperand::Uuid(uuid()).not_found("file").to_string(),
			"No item with UUID 01234567-89ab-cdef-0123-456789abcdef"
		);
	}

	fn glob(segment: &str) -> Glob {
		Glob::of(segment).unwrap_or_else(|| panic!("{segment} is no glob"))
	}

	#[test]
	fn a_name_without_wildcards_is_no_glob() {
		assert_eq!(Glob::of("a.txt"), None);
		assert_eq!(Glob::of("my notes (1).txt"), None);
		// no ']' closes the '['
		assert_eq!(Glob::of("a[b"), None);
		assert_eq!(Glob::of("[]"), None);
	}

	#[test]
	fn a_star_matches_any_run_of_characters() {
		let txt = glob("*.txt");
		assert!(txt.matches("a.txt"));
		assert!(!txt.matches(".a.txt"));
		assert!(txt.matches("notes 2024.txt"));
		assert!(!txt.matches("a.txt.bak"));
		assert!(!txt.matches("a.md"));
		assert!(glob("*").matches("anything"));
		assert!(glob("a*b*c").matches("aXbYbZc"));
		assert!(!glob("a*b*c").matches("aXbYbZ"));
		assert!(glob("a**").matches("a"));
	}

	#[test]
	fn globs_match_case_sensitively() {
		assert!(!glob("*.txt").matches("A.TXT"));
		assert!(glob("*.TXT").matches("A.TXT"));
	}

	#[test]
	fn a_question_mark_matches_one_character() {
		let one = glob("file?.txt");
		assert!(one.matches("file1.txt"));
		assert!(one.matches("fileä.txt"));
		assert!(!one.matches("file10.txt"));
		assert!(!one.matches("file.txt"));
	}

	#[test]
	fn brackets_match_one_of_their_characters() {
		assert!(glob("[ab]*").matches("a1"));
		assert!(glob("[ab]*").matches("b2"));
		assert!(!glob("[ab]*").matches("c3"));
		assert!(glob("x[0-9]").matches("x7"));
		assert!(!glob("x[0-9]").matches("xa"));
		assert!(glob("[!a]*").matches("b"));
		assert!(!glob("[!a]*").matches("a"));
		assert!(glob("[^a]*").matches("b"));
		// a ']' first in the brackets is one of the characters
		assert!(glob("[]]").matches("]"));
		assert!(glob("[a-]").matches("-"));
	}

	#[test]
	fn a_name_starting_with_a_dot_is_only_matched_by_a_glob_starting_with_one() {
		assert!(!glob("*").matches(".filenignore"));
		assert!(!glob("?filenignore").matches(".filenignore"));
		assert!(glob(".*").matches(".filenignore"));
	}

	#[test]
	fn the_names_a_glob_matches_are_sorted_as_a_listing_sorts_them() {
		let named = ["file10.txt", "notes.md", "file2.txt", "File3.txt"]
			.into_iter()
			.map(|name| (name.to_owned(), ()))
			.collect();

		let matched = matching(&glob("*.txt"), named)
			.into_iter()
			.map(|(name, ())| name)
			.collect::<Vec<String>>();

		assert_eq!(matched, ["file2.txt", "File3.txt", "file10.txt"]);
	}

	#[test]
	fn only_the_last_segment_of_a_path_is_a_glob() {
		let operand = |path| ItemOperand::Path(RemotePath::new(path));

		assert_eq!(glob_of(&operand("/a*/b.txt")), None);
		assert_eq!(glob_of(&operand("/a/*.txt")), Some(glob("*.txt")));
		assert_eq!(glob_of(&ItemOperand::Uuid(uuid())), None);
		assert_eq!(glob_of(&operand("/")), None);
	}

	#[test]
	fn a_command_given_one_item_fails_with_its_failure() {
		let printed = Printed::default();
		let mut ui = printed.ui();
		let mut failures = Failures {
			given: 1,
			failed: 0,
		};

		let error = failures
			.record(&mut ui, Err(UI::failure("No such file: /a")))
			.unwrap_err();

		assert_eq!(error.to_string(), "No such file: /a");
		assert_eq!(printed.lines_or_empty(), "");
	}

	#[test]
	fn a_command_given_several_items_reports_each_failure_and_fails_at_the_end() {
		let printed = Printed::default();
		let mut ui = printed.ui();
		let mut failures = Failures {
			given: 3,
			failed: 1,
		};

		failures.record(&mut ui, Ok(())).unwrap();
		failures
			.record(&mut ui, Err(UI::failure("No such file: /b")))
			.unwrap();

		assert_eq!(
			console::strip_ansi_codes(&printed.lines()),
			"✘ No such file: /b"
		);
		assert_eq!(
			failures.finish("trashed").unwrap_err().to_string(),
			"2 of 3 items could not be trashed"
		);
	}

	#[test]
	fn ctrl_c_stops_a_command_given_several_items() {
		let printed = Printed::default();
		let mut ui = printed.ui();
		let mut failures = Failures {
			given: 3,
			failed: 0,
		};

		let error = failures
			.record(&mut ui, Err(CliError::Interrupted.into()))
			.unwrap_err();

		assert!(matches!(
			find_cli_error(&error),
			Some(CliError::Interrupted)
		));
	}

	#[test]
	fn a_command_that_failed_for_no_item_succeeds() {
		let failures = Failures {
			given: 2,
			failed: 0,
		};

		assert!(failures.finish("trashed").is_ok());
	}
}

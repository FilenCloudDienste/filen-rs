//! Recursive listings of a directory: `ls -R`, `tree` and `du`. Each lists the whole directory
//! tree in one go, so `du` sums the sizes of its files instead of asking the server for each
//! child directory's size, which the server recomputes only every so often.

use std::{borrow::Cow, collections::HashMap, sync::Arc};

use anyhow::{Context as _, Result};
use console::style;
use filen_sdk_rs::{
	auth::Client,
	fs::{
		categories::{DirType, NonRootFileType, Normal, fs::CategoryFSExt as _},
		file::traits::HasFileInfo as _,
	},
	io::RemoteFile,
};
use serde_json::json;

use super::{
	ItemOperand, ListedBy, ListedItem, ListedKind, StderrProgress, listing_order,
	print_long_listing, resolve_item,
};
use crate::{
	auth::LazyClient,
	ui::{self, UI},
};

/// At most this many items that could not be listed are named; a broken tree can have many.
const MAX_PRINTED_LISTING_ERRORS: usize = 10;

/// What is at the path a recursive listing is asked for.
enum ListedPath<'a> {
	Dir(DirType<'a, Normal>),
	File(Cow<'a, RemoteFile>),
}

async fn find_listed_path<'a>(client: &'a Client, path: &ItemOperand) -> Result<ListedPath<'a>> {
	match resolve_item(client, path, "file or directory").await? {
		NonRootFileType::Dir(dir) => Ok(ListedPath::Dir(DirType::Dir(dir))),
		NonRootFileType::Root(root) => Ok(ListedPath::Dir(DirType::Root(root))),
		NonRootFileType::File(file) => Ok(ListedPath::File(file)),
	}
}

/// Everything below `dir`, each item's path relative to `dir`, in no particular order. Items
/// that cannot be read (with what is below them) are left out, and named in warnings.
async fn list_recursively(
	ui: &mut UI,
	client: &Arc<Client>,
	dir: DirType<'_, Normal>,
) -> Result<Vec<ListedItem>> {
	let progress = StderrProgress::new(ui);
	let mut errors = Vec::new();
	let listed = Normal::list_dir_recursive_with_paths(
		Arc::clone(client),
		dir,
		Some(&|done: u64, total: Option<u64>| {
			progress.draw(&listing_progress_text(done, total));
		}),
		&mut |more| errors.extend(more),
		(),
	)
	.await;
	progress.end();
	let (dirs, files) = listed.context("Failed to list directory")?;
	for error in errors.iter().take(MAX_PRINTED_LISTING_ERRORS) {
		ui.print_warning(&format!("Left out what could not be listed: {}", error));
	}
	if errors.len() > MAX_PRINTED_LISTING_ERRORS {
		ui.print_warning(&format!(
			"... and {} more",
			errors.len() - MAX_PRINTED_LISTING_ERRORS
		));
	}
	Ok(dirs
		.into_iter()
		.map(|(dir, path)| ListedItem {
			path: Some(path),
			..ListedItem::of_dir(&dir)
		})
		.chain(files.into_iter().map(|(file, path)| ListedItem {
			path: Some(path),
			..ListedItem::of_file(&file)
		}))
		.collect())
}

/// The progress line while the directory tree is downloaded.
fn listing_progress_text(done: u64, total: Option<u64>) -> String {
	match total {
		Some(total) => format!(
			"Listing... {} of {}",
			ui::format_size(done),
			ui::format_size(total)
		),
		None => format!("Listing... {}", ui::format_size(done)),
	}
}

/// The items of a recursive listing as a tree: which items each item holds, and which are at the
/// top, each in listing order.
#[derive(Debug, PartialEq, Eq)]
struct ListingTree {
	top: Vec<usize>,
	/// The indices of the items each item holds, by the item's index.
	children: Vec<Vec<usize>>,
}

impl ListingTree {
	/// Arranges `items`, whose paths are relative to the listed directory, by their paths. An
	/// item whose parent is not listed goes at the top.
	fn of(items: &[ListedItem]) -> Self {
		let by_path = items
			.iter()
			.enumerate()
			.filter_map(|(index, item)| Some((item.path.as_deref()?, index)))
			.collect::<HashMap<&str, usize>>();
		let mut top = Vec::new();
		let mut children = vec![Vec::new(); items.len()];
		for (index, item) in items.iter().enumerate() {
			let parent = item
				.path
				.as_deref()
				.and_then(|path| path.rsplit_once('/'))
				.and_then(|(parent, _)| by_path.get(parent));
			match parent.and_then(|&parent| children.get_mut(parent)) {
				Some(siblings) => siblings.push(index),
				None => top.push(index),
			}
		}
		let order = |a: &usize, b: &usize| listing_order(&items[*a], &items[*b], ListedBy::Name);
		top.sort_by(order);
		for siblings in &mut children {
			siblings.sort_by(order);
		}
		Self { top, children }
	}

	/// Every item, each directory followed by what it holds, and the prefix that draws its
	/// branch of the tree: `├── ` or `└── ` for the last item of a directory, after a `│   ` or
	/// `    ` for each directory above it.
	fn lines(&self) -> Vec<(usize, String)> {
		let mut lines = Vec::new();
		// the items of each directory being walked, how many of them are done, and their indent
		let mut stack = vec![(self.top.as_slice(), 0, String::new())];
		while let Some((siblings, done, indent)) = stack.last_mut() {
			let Some(&item) = siblings.get(*done) else {
				stack.pop();
				continue;
			};
			*done += 1;
			let last = *done == siblings.len();
			let (branch, below) = if last {
				("└── ", "    ")
			} else {
				("├── ", "│   ")
			};
			lines.push((item, format!("{}{}", indent, branch)));
			let child_indent = format!("{}{}", indent, below);
			if let Some(children) = self.children.get(item)
				&& !children.is_empty()
			{
				stack.push((children.as_slice(), 0, child_indent));
			}
		}
		lines
	}
}

/// `items` in the order `order` gives their indices in.
fn in_order(items: Vec<ListedItem>, order: impl IntoIterator<Item = usize>) -> Vec<ListedItem> {
	let mut items = items
		.into_iter()
		.map(Some)
		.collect::<Vec<Option<ListedItem>>>();
	order
		.into_iter()
		.filter_map(|index| items.get_mut(index).and_then(Option::take))
		.collect()
}

fn styled_label(item: &ListedItem, label: &str) -> String {
	match item.kind {
		ListedKind::Directory => style(label).blue().to_string(),
		ListedKind::File => label.to_owned(),
	}
}

/// `ls -R`: everything below the directory at `path`, one path relative to it per line, each
/// directory followed by what it holds.
pub(super) async fn list_directory_recursively(
	ui: &mut UI,
	client: &mut LazyClient,
	path: &ItemOperand,
	long: bool,
) -> Result<()> {
	let client = client.get(ui).await?;
	let items = match find_listed_path(client, path).await? {
		ListedPath::Dir(dir) => list_recursively(ui, client, dir).await?,
		ListedPath::File(file) => {
			let item = ListedItem::of_file(&file);
			return super::print_listing(ui, &[item], ListedBy::Name, "", long);
		}
	};
	let tree = ListingTree::of(&items);
	let items = in_order(items, tree.lines().into_iter().map(|(index, _)| index));
	print_recursive_listing(ui, &items, long)
}

fn print_recursive_listing(ui: &mut UI, items: &[ListedItem], long: bool) -> Result<()> {
	if ui.json {
		return ui.print_json(serde_json::Value::Array(
			items
				.iter()
				.map(|item| item.to_json(ListedBy::Path))
				.collect(),
		));
	}
	if items.is_empty() {
		ui.print_muted("Directory is empty");
		return Ok(());
	}
	if long {
		print_long_listing(ui, items, ListedBy::Path);
		return Ok(());
	}
	for item in items {
		ui.print(&styled_label(item, &item.label(ListedBy::Path)));
	}
	Ok(())
}

/// `tree`: everything below the directory at `path`, drawn as a tree.
pub(super) async fn print_tree(
	ui: &mut UI,
	client: &mut LazyClient,
	path: &ItemOperand,
) -> Result<()> {
	let client = client.get(ui).await?;
	let ListedPath::Dir(dir) = find_listed_path(client, path).await? else {
		return Err(UI::failure(&format!("Not a directory: {}", path)));
	};
	let items = list_recursively(ui, client, dir).await?;
	let tree = ListingTree::of(&items);
	let lines = tree.lines();
	if ui.json {
		let items = in_order(items, lines.into_iter().map(|(index, _)| index));
		return print_recursive_listing(ui, &items, false);
	}
	for line in tree_text(&path.to_string(), &items, &lines) {
		ui.print(&line);
	}
	Ok(())
}

/// The lines `tree` prints: the directory, a line per item below it, and how many there are.
fn tree_text(directory: &str, items: &[ListedItem], lines: &[(usize, String)]) -> Vec<String> {
	let mut text = vec![style(directory).blue().to_string()];
	for (index, prefix) in lines {
		if let Some(item) = items.get(*index) {
			text.push(format!("{}{}", prefix, styled_label(item, &item.name)));
		}
	}
	let dirs = items
		.iter()
		.filter(|item| item.kind == ListedKind::Directory)
		.count();
	let files = items.len() - dirs;
	text.push(String::new());
	text.push(format!(
		"{} {}, {} {}",
		dirs,
		if dirs == 1 {
			"directory"
		} else {
			"directories"
		},
		files,
		if files == 1 { "file" } else { "files" }
	));
	text
}

/// What `du` reports: the size of each item at the top of the listed directory, the size of a
/// directory being that of every file below it, and the total.
#[derive(Debug, PartialEq, Eq)]
struct DiskUsage {
	/// The index of each top item, with its size, in listing order.
	top: Vec<(usize, u64)>,
	total: u64,
}

impl DiskUsage {
	fn of(items: &[ListedItem]) -> Result<Self> {
		let tree = ListingTree::of(items);
		let slots = tree
			.top
			.iter()
			.enumerate()
			.filter_map(|(slot, &index)| Some((items.get(index)?.path.as_deref()?, slot)))
			.collect::<HashMap<&str, usize>>();
		let mut top = tree
			.top
			.iter()
			.map(|&index| (index, 0))
			.collect::<Vec<(usize, u64)>>();
		let mut total = 0u64;
		for item in items {
			let Some(size) = item.size else {
				continue;
			};
			total = total
				.checked_add(size)
				.context("The sizes add up to too much")?;
			// the item at the top that holds this file, or the file itself
			let top_slot = item
				.path
				.as_deref()
				.and_then(|path| path.split('/').next())
				.and_then(|top_path| slots.get(top_path))
				.and_then(|&slot| top.get_mut(slot));
			if let Some((_, top_size)) = top_slot {
				*top_size = top_size
					.checked_add(size)
					.context("The sizes add up to too much")?;
			}
		}
		Ok(Self { top, total })
	}
}

/// `du`: the size of each item in the directory at `path` and the total, or only the total with
/// `summary`. A file's size is its own.
pub(super) async fn print_disk_usage(
	ui: &mut UI,
	client: &mut LazyClient,
	path: &ItemOperand,
	summary: bool,
) -> Result<()> {
	let client = client.get(ui).await?;
	let (items, usage) = match find_listed_path(client, path).await? {
		ListedPath::Dir(dir) => {
			let items = list_recursively(ui, client, dir).await?;
			let usage = DiskUsage::of(&items)?;
			(items, usage)
		}
		ListedPath::File(file) => (
			Vec::new(),
			DiskUsage {
				top: Vec::new(),
				total: file.size(),
			},
		),
	};
	if ui.json {
		let mut object = serde_json::Map::new();
		object.insert("path".to_owned(), json!(path.to_string()));
		object.insert("size".to_owned(), json!(usage.total));
		if !summary {
			let top = usage
				.top
				.iter()
				.filter_map(|&(index, size)| {
					let item = items.get(index)?;
					Some(json!({ "name": item.name, "type": item.kind.name(), "size": size }))
				})
				.collect();
			object.insert("items".to_owned(), serde_json::Value::Array(top));
		}
		return ui.print_json(serde_json::Value::Object(object));
	}
	let shown = if summary { &[][..] } else { &usage.top[..] };
	for line in disk_usage_text(&path.to_string(), &items, shown, usage.total) {
		ui.print(&line);
	}
	Ok(())
}

/// The lines `du` prints: a right-aligned size and the name of each item in `top`, then the
/// total and the directory.
fn disk_usage_text(
	directory: &str,
	items: &[ListedItem],
	top: &[(usize, u64)],
	total: u64,
) -> Vec<String> {
	let rows = top
		.iter()
		.filter_map(|&(index, size)| {
			let item = items.get(index)?;
			Some((ui::format_size(size), styled_label(item, &item.name)))
		})
		.chain(std::iter::once((
			ui::format_size(total),
			directory.to_owned(),
		)))
		.collect::<Vec<(String, String)>>();
	let width = rows.iter().map(|(size, _)| size.len()).max().unwrap_or(0);
	rows.into_iter()
		.map(|(size, label)| format!("{:>width$}  {}", size, label))
		.collect()
}

#[cfg(test)]
mod tests {
	use super::*;
	use filen_types::{api::v3::dir::color::DirColor, fs::Uuid};

	fn dir(path: &str, uuid: u128) -> ListedItem {
		ListedItem {
			kind: ListedKind::Directory,
			name: path.rsplit('/').next().unwrap().to_owned(),
			path: Some(path.to_owned()),
			size: None,
			modified: None,
			created: None,
			favorited: false,
			color: Some(DirColor::Default),
			uuid: Uuid::from_u128(uuid),
		}
	}

	fn file(path: &str, size: u64, uuid: u128) -> ListedItem {
		ListedItem {
			kind: ListedKind::File,
			size: Some(size),
			color: None,
			..dir(path, uuid)
		}
	}

	/// A small tree, listed in the order the SDK might return it: directories, then files.
	fn listing() -> Vec<ListedItem> {
		vec![
			dir("docs", 1),
			dir("docs/old", 2),
			dir("empty", 3),
			file("docs/old/file10.txt", 100, 4),
			file("docs/old/file2.txt", 20, 5),
			file("docs/b.txt", 3, 6),
			file("a.txt", 7, 7),
		]
	}

	fn tree_of(items: &[ListedItem]) -> Vec<String> {
		let lines = ListingTree::of(items).lines();
		tree_text("/test", items, &lines)
			.iter()
			.map(|line| console::strip_ansi_codes(line).into_owned())
			.collect()
	}

	#[test]
	fn tree_draws_each_directory_before_what_it_holds_directories_first_in_natural_order() {
		assert_eq!(
			tree_of(&listing()),
			[
				"/test",
				"├── docs",
				"│   ├── old",
				"│   │   ├── file2.txt",
				"│   │   └── file10.txt",
				"│   └── b.txt",
				"├── empty",
				"└── a.txt",
				"",
				"3 directories, 4 files",
			]
		);
	}

	#[test]
	fn tree_of_an_empty_directory_draws_only_the_directory() {
		assert_eq!(tree_of(&[]), ["/test", "", "0 directories, 0 files"]);
	}

	#[test]
	fn tree_counts_one_directory_and_one_file_in_the_singular() {
		assert_eq!(
			tree_of(&[dir("d", 1), file("d/f", 1, 2)]),
			["/test", "└── d", "    └── f", "", "1 directory, 1 file"]
		);
	}

	#[test]
	fn an_item_whose_directory_is_not_listed_goes_at_the_top() {
		assert_eq!(
			tree_of(&[file("gone/f", 1, 1), file("a", 1, 2)]),
			["/test", "├── a", "└── f", "", "0 directories, 2 files"]
		);
	}

	#[test]
	fn the_recursive_listing_puts_each_directory_before_what_it_holds() {
		let items = listing();
		let order = ListingTree::of(&items)
			.lines()
			.into_iter()
			.map(|(index, _)| index);
		let paths = in_order(items, order)
			.into_iter()
			.map(|item| item.path.unwrap())
			.collect::<Vec<String>>();
		assert_eq!(
			paths,
			[
				"docs",
				"docs/old",
				"docs/old/file2.txt",
				"docs/old/file10.txt",
				"docs/b.txt",
				"empty",
				"a.txt",
			]
		);
	}

	#[test]
	fn du_sums_the_files_below_each_top_item_and_in_total() {
		let items = listing();
		let usage = DiskUsage::of(&items).unwrap();
		assert_eq!(
			usage,
			DiskUsage {
				// docs, empty, a.txt
				top: vec![(0, 123), (2, 0), (6, 7)],
				total: 130,
			}
		);
		assert_eq!(
			disk_usage_text("/test", &items, &usage.top, usage.total)
				.iter()
				.map(|line| console::strip_ansi_codes(line).into_owned())
				.collect::<Vec<String>>(),
			[
				"123 B  docs",
				"  0 B  empty",
				"  7 B  a.txt",
				"130 B  /test"
			]
		);
	}

	#[test]
	fn du_of_an_empty_directory_is_zero() {
		assert_eq!(
			DiskUsage::of(&[]).unwrap(),
			DiskUsage {
				top: Vec::new(),
				total: 0
			}
		);
	}

	#[test]
	fn du_fails_when_the_sizes_add_up_to_more_than_a_u64_holds() {
		assert!(DiskUsage::of(&[file("a", u64::MAX, 1), file("b", 1, 2)]).is_err());
	}

	#[test]
	fn du_sums_up_to_the_largest_size_a_u64_holds() {
		assert_eq!(
			DiskUsage::of(&[file("d/a", u64::MAX - 1, 1), file("d/b", 1, 2), dir("d", 3)]).unwrap(),
			DiskUsage {
				top: vec![(2, u64::MAX)],
				total: u64::MAX,
			}
		);
	}

	#[test]
	fn listing_progress_says_how_much_of_the_tree_is_downloaded() {
		assert_eq!(
			listing_progress_text(1024, Some(4096)),
			"Listing... 1 KiB of 4 KiB"
		);
		assert_eq!(listing_progress_text(1024, None), "Listing... 1 KiB");
	}
}

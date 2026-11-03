//! [cli-doc] search-queries
//! `search <query>` prints every file and directory under the working directory (or
//! `--working-path`), at any depth, whose name contains the query, ignoring case. Names and the
//! query are compared in Unicode NFC, and spaces around the query are ignored. It prints one full
//! path per line, directories first, or with `--json`
//! `{"results": [{"path", "type", "size", "uuid"}], "total", "complete"}`, where only files have a
//! `size`.
//!
//! The search reads a local cache of your drive, which it brings up to date first. If that takes
//! longer than 30 seconds, it prints what the cache holds and warns that the results may be
//! incomplete (`"complete": false` with `--json`).
//!
//! Without a query, `search` is interactive in a terminal: results appear as you type, and
//! choosing one goes to that directory, or to the directory of that file.

use std::{
	num::NonZeroUsize,
	sync::{Arc, Mutex},
	time::Duration,
};

use crate::{
	CommandResult, auth::LazyClient, commands::CommandContext, docs::DocElement, ui::UI,
	util::RemotePath,
};
use anyhow::{Context, Result, anyhow};
use clap::{Subcommand, ValueEnum};
use filen_sdk_rs::{
	cache::{
		CacheMessage, ResyncProgress, SearchConfig, SearchHit, SearchItemType, SearchResult,
		SearchSnapshot,
	},
	fs::HasUUID as _,
};
use filen_types::fs::Uuid;
use serde_json::json;
use tokio::{sync::mpsc::UnboundedReceiver, time::Instant};

#[derive(Debug, Subcommand)]
pub(crate) enum SearchCommands {
	/// Search for files and directories by name
	Search {
		/// Part of the names to look for, matched ignoring case (without it, search
		/// interactively)
		query: Option<String>,
		/// Only list files, or only directories
		#[arg(long = "type", value_name = "TYPE")]
		item_type: Option<ItemKind>,
		/// Print at most this many results
		#[arg(long, default_value_t = DEFAULT_LIMIT, requires = "query")]
		limit: NonZeroUsize,
	},
}

/// The docs' "Accessing Files" section places `search` between browsing and organizing files.
pub(crate) const DOCS: &[DocElement] = &[
	DocElement::DocFragment("search-queries"),
	DocElement::CommandHelp("search"),
];

impl SearchCommands {
	pub(super) async fn execute(self, ctx: CommandContext<'_>) -> Result<CommandResult> {
		let CommandContext {
			ui,
			client,
			working_path,
			..
		} = ctx;
		match self {
			Self::Search {
				query: Some(query),
				item_type,
				limit,
			} => {
				let config = SearchConfig::new()
					.with_name(query)
					.with_item_type(search_item_type(item_type));
				search_cmd_non_interactive(ui, client, working_path, config, limit).await?;
				Ok(CommandResult::default())
			}
			Self::Search {
				query: None,
				item_type,
				..
			} => Ok(
				search_cmd(ui, client, working_path, search_item_type(item_type))
					.await?
					.unwrap_or_default(),
			),
		}
	}
}

pub(crate) async fn search_cmd(
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	item_type: SearchItemType,
) -> Result<Option<CommandResult>> {
	let client = client.get(ui).await?;
	let working_dir_uuid = client
		.find_item_at_path(&working_path.0)
		.await
		.context("Failed to find working directory")?
		.ok_or(anyhow::anyhow!("Working directory not found"))?
		.uuid();

	let (results_tx, mut results_rx) = tokio::sync::mpsc::unbounded_channel();
	let (set_search_query_tx, mut set_search_query_rx) = tokio::sync::mpsc::unbounded_channel();
	let search_autocompleter = SearchAutocomplete {
		set_search_query_tx,
		last_set_query: String::new(),
		results: Arc::new(Mutex::new(SearchAutocompleteResults {
			results: Vec::new(),
			new_results_available: false,
		})),
	};
	let search = client
		.clone()
		.create_search(
			working_dir_uuid,
			SearchConfig::new().with_item_type(item_type),
		)
		.await
		.context("Failed to create search")?;
	let callback = Box::new(move |snapshot: SearchSnapshot| {
		if let Err(e) = results_tx.send(
			snapshot
				.results
				.iter()
				.map(|r| r.full_path())
				.collect::<Vec<_>>(),
		) {
			log::error!("Failed to handle search results: {}", e);
		}
	});
	let (_, _search_window_handle) = search.get_range(0..100, callback).await?;
	let mut prompt_task = {
		let search_autocompleter = search_autocompleter.clone();
		let working_path = working_path.clone();
		let prompt_text = if working_path.is_root() {
			String::from("Search globally:")
		} else {
			format!("Search in {}:", working_path)
		};
		ui.ensure_can_prompt(&prompt_text)?;
		let prompt_text = ui.redact_for_replay_testing(prompt_text);
		tokio::task::spawn_blocking(move || {
			inquire::Text::new(&prompt_text)
				.with_placeholder("(your query)")
				.with_help_message(
					"Results will appear as you type, ↑↓ to select, enter to navigate, esc to cancel",
				)
				.with_autocomplete(search_autocompleter.clone())
				.prompt_skippable()
		})
	};
	let search_autocompleter = search_autocompleter.clone();
	let result = loop {
		tokio::select! {
			set_search_query_rx = set_search_query_rx.recv() => {
				if let Some(query) = set_search_query_rx {
					search
							.set_config(SearchConfig::new().with_name(query).with_item_type(item_type))
							.await?;
				}
			}
			results_rx = results_rx.recv() => {
				if let Some(new_results) = results_rx {
					let mut results = search_autocompleter
						.results
						.lock()
						.map_err(|e| anyhow!("Failed to lock search results mutex: {}", e))?;
					results.results.clear();
					results.results.extend(new_results);
					results.new_results_available = true;
				}
			}
			result = &mut prompt_task => {
				break result.context("Failed to await prompt task")?.context("Failed to prompt for search")?;
			}
		}
	};
	let Some(result) = result else {
		return Ok(None);
	};
	let selected_path = working_path.navigate(&result);
	let navigate_to = match client
		.find_item_at_path(&selected_path.0)
		.await
		.context("Failed to find selected item")?
		.ok_or(UI::failure("Selected item not found"))?
	{
		filen_sdk_rs::fs::categories::NonRootFileType::Dir(_) => {
			ui.print_muted(&format!("Navigating to directory: {}", selected_path));
			selected_path
		}
		filen_sdk_rs::fs::categories::NonRootFileType::File(_) => {
			ui.print_muted(&format!(
				"Navigating to parent directory: {}",
				selected_path.parent().0
			));
			selected_path.parent()
		}
		filen_sdk_rs::fs::categories::NonRootFileType::Root(_) => {
			ui.print_muted("Navigating to root directory");
			RemotePath::new("/")
		}
	};
	Ok(Some(CommandResult {
		working_path: Some(navigate_to),
		..Default::default()
	}))
}

#[derive(Clone)]
struct SearchAutocomplete {
	set_search_query_tx: tokio::sync::mpsc::UnboundedSender<String>,
	last_set_query: String,
	results: Arc<Mutex<SearchAutocompleteResults>>,
}
struct SearchAutocompleteResults {
	results: Vec<String>,
	new_results_available: bool,
}

impl inquire::Autocomplete for SearchAutocomplete {
	fn get_suggestions(
		&mut self,
		input: &str,
	) -> std::prelude::v1::Result<Vec<String>, inquire::CustomUserError> {
		if input != self.last_set_query {
			self.set_search_query_tx
				.send(input.to_string())
				.map_err(|e| anyhow!("Failed to send search query: {}", e))?;
			self.last_set_query = input.to_string();
		}
		let mut results = self
			.results
			.lock()
			.map_err(|e| anyhow!("Failed to lock search results mutex: {}", e))?;
		results.new_results_available = false;
		Ok(results.results.clone())
	}

	fn get_completion(
		&mut self,
		_input: &str,
		highlighted_suggestion: Option<String>,
	) -> std::prelude::v1::Result<inquire::autocompletion::Replacement, inquire::CustomUserError> {
		Ok(highlighted_suggestion)
	}

	fn updated_suggestions_available(&mut self, _input: &str) -> bool {
		self.results
			.lock()
			.map(|r| r.new_results_available)
			.unwrap_or(false)
	}
}

pub(crate) async fn search_cmd_non_interactive(
	ui: &mut UI,
	lazy_client: &mut LazyClient,
	working_path: &RemotePath,
	config: SearchConfig,
	limit: NonZeroUsize,
) -> Result<()> {
	// An owned handle, so that the cache's status channel can be borrowed from the lazy client
	let client = Arc::clone(lazy_client.get(ui).await?);
	let cache_status = lazy_client
		.get_cache_callback_rx()
		.ok_or(anyhow::anyhow!("Failed to get cache callback receiver"))?;
	// What an earlier command left in the channel is about other resyncs.
	while cache_status.try_recv().is_ok() {}

	let working_dir_uuid = client
		.find_item_at_path(&working_path.0)
		.await
		.context("Failed to find working directory")?
		.ok_or(anyhow::anyhow!("Working directory not found"))?
		.uuid();
	let search = client
		.create_search(working_dir_uuid, config)
		.await
		.context("Failed to create search")?;

	let synced = wait_for_resync(
		cache_status,
		search.root_uuid(),
		Instant::now() + RESYNC_TIMEOUT,
	)
	.await;
	// Queried after the resync, so the snapshot holds everything it committed. The window
	// stays open until the results are printed.
	let (snapshot, _window) = search
		.get_range(0..limit.get(), Box::new(|_| {}))
		.await
		.context("Failed to get search results")?;

	let found = snapshot
		.results
		.iter()
		.map(|hit| FoundItem::new(working_path, hit))
		.collect::<Vec<_>>();
	print_found_items(ui, &found, snapshot.total, synced)?;

	Ok(())
}

/// How long a search waits for the cache to sync the searched directory before it prints what
/// the cache holds.
const RESYNC_TIMEOUT: Duration = Duration::from_secs(30);

/// How many results a search prints unless `--limit` says otherwise.
const DEFAULT_LIMIT: NonZeroUsize = NonZeroUsize::new(100_000).expect("is not zero");

/// Which kind of item `search --type` lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum ItemKind {
	File,
	#[value(alias = "directory")]
	Dir,
}

/// What a search with `--type item_type` (or without it) matches.
fn search_item_type(item_type: Option<ItemKind>) -> SearchItemType {
	match item_type {
		None => SearchItemType::All,
		Some(ItemKind::File) => SearchItemType::File,
		Some(ItemKind::Dir) => SearchItemType::Dir,
	}
}

/// A search result as a search prints it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FoundItem {
	/// The full path, which other commands take as it is.
	path: String,
	kind: FoundKind,
	uuid: Uuid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FoundKind {
	Directory,
	File { size: u64 },
}

impl FoundItem {
	/// `hit` of a search in `working_path`.
	fn new(working_path: &RemotePath, hit: &SearchHit) -> Self {
		let relative = hit.full_path();
		let path = if working_path.is_root() {
			format!("/{relative}")
		} else {
			format!("{working_path}/{relative}")
		};
		let kind = match &hit.result {
			SearchResult::Dir(_) => FoundKind::Directory,
			SearchResult::File(file) => FoundKind::File { size: file.size },
		};
		Self {
			path,
			kind,
			uuid: hit.result.uuid(),
		}
	}

	/// The item's object in `--json` output. Its keys are named as `stat` and `ls --json` name
	/// them.
	fn to_json(&self) -> serde_json::Value {
		match self.kind {
			FoundKind::Directory => json!({
				"path": self.path,
				"type": "directory",
				"uuid": self.uuid,
			}),
			FoundKind::File { size } => json!({
				"path": self.path,
				"type": "file",
				"size": size,
				"uuid": self.uuid,
			}),
		}
	}
}

/// Prints `found`, one path per line or as `{"results", "total", "complete"}` with `--json`,
/// then warns on stderr when they are fewer than the `total` matches or the resync did not
/// finish.
fn print_found_items(ui: &mut UI, found: &[FoundItem], total: usize, resync: Resync) -> Result<()> {
	if ui.json {
		ui.print_json(json!({
			"results": found.iter().map(FoundItem::to_json).collect::<Vec<_>>(),
			"total": total,
			"complete": resync == Resync::Converged,
		}))?;
	} else if found.is_empty() {
		if !ui.is_quiet() {
			ui.print_muted_to_stderr("No items match");
		}
	} else {
		for item in found {
			ui.print(&item.path);
		}
	}
	for warning in search_warnings(resync, found.len(), total) {
		ui.print_warning(&warning);
	}
	Ok(())
}

/// Whether the cache finished syncing the searched directory before the search printed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Resync {
	Converged,
	Unfinished,
}

/// Waits until the cache reports a resync of `root` that converged, or until `deadline`. A
/// resync that did not converge (another device held the drive lock, say) is retried by the
/// cache, so the wait goes on for the retry.
///
/// A root that is already an active sync root of the cache, or lies below one, is registered
/// without a resync, so no progress arrives and the wait runs to `deadline`, then reports the
/// results as possibly incomplete although they are not: the cache does not say that a
/// registration needed no resync. A one-off search is the only registration in its process, but
/// in the REPL one an earlier command left, if still active, has this effect. Ctrl-C cancels the
/// wait, as it cancels every command ([`crate::interrupt::cancellable`]).
async fn wait_for_resync(
	cache_status: &mut UnboundedReceiver<CacheMessage>,
	root: Uuid,
	deadline: Instant,
) -> Resync {
	let mut syncing_root = false;
	loop {
		match tokio::time::timeout_at(deadline, cache_status.recv()).await {
			Err(_elapsed) => return Resync::Unfinished,
			Ok(None) => {
				log::warn!("The cache's status channel closed during a search");
				return Resync::Unfinished;
			}
			Ok(Some(CacheMessage::ResyncProgress(ResyncProgress::Started { roots }))) => {
				syncing_root = roots.contains(&root);
			}
			Ok(Some(CacheMessage::ResyncProgress(ResyncProgress::Finished { converged }))) => {
				if syncing_root && converged {
					return Resync::Converged;
				}
				syncing_root = false;
			}
			// listing progress, errors (which the cache retries) and deleted roots
			Ok(Some(message)) => log::debug!("Cache status during a search: {message:?}"),
		}
	}
}

/// What a search says on stderr about the results it printed: `shown` of `total` matches.
fn search_warnings(resync: Resync, shown: usize, total: usize) -> Vec<String> {
	let mut warnings = Vec::new();
	if resync == Resync::Unfinished {
		warnings.push(format!(
			"The search index did not finish syncing within {} seconds, so results may be \
			 incomplete",
			RESYNC_TIMEOUT.as_secs()
		));
	}
	if total > shown {
		warnings.push(format!(
			"Showing {shown} of {total} results. Use --limit to show more"
		));
	}
	warnings
}

#[cfg(test)]
mod tests {
	use clap::Parser;
	use tokio::sync::mpsc::unbounded_channel;

	use super::*;
	use crate::ui::test_support::Printed;

	const ROOT: Uuid = Uuid::from_u128(1);
	const OTHER_ROOT: Uuid = Uuid::from_u128(2);

	fn started(roots: &[Uuid]) -> CacheMessage {
		CacheMessage::ResyncProgress(ResyncProgress::Started {
			roots: roots.to_vec(),
		})
	}

	fn finished(converged: bool) -> CacheMessage {
		CacheMessage::ResyncProgress(ResyncProgress::Finished { converged })
	}

	/// What [`wait_for_resync`] makes of `messages` with a deadline that has passed, so it
	/// returns once it has read them all.
	async fn resync_after(messages: Vec<CacheMessage>) -> Resync {
		let (sender, mut receiver) = unbounded_channel();
		for message in messages {
			sender.send(message).unwrap();
		}
		wait_for_resync(&mut receiver, ROOT, Instant::now()).await
	}

	#[tokio::test]
	async fn a_converged_resync_of_the_searched_directory_ends_the_wait() {
		assert_eq!(
			resync_after(vec![started(&[OTHER_ROOT, ROOT]), finished(true)]).await,
			Resync::Converged
		);
	}

	#[tokio::test]
	async fn a_resync_of_other_directories_does_not_end_the_wait() {
		assert_eq!(
			resync_after(vec![started(&[OTHER_ROOT]), finished(true)]).await,
			Resync::Unfinished
		);
	}

	#[tokio::test]
	async fn a_resync_that_did_not_converge_is_waited_out_for_its_retry() {
		assert_eq!(
			resync_after(vec![
				started(&[ROOT]),
				finished(false),
				started(&[ROOT]),
				finished(true)
			])
			.await,
			Resync::Converged
		);
	}

	#[tokio::test]
	async fn a_resync_that_did_not_converge_leaves_the_results_unfinished() {
		assert_eq!(
			resync_after(vec![started(&[ROOT]), finished(false)]).await,
			Resync::Unfinished
		);
	}

	#[tokio::test]
	async fn a_resync_finishing_without_its_start_is_not_the_searched_directorys() {
		assert_eq!(resync_after(vec![finished(true)]).await, Resync::Unfinished);
	}

	#[tokio::test]
	async fn a_closed_status_channel_leaves_the_results_unfinished() {
		let (sender, mut receiver) = unbounded_channel();
		drop(sender);
		let far_away = Instant::now() + Duration::from_secs(3600);

		assert_eq!(
			wait_for_resync(&mut receiver, ROOT, far_away).await,
			Resync::Unfinished
		);
	}

	#[test]
	fn all_results_of_a_converged_resync_need_no_warning() {
		assert_eq!(
			search_warnings(Resync::Converged, 3, 3),
			Vec::<String>::new()
		);
	}

	#[test]
	fn more_results_than_the_limit_are_warned_about() {
		assert_eq!(
			search_warnings(Resync::Converged, 3, 4),
			["Showing 3 of 4 results. Use --limit to show more"]
		);
	}

	#[test]
	fn an_unfinished_resync_is_warned_about() {
		assert_eq!(
			search_warnings(Resync::Unfinished, 1, 2),
			[
				"The search index did not finish syncing within 30 seconds, so results may be \
				 incomplete",
				"Showing 1 of 2 results. Use --limit to show more"
			]
		);
	}

	#[derive(Debug, Parser)]
	struct Args {
		#[command(subcommand)]
		command: SearchCommands,
	}

	#[test]
	fn the_limit_defaults_to_a_hundred_thousand_results() {
		let Args {
			command: SearchCommands::Search { limit, .. },
		} = Args::try_parse_from(["filen", "search", "a"]).unwrap();

		assert_eq!(limit.get(), 100_000);
	}

	#[test]
	fn a_limit_of_zero_is_refused() {
		assert!(Args::try_parse_from(["filen", "search", "a", "--limit", "0"]).is_err());
	}

	#[test]
	fn a_limit_without_a_query_is_refused() {
		assert_eq!(
			Args::try_parse_from(["filen", "search", "--limit", "5"])
				.unwrap_err()
				.kind(),
			clap::error::ErrorKind::MissingRequiredArgument
		);
	}

	fn found(path: &str, kind: FoundKind, uuid: u128) -> FoundItem {
		FoundItem {
			path: path.to_owned(),
			kind,
			uuid: Uuid::from_u128(uuid),
		}
	}

	fn printed_results(
		found: &[FoundItem],
		total: usize,
		resync: Resync,
		json: bool,
	) -> (String, String) {
		let (out, err) = (Printed::default(), Printed::default());
		let mut ui = Printed::split_ui(&out, &err);
		ui.json = json;
		print_found_items(&mut ui, found, total, resync).unwrap();
		(
			out.lines_or_empty(),
			console::strip_ansi_codes(&err.lines_or_empty()).into_owned(),
		)
	}

	#[test]
	fn results_print_one_full_path_per_line() {
		let results = [
			found("/dir/needle", FoundKind::Directory, 1),
			found("/dir/sub/needle.txt", FoundKind::File { size: 7 }, 2),
		];

		assert_eq!(
			printed_results(&results, 2, Resync::Converged, false),
			("/dir/needle\n/dir/sub/needle.txt".to_owned(), String::new())
		);
	}

	#[test]
	fn no_results_say_so_on_stderr() {
		assert_eq!(
			printed_results(&[], 0, Resync::Converged, false),
			(String::new(), "No items match".to_owned())
		);
	}

	#[test]
	fn json_results_give_each_items_path_type_size_and_uuid() {
		let results = [
			found("/needle", FoundKind::Directory, 1),
			found("/needle.txt", FoundKind::File { size: 7 }, 2),
		];

		let (out, err) = printed_results(&results, 3, Resync::Unfinished, true);

		assert_eq!(
			serde_json::from_str::<serde_json::Value>(&out).unwrap(),
			json!({
				"results": [
					{
						"path": "/needle",
						"type": "directory",
						"uuid": "00000000-0000-0000-0000-000000000001",
					},
					{
						"path": "/needle.txt",
						"type": "file",
						"size": 7,
						"uuid": "00000000-0000-0000-0000-000000000002",
					},
				],
				"total": 3,
				"complete": false,
			})
		);
		assert_eq!(
			err,
			"⚠ The search index did not finish syncing within 30 seconds, so results may be \
			 incomplete\n⚠ Showing 2 of 3 results. Use --limit to show more"
		);
	}

	#[test]
	fn no_json_results_are_an_empty_list() {
		let (out, err) = printed_results(&[], 0, Resync::Converged, true);

		assert_eq!(
			serde_json::from_str::<serde_json::Value>(&out).unwrap(),
			json!({ "results": [], "total": 0, "complete": true })
		);
		assert_eq!(err, "");
	}

	#[test]
	fn the_type_filter_takes_file_dir_and_directory() {
		for (arg, expected) in [
			("file", SearchItemType::File),
			("dir", SearchItemType::Dir),
			("directory", SearchItemType::Dir),
		] {
			let Args {
				command: SearchCommands::Search { item_type, .. },
			} = Args::try_parse_from(["filen", "search", "a", "--type", arg]).unwrap();

			assert_eq!(search_item_type(item_type), expected, "--type {arg}");
		}
	}

	#[test]
	fn no_type_filter_matches_every_item() {
		let Args {
			command: SearchCommands::Search { item_type, .. },
		} = Args::try_parse_from(["filen", "search", "a"]).unwrap();

		assert_eq!(search_item_type(item_type), SearchItemType::All);
	}
}

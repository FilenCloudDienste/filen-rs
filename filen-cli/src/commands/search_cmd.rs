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
use clap::Subcommand;
use filen_sdk_rs::{
	cache::{CacheMessage, ResyncProgress, SearchConfig, SearchSnapshot},
	fs::HasUUID as _,
};
use filen_types::fs::Uuid;
use serde_json::json;
use tokio::{sync::mpsc::UnboundedReceiver, time::Instant};

#[derive(Debug, Subcommand)]
pub(crate) enum SearchCommands {
	/// Search for a file or directory interactively, or specify a query
	Search {
		query: Option<String>,
		/// Print at most this many results
		#[arg(long, default_value_t = DEFAULT_LIMIT, requires = "query")]
		limit: NonZeroUsize,
	},
}

/// The docs' "Accessing Files" section places `search` between browsing and organizing files.
pub(crate) const DOCS: &[DocElement] = &[DocElement::CommandHelp("search")];

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
				limit,
			} => {
				search_cmd_non_interactive(ui, client, working_path, &query, limit).await?;
				Ok(CommandResult::default())
			}
			Self::Search { query: None, .. } => Ok(search_cmd(ui, client, working_path)
				.await?
				.unwrap_or_default()),
		}
	}
}

pub(crate) async fn search_cmd(
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
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
		.create_search(working_dir_uuid, SearchConfig::new())
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
					search.set_config(SearchConfig::new().with_name(query)).await?;
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
	query: &str,
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
		.create_search(working_dir_uuid, SearchConfig::new().with_name(query))
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

	let results = snapshot
		.results
		.iter()
		.map(|hit| hit.full_path())
		.collect::<Vec<_>>();
	if ui.json {
		ui.print_json(json!({
			"results": results,
		}))?;
	} else {
		ui.print_grid(
			results
				.iter()
				.map(|r| r.as_str())
				.collect::<Vec<_>>()
				.as_slice(),
		)
	};
	for warning in search_warnings(synced, results.len(), snapshot.total) {
		ui.print_warning(&warning);
	}

	Ok(())
}

/// How long a search waits for the cache to sync the searched directory before it prints what
/// the cache holds.
const RESYNC_TIMEOUT: Duration = Duration::from_secs(30);

/// How many results a search prints unless `--limit` says otherwise.
const DEFAULT_LIMIT: NonZeroUsize = NonZeroUsize::new(100_000).expect("is not zero");

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
}

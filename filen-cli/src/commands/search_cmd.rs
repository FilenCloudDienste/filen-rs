use std::{
	sync::{Arc, Mutex},
	time::Duration,
};

use crate::{CommandResult, auth::LazyClient, ui::UI, util::RemotePath};
use anyhow::{Context, Result, anyhow};
use filen_sdk_rs::{
	cache::{CacheMessage, ResyncProgress, SearchConfig, SearchSnapshot},
	fs::HasUUID as _,
};
use serde_json::json;

// todo: allow non-interactive search

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
	client: &mut LazyClient,
	working_path: &RemotePath,
	query: &str,
) -> Result<()> {
	let lazy_client = client;
	let client = lazy_client.get(ui).await?;

	let working_dir_uuid = client
		.find_item_at_path(&working_path.0)
		.await
		.context("Failed to find working directory")?
		.ok_or(anyhow::anyhow!("Working directory not found"))?
		.uuid();
	let search = client
		.clone()
		.create_search(working_dir_uuid, SearchConfig::new().with_name(query))
		.await
		.context("Failed to create search")?;
	let results = Arc::new(Mutex::new(Vec::new()));
	let results_ = results.clone();
	let cache_callback_rx = lazy_client
		.get_cache_callback_rx()
		.ok_or(anyhow::anyhow!("Failed to get cache callback receiver"))?;

	let (initial_snapshot, _search_window_handle) = search
		.get_range(
			0..100_000,
			Box::new(move |snapshot| {
				let mut results = match results_.lock() {
					Ok(results) => results,
					Err(e) => {
						log::warn!("Failed to lock search results mutex: {}", e);
						return;
					}
				};
				if snapshot.total > 100_000 {
					log::warn!(
						"Search results truncated to 100,000 items, total results: {}",
						snapshot.total
					);
				}
				results.clear();
				results.extend(snapshot.results.iter().map(|r| r.full_path()));
			}),
		)
		.await
		.context("Failed to get search results")?;
	// the initial snapshot is not delivered through the callback
	results
		.lock()
		.map_err(|e| anyhow!("Failed to lock search results mutex: {}", e))?
		.extend(initial_snapshot.results.iter().map(|r| r.full_path()));

	let start_time = std::time::Instant::now();
	loop {
		match cache_callback_rx.try_recv() {
			Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
				tokio::time::sleep(Duration::from_millis(1)).await;
				if start_time.elapsed() > Duration::from_secs(30) {
					return Err(anyhow!(
						"Cache resync did not finish within 30 seconds, search results may be incomplete"
					));
				}
			}
			Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
				return Err(anyhow::anyhow!("Cache callback channel disconnected"));
			}
			Ok(msg) => {
				if let CacheMessage::ResyncProgress(progress) = msg
					&& let ResyncProgress::Finished { converged } = progress
				{
					if !converged {
						return Err(anyhow!(
							"Cache resync did not converge, search results may be incomplete"
						));
					}
					break;
				}
			}
		}
	}
	tokio::time::sleep(Duration::from_millis(10)).await; // wait a bit for the last search results to arrive, not sure if global cache state callback or get_range callback is called first
	let results = results
		.lock()
		.map_err(|e| anyhow!("Failed to lock search results mutex: {}", e))?;
	if ui.json {
		ui.print_json(json!({
			"results": results.clone(),
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

	Ok(())
}

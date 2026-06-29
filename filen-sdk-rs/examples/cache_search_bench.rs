//! Benchmark the cache-backed search end to end against a real account: how long the initial
//! population takes (drive-locked listing download vs. cache insertion) and how long actual
//! search queries take once the cache is warm.
//!
//! Authenticates with an API key (no 2FA code needed — `/v3/login` is never called):
//!
//! ```sh
//! cargo run -p filen-sdk-rs --release --features cache --example cache_search_bench -- \
//!     <email> <password> <api_key> </remote/path/to/dir> [needle]
//! ```
//!
//! The cache DB is a fresh temp file per run (cold cache), removed afterwards.

use std::{
	sync::{Arc, Mutex},
	time::{Duration, Instant},
};

use filen_sdk_rs::{
	auth::{http::ClientConfig, unauth::UnauthClient},
	cache::{CacheMessage, ResyncProgress, SearchConfig},
	fs::{HasUUID, categories::NonRootFileType},
};

#[derive(Debug)]
struct ProgressLog {
	started: Option<Instant>,
	first_listing: Option<Instant>,
	last_listing: Option<Instant>,
	bytes_downloaded: u64,
	applying: Option<Instant>,
	finished: Option<(Instant, bool)>,
}

fn ms(duration: Duration) -> String {
	format!("{:.1} ms", duration.as_secs_f64() * 1000.0)
}

#[tokio::main]
async fn main() {
	let args: Vec<String> = std::env::args().collect();
	let [_, email, password, api_key, remote_path, rest @ ..] = args.as_slice() else {
		eprintln!(
			"usage: cache_search_bench <email> <password> <api_key> </remote/path/to/dir> [needle]"
		);
		std::process::exit(2);
	};
	let needle = rest.first().map(String::as_str).unwrap_or("a");

	let unauth = UnauthClient::from_config(ClientConfig::default()).expect("client config");

	let t = Instant::now();
	let client = Arc::new(
		unauth
			.login_with_api_key(email.clone(), password, api_key.clone())
			.await
			.expect("login_with_api_key failed"),
	);
	println!("login (api key):            {}", ms(t.elapsed()));

	let t = Instant::now();
	let dir_uuid = if remote_path == "/" || remote_path.is_empty() {
		uuid::Uuid::from(client.root().uuid())
	} else {
		let item = client
			.find_item_at_path(remote_path)
			.await
			.expect("path resolution failed")
			.unwrap_or_else(|| panic!("nothing found at {remote_path}"));
		match item {
			NonRootFileType::Dir(dir) => uuid::Uuid::from(dir.uuid()),
			NonRootFileType::Root(root) => uuid::Uuid::from(root.uuid()),
			NonRootFileType::File(_) => {
				panic!("{remote_path} is a file; the search root must be a directory")
			}
		}
	};
	println!("path -> uuid:               {}", ms(t.elapsed()));

	// Fresh DB per run = cold cache; the resync that populates it is what we are measuring.
	let db_path =
		std::env::temp_dir().join(format!("filen-search-bench-{}.db", uuid::Uuid::new_v4()));
	let progress: Arc<Mutex<ProgressLog>> = Arc::new(Mutex::new(ProgressLog {
		started: None,
		first_listing: None,
		last_listing: None,
		bytes_downloaded: 0,
		applying: None,
		finished: None,
	}));
	let progress_writer = progress.clone();
	client
		.configure_cache(db_path.clone(), move |messages| {
			let now = Instant::now();
			let mut log = progress_writer.lock().unwrap();
			for message in messages {
				match message {
					CacheMessage::ResyncProgress(ResyncProgress::Started { roots })
						// The startup resync of an empty cache has no roots; the populate
						// resync (the one we measure) lists ours.
						if !roots.is_empty() =>
					{
						log.started.get_or_insert(now);
					}
					CacheMessage::ResyncProgress(ResyncProgress::Listing {
						bytes_downloaded, ..
					}) => {
						tracing::info!(
							"cache resync listing progress: {} bytes downloaded",
							bytes_downloaded
						);
						log.first_listing.get_or_insert(now);
						log.last_listing = Some(now);
						log.bytes_downloaded = bytes_downloaded;
					}
					CacheMessage::ResyncProgress(ResyncProgress::Applying) => {
						if log.started.is_some() {
							log.applying.get_or_insert(now);
						}
					}
					CacheMessage::ResyncProgress(ResyncProgress::Finished { converged }) => {
						if log.started.is_some() {
							log.finished.get_or_insert((now, converged));
						}
					}
					CacheMessage::Error(errors) => {
						for error in errors {
							eprintln!("cache error: {error}");
						}
					}
					_ => {}
				}
			}
		})
		.await
		.expect("configure_cache failed");

	// create_search registers the root (remote validation + worker spawn) and returns BEFORE
	// the populate resync; the progress log above brackets the resync itself.
	let t_create = Instant::now();
	let search = client
		.clone()
		.create_search(dir_uuid, SearchConfig::new())
		.await
		.expect("create_search failed");
	println!("create_search (ack):        {}", ms(t_create.elapsed()));

	let resync_deadline = Instant::now() + Duration::from_secs(600);
	let (finished_at, converged) = loop {
		if let Some(finished) = progress.lock().unwrap().finished {
			break finished;
		}
		if Instant::now() > resync_deadline {
			panic!("resync did not finish within 10 minutes");
		}
		tokio::time::sleep(Duration::from_millis(20)).await;
	};

	{
		let log = progress.lock().unwrap();
		let started = log.started.expect("resync never reported Started");
		let applying = log.applying.expect("resync never reported Applying");
		println!(
			"resync: lock + listing:     {}  ({} bytes downloaded{})",
			ms(applying - started),
			log.bytes_downloaded,
			log.first_listing
				.zip(log.last_listing)
				.map(|(first, last)| format!(
					"; first byte after {}, transfer {}",
					ms(first - started),
					ms(last - first)
				))
				.unwrap_or_default(),
		);
		println!(
			"resync: cache insertion:    {}  (diff + commit + dispatch)",
			ms(finished_at - applying)
		);
		println!(
			"resync: total (converged={converged}): {}",
			ms(finished_at - started)
		);
	}

	// Cold DB: everything cached under the root came from this run's listing, so the
	// unfiltered total IS the listed item count (the root's own node is not a result).
	let t = Instant::now();
	let (snapshot, count_window) = search
		.get_range(0..1, Box::new(|_| {}))
		.await
		.expect("count query failed");
	println!(
		"items listed (dirs+files):  {}  (counted in {})",
		snapshot.total,
		ms(t.elapsed()),
	);
	drop(count_window);

	// The engine itself: a filter change re-runs the filtered count over the whole scope, and
	// get_range hydrates a window of full results.
	let t = Instant::now();
	search
		.set_config(SearchConfig::new().with_name(needle))
		.await
		.expect("set_config failed");
	println!("search: filter \"{needle}\" (count): {}", ms(t.elapsed()));

	let t = Instant::now();
	let (snapshot, window) = search
		.get_range(0..50, Box::new(|_| {}))
		.await
		.expect("get_range failed");
	println!(
		"search: get_range(0..50):   {}  ({} of {} matches hydrated)",
		ms(t.elapsed()),
		snapshot.results.len(),
		snapshot.total,
	);
	println!(
		"search: sample results:     {:#?}",
		snapshot
			.results
			.iter()
			.map(|r| format!(
				"parent_path: {}, name: {}",
				r.parent_path(),
				r.result.name()
			))
			.collect::<Vec<_>>()
	);

	drop(window);
	search.close().await;
	client.flush_cache().await;
	for suffix in ["", "-wal", "-shm"] {
		let _ = std::fs::remove_file(db_path.with_file_name(format!(
			"{}{suffix}",
			db_path.file_name().unwrap().to_string_lossy()
		)));
	}
}

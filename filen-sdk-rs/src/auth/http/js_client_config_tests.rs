use std::time::Duration;

use super::{ClientConfig, JsClientConfig, SharedClientState};

fn base() -> JsClientConfig {
	JsClientConfig {
		concurrency: None,
		rate_limit_per_sec: None,
		upload_bandwidth_kilobytes_per_sec: None,
		download_bandwidth_kilobytes_per_sec: None,
		log_level: None,
		file_io_memory_budget: None,
		thumbnail_mem_budget: None,
		thumbnail_max_source_bytes: None,
		thumbnail_decode_concurrency: None,
		archive_codec_mem_budget: None,
		archive_job_concurrency: None,
		batch_window_ms: None,
	}
}

#[test]
fn archive_settings_reach_the_client_config() {
	let config = ClientConfig::from(JsClientConfig {
		archive_codec_mem_budget: Some(512 << 20),
		archive_job_concurrency: Some(3),
		..base()
	});
	assert_eq!(
		(
			config.archive_codec_mem_budget,
			config.archive_job_concurrency
		),
		(512 << 20, 3)
	);
	let defaults = ClientConfig::from(base());
	let platform = ClientConfig::default();
	assert_eq!(
		(
			defaults.archive_codec_mem_budget,
			defaults.archive_job_concurrency
		),
		(
			platform.archive_codec_mem_budget,
			platform.archive_job_concurrency
		),
		"left out, both stay the platform's"
	);
}

/// concurrency 0 (a natural "unlimited" assumption from FFI callers) must not become a
/// zero-permit limiter that makes every request await its permit forever.
#[test]
fn zero_concurrency_is_clamped_to_one() {
	let config = ClientConfig::from(JsClientConfig {
		concurrency: Some(0),
		..base()
	});
	assert_eq!(
		config.concurrency, 1,
		"concurrency 0 must be clamped to 1, not left as a zero-permit (hang-forever) limiter"
	);
}

/// An oversized memory budget must be clamped instead of panicking tokio's Semaphore::new,
/// which rejects any permit count above MAX_PERMITS (usize::MAX >> 3).
#[test]
fn huge_memory_budget_is_clamped_to_semaphore_max() {
	let config = ClientConfig::from(JsClientConfig {
		file_io_memory_budget: Some(u64::MAX),
		..base()
	});
	assert_eq!(
		config.file_io_memory_budget,
		tokio::sync::Semaphore::MAX_PERMITS,
		"an oversized budget must be clamped to Semaphore::MAX_PERMITS to avoid a panic"
	);
}

#[test]
fn a_batch_window_left_out_is_five_milliseconds() {
	assert_eq!(
		ClientConfig::from(base()).batch_window,
		Duration::from_millis(5)
	);
}

#[test]
fn a_batch_window_of_zero_turns_batching_off() {
	let config = ClientConfig::from(JsClientConfig {
		batch_window_ms: Some(0),
		..base()
	});
	assert_eq!(config.batch_window, Duration::ZERO);
}

#[test]
fn a_batch_window_in_milliseconds_reaches_the_client_config() {
	let config = ClientConfig::from(JsClientConfig {
		batch_window_ms: Some(20),
		..base()
	});
	assert_eq!(config.batch_window, Duration::from_millis(20));
}

#[test]
fn a_batch_window_above_one_second_counts_as_one_second() {
	let state = SharedClientState::new(ClientConfig::from(JsClientConfig {
		batch_window_ms: Some(u32::MAX),
		..base()
	}))
	.expect("the defaults are valid");
	assert_eq!(state.batch_window(), Duration::from_secs(1));
}

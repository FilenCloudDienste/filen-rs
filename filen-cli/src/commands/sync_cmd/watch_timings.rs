//! `--debounce-ms` and `--safety-net-secs`: the timings a continuous sync watches on.

use std::time::Duration;

use filen_sdk_rs::sync_engine::WatchConfig;

use crate::error::CliError;

/// The longest `--debounce-ms` takes: a minute, so that a change is never held back longer.
const MAX_DEBOUNCE_MS: u64 = 60 * 1000;
/// The longest `--safety-net-secs` takes: a day.
const MAX_SAFETY_NET_SECS: u64 = 24 * 60 * 60;

/// Parses `--debounce-ms`: a whole number of milliseconds from 1 to [`MAX_DEBOUNCE_MS`].
pub(super) fn parse_debounce_ms(value: &str) -> Result<Duration, String> {
	parse_in_range(value, MAX_DEBOUNCE_MS).map(Duration::from_millis)
}

/// Parses `--safety-net-secs`: a whole number of seconds from 1 to [`MAX_SAFETY_NET_SECS`].
pub(super) fn parse_safety_net_secs(value: &str) -> Result<Duration, String> {
	parse_in_range(value, MAX_SAFETY_NET_SECS).map(Duration::from_secs)
}

fn parse_in_range(value: &str, max: u64) -> Result<u64, String> {
	let expected = || format!("expected a whole number from 1 to {max}");
	let n = value.parse::<u64>().map_err(|_| expected())?;
	if n == 0 || n > max {
		return Err(expected());
	}
	Ok(n)
}

/// The watch timings, the defaults where no flag gives one.
///
/// Fails, as a usage error, when the safety net would not be longer than the debounce: the safety
/// net would then fire before a burst of changes could ever settle.
pub(super) fn watch_config(
	debounce: Option<Duration>,
	safety_net: Option<Duration>,
) -> Result<WatchConfig, CliError> {
	let default = WatchConfig::default();
	let config = WatchConfig {
		debounce: debounce.unwrap_or(default.debounce),
		safety_net: safety_net.unwrap_or(default.safety_net),
	};
	if config.safety_net <= config.debounce {
		return Err(CliError::Usage(format!(
			"The safety net ({} s) must be longer than the debounce ({} ms). Give a longer \
			 --safety-net-secs or a shorter --debounce-ms.",
			config.safety_net.as_secs(),
			config.debounce.as_millis(),
		)));
	}
	Ok(config)
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use filen_sdk_rs::sync_engine::WatchConfig;

	use super::{parse_debounce_ms, parse_safety_net_secs, watch_config};
	use crate::error::CliError;

	#[test]
	fn the_debounce_is_a_number_of_milliseconds_from_1_to_a_minute() {
		assert_eq!(parse_debounce_ms("1"), Ok(Duration::from_millis(1)));
		assert_eq!(
			parse_debounce_ms("60000"),
			Ok(Duration::from_millis(60_000))
		);
		for value in ["0", "60001", "-1", "1.5", "", "abc"] {
			assert_eq!(
				parse_debounce_ms(value),
				Err("expected a whole number from 1 to 60000".to_owned()),
				"{value}"
			);
		}
	}

	#[test]
	fn the_safety_net_is_a_number_of_seconds_from_1_to_a_day() {
		assert_eq!(parse_safety_net_secs("1"), Ok(Duration::from_secs(1)));
		assert_eq!(
			parse_safety_net_secs("86400"),
			Ok(Duration::from_secs(86_400))
		);
		for value in ["0", "86401", "18446744073709551616"] {
			assert_eq!(
				parse_safety_net_secs(value),
				Err("expected a whole number from 1 to 86400".to_owned()),
				"{value}"
			);
		}
	}

	#[test]
	fn without_flags_the_watch_runs_on_the_engines_defaults() {
		assert_eq!(watch_config(None, None).unwrap(), WatchConfig::default());
	}

	#[test]
	fn a_flag_replaces_only_its_own_timing() {
		let default = WatchConfig::default();
		assert_eq!(
			watch_config(Some(Duration::from_millis(50)), None).unwrap(),
			WatchConfig {
				debounce: Duration::from_millis(50),
				safety_net: default.safety_net,
			}
		);
		assert_eq!(
			watch_config(None, Some(Duration::from_secs(60))).unwrap(),
			WatchConfig {
				debounce: default.debounce,
				safety_net: Duration::from_secs(60),
			}
		);
	}

	#[test]
	fn a_safety_net_no_longer_than_the_debounce_is_a_usage_error() {
		let err = watch_config(
			Some(Duration::from_millis(2000)),
			Some(Duration::from_secs(2)),
		)
		.unwrap_err();
		assert!(matches!(err, CliError::Usage(_)));
		assert_eq!(
			err.to_string(),
			"The safety net (2 s) must be longer than the debounce (2000 ms). Give a longer \
			 --safety-net-secs or a shorter --debounce-ms."
		);
		assert!(
			watch_config(
				Some(Duration::from_millis(2001)),
				Some(Duration::from_secs(3))
			)
			.is_ok()
		);
	}
}

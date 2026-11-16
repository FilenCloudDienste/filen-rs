//! The helper's logs: plain text on stderr, which the desktop app captures and redacts. Stdout
//! carries the protocol alone, so this subscriber is installed before anything else logs: the
//! cache installs one writing to stdout when none is installed yet.

use tracing::level_filters::LevelFilter;
use tracing_subscriber::EnvFilter;

/// The crates whose logs pass at the helper's level; every other crate passes at WARN.
const FIRST_PARTY: [&str; 4] = [
	"filen_cloud_files",
	"filen_sdk_rs",
	"filen_mobile_native_cache",
	"filen_types",
];

/// The filter for `level`: first-party crates at it, everything else at WARN, as the SDK's own
/// logger filters.
fn directives(level: LevelFilter) -> String {
	let mut directives = LevelFilter::WARN.min(level).to_string().to_lowercase();
	for target in FIRST_PARTY {
		directives.push_str(&format!(",{target}={}", level.to_string().to_lowercase()));
	}
	directives
}

/// Installs the helper's logger on stderr, without ANSI colours, filtered by `RUST_LOG` when it is
/// set and by `level` otherwise; `false` when a logger was installed already.
pub fn install_stderr_logging(level: LevelFilter) -> bool {
	let filter =
		EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(directives(level)));
	tracing_subscriber::fmt()
		.with_writer(std::io::stderr)
		.with_ansi(false)
		.with_env_filter(filter)
		.try_init()
		.is_ok()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn first_party_crates_log_at_the_level_and_the_rest_at_warn() {
		assert_eq!(
			directives(LevelFilter::INFO),
			"warn,filen_cloud_files=info,filen_sdk_rs=info,filen_mobile_native_cache=info,\
			 filen_types=info"
		);
		assert_eq!(
			directives(LevelFilter::DEBUG),
			"warn,filen_cloud_files=debug,filen_sdk_rs=debug,filen_mobile_native_cache=debug,\
			 filen_types=debug"
		);
	}

	#[test]
	fn a_level_below_warn_quietens_every_crate() {
		assert_eq!(
			directives(LevelFilter::ERROR),
			"error,filen_cloud_files=error,filen_sdk_rs=error,filen_mobile_native_cache=error,\
			 filen_types=error"
		);
	}
}

//! The binding types the rclone module's own types cannot be: paths and a checksum cross as
//! strings.

use std::path::PathBuf;

use filen_macros::js_type;

use crate::{
	Error, ErrorKind,
	rclone::{RcloneBinary, RcloneDaemonConfig, VfsTuning},
};

/// Mirror of [`RcloneDaemonConfig`], with the rclone binary the app ships.
#[js_type(import, no_ser, napi)]
pub struct RcloneDaemonOptions {
	/// The rclone binary the app ships.
	pub binary_path: String,
	/// The binary's SHA-256, in hex, checked on every start. `None` where the platform's code
	/// signing rewrites the binary after the build (macOS, Windows) and the app checks the
	/// signature itself.
	pub binary_sha256: Option<String>,
	/// A directory only the daemon uses, for its own state. Created (0700 on Unix) if missing.
	pub state_dir: String,
	/// rclone's cache directory, whose VFS cache holds what is not uploaded yet. Created (0700 on
	/// Unix) if missing.
	pub cache_dir: String,
	/// rclone's log file, rotated at 10 MB with three old ones kept. Without one, rclone's output
	/// goes to the SDK's log.
	pub log_file: Option<String>,
	/// The user agent rclone sends.
	pub user_agent: String,
	/// The VFS cache's limits, the same for every mount and serve.
	pub vfs: VfsTuning,
}

impl TryFrom<RcloneDaemonOptions> for RcloneDaemonConfig {
	type Error = Error;

	fn try_from(options: RcloneDaemonOptions) -> Result<Self, Error> {
		let RcloneDaemonOptions {
			binary_path,
			binary_sha256,
			state_dir,
			cache_dir,
			log_file,
			user_agent,
			vfs,
		} = options;
		let sha256 = binary_sha256
			.map(hex::decode_to_array::<_, 32>)
			.transpose()
			.map_err(|e| {
				Error::custom_with_source(
					ErrorKind::Conversion,
					e,
					Some("the rclone binary's SHA-256 is not 64 hex digits"),
				)
			})?;
		Ok(Self {
			binary: RcloneBinary::Bundled {
				path: PathBuf::from(binary_path),
				sha256,
			},
			state_dir: PathBuf::from(state_dir),
			cache_dir: PathBuf::from(cache_dir),
			log_file: log_file.map(PathBuf::from),
			user_agent,
			vfs,
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn options(binary_sha256: Option<&str>) -> RcloneDaemonOptions {
		RcloneDaemonOptions {
			binary_path: "/opt/filen/resources/rclone/rclone".to_owned(),
			binary_sha256: binary_sha256.map(str::to_owned),
			state_dir: "/home/user/.config/filen/mounts/1/state".to_owned(),
			cache_dir: "/home/user/.config/filen/mounts/1/cache".to_owned(),
			log_file: None,
			user_agent: "filen-desktop/4.0.0".to_owned(),
			vfs: VfsTuning::default(),
		}
	}

	#[test]
	fn options_become_the_daemon_config_of_a_bundled_binary() {
		let sha256 = "ab".repeat(32);

		let config = RcloneDaemonConfig::try_from(options(Some(&sha256))).unwrap();

		assert_eq!(
			config,
			RcloneDaemonConfig {
				binary: RcloneBinary::Bundled {
					path: PathBuf::from("/opt/filen/resources/rclone/rclone"),
					sha256: Some([0xab; 32]),
				},
				state_dir: PathBuf::from("/home/user/.config/filen/mounts/1/state"),
				cache_dir: PathBuf::from("/home/user/.config/filen/mounts/1/cache"),
				log_file: None,
				user_agent: "filen-desktop/4.0.0".to_owned(),
				vfs: VfsTuning::default(),
			}
		);
	}

	#[test]
	fn a_checksum_that_is_not_64_hex_digits_is_refused() {
		for sha256 in ["ab".repeat(31), "zz".repeat(32), "ab".repeat(33)] {
			let error = RcloneDaemonConfig::try_from(options(Some(&sha256))).unwrap_err();
			assert_eq!(error.kind(), ErrorKind::Conversion, "{sha256}");
		}
	}
}

//! The VFS options every mount and serve gets. rclone shares one VFS (and its cache) between every
//! role on the same remote only when their options are identical, so they all come from here: a
//! mount takes them as rc's `vfsOpt`, a serve as its command-line flags.

use std::time::Duration;

use serde_json::{Map, Value, json};

/// The VFS cache's limits, which a user may tune. Every other VFS option is fixed. The defaults
/// are the old desktop client's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VfsTuning {
	/// The most the VFS cache keeps on disk, in bytes; files not in use are evicted beyond it.
	pub cache_max_size: u64,
	/// The disk space, in bytes, the VFS cache leaves free; files not in use are evicted below it.
	pub cache_min_free_space: u64,
	/// How long, in seconds, a file not in use stays in the VFS cache.
	pub cache_max_age_secs: u64,
	/// How long, in seconds, a directory listing is cached before it is read again.
	pub dir_cache_time_secs: u32,
}

impl Default for VfsTuning {
	fn default() -> Self {
		Self {
			cache_max_size: 32 * GIB,
			cache_min_free_space: 16 * GIB,
			cache_max_age_secs: 720 * 60 * 60,
			dir_cache_time_secs: 15,
		}
	}
}

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

/// How long a written file waits in the cache before it is uploaded.
const WRITE_BACK: Duration = Duration::from_secs(5);
/// The VFS's read tuning, the old desktop client's values.
const READ_CHUNK_SIZE: u64 = 64 * MIB;
const READ_CHUNK_STREAMS: u32 = 8;
const READ_AHEAD: u64 = 128 * MIB;
const FILE_PERMS: u32 = 0o666;
const DIR_PERMS: u32 = 0o777;

/// rc's `vfsOpt` for `tuning`: a full disk cache, plus the fixed options. Sizes are bytes and
/// durations nanoseconds, as rc reads them; values past rclone's `int64` are capped there.
pub(crate) fn vfs_opt(tuning: &VfsTuning, read_only: bool) -> Value {
	json!({
		"CacheMode": "full",
		"CacheMaxSize": int64(tuning.cache_max_size),
		"CacheMinFreeSpace": int64(tuning.cache_min_free_space),
		"CacheMaxAge": nanos(Duration::from_secs(tuning.cache_max_age_secs)),
		"DirCacheTime": nanos(Duration::from_secs(tuning.dir_cache_time_secs.into())),
		"WriteBack": nanos(WRITE_BACK),
		"ChunkSize": int64(READ_CHUNK_SIZE),
		"ChunkStreams": READ_CHUNK_STREAMS,
		"ReadAhead": int64(READ_AHEAD),
		"FilePerms": FILE_PERMS,
		"DirPerms": DIR_PERMS,
		"NoChecksum": true,
		"ReadOnly": read_only,
	})
}

/// [`vfs_opt`] as `serve/start` takes it: the serve's command-line flags, without their `--` and
/// with `_` for `-`, every value a string. rclone parses these into the same options as the
/// mount's `vfsOpt`, so a serve and a mount on one remote share its VFS: sizes in bytes (with the
/// `B` suffix, as a bare number is read as KiB), durations in nanoseconds, permissions in octal.
pub(crate) fn vfs_flags(tuning: &VfsTuning, read_only: bool) -> Map<String, Value> {
	let flags = [
		("vfs_cache_mode", "full".to_owned()),
		("vfs_cache_max_size", bytes(tuning.cache_max_size)),
		(
			"vfs_cache_min_free_space",
			bytes(tuning.cache_min_free_space),
		),
		(
			"vfs_cache_max_age",
			duration(Duration::from_secs(tuning.cache_max_age_secs)),
		),
		(
			"dir_cache_time",
			duration(Duration::from_secs(tuning.dir_cache_time_secs.into())),
		),
		("vfs_write_back", duration(WRITE_BACK)),
		("vfs_read_chunk_size", bytes(READ_CHUNK_SIZE)),
		("vfs_read_chunk_streams", READ_CHUNK_STREAMS.to_string()),
		("vfs_read_ahead", bytes(READ_AHEAD)),
		("file_perms", format!("{FILE_PERMS:04o}")),
		("dir_perms", format!("{DIR_PERMS:04o}")),
		("no_checksum", true.to_string()),
		("read_only", read_only.to_string()),
	];
	flags
		.into_iter()
		.map(|(flag, value)| (flag.to_owned(), Value::String(value)))
		.collect()
}

fn bytes(n: u64) -> String {
	format!("{}B", int64(n))
}

fn duration(duration: Duration) -> String {
	format!("{}ns", nanos(duration))
}

fn int64(n: u64) -> i64 {
	i64::try_from(n).unwrap_or(i64::MAX)
}

fn nanos(duration: Duration) -> i64 {
	i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
	use serde_json::{Value, json};

	use super::{VfsTuning, vfs_flags, vfs_opt};

	#[test]
	fn vfs_opt_matches_the_old_tuning() {
		assert_eq!(
			vfs_opt(&VfsTuning::default(), false),
			json!({
				"CacheMode": "full",
				"CacheMaxSize": 34_359_738_368_i64,
				"CacheMinFreeSpace": 17_179_869_184_i64,
				"CacheMaxAge": 2_592_000_000_000_000_i64,
				"DirCacheTime": 15_000_000_000_i64,
				"WriteBack": 5_000_000_000_i64,
				"ChunkSize": 67_108_864,
				"ChunkStreams": 8,
				"ReadAhead": 134_217_728,
				"FilePerms": 438,
				"DirPerms": 511,
				"NoChecksum": true,
				"ReadOnly": false,
			})
		);
	}

	#[test]
	fn a_read_only_role_gets_a_read_only_vfs() {
		assert_eq!(
			vfs_opt(&VfsTuning::default(), true)["ReadOnly"],
			json!(true)
		);
	}

	#[test]
	fn values_past_int64_are_capped() {
		let tuning = VfsTuning {
			cache_max_size: u64::MAX,
			cache_min_free_space: 0,
			cache_max_age_secs: u64::MAX,
			dir_cache_time_secs: u32::MAX,
		};

		let opt = vfs_opt(&tuning, false);

		assert_eq!(opt["CacheMaxSize"], json!(i64::MAX));
		assert_eq!(opt["CacheMaxAge"], json!(i64::MAX));
		assert_eq!(opt["DirCacheTime"], json!(4_294_967_295_000_000_000_i64));
	}

	#[test]
	fn vfs_flags_spell_the_same_options() {
		assert_eq!(
			Value::Object(vfs_flags(&VfsTuning::default(), false)),
			json!({
				"vfs_cache_mode": "full",
				"vfs_cache_max_size": "34359738368B",
				"vfs_cache_min_free_space": "17179869184B",
				"vfs_cache_max_age": "2592000000000000ns",
				"dir_cache_time": "15000000000ns",
				"vfs_write_back": "5000000000ns",
				"vfs_read_chunk_size": "67108864B",
				"vfs_read_chunk_streams": "8",
				"vfs_read_ahead": "134217728B",
				"file_perms": "0666",
				"dir_perms": "0777",
				"no_checksum": "true",
				"read_only": "false",
			})
		);
	}

	#[test]
	fn vfs_flags_cap_values_past_int64_as_vfs_opt_does() {
		let tuning = VfsTuning {
			cache_max_size: u64::MAX,
			cache_min_free_space: 0,
			cache_max_age_secs: u64::MAX,
			dir_cache_time_secs: 1,
		};

		let flags = vfs_flags(&tuning, true);

		assert_eq!(flags["vfs_cache_max_size"], json!("9223372036854775807B"));
		assert_eq!(flags["vfs_cache_max_age"], json!("9223372036854775807ns"));
		assert_eq!(flags["read_only"], json!("true"));
	}
}

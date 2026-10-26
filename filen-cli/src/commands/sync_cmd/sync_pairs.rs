use std::{
	borrow::Cow,
	path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use filen_sdk_rs::{
	auth::Client,
	fs::{
		HasUUID as _,
		categories::{NonRootFileType, NonRootItemType},
	},
	sync_engine::{PairRecord, SyncEngine, SyncMode},
};
use serde::Deserialize;

use crate::{ui::UI, util::RemotePath};

/// Adds a sync pair to `engine` for each of `pair_definitions`, which are either `<local>:<remote>` (two-way),
/// `<local>:<mode>:<remote>`, or the path to a syncPairs.yaml file. Relative remote paths are resolved
/// against `working_path`, relative local paths in a file against the file's directory.
pub(crate) async fn register_sync_pairs(
	engine: &SyncEngine,
	client: &Client,
	working_path: &RemotePath,
	pair_definitions: &Vec<String>,
) -> Result<()> {
	for pair in pair_definitions {
		let path = Path::new(pair);
		let pair_definitions = if path.is_file() {
			// if it is a file, read it as a syncPairs.yaml file
			read_sync_pairs_file(path)?
		} else {
			// otherwise, parse the sync pair definition
			vec![parse_sync_pair_definition(pair)?]
		};
		for (local, mode, remote) in pair_definitions {
			// resolve remote root
			let remote_path = working_path.navigate(&remote);
			let remote_uuid = match client
				.find_item_at_path(&remote_path.0)
				.await
				.context("Failed to find remote directory")?
			{
				Some(NonRootFileType::Dir(dir)) => dir.uuid(),
				Some(NonRootFileType::Root(root)) => root.uuid(),
				Some(NonRootFileType::File(_)) => {
					return Err(UI::failure(&format!(
						"{} is a file, not a directory",
						remote_path
					)));
				}
				None => {
					return Err(UI::failure(&format!(
						"Remote directory {} not found",
						remote_path
					)));
				}
			};
			engine
				.add_pair(local.clone(), remote_uuid, mode)
				.await
				.map_err(|e| {
					UI::failure(&format!(
						"Failed to set up the sync pair {} <> {remote_path}: {e}",
						local.display()
					))
				})?;
		}
	}
	Ok(())
}

/// Parses `<local>:<remote>` or `<local>:<mode>:<remote>`.
fn parse_sync_pair_definition(pair: &str) -> Result<(PathBuf, SyncMode, String)> {
	// (the split is taken from the right, so a local path with a colon in it (a Windows drive letter) stays whole)
	let (rest, remote) = pair.rsplit_once(':').ok_or_else(|| {
		UI::failure(&format!(
			"Invalid sync pair \"{pair}\": expected <local>:<remote>, <local>:<mode>:<remote> or the path to a sync pairs file"
		))
	})?;
	let (local, mode) = match rest.rsplit_once(':') {
		Some((local, mode)) if let Some(mode) = parse_sync_mode_str(mode) => (local, mode),
		_ => (rest, SyncMode::TwoWay),
	};
	Ok((PathBuf::from(local), mode, remote.to_owned()))
}

/// Reads a syncPairs.yaml file and returns the list of sync pairs it defines, with local paths resolved relative to the file's directory.
fn read_sync_pairs_file(path: &Path) -> Result<Vec<(PathBuf, SyncMode, String)>> {
	let content = std::fs::read_to_string(path)
		.with_context(|| format!("Failed to read sync pairs file {}", path.display()))?;
	let file: SyncPairsFile = serde_yaml::from_str(&content)
		.with_context(|| format!("Failed to parse sync pairs file {}", path.display()))?;
	let base = path.parent().unwrap_or(Path::new(""));
	file.sync_pairs
		.into_iter()
		.map(|entry| {
			let mode = match entry.mode.as_deref() {
				None => SyncMode::TwoWay,
				Some(mode) => parse_sync_mode_str(mode).ok_or_else(|| {
					UI::failure(&format!(
						"Invalid sync mode \"{mode}\" in {}",
						path.display()
					))
				})?,
			};
			Ok((base.join(entry.local), mode, entry.remote))
		})
		.collect()
}

/// syncPairs.yaml file format
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SyncPairsFile {
	sync_pairs: Vec<SyncPairEntry>,
}

#[derive(Deserialize)]
struct SyncPairEntry {
	local: PathBuf,
	remote: String,
	mode: Option<String>,
}

/// Accepts the SDK's mode names in camelCase, and the "cloud" names the TypeScript CLI used.
fn parse_sync_mode_str(mode: &str) -> Option<SyncMode> {
	Some(match mode {
		"twoWay" => SyncMode::TwoWay,
		"localToRemote" | "localToCloud" => SyncMode::LocalToRemote,
		"remoteToLocal" | "cloudToLocal" => SyncMode::RemoteToLocal,
		"localBackup" => SyncMode::LocalBackup,
		"remoteBackup" | "cloudBackup" => SyncMode::RemoteBackup,
		_ => return None,
	})
}

/// The name of `mode` as [`parse_sync_mode_str`] accepts it.
fn sync_mode_str(mode: SyncMode) -> &'static str {
	match mode {
		SyncMode::TwoWay => "twoWay",
		SyncMode::LocalToRemote => "localToRemote",
		SyncMode::RemoteToLocal => "remoteToLocal",
		SyncMode::LocalBackup => "localBackup",
		SyncMode::RemoteBackup => "remoteBackup",
	}
}

/// A pair as `<local> <(mode)> <remote path>`, for messages.
pub(crate) async fn format_sync_pair(client: &Client, pair: &PairRecord) -> Result<String> {
	let remote_dir = client
		.get_dir(pair.remote_root)
		.await
		.context("Failed to get the remote directory of a sync pair")?;
	let (remote_path, _) = client
		.get_item_path(&NonRootItemType::Dir(Cow::Owned(remote_dir)))
		.await
		.context("Failed to get the remote path of a sync pair")?;
	Ok(format!(
		"{} <({})> {}",
		pair.local_root,
		sync_mode_str(pair.mode),
		// a directory's path ends with `/`
		remote_path.trim_end_matches('/')
	))
}

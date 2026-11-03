use std::{
	borrow::Cow,
	collections::HashMap,
	fmt::Display,
	path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use filen_sdk_rs::{
	auth::Client,
	fs::{
		HasUUID as _,
		categories::{NonRootFileType, NonRootItemType},
	},
	sync_engine::{DeleteGuard, PairId, PairRecord, SyncEngine, SyncMode},
};
use serde::Deserialize;

use crate::{ui::UI, util::RemotePath};

/// Adds a sync pair to `engine` for each of `pair_definitions`, which are either `<local>:<remote>` (two-way),
/// `<local>:<mode>:<remote>`, or the path to a syncPairs.yaml file. Relative remote paths are resolved
/// against `working_path`, relative local paths in a file against the file's directory.
///
/// Returns the ids of the pairs registered, each once, in the order they were given.
pub(crate) async fn register_sync_pairs(
	engine: &SyncEngine,
	client: &Client,
	working_path: &RemotePath,
	pair_definitions: &Vec<String>,
) -> Result<Vec<PairId>> {
	let mut pair_ids = Vec::new();
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
			let pair = engine
				.add_pair(local.clone(), remote_uuid, mode)
				.await
				.map_err(|e| {
					UI::failure(&format!(
						"Failed to set up the sync pair {} <> {remote_path}: {e}",
						local.display()
					))
				})?;
			// Set on every run, not only when the pair is new: earlier versions stored a guard that
			// held every deletion, and a pair they registered keeps it until it is overwritten.
			engine
				.set_delete_guard(pair, DeleteGuard::default())
				.await
				.with_context(|| {
					format!(
						"Failed to set the delete guard of the sync pair {} <> {remote_path}",
						local.display()
					)
				})?;
			// the same pair given twice is registered (and watched) once
			if !pair_ids.contains(&pair) {
				pair_ids.push(pair);
			}
		}
	}
	Ok(pair_ids)
}

/// The records of the pairs `pair_ids` names, in that order, out of every pair `registered`
/// holds, which includes the pairs earlier runs registered.
pub(crate) fn records_of(
	mut registered: Vec<PairRecord>,
	pair_ids: &[PairId],
) -> Result<Vec<PairRecord>> {
	pair_ids
		.iter()
		.map(|id| {
			let index = registered
				.iter()
				.position(|pair| pair.id == *id)
				.with_context(|| format!("Sync pair {id} is not registered"))?;
			Ok(registered.swap_remove(index))
		})
		.collect()
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

/// The label of each of `pairs`, by id, for the lines the sync prints: `<local> <(mode)> <remote
/// path>`, read from the server once, before the sync starts. A pair whose remote path cannot be
/// read is named by its remote root's uuid instead, and a warning is logged.
pub(crate) async fn pair_labels(client: &Client, pairs: &[PairRecord]) -> HashMap<PairId, String> {
	futures::future::join_all(pairs.iter().map(async |pair| {
		let label = match format_sync_pair(client, pair).await {
			Ok(label) => label,
			Err(e) => {
				log::warn!(
					"Failed to read the remote path of sync pair {}, naming it by its uuid: {e:#}",
					pair.id
				);
				fallback_label(pair)
			}
		};
		(pair.id, label)
	}))
	.await
	.into_iter()
	.collect()
}

/// A pair as `<local> <(mode)> <remote path>`, for messages.
async fn format_sync_pair(client: &Client, pair: &PairRecord) -> Result<String> {
	let remote_dir = client
		.get_dir(pair.remote_root)
		.await
		.context("Failed to get the remote directory of a sync pair")?;
	let (remote_path, _) = client
		.get_item_path(&NonRootItemType::Dir(Cow::Owned(remote_dir)))
		.await
		.context("Failed to get the remote path of a sync pair")?;
	// a directory's path ends with `/`
	Ok(label(pair, remote_path.trim_end_matches('/')))
}

/// A pair as `<local> <(mode)> <remote root uuid>`, for when its remote path cannot be read.
fn fallback_label(pair: &PairRecord) -> String {
	label(pair, &pair.remote_root)
}

fn label(pair: &PairRecord, remote: &(impl Display + ?Sized)) -> String {
	format!(
		"{} <({})> {remote}",
		pair.local_root,
		sync_mode_str(pair.mode)
	)
}

#[cfg(test)]
mod tests {
	use filen_sdk_rs::sync_engine::{DeleteGuard, PairRecord, SyncMode};

	use super::{fallback_label, records_of};

	fn record(id: i64) -> PairRecord {
		PairRecord {
			id,
			local_root: format!("/local/{id}"),
			remote_root: Default::default(),
			mode: SyncMode::TwoWay,
			delete_guard: DeleteGuard::default(),
			paused: false,
			first_pass_completed: true,
		}
	}

	/// The database keeps every pair any run registered; a run watches only the pairs it was given.
	#[test]
	fn a_run_takes_only_the_pairs_it_registered() {
		let registered = vec![record(1), record(2), record(3), record(4)];

		let ids = records_of(registered, &[4, 2])
			.unwrap()
			.into_iter()
			.map(|pair| pair.id)
			.collect::<Vec<_>>();

		assert_eq!(ids, [4, 2]);
	}

	/// A pair whose remote path cannot be read is still named, by its remote root's uuid.
	#[test]
	fn a_pair_without_a_remote_path_is_named_by_its_remote_uuid() {
		let pair = PairRecord {
			remote_root: "00000000-0000-0000-0000-000000001234".parse().unwrap(),
			mode: SyncMode::LocalBackup,
			..record(7)
		};

		assert_eq!(
			fallback_label(&pair),
			"/local/7 <(localBackup)> 00000000-0000-0000-0000-000000001234"
		);
	}

	#[test]
	fn a_pair_missing_from_the_database_is_an_error() {
		let error = records_of(vec![record(1)], &[1, 2]).unwrap_err();

		assert_eq!(error.to_string(), "Sync pair 2 is not registered");
	}
}

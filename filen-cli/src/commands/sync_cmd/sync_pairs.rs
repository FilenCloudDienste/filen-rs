use std::{
	borrow::Cow,
	collections::HashMap,
	fmt::Display,
	path::{Path, PathBuf},
	slice,
};

use anyhow::{Context, Result};
use filen_sdk_rs::{
	auth::Client,
	fs::{
		HasUUID as _,
		categories::{NonRootFileType, NonRootItemType},
	},
	sync_engine::{DeleteGuard, PairId, PairOverlap, PairRecord, SyncEngine, SyncMode},
};
use filen_types::fs::Uuid;
use serde::Deserialize;

use crate::{ui::UI, util::RemotePath};

/// A sync pair to register: its local root, its mode and the remote directory it syncs with.
pub(crate) struct PairDefinition {
	local: PathBuf,
	mode: SyncMode,
	remote_path: RemotePath,
	remote_root: Uuid,
}

/// The pairs `pair_definitions` define, each of which is either `<local>:<remote>` (two-way),
/// `<local>:<mode>:<remote>`, or the path to a syncPairs.yaml file, with each remote root found.
/// Relative remote paths are resolved against `working_path`, relative local paths in a file
/// against the file's directory.
///
/// Every definition is read and every remote root found before any pair is registered, so a bad
/// definition fails the run before anything is stored.
pub(crate) async fn resolve_sync_pairs(
	client: &Client,
	working_path: &RemotePath,
	pair_definitions: &[String],
) -> Result<Vec<PairDefinition>> {
	let mut parsed = Vec::new();
	for pair in pair_definitions {
		let path = Path::new(pair);
		if path.is_file() {
			// if it is a file, read it as a syncPairs.yaml file
			parsed.extend(read_sync_pairs_file(path)?);
		} else {
			// otherwise, parse the sync pair definition
			parsed.push(parse_sync_pair_definition(pair)?);
		}
	}
	let mut pairs = Vec::with_capacity(parsed.len());
	for (local, mode, remote) in parsed {
		let remote_path = working_path.navigate(&remote);
		let remote_root = match client
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
		pairs.push(PairDefinition {
			local,
			mode,
			remote_path,
			remote_root,
		});
	}
	Ok(pairs)
}

/// Adds each of `pairs` to `engine`, or finds it there from an earlier run.
///
/// Returns the ids of the pairs registered, each once, in the order they were given. A pair the
/// engine refuses fails the run, but the pairs registered before it stay registered, as every
/// pair does after the run that set it up.
pub(crate) async fn register_sync_pairs(
	engine: &SyncEngine,
	client: &Client,
	pairs: &[PairDefinition],
) -> Result<Vec<PairId>> {
	let mut pair_ids = Vec::new();
	for PairDefinition {
		local,
		mode,
		remote_path,
		remote_root,
	} in pairs
	{
		// the engine takes the path, which the messages below still name
		let pair = match engine.add_pair(local.clone(), *remote_root, *mode).await {
			Ok(pair) => pair,
			Err(e) => {
				let message = match e.downcast_ref::<PairOverlap>() {
					Some(overlap) => {
						let other = other_pair_label(engine, client, overlap_pair(overlap)).await;
						overlap_message(local, remote_path, overlap, &other)
					}
					None => format!(
						"Failed to set up the sync pair {} <> {remote_path}: {e}",
						local.display()
					),
				};
				return Err(UI::failure(&message));
			}
		};
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
	Ok(pair_ids)
}

/// The registered pair `overlap` names.
fn overlap_pair(overlap: &PairOverlap) -> PairId {
	match overlap {
		PairOverlap::LocalRootInUse { pair, .. }
		| PairOverlap::LocalRootNested { pair, .. }
		| PairOverlap::LocalRootContains { pair, .. }
		| PairOverlap::RemoteRootInUse { pair, .. }
		| PairOverlap::RemoteRootNested { pair, .. }
		| PairOverlap::RemoteRootContains { pair, .. } => *pair,
	}
}

/// The label of the registered pair `pair`, or its id should it not be found.
async fn other_pair_label(engine: &SyncEngine, client: &Client, pair: PairId) -> String {
	let record = match engine.list_pairs().await {
		Ok(pairs) => pairs.into_iter().find(|record| record.id == pair),
		Err(e) => {
			log::warn!("Failed to list the sync pairs: {e}");
			None
		}
	};
	match record {
		Some(record) => pair_labels(client, slice::from_ref(&record))
			.await
			.remove(&pair)
			.unwrap_or_else(|| fallback_label(&record)),
		None => format!("with the id {pair}"),
	}
}

/// Why the pair `local` <> `remote` cannot be registered, as `overlap` says, naming the
/// registered pair `other` it overlaps.
fn overlap_message(
	local: &Path,
	remote: &RemotePath,
	overlap: &PairOverlap,
	other: &str,
) -> String {
	let relation = match overlap {
		PairOverlap::LocalRootInUse { .. } => "its local folder is already synced by",
		PairOverlap::LocalRootNested { .. } => "its local folder is inside the local folder of",
		PairOverlap::LocalRootContains { .. } => "its local folder contains the local folder of",
		PairOverlap::RemoteRootInUse { .. } => "its remote folder is already synced by",
		PairOverlap::RemoteRootNested { .. } => "its remote folder is inside the remote folder of",
		PairOverlap::RemoteRootContains { .. } => "its remote folder contains the remote folder of",
	};
	format!(
		"Cannot set up the sync pair {} <> {remote}: {relation} the sync pair {other}, and sync \
		 pairs may not overlap. A sync pair stays registered after the run that set it up.",
		local.display()
	)
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

	use std::path::Path;

	use filen_sdk_rs::sync_engine::PairOverlap;

	use super::{fallback_label, overlap_message, records_of};
	use crate::util::RemotePath;

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

	/// A pair that overlaps a registered one names both, and what the overlap is.
	#[test]
	fn an_overlapping_pair_names_both_pairs() {
		let overlap = PairOverlap::LocalRootNested {
			pair: 3,
			existing: "/home/me".to_string(),
		};

		assert_eq!(
			overlap_message(
				Path::new("/home/me/docs"),
				&RemotePath::new("/docs"),
				&overlap,
				"/home/me <(twoWay)> /backup",
			),
			"Cannot set up the sync pair /home/me/docs <> /docs: its local folder is inside the \
			 local folder of the sync pair /home/me <(twoWay)> /backup, and sync pairs may not \
			 overlap. A sync pair stays registered after the run that set it up."
		);
	}

	#[test]
	fn a_pair_missing_from_the_database_is_an_error() {
		let error = records_of(vec![record(1)], &[1, 2]).unwrap_err();

		assert_eq!(error.to_string(), "Sync pair 2 is not registered");
	}
}

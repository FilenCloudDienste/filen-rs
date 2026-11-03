use std::{
	borrow::Cow,
	collections::{HashMap, HashSet},
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

/// A sync pair as its definition gives it: its local root, its mode and its remote path, which
/// may be relative.
pub(crate) type ParsedPair = (PathBuf, SyncMode, String);

/// The pairs `pair_definitions` define, each of which is either `<local>:<remote>` (two-way),
/// `<local>:<mode>:<remote>`, or the path to a syncPairs.yaml file. Without definitions, the pairs
/// of the default sync pairs file in `config_dir`. Relative local paths in a file are resolved
/// against the file's directory.
pub(crate) fn parse_sync_pairs(
	config_dir: &Path,
	pair_definitions: &[String],
) -> Result<Vec<ParsedPair>> {
	if pair_definitions.is_empty() {
		return read_default_sync_pairs(config_dir);
	}
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
	Ok(parsed)
}

/// The pairs `parsed` holds, with each remote root found. Relative remote paths are resolved
/// against `working_path`.
///
/// Every remote root is found before any pair is registered, so a bad definition fails the run
/// before anything is stored.
pub(crate) async fn resolve_sync_pairs(
	client: &Client,
	working_path: &RemotePath,
	parsed: Vec<ParsedPair>,
) -> Result<Vec<PairDefinition>> {
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
/// engine refuses fails the run, and the pairs this call added before it are removed again, so a
/// failed run registers nothing; the pairs earlier runs registered stay as they were.
pub(crate) async fn register_sync_pairs(
	engine: &SyncEngine,
	client: &Client,
	pairs: &[PairDefinition],
) -> Result<Vec<PairId>> {
	let earlier = engine
		.list_pairs()
		.await
		.context("Failed to list sync pairs")?
		.into_iter()
		.map(|pair| pair.id)
		.collect::<HashSet<_>>();
	let mut pair_ids = Vec::new();
	if let Err(e) = add_sync_pairs(engine, client, pairs, &mut pair_ids).await {
		for pair in pair_ids.into_iter().filter(|pair| !earlier.contains(pair)) {
			if let Err(remove) = engine.remove_pair(pair).await {
				log::warn!("Failed to remove the sync pair {pair} after the run failed: {remove}");
			}
		}
		return Err(e);
	}
	Ok(pair_ids)
}

/// Adds each of `pairs` to `engine`, pushing the id of each onto `pair_ids` as soon as the engine
/// has it, so that a failure leaves there every pair added so far.
async fn add_sync_pairs(
	engine: &SyncEngine,
	client: &Client,
	pairs: &[PairDefinition],
	pair_ids: &mut Vec<PairId>,
) -> Result<()> {
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
		// the same pair given twice is registered (and watched) once
		if !pair_ids.contains(&pair) {
			pair_ids.push(pair);
		}
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
	}
	Ok(())
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
fn parse_sync_pair_definition(pair: &str) -> Result<ParsedPair> {
	// (the split is taken from the right, so a local path with a colon in it (a Windows drive letter) stays whole)
	let (rest, remote) = pair.rsplit_once(':').ok_or_else(|| {
		UI::usage_failure(&format!(
			"Invalid sync pair \"{pair}\": expected <local>:<remote>, <local>:<mode>:<remote> or the path to a sync pairs file"
		))
	})?;
	let (local, mode) = match rest.rsplit_once(':') {
		Some((local, token)) => match parse_sync_mode_str(token) {
			Some(mode) => (local, mode),
			// `C:dir`, a path on the drive C:, is no mode
			None if is_mode_token(token) && !is_drive_letter(local) => {
				return Err(UI::usage_failure(&format!(
					"Invalid sync mode \"{token}\" in the sync pair \"{pair}\": expected one of {}",
					sync_mode_names()
				)));
			}
			// a colon in the local path, as in `C:\dir`
			None => (rest, SyncMode::TwoWay),
		},
		None => (rest, SyncMode::TwoWay),
	};
	Ok((PathBuf::from(local), mode, remote.to_owned()))
}

/// Whether `token`, the part of a sync pair between its last two colons, is meant as a mode: a
/// word, which a path with a separator in it is not.
fn is_mode_token(token: &str) -> bool {
	!token.is_empty() && token.chars().all(|c| c.is_ascii_alphabetic())
}

/// Whether `s` is a drive letter, as the `C` of `C:dir` is.
fn is_drive_letter(s: &str) -> bool {
	let mut chars = s.chars();
	matches!((chars.next(), chars.next()), (Some(letter), None) if letter.is_ascii_alphabetic())
}

/// The sync pairs files in the config directory that `sync` reads when it is given no pairs, in
/// the order it looks for them. It reads the first that exists.
const DEFAULT_SYNC_PAIRS_FILES: &[&str] = &["syncPairs.yaml"];

/// The pairs of the first of [`DEFAULT_SYNC_PAIRS_FILES`] that exists in `config_dir`.
fn read_default_sync_pairs(config_dir: &Path) -> Result<Vec<ParsedPair>> {
	let candidates = DEFAULT_SYNC_PAIRS_FILES
		.iter()
		.map(|name| config_dir.join(name))
		.collect::<Vec<_>>();
	match candidates.iter().find(|path| path.is_file()) {
		Some(path) => read_sync_pairs_file(path),
		None => Err(UI::usage_failure(&no_sync_pairs_message(&candidates))),
	}
}

/// Why `sync` without pairs cannot run when none of the default sync pairs files, `candidates`,
/// exists, and how to give it pairs.
fn no_sync_pairs_message(candidates: &[PathBuf]) -> String {
	let paths = candidates
		.iter()
		.map(|path| path.display().to_string())
		.collect::<Vec<_>>()
		.join(" or ");
	format!(
		"No sync pairs given, and there is no sync pairs file at {paths}.\n\
		 Give each sync pair as <local>:<remote> or <local>:<mode>:<remote>, as in \
		 `filen sync --pair /home/me/docs:/docs`, or the path to a sync pairs file, \
		 or create that file like this:\n\
		 syncPairs:\n  \
		 - local: /home/me/docs\n    \
		 remote: /docs\n    \
		 mode: twoWay"
	)
}

/// Reads a syncPairs.yaml file and returns the list of sync pairs it defines, with local paths resolved relative to the file's directory.
fn read_sync_pairs_file(path: &Path) -> Result<Vec<ParsedPair>> {
	let content = std::fs::read_to_string(path).map_err(|e| {
		UI::failure(&format!(
			"Failed to read sync pairs file {}: {e}",
			path.display()
		))
	})?;
	let file: SyncPairsFile = serde_yaml::from_str(&content).map_err(|e| {
		UI::failure(&format!(
			"Failed to parse sync pairs file {}: {e}",
			path.display()
		))
	})?;
	if file.sync_pairs.is_empty() {
		return Err(UI::failure(&format!(
			"The sync pairs file {} defines no sync pairs",
			path.display()
		)));
	}
	let base = path.parent().unwrap_or(Path::new(""));
	file.sync_pairs
		.into_iter()
		.map(|entry| {
			let mode = match entry.mode.as_deref() {
				None => SyncMode::TwoWay,
				Some(mode) => parse_sync_mode_str(mode).ok_or_else(|| {
					UI::failure(&format!(
						"Invalid sync mode \"{mode}\" in {}: expected one of {}",
						path.display(),
						sync_mode_names()
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

/// Every mode with the names it is given by: the SDK's name in camelCase first, then the "cloud"
/// name and the short token the TypeScript CLI used.
const SYNC_MODE_NAMES: &[(SyncMode, &[&str])] = &[
	(SyncMode::TwoWay, &["twoWay", "tw"]),
	(
		SyncMode::LocalToRemote,
		&["localToRemote", "localToCloud", "ltc"],
	),
	(
		SyncMode::RemoteToLocal,
		&["remoteToLocal", "cloudToLocal", "ctl"],
	),
	(SyncMode::LocalBackup, &["localBackup", "lb"]),
	(
		SyncMode::RemoteBackup,
		&["remoteBackup", "cloudBackup", "cb"],
	),
];

/// The mode `mode` names, by any of its [`SYNC_MODE_NAMES`].
fn parse_sync_mode_str(mode: &str) -> Option<SyncMode> {
	SYNC_MODE_NAMES
		.iter()
		.find(|(_, names)| names.contains(&mode))
		.map(|(mode, _)| *mode)
}

/// Every name of [`SYNC_MODE_NAMES`], for messages.
fn sync_mode_names() -> String {
	SYNC_MODE_NAMES
		.iter()
		.flat_map(|(_, names)| names.iter().copied())
		.collect::<Vec<_>>()
		.join(", ")
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
	use std::path::{Path, PathBuf};

	use filen_sdk_rs::sync_engine::{DeleteGuard, PairOverlap, PairRecord, SyncMode};

	use super::{
		fallback_label, overlap_message, parse_sync_pair_definition, parse_sync_pairs,
		read_sync_pairs_file, records_of,
	};
	use crate::{
		error::{CliError, exit_code, find_cli_error},
		util::RemotePath,
	};

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

	/// Without pairs and without a sync pairs file, there is nothing to sync, and the message says
	/// where the file goes and what it holds.
	#[test]
	fn no_pairs_and_no_sync_pairs_file_is_a_usage_error() {
		let config_dir = tempfile::tempdir().unwrap();

		let error = parse_sync_pairs(config_dir.path(), &[]).unwrap_err();

		assert_eq!(exit_code(&error), 2);
		let message = error.to_string();
		let path = config_dir.path().join("syncPairs.yaml");
		assert!(
			message.starts_with(&format!(
				"No sync pairs given, and there is no sync pairs file at {}.\n",
				path.display()
			)),
			"{message}"
		);
		assert!(
			message.ends_with(
				"syncPairs:\n  - local: /home/me/docs\n    remote: /docs\n    mode: twoWay"
			),
			"{message}"
		);
	}

	/// Without pairs, the pairs of the sync pairs file in the config dir.
	#[test]
	fn no_pairs_reads_the_sync_pairs_file_in_the_config_dir() {
		let config_dir = tempfile::tempdir().unwrap();
		std::fs::write(
			config_dir.path().join("syncPairs.yaml"),
			"syncPairs:\n  - local: docs\n    remote: /docs\n",
		)
		.unwrap();

		let pairs = parse_sync_pairs(config_dir.path(), &[]).unwrap();

		assert_eq!(
			pairs,
			[(
				config_dir.path().join("docs"),
				SyncMode::TwoWay,
				"/docs".to_owned()
			)]
		);
	}

	/// A word between the local path and the remote path that names no mode is reported, rather
	/// than taken as part of the local path.
	#[test]
	fn an_unknown_mode_is_a_usage_error() {
		let error = parse_sync_pair_definition("/home/me/docs:twoway:/docs").unwrap_err();

		assert_eq!(exit_code(&error), 2);
		assert_eq!(
			error.to_string(),
			"Invalid sync mode \"twoway\" in the sync pair \"/home/me/docs:twoway:/docs\": \
			 expected one of twoWay, tw, localToRemote, localToCloud, ltc, remoteToLocal, \
			 cloudToLocal, ctl, localBackup, lb, remoteBackup, cloudBackup, cb"
		);
	}

	#[test]
	fn a_local_path_on_a_windows_drive_keeps_its_drive_letter() {
		assert_eq!(
			parse_sync_pair_definition(r"C:\Users\me:/docs").unwrap(),
			(
				PathBuf::from(r"C:\Users\me"),
				SyncMode::TwoWay,
				"/docs".to_owned()
			)
		);
	}

	#[test]
	fn a_local_path_on_a_windows_drive_can_have_a_mode() {
		assert_eq!(
			parse_sync_pair_definition(r"C:\Users\me:localBackup:/docs").unwrap(),
			(
				PathBuf::from(r"C:\Users\me"),
				SyncMode::LocalBackup,
				"/docs".to_owned()
			)
		);
	}

	/// `C:docs` is the directory `docs` in the current directory of the drive C:, not a mode.
	#[test]
	fn a_drive_relative_local_path_is_no_mode() {
		assert_eq!(
			parse_sync_pair_definition("C:docs:/docs").unwrap(),
			(
				PathBuf::from("C:docs"),
				SyncMode::TwoWay,
				"/docs".to_owned()
			)
		);
	}

	#[test]
	fn a_pair_without_a_colon_is_a_usage_error() {
		let error = parse_sync_pair_definition("/home/me/docs").unwrap_err();

		assert_eq!(exit_code(&error), 2);
		assert_eq!(
			error.to_string(),
			"Invalid sync pair \"/home/me/docs\": expected <local>:<remote>, \
			 <local>:<mode>:<remote> or the path to a sync pairs file"
		);
	}

	/// The short tokens of the TypeScript CLI name the modes their long names do.
	#[test]
	fn a_short_mode_token_names_its_mode() {
		let modes = ["tw", "ltc", "ctl", "lb", "cb"].map(|token| {
			parse_sync_pair_definition(&format!("/l:{token}:/r"))
				.unwrap()
				.1
		});

		assert_eq!(
			modes,
			[
				SyncMode::TwoWay,
				SyncMode::LocalToRemote,
				SyncMode::RemoteToLocal,
				SyncMode::LocalBackup,
				SyncMode::RemoteBackup,
			]
		);
	}

	/// The sync pairs file at `name` in a new directory, which holds `content`.
	fn sync_pairs_file(name: &str, content: &str) -> (tempfile::TempDir, PathBuf) {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join(name);
		std::fs::write(&path, content).unwrap();
		(dir, path)
	}

	/// A sync pairs file that is not YAML of its shape is the user's to fix, not a bug.
	#[test]
	fn a_sync_pairs_file_without_a_remote_is_a_failure() {
		let (_dir, path) = sync_pairs_file("pairs.yaml", "syncPairs:\n  - local: /home/me/docs\n");

		let error = read_sync_pairs_file(&path).unwrap_err();

		assert!(matches!(find_cli_error(&error), Some(CliError::Failure(_))));
		let message = error.to_string();
		assert!(
			message.starts_with(&format!(
				"Failed to parse sync pairs file {}: syncPairs[0]: missing field `remote`",
				path.display()
			)),
			"{message}"
		);
	}

	#[test]
	fn a_sync_pairs_file_that_cannot_be_read_is_a_failure() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("missing.yaml");

		let error = read_sync_pairs_file(&path).unwrap_err();

		assert!(matches!(find_cli_error(&error), Some(CliError::Failure(_))));
		assert!(
			error.to_string().starts_with(&format!(
				"Failed to read sync pairs file {}: ",
				path.display()
			)),
			"{error}"
		);
	}

	/// A file without pairs syncs nothing, which is not what whoever wrote it meant.
	#[test]
	fn a_sync_pairs_file_without_pairs_is_a_failure() {
		let (_dir, path) = sync_pairs_file("pairs.yaml", "syncPairs: []\n");

		let error = read_sync_pairs_file(&path).unwrap_err();

		assert!(matches!(find_cli_error(&error), Some(CliError::Failure(_))));
		assert_eq!(
			error.to_string(),
			format!(
				"The sync pairs file {} defines no sync pairs",
				path.display()
			)
		);
	}
}

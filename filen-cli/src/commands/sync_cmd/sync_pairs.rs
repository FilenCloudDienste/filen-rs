use std::{
	borrow::Cow,
	collections::{HashMap, HashSet},
	fmt::Display,
	io,
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

use super::ignore_file::{self, EXCLUDE_DOT_FILES_RULES};
use crate::{ui::UI, util::RemotePath};

/// A sync pair to register: its local root, its mode, the remote directory it syncs with and the
/// ignore rules to add to its root's `.filenignore`.
pub(crate) struct PairDefinition {
	local: PathBuf,
	mode: SyncMode,
	remote_path: RemotePath,
	remote_root: Uuid,
	ignore: Vec<String>,
}

/// A sync pair as its definition gives it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ParsedPair {
	local: PathBuf,
	mode: SyncMode,
	/// The remote path, which may be relative.
	remote: String,
	/// The name a sync pairs file gives the pair by, which `sync <alias>` selects it with.
	alias: Option<String>,
	/// The ignore rules to add to the `.filenignore` at the local root.
	ignore: Vec<String>,
	/// Whether a sync pairs file asks for the local trash to be turned off, which the CLI cannot do
	/// and warns about (see [`local_trash_warnings`]).
	disable_local_trash: bool,
}

/// The pairs `pair_definitions` define, each of which is either `<local>:<remote>` (two-way),
/// `<local>:<mode>:<remote>`, the path to a sync pairs file, or the alias of a pair in the default
/// sync pairs file in `dirs`. Without definitions, the pairs of that default file. Relative
/// local paths in a file are resolved against the file's directory.
pub(crate) fn parse_sync_pairs(
	dirs: &SyncDirs<'_>,
	pair_definitions: &[String],
) -> Result<Vec<ParsedPair>> {
	if pair_definitions.is_empty() {
		return read_default_sync_pairs(dirs);
	}
	let mut parsed = Vec::new();
	// the pairs of the default file, read for the first alias given
	let mut aliased = None;
	for pair in pair_definitions {
		let path = Path::new(pair);
		if path.is_file() {
			parsed.extend(read_sync_pairs_file(path)?);
		} else if is_alias(pair) {
			let aliased = match &mut aliased {
				Some(aliased) => aliased,
				None => aliased.insert(read_aliased_pairs(dirs, pair)?),
			};
			parsed.push(pair_by_alias(aliased, pair)?);
		} else {
			parsed.push(parse_sync_pair_definition(pair)?);
		}
	}
	Ok(parsed)
}

/// A warning for each of `parsed` whose sync pairs file asks for `disableLocalTrash`: the sync
/// engine always keeps what a pass deletes or replaces locally, for a fixed 30 days.
pub(crate) fn local_trash_warnings(parsed: &[ParsedPair]) -> Vec<String> {
	parsed
		.iter()
		.filter(|pair| pair.disable_local_trash)
		.map(|pair| {
			format!(
				"The sync pair {} asks for disableLocalTrash, which is not supported: the local \
				 files a sync deletes or replaces are kept in {} for 30 days.",
				pair.local.display(),
				pair.local.join(".filen-sync-trash").display()
			)
		})
		.collect()
}

/// Whether `definition`, which names no file, is meant as an alias: a word, without the `:` of
/// a sync pair or the separator of a path.
fn is_alias(definition: &str) -> bool {
	!definition.is_empty()
		&& !definition.contains(':')
		&& !definition.contains(std::path::is_separator)
}

/// The pairs of the default sync pairs file in `dirs`, to look `alias` up among.
fn read_aliased_pairs(dirs: &SyncDirs<'_>, alias: &str) -> Result<Vec<ParsedPair>> {
	let candidates = default_sync_pairs_files(dirs);
	match candidates.iter().find(|path| path.is_file()) {
		Some(path) => read_sync_pairs_file(path),
		None => Err(UI::usage_failure(&format!(
			"Unknown sync pair alias: {alias}. There is no sync pairs file at {} to look it up in.",
			display_paths(&candidates)
		))),
	}
}

/// The pair of `pairs`, those of the default sync pairs file, that `alias` names.
fn pair_by_alias(pairs: &[ParsedPair], alias: &str) -> Result<ParsedPair> {
	pairs
		.iter()
		.find(|pair| pair.alias.as_deref() == Some(alias))
		// cloned, not taken: the same alias may be given twice
		.cloned()
		.ok_or_else(|| UI::usage_failure(&format!("Unknown sync pair alias: {alias}")))
}

/// The pairs `parsed` holds, with each remote root found. Relative remote paths are resolved
/// against `working_path`. A local or remote root that does not exist is created once the user
/// confirms it (or passed `--yes`).
///
/// Every root is looked up, and every missing one asked about, before any is created, and all of
/// them before any pair is registered, so a bad definition or a "no" fails the run before anything
/// is created or stored. A root that several pairs share is asked about and created once.
pub(crate) async fn resolve_sync_pairs(
	ui: &mut UI,
	client: &Client,
	working_path: &RemotePath,
	parsed: Vec<ParsedPair>,
) -> Result<Vec<PairDefinition>> {
	let mut found = Vec::with_capacity(parsed.len());
	// each remote root once, however many pairs share it
	let mut remote_roots = HashMap::new();
	for pair in parsed {
		let remote_path = working_path.navigate(&pair.remote);
		let remote_root = match remote_roots.get(&remote_path) {
			Some(remote_root) => *remote_root,
			None => {
				let remote_root = find_remote_root(client, &remote_path).await?;
				remote_roots.insert(remote_path.clone(), remote_root);
				remote_root
			}
		};
		let local_exists = local_root_exists(&pair.local)?;
		found.push((pair, remote_path, remote_root, local_exists));
	}
	let (missing_locals, missing_remotes) = missing_roots(&found);
	for local in &missing_locals {
		confirm_local_root(ui, local)?;
	}
	for remote_path in &missing_remotes {
		confirm_remote_root(ui, remote_path)?;
	}
	for local in missing_locals {
		create_local_root(ui, local)?;
	}
	let mut created_remotes = HashMap::with_capacity(missing_remotes.len());
	for remote_path in missing_remotes {
		let remote_root = create_remote_root(ui, client, remote_path).await?;
		created_remotes.insert(remote_path.clone(), remote_root);
	}
	let mut pairs = Vec::with_capacity(found.len());
	for (
		ParsedPair {
			local,
			mode,
			// resolved into `remote_path`
			remote: _,
			// it selected the pair, and is done with
			alias: _,
			ignore,
			// warned about before logging in
			disable_local_trash: _,
		},
		remote_path,
		remote_root,
		// created above
		_,
	) in found
	{
		let remote_root = remote_root
			.or_else(|| created_remotes.get(&remote_path).copied())
			.with_context(|| {
				format!(
					"Remote directory {remote_path} was neither found nor created (should be impossible)"
				)
			})?;
		pairs.push(PairDefinition {
			local,
			mode,
			remote_path,
			remote_root,
			ignore,
		});
	}
	Ok(pairs)
}

/// The local and the remote roots that `found`, each pair with its remote path, its remote root
/// if it exists and whether its local root does, misses, each once in the order first given.
fn missing_roots(
	found: &[(ParsedPair, RemotePath, Option<Uuid>, bool)],
) -> (Vec<&Path>, Vec<&RemotePath>) {
	let mut locals = Vec::new();
	let mut remotes = Vec::new();
	for (pair, remote_path, remote_root, local_exists) in found {
		if !local_exists && !locals.contains(&pair.local.as_path()) {
			locals.push(pair.local.as_path());
		}
		if remote_root.is_none() && !remotes.contains(&remote_path) {
			remotes.push(remote_path);
		}
	}
	(locals, remotes)
}

/// The uuid of the remote directory at `remote_path`, or `None` if there is nothing there.
async fn find_remote_root(client: &Client, remote_path: &RemotePath) -> Result<Option<Uuid>> {
	match client
		.find_item_at_path(&remote_path.0)
		.await
		.context("Failed to find remote directory")?
	{
		Some(NonRootFileType::Dir(dir)) => Ok(Some(dir.uuid())),
		Some(NonRootFileType::Root(root)) => Ok(Some(root.uuid())),
		Some(NonRootFileType::File(_)) => Err(UI::failure(&format!(
			"{remote_path} is a file, not a directory"
		))),
		None => Ok(None),
	}
}

/// Whether the local root `local` exists. One that exists but is not a directory is an error.
fn local_root_exists(local: &Path) -> Result<bool> {
	match std::fs::metadata(local) {
		Ok(metadata) if metadata.is_dir() => Ok(true),
		Ok(_) => Err(UI::failure(&format!(
			"{} is not a directory",
			local.display()
		))),
		Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
		Err(e) => Err(UI::failure(&format!(
			"Failed to read the local directory {}: {e}",
			local.display()
		))),
	}
}

/// Asks whether to create the missing local root `local`, and fails unless the answer is yes.
fn confirm_local_root(ui: &mut UI, local: &Path) -> Result<()> {
	if ui.prompt_confirm(
		&format!(
			"Local directory {} does not exist. Create it?",
			local.display()
		),
		true,
	)? {
		Ok(())
	} else {
		Err(UI::failure(&format!(
			"Local directory {} not found",
			local.display()
		)))
	}
}

/// Creates the missing local root `local`, with every missing directory above it, which the user
/// confirmed with [`confirm_local_root`].
fn create_local_root(ui: &mut UI, local: &Path) -> Result<()> {
	std::fs::create_dir_all(local).map_err(|e| {
		UI::failure(&format!(
			"Failed to create the local directory {}: {e}",
			local.display()
		))
	})?;
	ui.print_success(&format!("Created the local directory {}", local.display()));
	Ok(())
}

/// Asks whether to create the missing remote root `remote_path`, and fails unless the answer is
/// yes.
fn confirm_remote_root(ui: &mut UI, remote_path: &RemotePath) -> Result<()> {
	if ui.prompt_confirm(
		&format!("Remote directory {remote_path} does not exist. Create it?"),
		true,
	)? {
		Ok(())
	} else {
		Err(UI::failure(&format!(
			"Remote directory {remote_path} not found"
		)))
	}
}

/// Creates the missing remote root `remote_path`, with every missing directory above it, which the
/// user confirmed with [`confirm_remote_root`], and returns its uuid.
async fn create_remote_root(
	ui: &mut UI,
	client: &Client,
	remote_path: &RemotePath,
) -> Result<Uuid> {
	let dir = client
		.find_or_create_dir(&remote_path.0)
		.await
		.with_context(|| format!("Failed to create the remote directory {remote_path}"))?;
	ui.print_success(&format!("Created the remote directory {remote_path}"));
	Ok(dir.uuid())
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
		ignore,
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
		// After the engine took the pair, which checked that its root is a directory, and before its
		// first pass: the pair is not synced without its rules.
		ignore_file::add_rules(local, ignore).with_context(|| {
			format!(
				"Failed to set up the sync pair {} <> {remote_path}",
				local.display()
			)
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
					"Invalid sync mode \"{token}\" in the sync pair \"{pair}\": expected one of {}. \
					 The remote path is what follows the last \":\", so a remote path with \":\" in \
					 it can only be given in a sync pairs file.",
					sync_mode_names()
				)));
			}
			// a colon in the local path, as in `C:\dir`
			None => (rest, SyncMode::TwoWay),
		},
		None => (rest, SyncMode::TwoWay),
	};
	Ok(ParsedPair {
		local: expand_home(local, dirs::home_dir().as_deref()),
		mode,
		remote: remote.to_owned(),
		alias: None,
		ignore: Vec::new(),
		disable_local_trash: false,
	})
}

/// `local` with a leading `~` (alone, or followed by a path separator) replaced by `home`, as a
/// shell would: the REPL and a sync pairs file get no shell to do it. `~user` is left as it is,
/// and so is every path when the home directory is unknown.
fn expand_home(local: &str, home: Option<&Path>) -> PathBuf {
	let Some(home) = home else {
		return PathBuf::from(local);
	};
	match local.strip_prefix('~') {
		Some("") => home.to_path_buf(),
		Some(rest) if rest.starts_with(std::path::is_separator) => {
			home.join(rest.trim_start_matches(std::path::is_separator))
		}
		_ => PathBuf::from(local),
	}
}

/// The local root `local` of a pair in a sync pairs file in `base`: a leading `~` expanded to
/// `home`, then a relative path resolved against `base`.
fn local_path_in_file(local: &str, base: &Path, home: Option<&Path>) -> PathBuf {
	base.join(expand_home(local, home))
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
/// the order it looks for them. It reads the first that exists. syncPairs.json is where the
/// TypeScript CLI kept its sync pairs, in the same directory.
const DEFAULT_SYNC_PAIRS_FILES: &[&str] = &["syncPairs.yaml", TYPESCRIPT_SYNC_PAIRS_FILE];

/// The name of the TypeScript CLI's sync pairs file, in its data directory.
const TYPESCRIPT_SYNC_PAIRS_FILE: &str = "syncPairs.json";

/// The directories `sync` reads files from besides those it is given: the config directory, and
/// where the TypeScript CLI kept its data, if that is not the config directory.
pub(crate) struct SyncDirs<'a> {
	config_dir: &'a Path,
	/// `~/.filen-cli`, which the TypeScript CLI used instead of the default config directory when
	/// it existed, as its install script created it. `None` when there is no such directory, and
	/// when the config directory was given with `--config-dir`, as the TypeScript CLI's
	/// `--data-dir` would have been: a sync run against a config directory of its own reads
	/// nothing from the home directory.
	typescript_home_dir: Option<PathBuf>,
}

impl<'a> SyncDirs<'a> {
	/// The directories of a run whose config directory is `config_dir`, the default one if
	/// `config_dir_is_default`, for the user whose home directory is `home`.
	pub(crate) fn new(
		config_dir: &'a Path,
		config_dir_is_default: bool,
		home: Option<&Path>,
	) -> Self {
		let typescript_home_dir = home
			.filter(|_| config_dir_is_default)
			.map(typescript_home_dir)
			.filter(|dir| dir.is_dir());
		Self {
			config_dir,
			typescript_home_dir,
		}
	}
}

/// The data directory the TypeScript CLI used in the home directory `home`: `~/.filen-cli`, and
/// in a development build its `dev` directory, as the TypeScript CLI's own development builds kept
/// apart from the released one.
fn typescript_home_dir(home: &Path) -> PathBuf {
	let dir = home.join(".filen-cli");
	if cfg!(debug_assertions) {
		dir.join("dev")
	} else {
		dir
	}
}

/// The paths of [`DEFAULT_SYNC_PAIRS_FILES`] in the config directory, then, if the TypeScript CLI
/// kept its data in `~/.filen-cli` (see [`SyncDirs`]), the syncPairs.json there.
fn default_sync_pairs_files(dirs: &SyncDirs<'_>) -> Vec<PathBuf> {
	DEFAULT_SYNC_PAIRS_FILES
		.iter()
		.map(|name| dirs.config_dir.join(name))
		.chain(
			dirs.typescript_home_dir
				.as_deref()
				.map(|dir| dir.join(TYPESCRIPT_SYNC_PAIRS_FILE)),
		)
		.collect()
}

/// `paths` for a message, as `a or b`.
fn display_paths(paths: &[PathBuf]) -> String {
	paths
		.iter()
		.map(|path| path.display().to_string())
		.collect::<Vec<_>>()
		.join(" or ")
}

/// The pairs of the first of [`default_sync_pairs_files`] that exists.
fn read_default_sync_pairs(dirs: &SyncDirs<'_>) -> Result<Vec<ParsedPair>> {
	let candidates = default_sync_pairs_files(dirs);
	match candidates.iter().find(|path| path.is_file()) {
		Some(path) => read_sync_pairs_file(path),
		None => Err(UI::usage_failure(&no_sync_pairs_message(&candidates))),
	}
}

/// Why `sync` without pairs cannot run when none of the default sync pairs files, `candidates`,
/// exists, and how to give it pairs.
fn no_sync_pairs_message(candidates: &[PathBuf]) -> String {
	format!(
		"No sync pairs given, and there is no sync pairs file at {}.\n\
		 Give each sync pair as <local>:<remote> or <local>:<mode>:<remote>, as in \
		 `filen sync /home/me/docs:/docs`, or the path to a sync pairs file, \
		 or create that file like this:\n\
		 syncPairs:\n  \
		 - local: /home/me/docs\n    \
		 remote: /docs\n    \
		 mode: twoWay",
		display_paths(candidates)
	)
}

/// Reads a sync pairs file and returns the list of sync pairs it defines, with local paths resolved
/// relative to the file's directory. The file is YAML, which JSON also is, of one of the shapes
/// [`sync_pair_entries`] reads.
fn read_sync_pairs_file(path: &Path) -> Result<Vec<ParsedPair>> {
	let content = std::fs::read_to_string(path).map_err(|e| {
		UI::failure(&format!(
			"Failed to read sync pairs file {}: {e}",
			path.display()
		))
	})?;
	let entries = sync_pair_entries(&content).map_err(|e| {
		UI::failure(&format!(
			"Failed to parse sync pairs file {}: {e}",
			path.display()
		))
	})?;
	if entries.is_empty() {
		return Err(UI::failure(&format!(
			"The sync pairs file {} defines no sync pairs",
			path.display()
		)));
	}
	let mut aliases = HashSet::new();
	if let Some(alias) = entries
		.iter()
		.filter_map(|entry| entry.alias.as_deref())
		.find(|alias| !aliases.insert(*alias))
	{
		return Err(UI::failure(&format!(
			"The sync pairs file {} gives the alias \"{alias}\" to more than one sync pair",
			path.display()
		)));
	}
	let base = path.parent().unwrap_or(Path::new(""));
	let home = dirs::home_dir();
	entries
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
			Ok(ParsedPair {
				local: local_path_in_file(&entry.local, base, home.as_deref()),
				mode,
				remote: entry.remote,
				alias: entry.alias,
				ignore: entry_ignore_rules(entry.exclude_dot_files, entry.ignore),
				disable_local_trash: entry.disable_local_trash,
			})
		})
		.collect()
}

/// The ignore rules of a pair whose `ignore` key gives `ignore`: those, after the rules
/// [`EXCLUDE_DOT_FILES_RULES`] if `exclude_dot_files`, so that an `ignore` rule can re-include a
/// dot file.
fn entry_ignore_rules(exclude_dot_files: bool, ignore: Vec<String>) -> Vec<String> {
	if !exclude_dot_files {
		return ignore;
	}
	EXCLUDE_DOT_FILES_RULES
		.iter()
		.map(|rule| (*rule).to_owned())
		.chain(ignore)
		.collect()
}

/// The sync pairs `content`, a sync pairs file, defines: a map whose `syncPairs` key lists them, a
/// list of them, or a single one, the last two as the TypeScript CLI's syncPairs.json gave them.
fn sync_pair_entries(content: &str) -> serde_yaml::Result<Vec<SyncPairEntry>> {
	// The shape is read first, then the content again as that shape, which keeps the place of an
	// error in it, such as `syncPairs[0]:`, in the error.
	match serde_yaml::from_str(content)? {
		serde_yaml::Value::Sequence(_) => serde_yaml::from_str(content),
		serde_yaml::Value::Mapping(map) if map.contains_key("syncPairs") => {
			serde_yaml::from_str::<SyncPairsFile>(content).map(|file| file.sync_pairs)
		}
		_ => serde_yaml::from_str::<SyncPairEntry>(content).map(|entry| vec![entry]),
	}
}

/// A sync pairs file that lists its sync pairs under `syncPairs`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SyncPairsFile {
	sync_pairs: Vec<SyncPairEntry>,
}

/// A sync pair in a sync pairs file. A key it does not know is an error rather than ignored: a
/// misspelt key would otherwise leave the pair syncing in a way the file does not say.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SyncPairEntry {
	local: String,
	remote: String,
	/// `syncMode` is the key the TypeScript CLI's syncPairs.json gives it by.
	#[serde(alias = "syncMode")]
	mode: Option<String>,
	/// The name `sync <alias>` selects the pair by, in the default sync pairs file.
	alias: Option<String>,
	/// Ignore rules, in `.filenignore` syntax, for the `.filenignore` at the local root.
	#[serde(default)]
	ignore: Vec<String>,
	/// Whether to leave out every dot file and dot directory, through [`EXCLUDE_DOT_FILES_RULES`].
	#[serde(default)]
	exclude_dot_files: bool,
	/// The TypeScript CLI's switch for its local trash. The sync engine's cannot be turned off, so
	/// this only gives a warning.
	#[serde(default)]
	disable_local_trash: bool,
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
	use filen_types::fs::Uuid;

	use super::{
		ParsedPair, SyncDirs, create_local_root, expand_home, fallback_label, local_path_in_file,
		local_root_exists, local_trash_warnings, missing_roots, overlap_message,
		parse_sync_mode_str, parse_sync_pair_definition, parse_sync_pairs, read_sync_pairs_file,
		records_of, sync_mode_str, typescript_home_dir,
	};
	use crate::{
		error::{CliError, exit_code, find_cli_error},
		ui::UI,
		util::RemotePath,
	};

	/// The directories of a run with `--config-dir config_dir`.
	fn given_config_dir(config_dir: &Path) -> SyncDirs<'_> {
		SyncDirs::new(config_dir, false, None)
	}

	fn pair(local: impl Into<PathBuf>, mode: SyncMode, remote: &str) -> ParsedPair {
		ParsedPair {
			local: local.into(),
			mode,
			remote: remote.to_owned(),
			alias: None,
			ignore: Vec::new(),
			disable_local_trash: false,
		}
	}

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
	fn a_local_root_that_is_a_directory_exists() {
		let dir = tempfile::tempdir().unwrap();

		assert!(local_root_exists(dir.path()).unwrap());
		assert!(!local_root_exists(&dir.path().join("missing")).unwrap());
	}

	/// A file where the local root should be is not replaced by a directory, but reported.
	#[test]
	fn a_local_root_that_is_a_file_is_a_failure() {
		let dir = tempfile::tempdir().unwrap();
		let file = dir.path().join("file");
		std::fs::write(&file, "").unwrap();

		let error = local_root_exists(&file).unwrap_err();

		assert_eq!(exit_code(&error), 1);
		assert_eq!(
			error.to_string(),
			format!("{} is not a directory", file.display())
		);
	}

	#[test]
	fn a_missing_local_root_is_created_with_the_directories_above_it_under_yes() {
		let dir = tempfile::tempdir().unwrap();
		let local = dir.path().join("a").join("b");
		let mut ui = UI::new();
		ui.set_assume_yes(true);

		create_local_root(&mut ui, &local).unwrap();

		assert!(local.is_dir());
	}

	/// Two pairs sharing a missing root are asked about it, and create it, once.
	#[test]
	fn a_missing_root_two_pairs_share_is_named_once() {
		let shared = pair("/new/local", SyncMode::TwoWay, "/new/remote");
		let found = [
			(shared.clone(), RemotePath::new("/new/remote"), None, false),
			(
				pair("/old/local", SyncMode::TwoWay, "/old/remote"),
				RemotePath::new("/old/remote"),
				Some(Uuid::from_u128(1)),
				true,
			),
			(shared, RemotePath::new("/new/remote"), None, false),
			(
				pair("/old/local", SyncMode::TwoWay, "/other/remote"),
				RemotePath::new("/other/remote"),
				None,
				true,
			),
		];

		let (locals, remotes) = missing_roots(&found);

		assert_eq!(locals, [Path::new("/new/local")]);
		assert_eq!(
			remotes,
			[
				&RemotePath::new("/new/remote"),
				&RemotePath::new("/other/remote")
			]
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

		let error = parse_sync_pairs(&given_config_dir(config_dir.path()), &[]).unwrap_err();

		assert_eq!(exit_code(&error), 2);
		let message = error.to_string();
		assert!(
			message.starts_with(&format!(
				"No sync pairs given, and there is no sync pairs file at {} or {}.\n",
				config_dir.path().join("syncPairs.yaml").display(),
				config_dir.path().join("syncPairs.json").display()
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

		let pairs = parse_sync_pairs(&given_config_dir(config_dir.path()), &[]).unwrap();

		assert_eq!(
			pairs,
			[pair(
				config_dir.path().join("docs"),
				SyncMode::TwoWay,
				"/docs"
			)]
		);
	}

	/// The TypeScript CLI's sync pairs file, where `sync` without pairs finds it when there is no
	/// syncPairs.yaml.
	#[test]
	fn no_pairs_reads_the_typescript_clis_sync_pairs_file_without_a_yaml_one() {
		let config_dir = tempfile::tempdir().unwrap();
		std::fs::write(
			config_dir.path().join("syncPairs.json"),
			r#"[{"local": "/l", "remote": "/r", "syncMode": "localBackup"}]"#,
		)
		.unwrap();

		assert_eq!(
			parse_sync_pairs(&given_config_dir(config_dir.path()), &[]).unwrap(),
			[pair("/l", SyncMode::LocalBackup, "/r")]
		);
	}

	/// The TypeScript CLI's data directory in `home`, holding a syncPairs.json of `content`.
	fn typescript_sync_pairs_in(home: &Path, content: &str) {
		let typescript_dir = typescript_home_dir(home);
		std::fs::create_dir_all(&typescript_dir).unwrap();
		std::fs::write(typescript_dir.join("syncPairs.json"), content).unwrap();
	}

	/// The TypeScript CLI's install script put its data in `~/.filen-cli`, where it kept its sync
	/// pairs; with the default config directory, a run without pairs falls back to them.
	#[test]
	fn no_pairs_reads_the_typescript_clis_sync_pairs_file_in_the_home_directory() {
		let config_dir = tempfile::tempdir().unwrap();
		let home = tempfile::tempdir().unwrap();
		typescript_sync_pairs_in(
			home.path(),
			r#"[{"local": "/l", "remote": "/r", "syncMode": "cloudBackup"}]"#,
		);

		let dirs = SyncDirs::new(config_dir.path(), true, Some(home.path()));

		assert_eq!(
			parse_sync_pairs(&dirs, &[]).unwrap(),
			[pair("/l", SyncMode::RemoteBackup, "/r")]
		);
	}

	/// A config directory given with `--config-dir` keeps the run away from the home directory.
	#[test]
	fn a_config_dir_of_its_own_does_not_read_the_home_directory() {
		let config_dir = tempfile::tempdir().unwrap();
		let home = tempfile::tempdir().unwrap();
		typescript_sync_pairs_in(home.path(), r#"[{"local": "/l", "remote": "/r"}]"#);

		let dirs = SyncDirs::new(config_dir.path(), false, Some(home.path()));

		assert_eq!(exit_code(&parse_sync_pairs(&dirs, &[]).unwrap_err()), 2);
	}

	/// The config directory's own sync pairs files come first.
	#[test]
	fn the_config_dirs_sync_pairs_file_is_read_before_the_home_directorys() {
		let config_dir = config_dir_with(
			"syncPairs.json",
			r#"[{"local": "/config", "remote": "/r"}]"#,
		);
		let home = tempfile::tempdir().unwrap();
		typescript_sync_pairs_in(home.path(), r#"[{"local": "/home", "remote": "/r"}]"#);

		let dirs = SyncDirs::new(config_dir.path(), true, Some(home.path()));

		assert_eq!(
			parse_sync_pairs(&dirs, &[]).unwrap(),
			[pair("/config", SyncMode::TwoWay, "/r")]
		);
	}

	#[test]
	fn a_syncpairs_yaml_is_read_before_a_syncpairs_json() {
		let config_dir = tempfile::tempdir().unwrap();
		std::fs::write(
			config_dir.path().join("syncPairs.yaml"),
			"syncPairs:\n  - local: /yaml\n    remote: /r\n",
		)
		.unwrap();
		std::fs::write(
			config_dir.path().join("syncPairs.json"),
			r#"[{"local": "/json", "remote": "/r"}]"#,
		)
		.unwrap();

		assert_eq!(
			parse_sync_pairs(&given_config_dir(config_dir.path()), &[]).unwrap(),
			[pair("/yaml", SyncMode::TwoWay, "/r")]
		);
	}

	/// A list of pairs at the top, as the TypeScript CLI wrote its syncPairs.json, indented with
	/// tabs as JSON may be.
	#[test]
	fn a_sync_pairs_file_can_be_a_json_list() {
		let (_dir, path) = sync_pairs_file(
			"syncPairs.json",
			"[\n\t{\n\t\t\"local\": \"/a\",\n\t\t\"remote\": \"/r/a\",\n\t\t\"syncMode\": \"cloudBackup\"\n\t},\n\
			 \t{\"local\": \"/b\", \"remote\": \"/r/b\"}\n]\n",
		);

		assert_eq!(
			read_sync_pairs_file(&path).unwrap(),
			[
				pair("/a", SyncMode::RemoteBackup, "/r/a"),
				pair("/b", SyncMode::TwoWay, "/r/b"),
			]
		);
	}

	/// The TypeScript CLI also read a single pair, not in a list.
	#[test]
	fn a_sync_pairs_file_can_be_a_single_pair() {
		let (_dir, path) = sync_pairs_file(
			"syncPairs.json",
			r#"{"local": "/l", "remote": "/r", "syncMode": "cloudToLocal"}"#,
		);

		assert_eq!(
			read_sync_pairs_file(&path).unwrap(),
			[pair("/l", SyncMode::RemoteToLocal, "/r")]
		);
	}

	/// An error in a list names the pair it is in.
	#[test]
	fn a_json_list_with_a_pair_without_a_remote_is_a_failure() {
		let (_dir, path) = sync_pairs_file(
			"syncPairs.json",
			r#"[{"local": "/l", "remote": "/r"}, {"local": "/l"}]"#,
		);

		let error = read_sync_pairs_file(&path).unwrap_err();

		assert_eq!(exit_code(&error), 1);
		assert!(
			error.to_string().starts_with(&format!(
				"Failed to parse sync pairs file {}: .[1]: missing field `remote`",
				path.display()
			)),
			"{error}"
		);
	}

	/// The default sync pairs file in a new config dir, which holds `content`.
	fn config_dir_with(name: &str, content: &str) -> tempfile::TempDir {
		let config_dir = tempfile::tempdir().unwrap();
		std::fs::write(config_dir.path().join(name), content).unwrap();
		config_dir
	}

	#[test]
	fn an_alias_selects_its_pair_from_the_default_sync_pairs_file() {
		let config_dir = config_dir_with(
			"syncPairs.json",
			r#"[{"local": "/a", "remote": "/r/a", "alias": "docs"},
			    {"local": "/b", "remote": "/r/b", "alias": "photos", "syncMode": "lb"}]"#,
		);

		let pairs =
			parse_sync_pairs(&given_config_dir(config_dir.path()), &["photos".to_owned()]).unwrap();

		assert_eq!(
			pairs,
			[ParsedPair {
				alias: Some("photos".to_owned()),
				..pair("/b", SyncMode::LocalBackup, "/r/b")
			}]
		);
	}

	#[test]
	fn an_unknown_alias_is_a_usage_error() {
		let config_dir = config_dir_with(
			"syncPairs.yaml",
			"syncPairs:\n  - local: /a\n    remote: /r\n    alias: docs\n",
		);

		let error = parse_sync_pairs(&given_config_dir(config_dir.path()), &["photos".to_owned()])
			.unwrap_err();

		assert_eq!(exit_code(&error), 2);
		assert_eq!(error.to_string(), "Unknown sync pair alias: photos");
	}

	#[test]
	fn an_alias_without_a_default_sync_pairs_file_is_a_usage_error() {
		let config_dir = tempfile::tempdir().unwrap();

		let error = parse_sync_pairs(&given_config_dir(config_dir.path()), &["photos".to_owned()])
			.unwrap_err();

		assert_eq!(exit_code(&error), 2);
		assert_eq!(
			error.to_string(),
			format!(
				"Unknown sync pair alias: photos. There is no sync pairs file at {} or {} to look \
				 it up in.",
				config_dir.path().join("syncPairs.yaml").display(),
				config_dir.path().join("syncPairs.json").display()
			)
		);
	}

	/// A path without a colon is no alias, but a sync pair missing its remote path.
	#[test]
	fn a_path_without_a_colon_is_not_taken_for_an_alias() {
		let config_dir = tempfile::tempdir().unwrap();

		let error = parse_sync_pairs(
			&given_config_dir(config_dir.path()),
			&["/home/me/docs".to_owned()],
		)
		.unwrap_err();

		assert!(
			error
				.to_string()
				.starts_with("Invalid sync pair \"/home/me/docs\""),
			"{error}"
		);
	}

	#[test]
	fn a_sync_pair_takes_its_ignore_rules_from_the_file() {
		let (_dir, path) = sync_pairs_file(
			"syncPairs.json",
			r#"[{"local": "/l", "remote": "/r", "ignore": ["*.log", "build/"]}]"#,
		);

		assert_eq!(
			read_sync_pairs_file(&path).unwrap(),
			[ParsedPair {
				ignore: vec!["*.log".to_owned(), "build/".to_owned()],
				..pair("/l", SyncMode::TwoWay, "/r")
			}]
		);
	}

	/// `excludeDotFiles` is two rules for the `.filenignore`, before the pair's own, which can
	/// then re-include a dot file.
	#[test]
	fn exclude_dot_files_adds_the_rules_that_leave_out_dot_files() {
		let (_dir, path) = sync_pairs_file(
			"syncPairs.json",
			r#"[{"local": "/l", "remote": "/r", "excludeDotFiles": true, "ignore": ["!.config"]},
			    {"local": "/m", "remote": "/r", "excludeDotFiles": false}]"#,
		);

		assert_eq!(
			read_sync_pairs_file(&path).unwrap(),
			[
				ParsedPair {
					ignore: vec![
						".*".to_owned(),
						"!/.filenignore".to_owned(),
						"!.config".to_owned()
					],
					..pair("/l", SyncMode::TwoWay, "/r")
				},
				pair("/m", SyncMode::TwoWay, "/r"),
			]
		);
	}

	/// The local trash cannot be turned off, which a pair whose file asks for it is warned about.
	#[test]
	fn disable_local_trash_gives_a_warning() {
		let (_dir, path) = sync_pairs_file(
			"syncPairs.json",
			r#"[{"local": "/l", "remote": "/r", "disableLocalTrash": true},
			    {"local": "/m", "remote": "/r", "disableLocalTrash": false}]"#,
		);
		let parsed = read_sync_pairs_file(&path).unwrap();

		assert_eq!(
			local_trash_warnings(&parsed),
			[format!(
				"The sync pair /l asks for disableLocalTrash, which is not supported: the local \
				 files a sync deletes or replaces are kept in {} for 30 days.",
				Path::new("/l").join(".filen-sync-trash").display()
			)]
		);
	}

	#[test]
	fn a_sync_pairs_file_that_gives_an_alias_twice_is_a_failure() {
		let (_dir, path) = sync_pairs_file(
			"syncPairs.json",
			r#"[{"local": "/a", "remote": "/r/a", "alias": "docs"},
			    {"local": "/b", "remote": "/r/b", "alias": "docs"}]"#,
		);

		let error = read_sync_pairs_file(&path).unwrap_err();

		assert_eq!(exit_code(&error), 1);
		assert_eq!(
			error.to_string(),
			format!(
				"The sync pairs file {} gives the alias \"docs\" to more than one sync pair",
				path.display()
			)
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
			 cloudToLocal, ctl, localBackup, lb, remoteBackup, cloudBackup, cb. The remote path is \
			 what follows the last \":\", so a remote path with \":\" in it can only be given in a \
			 sync pairs file."
		);
	}

	#[test]
	fn a_local_path_on_a_windows_drive_keeps_its_drive_letter() {
		assert_eq!(
			parse_sync_pair_definition(r"C:\Users\me:/docs").unwrap(),
			pair(PathBuf::from(r"C:\Users\me"), SyncMode::TwoWay, "/docs")
		);
	}

	#[test]
	fn a_local_path_on_a_windows_drive_can_have_a_mode() {
		assert_eq!(
			parse_sync_pair_definition(r"C:\Users\me:localBackup:/docs").unwrap(),
			pair(
				PathBuf::from(r"C:\Users\me"),
				SyncMode::LocalBackup,
				"/docs"
			)
		);
	}

	/// `C:docs` is the directory `docs` in the current directory of the drive C:, not a mode.
	#[test]
	fn a_drive_relative_local_path_is_no_mode() {
		assert_eq!(
			parse_sync_pair_definition("C:docs:/docs").unwrap(),
			pair(PathBuf::from("C:docs"), SyncMode::TwoWay, "/docs")
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
				.mode
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

	#[test]
	fn a_leading_tilde_is_the_home_directory() {
		let home = Path::new("/home/me");

		assert_eq!(expand_home("~", Some(home)), PathBuf::from("/home/me"));
		assert_eq!(
			expand_home("~/docs/notes", Some(home)),
			PathBuf::from("/home/me/docs/notes")
		);
	}

	#[cfg(windows)]
	#[test]
	fn a_leading_tilde_before_a_backslash_is_the_home_directory() {
		assert_eq!(
			expand_home(r"~\docs", Some(Path::new(r"C:\Users\me"))),
			PathBuf::from(r"C:\Users\me\docs")
		);
	}

	/// `~user` is another user's home, which the CLI does not look up, and `~` inside a name is
	/// part of it.
	#[test]
	fn a_tilde_that_is_not_a_home_directory_is_kept() {
		let home = Some(Path::new("/home/me"));

		assert_eq!(
			expand_home("~other/docs", home),
			PathBuf::from("~other/docs")
		);
		assert_eq!(expand_home("docs/~/x", home), PathBuf::from("docs/~/x"));
		assert_eq!(expand_home("~/docs", None), PathBuf::from("~/docs"));
	}

	/// The home directory is expanded before a relative path is resolved against the file's
	/// directory, which would otherwise read `~` as a directory in it.
	#[test]
	fn a_tilde_in_a_sync_pairs_file_is_the_home_directory() {
		let base = Path::new("/etc/filen");
		let home = Some(Path::new("/home/me"));

		assert_eq!(
			local_path_in_file("~/docs", base, home),
			PathBuf::from("/home/me/docs")
		);
		assert_eq!(
			local_path_in_file("docs", base, home),
			PathBuf::from("/etc/filen/docs")
		);
	}

	#[test]
	fn an_inline_pair_expands_a_leading_tilde() {
		let home = dirs::home_dir().unwrap();

		assert_eq!(
			parse_sync_pair_definition("~/docs:/docs").unwrap(),
			pair(home.join("docs"), SyncMode::TwoWay, "/docs")
		);
	}

	#[test]
	fn a_pair_without_a_mode_syncs_two_way() {
		assert_eq!(
			parse_sync_pair_definition("/home/me/docs:/docs").unwrap(),
			pair(PathBuf::from("/home/me/docs"), SyncMode::TwoWay, "/docs")
		);
	}

	/// The SDK's names of the modes, and the TypeScript CLI's "cloud" names.
	#[test]
	fn every_mode_name_names_its_mode() {
		let modes = [
			"twoWay",
			"localToRemote",
			"localToCloud",
			"remoteToLocal",
			"cloudToLocal",
			"localBackup",
			"remoteBackup",
			"cloudBackup",
		]
		.map(|name| {
			parse_sync_pair_definition(&format!("/l:{name}:/r"))
				.unwrap()
				.mode
		});

		assert_eq!(
			modes,
			[
				SyncMode::TwoWay,
				SyncMode::LocalToRemote,
				SyncMode::LocalToRemote,
				SyncMode::RemoteToLocal,
				SyncMode::RemoteToLocal,
				SyncMode::LocalBackup,
				SyncMode::RemoteBackup,
				SyncMode::RemoteBackup,
			]
		);
	}

	/// The name a pair is labelled with names its mode again.
	#[test]
	fn the_name_of_each_mode_parses_back_to_it() {
		let modes = [
			SyncMode::TwoWay,
			SyncMode::LocalToRemote,
			SyncMode::RemoteToLocal,
			SyncMode::LocalBackup,
			SyncMode::RemoteBackup,
		];

		assert_eq!(
			modes.map(|mode| parse_sync_mode_str(sync_mode_str(mode))),
			modes.map(Some)
		);
	}

	#[test]
	fn mode_names_are_case_sensitive() {
		assert_eq!(parse_sync_mode_str("TwoWay"), None);
		assert_eq!(parse_sync_mode_str("TW"), None);
		assert_eq!(parse_sync_mode_str(""), None);
	}

	/// A relative local path in a sync pairs file is relative to the file's directory, and a pair
	/// without a mode syncs two-way.
	#[test]
	fn a_sync_pairs_file_resolves_relative_local_paths_against_its_directory() {
		let (dir, path) = sync_pairs_file(
			"pairs.yaml",
			"syncPairs:\n\
			 \x20 - local: docs\n\
			 \x20   remote: /docs\n\
			 \x20 - local: ../photos\n\
			 \x20   remote: photos\n\
			 \x20   mode: lb\n",
		);

		assert_eq!(
			read_sync_pairs_file(&path).unwrap(),
			[
				pair(dir.path().join("docs"), SyncMode::TwoWay, "/docs"),
				pair(
					dir.path().join("../photos"),
					SyncMode::LocalBackup,
					"photos"
				),
			]
		);
	}

	/// The TypeScript CLI's key for the mode names it too, rather than being passed over for a
	/// two-way sync.
	#[test]
	fn a_sync_pairs_file_can_give_the_mode_as_sync_mode() {
		let (_dir, path) = sync_pairs_file(
			"pairs.yaml",
			"syncPairs:\n  - local: /l\n    remote: /r\n    syncMode: cloudBackup\n",
		);

		assert_eq!(
			read_sync_pairs_file(&path).unwrap(),
			[pair(PathBuf::from("/l"), SyncMode::RemoteBackup, "/r")]
		);
	}

	#[test]
	fn a_sync_pair_with_both_mode_and_sync_mode_is_a_failure() {
		let (_dir, path) = sync_pairs_file(
			"pairs.yaml",
			"syncPairs:\n  - local: /l\n    remote: /r\n    mode: twoWay\n    syncMode: cloudBackup\n",
		);

		let error = read_sync_pairs_file(&path).unwrap_err();

		assert_eq!(exit_code(&error), 1);
		assert_eq!(
			error.to_string(),
			format!(
				"Failed to parse sync pairs file {}: syncPairs[0]: duplicate field `mode` at line 2 column 5",
				path.display()
			)
		);
	}

	/// A key the file format does not have, misspelt or not, is reported rather than ignored.
	#[test]
	fn a_sync_pair_with_an_unknown_key_is_a_failure() {
		let (_dir, path) = sync_pairs_file(
			"pairs.yaml",
			"syncPairs:\n  - local: /l\n    remote: /r\n    mdoe: cloudBackup\n",
		);

		let error = read_sync_pairs_file(&path).unwrap_err();

		assert_eq!(exit_code(&error), 1);
		assert_eq!(
			error.to_string(),
			format!(
				"Failed to parse sync pairs file {}: syncPairs[0]: unknown field `mdoe`, expected \
				 one of `local`, `remote`, `mode`, `syncMode`, `alias`, `ignore`, \
				 `excludeDotFiles`, `disableLocalTrash` at line 4 column 5",
				path.display()
			)
		);
	}

	#[test]
	fn a_sync_pairs_file_with_an_unknown_mode_is_a_failure() {
		let (_dir, path) = sync_pairs_file(
			"pairs.yaml",
			"syncPairs:\n  - local: /l\n    remote: /r\n    mode: both\n",
		);

		let error = read_sync_pairs_file(&path).unwrap_err();

		assert_eq!(exit_code(&error), 1);
		assert_eq!(
			error.to_string(),
			format!(
				"Invalid sync mode \"both\" in {}: expected one of twoWay, tw, localToRemote, \
				 localToCloud, ltc, remoteToLocal, cloudToLocal, ctl, localBackup, lb, \
				 remoteBackup, cloudBackup, cb",
				path.display()
			)
		);
	}
}

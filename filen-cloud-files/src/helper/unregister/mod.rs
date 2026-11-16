//! Unregistering a root without losing local data. Windows deletes, on unregister, every
//! placeholder whose data is not all on disk, and leaves the rest as ordinary items (measured).
//! So before it runs, with the root connected to refuse every request (the tree holds still, and
//! the helper sees placeholders as they are), every item is classified: a clean placeholder, in
//! sync with nothing written since, holds only what the drive has; anything else, an ordinary item
//! the drive never had, an edited placeholder, the local side of a conflict, an unreadable
//! identity, is kept, made an ordinary item first so the unregister cannot touch it. A kept item
//! whose data is not all on disk cannot be kept that way, and stops the unregister before anything
//! is deleted. No other single item stops it: a kept file the platform refuses to make ordinary is
//! copied to a new ordinary file beside it, which is what is kept, and the file is left to the
//! platform's unregister; an item whose state cannot be read, or a directory that cannot be
//! listed, is left as it is and reported kept. Only a kept file that can be neither made ordinary
//! nor copied, or a directory above a kept item that cannot be made ordinary, stops the unregister,
//! as the platform's unregister might take what they hold. What stays in the root afterwards is
//! moved aside with the root, which is renamed.
//!
//! What was kept is recorded in the roots index, with where the root goes, before the OS lets the
//! root go, and settled once it went. A helper killed anywhere in between finds the record on its
//! next `unregister` or `register` of the root, finishes the move, and reports it; the record goes
//! once a reply the parent answered (its next request) reported it.

use std::{
	path::{Path, PathBuf},
	sync::Arc,
	time::SystemTime,
};

use chrono::{DateTime, Utc};
use filen_sdk_rs::fs::name::aside_names;
use tracing::{info, warn};

use crate::{
	auth::{DataDir, KeyStore, SessionStore},
	helper::{
		blocking,
		index::{IndexError, KeptPlace, KeptRecord, RootEntry, RootsIndex, StateDir, index_error},
		platform::{BrandingScope, Platform, PlatformError, RootRefusal, platform_error},
		protocol::{
			ErrorCode, KeptId, KeptItem, KeptItemReason, MAX_LIST_ENTRIES, OutMessage, ReplyError,
			UnregisterAllEntry, UnregisterReport, UserId,
		},
	},
	identity::PlaceholderIdentity,
	log_safe::path_tag,
	names::PlaceholderName,
	os::{LocalEntry, LocalError, LocalFileId, LocalFs, LocalKind},
	root_path::RootPath,
};

/// How many free names beside a kept file the unregister tries for its copy.
const COPY_ATTEMPTS: usize = 100;

/// What became of the root directory after the unregister.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Tidied {
	/// Nothing is at the root's path any more.
	pub(crate) root_removed: bool,
	/// Where what stayed is: the root renamed aside, or the root itself when the rename failed.
	pub(crate) kept_in: Option<PathBuf>,
}

/// What is at an unregistered root's path once its empty directories went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cleared {
	/// Nothing: the root was empty and is removed, or it is not there (renamed aside already).
	Gone,
	/// The root, holding this many items other than directories.
	Left(u64),
}

/// A local item the unregister keeps.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Kept {
	path: RootPath,
	reason: KeptItemReason,
	/// The placeholder to make an ordinary item first; `None` for an item that is one already.
	revert: Option<LocalFileId>,
	/// Whether its data is not all on disk.
	partial: bool,
	/// Whether the item is inside the directory `path` names rather than at it: one whose name the
	/// core cannot spell.
	inside: bool,
}

/// The root, classified.
#[derive(Debug, Default)]
pub(crate) struct Walked {
	/// Clean placeholders all on disk, with their identity.
	clean: Vec<(RootPath, PlaceholderIdentity)>,
	kept: Vec<Kept>,
	/// Every directory placeholder.
	dirs: Vec<(RootPath, LocalFileId)>,
}

impl Walked {
	/// The kept items whose data is not all on disk.
	fn partial(&self) -> impl Iterator<Item = &Kept> {
		self.kept.iter().filter(|kept| kept.partial)
	}
}

/// Classifies every item in the root, every directory walked, populated or not. A link or reparse
/// point is kept and never followed. Only a root that cannot be listed fails the walk: an item
/// whose state cannot be read, or a directory below the root that cannot be listed, is kept as it
/// is, never changed. The platform's unregister keeps such a file as an ordinary one when all its
/// data is on disk, and deletes it when not, which loses nothing the drive lacks: a write hydrates
/// the whole file first (measured).
pub(crate) fn walk<L: LocalFs>(local: &L) -> Result<Walked, LocalError> {
	let mut walked = Walked::default();
	let mut dirs = vec![RootPath::root()];
	while let Some(dir) = dirs.pop() {
		let children = match local.list_dir(&dir) {
			Ok(children) => children,
			Err(error) if dir.is_root() => return Err(error),
			Err(error) => {
				warn!(%error, dir = %path_tag(&dir), "a directory cannot be listed before unregistering; it is left as it is");
				walked.kept.push(Kept {
					path: dir,
					reason: KeptItemReason::UnknownPlaceholder,
					revert: None,
					partial: false,
					inside: true,
				});
				continue;
			}
		};
		for child in children {
			let Ok(path) = child else {
				// A name the core cannot spell is never a placeholder the provider made: an
				// ordinary item, which the unregister leaves alone. It is reported by its
				// directory, which is made an ordinary one with it.
				walked.kept.push(Kept {
					path: dir.clone(),
					reason: KeptItemReason::UnrepresentableName,
					revert: None,
					partial: false,
					inside: true,
				});
				continue;
			};
			let entry = match local.inspect(&path) {
				Ok(entry) => entry,
				Err(error) => {
					warn!(%error, item = %path_tag(&path), "an item's state cannot be read before unregistering; it is left as it is");
					walked.kept.push(Kept {
						path,
						reason: KeptItemReason::UnknownPlaceholder,
						revert: None,
						partial: false,
						inside: false,
					});
					continue;
				}
			};
			match entry {
				LocalEntry::Missing => {}
				LocalEntry::Plain {
					kind: LocalKind::Dir { .. },
					..
				} => dirs.push(path),
				LocalEntry::Plain {
					kind: LocalKind::File(_),
					..
				} => walked.kept.push(Kept {
					path,
					reason: KeptItemReason::NotUploaded,
					revert: None,
					partial: false,
					inside: false,
				}),
				LocalEntry::Unsupported(_) => walked.kept.push(Kept {
					path,
					reason: KeptItemReason::Unsupported,
					revert: None,
					partial: false,
					inside: false,
				}),
				LocalEntry::Placeholder {
					kind: LocalKind::Dir { .. },
					id,
					..
				} => {
					walked.dirs.push((path.clone(), id));
					dirs.push(path);
				}
				LocalEntry::Placeholder {
					kind: LocalKind::File(_),
					id,
					identity,
					state,
				} => {
					let clean = state.in_sync() && state.modified_bytes() == 0;
					let reason = match identity {
						Ok(identity @ PlaceholderIdentity::File { .. }) if clean => {
							if !state.partial() {
								walked.clean.push((path, identity));
							}
							// A clean placeholder not all on disk holds nothing the drive lacks:
							// the unregister deletes it.
							continue;
						}
						Ok(PlaceholderIdentity::File { .. }) => KeptItemReason::Modified,
						Ok(PlaceholderIdentity::Detached { .. }) => KeptItemReason::ConflictCopy,
						Ok(PlaceholderIdentity::Root { .. } | PlaceholderIdentity::Dir { .. })
						| Err(_) => KeptItemReason::UnknownPlaceholder,
					};
					walked.kept.push(Kept {
						path,
						reason,
						revert: Some(id),
						partial: state.partial(),
						inside: false,
					});
				}
			}
		}
	}
	Ok(walked)
}

/// Fetches the data of every kept file not all on disk, while the root is still served.
/// Best effort: what is still not all on disk afterwards stops the unregister.
pub(crate) fn hydrate_kept<L: LocalFs>(local: &L, walked: &Walked) {
	for kept in walked.partial() {
		if let Some(id) = kept.revert
			&& let Err(error) = local.hydrate(&kept.path, id)
		{
			warn!(%error, item = %path_tag(&kept.path), "fetching a changed file's data before unregistering failed");
		}
	}
}

/// What the unregister did to the items, before the OS's own unregister.
#[derive(Debug, Default)]
struct Settled {
	deleted_hydrated: u64,
	kept: Vec<Kept>,
}

/// Why the items could not be settled.
#[derive(Debug)]
enum Unsettled {
	/// A file edited since the walk, at the path, has data not all on disk.
	Partial(RootPath),
	/// The kept item at the path could not be made an ordinary item, nor, a file, copied to one.
	Unkept { path: RootPath, error: LocalError },
}

impl Unsettled {
	/// The reply's message, after "preparing the root failed: ".
	fn message(&self) -> String {
		match self {
			Self::Partial(path) => format!("{path} is not all on disk"),
			Self::Unkept { path, error } => {
				format!("{path} could not be made an ordinary item nor copied to one: {error}")
			}
		}
	}
}

/// Deletes the clean files all on disk when `delete_hydrated`, then makes every kept placeholder,
/// and every directory placeholder holding one, an ordinary item.
fn settle<L: LocalFs>(
	local: &L,
	walked: Walked,
	delete_hydrated: bool,
) -> Result<Settled, Unsettled> {
	let Walked {
		clean,
		mut kept,
		mut dirs,
	} = walked;
	let mut deleted_hydrated = 0_u64;
	if delete_hydrated {
		for (path, identity) in clean {
			match local.remove_clean(&path, identity) {
				Ok(()) => deleted_hydrated = deleted_hydrated.saturating_add(1),
				Err(LocalError::NotFound) => {}
				// Written to since the walk: kept, as any edited file.
				Err(LocalError::Modified) => match local.inspect(&path) {
					Ok(LocalEntry::Placeholder { id, state, .. }) => kept.push(Kept {
						path,
						reason: KeptItemReason::Modified,
						revert: Some(id),
						partial: state.partial(),
						inside: false,
					}),
					Ok(LocalEntry::Plain { .. }) => kept.push(Kept {
						path,
						reason: KeptItemReason::NotUploaded,
						revert: None,
						partial: false,
						inside: false,
					}),
					Ok(LocalEntry::Missing | LocalEntry::Unsupported(_)) => {}
					// Left as it is, as the walk leaves an item whose state it cannot read.
					Err(error) => {
						warn!(%error, item = %path_tag(&path), "a file written since the walk cannot be read; it is left as it is");
						kept.push(Kept {
							path,
							reason: KeptItemReason::Modified,
							revert: None,
							partial: false,
							inside: false,
						});
					}
				},
				Err(error) => {
					// A clean file the unregister leaves as an ordinary file: nothing is lost.
					warn!(%error, item = %path_tag(&path), "a clean file could not be deleted; it stays");
					kept.push(Kept {
						path,
						reason: KeptItemReason::InUse,
						revert: None,
						partial: false,
						inside: false,
					});
				}
			}
		}
	}
	if let Some(partial) = kept.iter().find(|kept| kept.partial) {
		warn!(item = %path_tag(&partial.path), "a changed file's data is not all on disk");
		return Err(Unsettled::Partial(partial.path.clone()));
	}
	for item in &mut kept {
		if let Some(id) = item.revert
			&& let Err(error) = local.revert_to_plain(&item.path, id)
		{
			item.path = copy_kept(local, &item.path, id, error)?;
			item.revert = None;
		}
	}
	// Deepest first, each directory placeholder above a kept item.
	dirs.sort_by_key(|(dir, _)| std::cmp::Reverse(dir.depth()));
	for (dir, id) in &dirs {
		if kept
			.iter()
			.any(|item| item.path.starts_with(dir) && (item.inside || item.path != *dir))
		{
			local
				.revert_to_plain(dir, *id)
				.map_err(|error| Unsettled::Unkept {
					path: dir.clone(),
					error,
				})?;
		}
	}
	Ok(Settled {
		deleted_hydrated,
		kept,
	})
}

/// Keeps the kept file `id` at `path`, which the platform refused to make an ordinary file with
/// `error`, by copying its bytes, all on disk, to a new ordinary file beside it under the first
/// free `.old` name: the copy is what is kept, and the file is left to the platform's unregister.
/// Where the copy went, or [`Unsettled::Unkept`] when it could not be made.
fn copy_kept<L: LocalFs>(
	local: &L,
	path: &RootPath,
	id: LocalFileId,
	error: LocalError,
) -> Result<RootPath, Unsettled> {
	warn!(%error, item = %path_tag(path), "a kept file cannot be made an ordinary file; its bytes are copied");
	let unkept = |error| Unsettled::Unkept {
		path: path.clone(),
		error,
	};
	let name = path
		.name()
		.ok_or_else(|| unkept(error))?
		.as_str()
		.to_owned();
	let mut last = error;
	for candidate in aside_names(&name).take(COPY_ATTEMPTS) {
		let Some(to) = PlaceholderName::parse(&candidate)
			.ok()
			.and_then(|candidate| path.with_name(candidate))
		else {
			continue;
		};
		if !matches!(local.inspect(&to), Ok(LocalEntry::Missing)) {
			continue;
		}
		match local.copy_to_plain(path, id, &to) {
			Ok(()) => return Ok(to),
			Err(LocalError::Exists) => last = LocalError::Exists,
			Err(error) => return Err(unkept(error)),
		}
	}
	Err(unkept(last))
}

/// What unregistering a root did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RootOutcome {
	pub(crate) deleted_hydrated: u64,
	pub(crate) tidied: Tidied,
	/// What was kept, settled; `None` for a root left as it was.
	pub(crate) record: Option<KeptRecord>,
	/// Whether anything that showed the root in File Explorer may be left (see [`unbrand`]).
	pub(crate) branding_left: bool,
}

/// What [`unregister_entry`] did.
#[derive(Debug)]
pub(crate) struct EntryUnregistered {
	/// The reply's report.
	pub(crate) report: UnregisterReport,
	/// Whether anything that showed the root in File Explorer may be left: an elevated run leaves
	/// the per-user navigation-pane entry, and a failed removal leaves what it did not remove.
	pub(crate) branding_left: bool,
}

/// Removes what shows in File Explorer roots the OS no longer holds (see
/// [`Platform::sweep_branding`]), as the user only: an elevated process may hold another account's
/// hive. A failure is logged; the next run tries again.
pub(crate) async fn sweep_branding<P: Platform>(platform: &Arc<P>) {
	if platform.elevated() {
		return;
	}
	let sweeping = Arc::clone(platform);
	match blocking(move || sweeping.sweep_branding()).await {
		Ok(0) => {}
		Ok(removed) => info!(
			removed,
			"removed File Explorer entries of roots no longer registered"
		),
		Err(error) => {
			warn!(%error, "removing File Explorer entries of roots no longer registered failed")
		}
	}
}

/// Removes what showed `user`'s root at `root` in File Explorer, once the OS let the root go, as
/// far as this process may: everything as the user, only the machine-wide sync root list entry
/// elevated (see [`BrandingScope`]). Whether anything may be left; a failure is logged, and what
/// it left is cleared by a later run as the user.
pub(crate) async fn unbrand<P: Platform>(platform: &Arc<P>, root: &Path, user: UserId) -> bool {
	let scope = if platform.elevated() {
		BrandingScope::MachineOnly
	} else {
		BrandingScope::User
	};
	let unbranding = Arc::clone(platform);
	let root = root.to_path_buf();
	let removed = blocking(move || unbranding.unbrand(&root, user, scope))
		.await
		.inspect_err(|error| warn!(%error, "removing the root from File Explorer failed"))
		.is_ok();
	!removed || scope == BrandingScope::MachineOnly
}

/// What the OS holds of a root whose unregister stopped.
pub(crate) enum Held<R> {
	/// The registration, back.
	Registered(R),
	/// A registration this run no longer has a handle to, or none it took.
	Unknown,
	/// Nothing: the OS let the root go, and what was kept is recorded for the next `unregister` or
	/// `register` of the root to finish and report.
	Released,
}

/// Why unregistering a root stopped, with what the OS still holds of it.
pub(crate) struct Stopped<R> {
	pub(crate) held: Held<R>,
	pub(crate) error: ReplyError,
}

/// Why finishing an unregistered root failed.
#[derive(Debug, thiserror::Error)]
enum TidyError {
	#[error(transparent)]
	Platform(#[from] PlatformError),
	/// Recording what was kept, or where it is, failed: nothing was moved after it.
	#[error(transparent)]
	Index(#[from] IndexError),
}

impl TidyError {
	fn reply(&self) -> ReplyError {
		match self {
			Self::Platform(error) => platform_error(error),
			Self::Index(error) => index_error(error),
		}
	}
}

/// `refusal` of a root's path as a reply's error.
fn refusal_error(refusal: &RootRefusal) -> ReplyError {
	ReplyError::new(
		match refusal {
			RootRefusal::NotNtfs(_) => ErrorCode::NotNtfs,
			RootRefusal::Invalid(_) | RootRefusal::DriveRoot => ErrorCode::RootInvalid,
		},
		refusal.to_string(),
	)
}

/// Unregisters `registered`, `user`'s root at `root`, keeping every item that holds data the drive
/// may not have, and deleting clean downloaded files when `delete_hydrated`. Stops, with the root
/// still registered, before anything is deleted when a kept item's data is not all on disk. What
/// is kept is recorded in `index` before the OS lets the root go.
pub(crate) async fn unregister_root<P: Platform>(
	platform: &Arc<P>,
	index: &mut RootsIndex,
	user: UserId,
	root: &Path,
	registered: P::Registered,
	delete_hydrated: bool,
	date: String,
) -> Result<RootOutcome, Stopped<P::Registered>> {
	let connected = {
		let platform = Arc::clone(platform);
		blocking(move || platform.connect_refusing(registered)).await
	};
	let connected = match connected {
		Ok(connected) => connected,
		Err(failed) => {
			return Err(Stopped {
				held: Held::Registered(failed.held),
				error: platform_error(&failed.error),
			});
		}
	};
	let settled = {
		let platform = Arc::clone(platform);
		let root = root.to_path_buf();
		blocking(move || {
			let local = platform.local_fs(&root);
			let walked = walk(&*local).map_err(|error| error.to_string())?;
			let partial: Vec<String> = walked.partial().map(|kept| kept.path.to_string()).collect();
			if !partial.is_empty() {
				return Ok(Err(partial));
			}
			match settle(&*local, walked, delete_hydrated) {
				Ok(settled) => Ok(Ok(settled)),
				Err(Unsettled::Partial(path)) => Ok(Err(vec![path.to_string()])),
				Err(unsettled) => Err(unsettled.message()),
			}
		})
		.await
	};
	let disconnected = {
		let platform = Arc::clone(platform);
		blocking(move || platform.disconnect(connected)).await
	};
	let registered = match disconnected {
		Ok(registered) => registered,
		Err(failed) => {
			return Err(Stopped {
				held: Held::Unknown,
				error: platform_error(&failed.error),
			});
		}
	};
	let settled = match settled {
		Ok(Ok(settled)) => settled,
		Ok(Err(partial)) => {
			return Err(Stopped {
				held: Held::Registered(registered),
				error: ReplyError::new(
					ErrorCode::UnsyncedChanges,
					format!(
						"{} changed file(s) are not all on disk and no session can fetch them, \
						 first {}",
						partial.len(),
						partial.first().map_or("", String::as_str)
					),
				),
			});
		}
		Err(error) => {
			return Err(Stopped {
				held: Held::Registered(registered),
				error: ReplyError::new(
					ErrorCode::Os,
					format!("preparing the root failed: {error}"),
				),
			});
		}
	};
	let aside = {
		let platform = Arc::clone(platform);
		let root = root.to_path_buf();
		let date = date.clone();
		blocking(move || aside_path(&root, &date, |candidate| platform.exists(candidate))).await
	};
	let record = KeptRecord {
		id: KeptId::random(),
		kept_count: u64::try_from(settled.kept.len()).unwrap_or(u64::MAX),
		kept: settled
			.kept
			.into_iter()
			.take(MAX_LIST_ENTRIES)
			.map(|kept| KeptItem {
				path: kept.path.to_string(),
				reason: kept.reason,
			})
			.collect(),
		place: KeptPlace::InRoot { aside },
	};
	if let Err(error) = index.put_kept(user, record.clone()) {
		return Err(Stopped {
			held: Held::Registered(registered),
			error: index_error(&error),
		});
	}
	{
		let platform = Arc::clone(platform);
		if let Err(failed) = blocking(move || platform.unregister(registered)).await {
			// The root stays registered, its items in it. A record left by a failed write is
			// harmless: the next unregister replaces it, and the next register drops it.
			if let Err(error) = index.drop_unsettled(user) {
				warn!(%error, "dropping the record of what the unregister kept failed");
			}
			return Err(Stopped {
				held: Held::Registered(failed.held),
				error: platform_error(&failed.error),
			});
		}
	}
	// Only now the OS let the root go, so a failed unregister keeps the root shown as it is; first
	// of what follows, which may stop.
	let branding_left = unbrand(platform, root, user).await;
	// From here the OS holds nothing: a record that cannot be written stops the unregister before
	// the data and the key go, and the next `unregister` or `register` finishes it.
	let released = |error: &IndexError| Stopped {
		held: Held::Released,
		error: index_error(error),
	};
	let (tidied, record) =
		match tidy_kept(platform, index, user, root, Some(record.clone()), &date).await {
			Ok(tidied) => tidied,
			Err(TidyError::Index(error)) => return Err(released(&error)),
			Err(TidyError::Platform(error)) => {
				warn!(%error, "tidying the unregistered root failed; it stays as it is");
				let record = settle_record(index, user, record, Some(root.to_path_buf()))
					.map_err(|error| released(&error))?;
				(
					Tidied {
						root_removed: false,
						kept_in: Some(root.to_path_buf()),
					},
					Some(record),
				)
			}
		};
	Ok(RootOutcome {
		deleted_hydrated: settled.deleted_hydrated,
		tidied,
		record,
		branding_left,
	})
}

/// `record` settled at `kept_in`, and so recorded in `index`. A record that cannot be saved stays
/// as it was in the index, unsettled, for a later run to settle and report under its id.
fn settle_record(
	index: &mut RootsIndex,
	user: UserId,
	mut record: KeptRecord,
	kept_in: Option<PathBuf>,
) -> Result<KeptRecord, IndexError> {
	record.place = KeptPlace::Settled { kept_in };
	index.put_kept(user, record.clone())?;
	Ok(record)
}

/// Finishes `user`'s root at `root` once the OS let it go: removes its empty directories, then the
/// root when empty, else renames it aside. `record` is what the unregister kept, the aside it
/// planned included; for a root no record names (one the OS never held, which holds ordinary items
/// only, none known to be on the drive), every item but a directory counts as kept, recorded before
/// the rename, by count only. Nothing at the root means a run killed after the rename moved it
/// already: the items are at the planned aside, if anything is there. Each step can be run again
/// after a kill.
async fn tidy_kept<P: Platform>(
	platform: &Arc<P>,
	index: &mut RootsIndex,
	user: UserId,
	root: &Path,
	record: Option<KeptRecord>,
	date: &str,
) -> Result<(Tidied, Option<KeptRecord>), TidyError> {
	let cleared = {
		let platform = Arc::clone(platform);
		let root = root.to_path_buf();
		blocking(move || platform.clear_unregistered(&root)).await?
	};
	let left = match cleared {
		Cleared::Left(left) => left,
		Cleared::Gone => {
			let Some(record) = record else {
				let tidied = Tidied {
					root_removed: true,
					kept_in: None,
				};
				return Ok((tidied, None));
			};
			let kept_in = match record.aside().map(Path::to_path_buf) {
				Some(aside) => {
					let platform = Arc::clone(platform);
					blocking(move || platform.exists(&aside).then_some(aside)).await
				}
				None => None,
			};
			let record = settle_record(index, user, record, kept_in.clone())?;
			let tidied = Tidied {
				root_removed: kept_in.is_none(),
				kept_in,
			};
			return Ok((tidied, Some(record)));
		}
	};
	let mut record = match record {
		Some(record) => {
			// The root renamed to the planned aside by a run killed before it settled, and a
			// folder made at the root's path since: that folder is not the helper's to move.
			if let Some(moved) = moved_to_planned(platform, &record).await {
				let record = settle_record(index, user, record, Some(moved.clone()))?;
				let tidied = Tidied {
					root_removed: false,
					kept_in: Some(moved),
				};
				return Ok((tidied, Some(record)));
			}
			record
		}
		// Recorded with the aside picked below, before the rename.
		None => KeptRecord {
			id: KeptId::random(),
			kept: Vec::new(),
			kept_count: left,
			place: KeptPlace::InRoot { aside: None },
		},
	};
	let planned = record.aside().map(Path::to_path_buf);
	let aside = {
		let platform = Arc::clone(platform);
		let (root, date, planned) = (root.to_path_buf(), date.to_owned(), planned.clone());
		blocking(move || {
			planned
				.filter(|planned| !platform.exists(planned))
				.or_else(|| aside_path(&root, &date, |candidate| platform.exists(candidate)))
		})
		.await
	};
	// Recorded before the rename, so a run killed after it knows where the items went.
	if aside.is_some() && aside != planned {
		record.place = KeptPlace::InRoot {
			aside: aside.clone(),
		};
		index.put_kept(user, record.clone())?;
	}
	let kept_in = match aside {
		Some(aside) => {
			let platform = Arc::clone(platform);
			let (from, to) = (root.to_path_buf(), aside.clone());
			match blocking(move || platform.set_aside(&from, &to)).await {
				Ok(()) => aside,
				Err(error) => {
					warn!(%error, "moving the unregistered root aside failed; it stays where it is");
					root.to_path_buf()
				}
			}
		}
		None => root.to_path_buf(),
	};
	let record = settle_record(index, user, record, Some(kept_in.clone()))?;
	let tidied = Tidied {
		root_removed: false,
		kept_in: Some(kept_in),
	};
	Ok((tidied, Some(record)))
}

/// The aside `record` planned, when the root was renamed to it already: something is there and
/// holds the first item kept. A record with no item listed (a root found let go, counted only)
/// cannot tell, and counts as not renamed.
async fn moved_to_planned<P: Platform>(platform: &Arc<P>, record: &KeptRecord) -> Option<PathBuf> {
	let planned = record.aside()?.to_path_buf();
	let first = record.kept.first()?.path.clone();
	let platform = Arc::clone(platform);
	blocking(move || {
		(platform.exists(&planned) && platform.exists(&planned.join(&first))).then_some(planned)
	})
	.await
}

/// Finishes an unregister of `entry`'s root that stopped after the OS let the root go, before a
/// `register` uses the root's path again: the root, still holding what was kept, is moved aside.
/// When the OS holds the root registered again, what was kept is in it and served, so the record
/// goes. A root another provider registered since is refused, and left as it is. Elevated or with
/// no record of such an unregister, nothing is done.
pub(crate) async fn finish_stopped<P: Platform>(
	platform: &Arc<P>,
	index: &mut RootsIndex,
	entry: &RootEntry,
) -> Result<(), ReplyError> {
	let Some(record) = entry.unsettled().cloned() else {
		return Ok(());
	};
	if platform.elevated() {
		return Ok(());
	}
	{
		let checking = Arc::clone(platform);
		let root = entry.root.clone();
		blocking(move || checking.check_root(&root))
			.await
			.map_err(|refusal| refusal_error(&refusal))?;
	}
	let existing = {
		let platform = Arc::clone(platform);
		let root = entry.root.clone();
		blocking(move || platform.existing_registration(&root))
			.await
			.map_err(|e| platform_error(&e))?
	};
	match existing {
		Some(existing) if existing.ours => {
			return index
				.drop_unsettled(entry.user)
				.map_err(|e| index_error(&e));
		}
		Some(_) => return Err(foreign_root()),
		None => {}
	}
	let date = utc_date(platform.now());
	tidy_kept(
		platform,
		index,
		entry.user,
		&entry.root,
		Some(record),
		&date,
	)
	.await
	.map_err(|e| e.reply())?;
	Ok(())
}

/// Another provider holds the root's path registered.
fn foreign_root() -> ReplyError {
	ReplyError::new(
		ErrorCode::RootInUse,
		"another provider registered the root; it is left alone",
	)
}

/// Unregisters the root `entry` records, `registered` when this run holds its registration, then
/// wipes the user's data directory, deletes the sealing key and forgets the root.
///
/// The index is a file any process of the user can write, so its paths are checked again here
/// before anything is done to them. An elevated process (the uninstaller) does nothing to a path
/// the OS does not confirm as this provider's sync root: it touches a root only when the OS holds
/// this provider's registration of exactly that path, and leaves the data directory and the key,
/// so the index keeps the root for a later run as the user.
pub(crate) async fn unregister_entry<P: Platform>(
	platform: &Arc<P>,
	index: &mut RootsIndex,
	entry: &RootEntry,
	registered: Option<P::Registered>,
	delete_hydrated: bool,
) -> Result<EntryUnregistered, Stopped<P::Registered>> {
	let stopped = |error: ReplyError| Stopped {
		held: Held::Unknown,
		error,
	};
	if entry.unregistered {
		// An earlier unregister finished, root, data and key; only what it kept waits to be told.
		return Ok(EntryUnregistered {
			branding_left: false,
			report: UnregisterReport {
				unregistered: true,
				deleted_hydrated: 0,
				kept: Vec::new(),
				kept_count: 0,
				kept_in: None,
				root_removed: true,
				key_deleted: true,
				data_deleted: true,
				kept_id: None,
				kept_earlier: entry.earlier(None),
			},
		});
	}
	let data_dir = DataDir::new(entry.data_dir.clone())
		.map_err(|e| stopped(ReplyError::new(ErrorCode::DataDirInvalid, e.to_string())))?;
	{
		let checking = Arc::clone(platform);
		let root = entry.root.clone();
		blocking(move || checking.check_root(&root))
			.await
			.map_err(|refusal| stopped(refusal_error(&refusal)))?;
	}
	let elevated = platform.elevated();
	let registered = match registered {
		Some(registered) => Some(registered),
		None => {
			let inspecting = Arc::clone(platform);
			let root = entry.root.clone();
			let existing = blocking(move || inspecting.existing_registration(&root))
				.await
				.map_err(|e| stopped(platform_error(&e)))?;
			match existing {
				None => None,
				Some(existing) if !existing.ours => return Err(stopped(foreign_root())),
				Some(existing) => {
					let account_root =
						existing
							.account_root
							.or(entry.account_root)
							.ok_or_else(|| {
								stopped(ReplyError::new(
									ErrorCode::Internal,
									"the root's account is not known",
								))
							})?;
					let registering = Arc::clone(platform);
					let root = entry.root.clone();
					Some(
						blocking(move || registering.register(&root, account_root))
							.await
							.map_err(|e| stopped(platform_error(&e)))?,
					)
				}
			}
		}
	};
	let date = utc_date(platform.now());
	let outcome = match registered {
		Some(registered) => {
			unregister_root(
				platform,
				index,
				entry.user,
				&entry.root,
				registered,
				delete_hydrated,
				date,
			)
			.await?
		}
		// Not registered, and the path is only what the index says: left as it is.
		None if elevated => RootOutcome {
			deleted_hydrated: 0,
			tidied: Tidied {
				root_removed: false,
				kept_in: Some(entry.root.to_path_buf()),
			},
			record: None,
			// A sync root list entry naming exactly the root is gone with the registration.
			branding_left: unbrand(platform, &entry.root, entry.user).await,
		},
		// Not registered: an unregister stopped after the OS let the root go left the record of
		// what it kept, finished here; a root without one holds ordinary items only (see
		// `tidy_kept`).
		None => {
			let record = index
				.entry(entry.user)
				.and_then(RootEntry::unsettled)
				.cloned();
			let (tidied, record) =
				tidy_kept(platform, index, entry.user, &entry.root, record, &date)
					.await
					.map_err(|e| stopped(e.reply()))?;
			RootOutcome {
				deleted_hydrated: 0,
				tidied,
				record,
				// What an unregister stopped after the OS let the root go may have left.
				branding_left: unbrand(platform, &entry.root, entry.user).await,
			}
		}
	};
	let (data_deleted, key_deleted) = if elevated {
		(false, false)
	} else {
		let platform = Arc::clone(platform);
		let account = entry.user.key_account();
		blocking(move || {
			let data_deleted = SessionStore::wipe_dir(&data_dir)
				.inspect_err(|error| warn!(%error, "deleting the user's data failed"))
				.is_ok();
			let key_deleted = platform
				.key_store(&account)
				.and_then(|keys| keys.delete())
				.inspect_err(|error| warn!(%error, "deleting the user's sealing key failed"))
				.is_ok();
			(data_deleted, key_deleted)
		})
		.await
	};
	// Forgotten only once nothing of the user's is left to find through it, and what was kept
	// was reported.
	if data_deleted && let Err(error) = index.forget(entry.user) {
		warn!(%error, "forgetting the unregistered root failed");
	}
	let this = outcome.record.as_ref().map(|record| record.id);
	let kept_earlier = index
		.entry(entry.user)
		.map(|entry| entry.earlier(this))
		.unwrap_or_default();
	let (kept, kept_count, kept_id) = match outcome.record {
		Some(record) if record.kept_count > 0 => (record.kept, record.kept_count, Some(record.id)),
		Some(_) | None => (Vec::new(), 0, None),
	};
	Ok(EntryUnregistered {
		report: UnregisterReport {
			unregistered: true,
			deleted_hydrated: outcome.deleted_hydrated,
			kept,
			kept_count,
			kept_in: outcome.tidied.kept_in,
			root_removed: outcome.tidied.root_removed,
			key_deleted,
			data_deleted,
			kept_id,
			kept_earlier,
		},
		branding_left: outcome.branding_left,
	})
}

/// What `--unregister-all` did, root by root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnregisterAll {
	/// Each root the index named.
	pub roots: Vec<UnregisterAllEntry>,
	/// The index could not be read, so nothing was done.
	pub index_failed: bool,
}

impl UnregisterAll {
	/// The line reporting it.
	pub fn line(&self) -> OutMessage {
		OutMessage::UnregisterAll {
			roots: self.roots.clone(),
		}
	}

	/// The process's exit code: 0 when every root is unregistered and nothing stayed, 20 when an
	/// item was kept or a root left registered, 21 when nothing could be done. A sealing key left
	/// by an elevated run does not count: the line reports it.
	pub fn exit_code(&self) -> u8 {
		let failed = |root: &UnregisterAllEntry| root.outcome.is_err();
		if self.index_failed || (!self.roots.is_empty() && self.roots.iter().all(failed)) {
			return 21;
		}
		let left = self.roots.iter().any(|root| match &root.outcome {
			Ok(report) => {
				report.kept_count > 0 || report.kept_in.is_some() || !report.kept_earlier.is_empty()
			}
			Err(_) => true,
		});
		if left { 20 } else { 0 }
	}
}

/// Unregisters every root the index in `state_dir` names, as the uninstaller does: no session, so
/// a changed file whose data is not all on disk stops its root's unregister. Clean downloaded files
/// are deleted when `delete_hydrated`. An elevated process leaves the data directories and the
/// sealing keys, and any root the OS does not confirm as this provider's (see
/// [`unregister_entry`]). What was kept stays recorded in the index after the line reported it:
/// the uninstaller shows the user nothing of the line, so the app reports it, if it is installed
/// again, as for a run killed before its reply was read.
pub async fn unregister_all<P: Platform>(
	platform: P,
	state_dir: &StateDir,
	delete_hydrated: bool,
) -> UnregisterAll {
	let platform = Arc::new(platform);
	let dir = state_dir.clone();
	let mut index = match blocking(move || RootsIndex::load(&dir)).await {
		Ok(index) => index,
		Err(error) => {
			warn!(%error, "the roots index cannot be read");
			return UnregisterAll {
				roots: Vec::new(),
				index_failed: true,
			};
		}
	};
	let mut roots = Vec::new();
	for entry in index.entries().to_vec() {
		let outcome = unregister_entry(&platform, &mut index, &entry, None, delete_hydrated).await;
		let (outcome, key_left, branding_left) = match outcome {
			Ok(done) => {
				let key_left = !done.report.key_deleted;
				(Ok(done.report), key_left, done.branding_left)
			}
			// A root still registered is still shown, as it should be.
			Err(stopped) => (Err(stopped.error), true, false),
		};
		roots.push(UnregisterAllEntry {
			user: entry.user,
			root: entry.root,
			outcome,
			key_left,
			branding_left,
		});
	}
	sweep_branding(&platform).await;
	UnregisterAll {
		roots,
		index_failed: false,
	}
}

/// The date `now` falls on in UTC, as `YYYY-MM-DD`.
pub(crate) fn utc_date(now: SystemTime) -> String {
	DateTime::<Utc>::from(now).format("%Y-%m-%d").to_string()
}

/// Where the root at `root` is moved aside to: `<name> (not synced <date>)`, then ` 2`, ` 3` and
/// so on after it, the first that `taken` says is free.
fn aside_path(root: &Path, date: &str, taken: impl Fn(&Path) -> bool) -> Option<PathBuf> {
	let name = root.file_name()?.to_string_lossy().into_owned();
	let parent = root.parent()?;
	(1..=1000_u32).find_map(|n| {
		let candidate = match n {
			1 => parent.join(format!("{name} (not synced {date})")),
			n => parent.join(format!("{name} (not synced {date}) {n}")),
		};
		(!taken(&candidate)).then_some(candidate)
	})
}

#[cfg(any(windows, test))]
/// Whether anything is at `path`: a path whose metadata cannot be read for another reason than
/// its absence counts as taken.
pub(crate) fn path_taken(path: &Path) -> bool {
	!matches!(std::fs::symlink_metadata(path), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
}

#[cfg(any(windows, test))]
/// After an unregister, removes the root's empty ordinary directories deepest first (never a
/// directory with anything in it), then the root itself when empty. A root that is not there is
/// gone already.
pub(crate) fn clear_plain_root(root: &Path) -> std::io::Result<Cleared> {
	match std::fs::symlink_metadata(root) {
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Cleared::Gone),
		Err(e) => return Err(e),
		Ok(_) => {}
	}
	let left = remove_empty_dirs(root)?;
	match std::fs::remove_dir(root) {
		Ok(()) => Ok(Cleared::Gone),
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Cleared::Gone),
		// Not empty.
		Err(_) => Ok(Cleared::Left(left)),
	}
}

#[cfg(any(windows, test))]
/// Renames the unregistered root `root` to `aside`, so its path is free for the next account;
/// refused when anything is at `aside`.
pub(crate) fn set_plain_root_aside(root: &Path, aside: &Path) -> std::io::Result<()> {
	if path_taken(aside) {
		return Err(std::io::Error::new(
			std::io::ErrorKind::AlreadyExists,
			"something is at the aside path",
		));
	}
	std::fs::rename(root, aside)
}

#[cfg(any(windows, test))]
/// Removes the empty directories below `dir`, deepest first, following no link: how many items
/// other than directories are below it.
fn remove_empty_dirs(dir: &Path) -> std::io::Result<u64> {
	let mut left: u64 = 0;
	for entry in std::fs::read_dir(dir)? {
		let entry = entry?;
		if entry.file_type()?.is_dir() {
			let path = entry.path();
			left = left.saturating_add(remove_empty_dirs(&path)?);
			// Fails, and is left, when anything is in it.
			let _ = std::fs::remove_dir(&path);
		} else {
			left = left.saturating_add(1);
		}
	}
	Ok(left)
}

#[cfg(test)]
mod tests;

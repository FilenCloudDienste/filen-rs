//! The job on one path: read what is there and carry what changed to the drive.
//!
//! | What is at the path | Job |
//! |---|---|
//! | nothing | none: a delete reaches the drive only through its request, or, for a file uploaded and not converted yet, the remote file is trashed |
//! | a placeholder in sync, nothing written | none: this is every write of the provider's own |
//! | a file placeholder not in sync, or written | [`edit`]: a new version, or a conflict |
//! | a directory placeholder not in sync | mark it in sync while its remote directory exists |
//! | an ordinary file a placeholder held before | [`replace`]: an editor's atomic save |
//! | an ordinary file, or a detached placeholder | [`create`]: upload it as a new file |
//! | an ordinary directory | [`take_in_dir`]: create it remotely, then look at its children |
//! | a link, or a name the server refuses | skipped and reported |

use std::{
	collections::HashSet,
	sync::{Arc, LazyLock},
	time::Duration,
};

use filen_mobile_native_cache::{
	CacheError,
	ffi::{FfiFile, FfiId, FfiNonRootObject, FfiObject, QueryChildrenResponse, UploadFileInfo},
	remote::CheckedUpload,
};
use filen_sdk_rs::{
	fs::name::{ValidatedName, aside_names, collision_key},
	io::PathMatcher,
	sync_engine::{DEFAULT_IGNORE_PATTERNS, PathPatterns},
};
use filen_types::fs::Uuid;
use tokio::time::{Instant, sleep};
use tracing::{debug, warn};

use super::{
	FileVersion, Inner, Restore, hydration,
	leftover::{self, AsCreated, Leftover},
	lineage::{SAVE_WINDOW, Seen},
	lock,
	queue::{Delay, Kind, Outcome},
	relocate::{Place, Relocated, relocate_file},
	remote,
	report::{ConflictKind, KeptReason, LocalReport, LocalSkip, RemoteApplied},
	veto,
};
use crate::{
	drive::Drive,
	failure::{Answer, Failure, cache_answer},
	identity::{PlaceholderIdentity, stable_id},
	log_safe::{self, path_tag},
	names::{PlaceholderName, is_upload_name, transit_name, upload_name},
	os::{
		FileStamp, LocalEntry, LocalError, LocalFileId, LocalFs, LocalKind, PinAttr,
		PlaceholderState, ticks_to_unix_millis,
	},
	populate::spec_for,
	root_path::RootPath,
};

/// How many hydrations in a row may leave an edit partly on disk before it waits for the next walk.
const HYDRATE_ATTEMPTS: u32 = 3;
/// How many `.old` names are tried before a conflict gives up on keeping the local side aside:
/// fewer than [`aside_names`] offers, as each try costs a filesystem probe on the blocking pool.
const ASIDE_ATTEMPTS: usize = 1000;
/// How soon a job looks again that waits for the remote side to deal with a directory the drive
/// trashed.
const TREE_AGAIN: Duration = Duration::from_secs(1);
/// How soon an ordinary file looks again while the lineage a previous run saved is not taken back.
const RESTORE_WAIT: Duration = Duration::from_secs(1);
/// How soon an edit looks again while the edits a previous run left marked in the cache are not
/// taken over yet.
const TAKE_OVER_WAIT: Duration = Duration::from_secs(1);
/// How soon a look at a pending rename's target looks again: never read before the rename happened.
const RENAME_TARGET_WAIT: Duration = Duration::from_millis(250);
/// How many directories up [`Inner::in_drive_tree`] looks before it stops: past any real depth, so
/// a cycle in a damaged cache cannot hold a job.
const MAX_TREE_DEPTH: usize = 4096;

/// The built-in ignore patterns, compiled once.
static DEFAULT_IGNORES: LazyLock<PathPatterns> = LazyLock::new(|| {
	PathPatterns::parse(DEFAULT_IGNORE_PATTERNS.iter().copied())
		.expect("the built-in ignore patterns compile")
});

/// How a job ends after the cache failed it.
pub(super) fn failed(error: &CacheError) -> Outcome {
	match cache_answer(error) {
		Answer::Fail(failure @ (Failure::Undecryptable | Failure::Internal)) => {
			Outcome::Park(failure)
		}
		Answer::Fail(failure) => Outcome::Failed(failure),
		Answer::Cancelled => Outcome::Again(Duration::ZERO),
		Answer::Damaged => Outcome::Failed(Failure::Transient),
	}
}

/// How a job ends after a local operation failed.
pub(super) fn local_failed(error: LocalError) -> Outcome {
	match error {
		// The item is gone, or is not the one the job started on: whatever is there now is
		// looked at on its own event.
		LocalError::NotFound => Outcome::Done,
		LocalError::Busy => Outcome::Busy,
		// Changed under the job: its next look reads the new state.
		LocalError::Exists | LocalError::Modified => Outcome::Again(Duration::ZERO),
		LocalError::Os(_) => Outcome::Failed(Failure::Transient),
		// Stays so until the user unpins it: retried on its next event or walk.
		LocalError::Pinned => Outcome::Park(Failure::Transient),
	}
}

/// Why the item at `path` is not synced, if it is not.
fn skip_reason(path: &RootPath, is_dir: bool) -> Option<LocalSkip> {
	let name = path.name()?;
	if let Err(error) = ValidatedName::try_from(name.as_str()) {
		return Some(LocalSkip::Name(error.kind));
	}
	if is_upload_name(name.as_str()) {
		// The provider's own: a copy it writes before naming it, left by a stop in the middle.
		return Some(LocalSkip::DefaultIgnored);
	}
	DEFAULT_IGNORES
		.matches(&path.slash_joined(), is_dir)
		.then_some(LocalSkip::DefaultIgnored)
}

/// The name the server knows the item at `path` by: its name, normalized as the server stores
/// it. `None` for the root, or a name the server refuses (skipped before any job).
fn server_name(path: &RootPath) -> Option<ValidatedName> {
	ValidatedName::try_from(path.name()?.as_str()).ok()
}

/// Whether `declared`, a remote file's hash, is the BLAKE3 hash `local`.
fn same_hash(file: &FfiFile, local: &[u8; 32]) -> bool {
	file.meta
		.as_ref()
		.and_then(|meta| meta.hash.as_deref())
		.is_some_and(|declared| declared == local.as_slice())
}

/// Whether a remote file declares no hash, as one an older client uploaded.
fn declares_no_hash(file: &FfiFile) -> bool {
	file.meta.as_ref().is_some_and(|meta| meta.hash.is_none())
}

/// A remote file's stable id and version.
pub(super) fn file_ids(file: &FfiFile) -> Option<(Uuid, Uuid)> {
	Some((
		Uuid::parse_str(&file.stable_uuid).ok()?,
		Uuid::parse_str(&file.uuid).ok()?,
	))
}

/// A listed item's name, when its metadata decrypts.
pub(super) fn object_name(object: &FfiNonRootObject) -> Option<&str> {
	match object {
		FfiNonRootObject::File(file) => file.meta.as_ref().map(|meta| meta.name.as_str()),
		FfiNonRootObject::Dir(dir) => dir.meta.as_ref().map(|meta| meta.name.as_str()),
	}
}

/// A listed item's uuid: a file's stable id, a directory's uuid.
pub(super) fn object_uuid(object: &FfiNonRootObject) -> &str {
	match object {
		FfiNonRootObject::File(file) => &file.stable_uuid,
		FfiNonRootObject::Dir(dir) => &dir.uuid,
	}
}

/// Whether the drive holds `object` in its trash.
pub(super) fn is_trashed(object: &FfiObject) -> bool {
	match object {
		FfiObject::File(file) => file.original_parent.is_some(),
		FfiObject::Dir(dir) => dir.original_parent.is_some(),
		FfiObject::Root(_) => false,
	}
}

/// The uuid text of the directory `object` is in.
pub(super) fn parent_of(object: &FfiObject) -> Option<&str> {
	match object {
		FfiObject::File(file) => Some(&file.parent),
		FfiObject::Dir(dir) => Some(&dir.parent),
		FfiObject::Root(_) => None,
	}
}

/// The child of `listing` whose name is `name` in the server's case folding.
pub(super) fn find_by_name<'a>(
	listing: &'a QueryChildrenResponse,
	name: &str,
) -> Option<&'a FfiNonRootObject> {
	let key = collision_key(name);
	listing
		.objects
		.iter()
		.find(|object| object_name(object).map(collision_key).as_ref() == Some(&key))
}

/// Whether two uuid texts name the same uuid; text that is not a uuid names none.
pub(super) fn same_uuid(a: &str, b: &str) -> bool {
	matches!((Uuid::parse_str(a), Uuid::parse_str(b)), (Ok(a), Ok(b)) if a == b)
}

/// What kind of item a job converts at its end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Was {
	/// An ordinary file: converted into a placeholder.
	Plain,
	/// A placeholder (detached, or one whose identity is updated): given its new identity.
	Placeholder,
}

/// Where a job's convert or update of a file or directory left it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Tagged {
	/// Given its identity at the job's path.
	Here,
	/// Renamed away while still ordinary before its convert, which was taken back: the item is
	/// ordinary again wherever it went, `to`, and a look is queued there
	/// ([`Inner::follow_overtaken`]). `to` is `None` when where it went could not be read, or it
	/// left the root.
	Overtaken { to: Option<RootPath> },
}

/// What a convert through a job's handle left, as the blocking call reads it.
#[derive(Debug)]
enum Converted {
	/// Given its identity at the job's path.
	Here,
	/// Taken back after a rename overtook it ([`Tagged::Overtaken`]), and where the item went, read
	/// through the handle: `Ok(None)` once it left the root.
	Overtaken(Result<Option<RootPath>, LocalError>),
}

/// What an upload made on the drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sent {
	/// A file of its own, which only this job knows of.
	NewFile,
	/// A version of a file the drive held already.
	Version,
}

impl<D: Drive, L: LocalFs> Inner<D, L> {
	/// What is at `path`.
	pub(super) async fn inspect(&self, path: &RootPath) -> Result<LocalEntry, LocalError> {
		let path = path.clone();
		self.fs(move |local| local.inspect(&path)).await
	}

	/// The directory `path` lives in as the drive names it, with its uuid text, or how the job
	/// ends instead: blocked on an ordinary directory, done when it is gone.
	/// A directory whose remote directory the drive holds in its trash, alone or with one above it,
	/// is waited on: the remote side keeps what holds local data in it under a directory created
	/// anew, and nothing is uploaded into the trash meanwhile.
	pub(super) async fn remote_parent(
		self: &Arc<Self>,
		path: &RootPath,
	) -> Result<(FfiId, String), Outcome> {
		let parent = path.parent().ok_or(Outcome::Done)?;
		if parent.is_root() {
			let root = self
				.drive
				.root_uuid()
				.await
				.map_err(|error| failed(&error))?;
			return Ok((FfiId(root.clone()), root));
		}
		match self.inspect(&parent).await {
			Ok(LocalEntry::Placeholder {
				kind: LocalKind::Dir { .. },
				identity: Ok(PlaceholderIdentity::Dir { uuid }),
				..
			}) => {
				let uuid_text = uuid.to_string();
				if !self.in_drive_tree(&uuid_text).await? {
					self.push_kind(&parent, Delay::Now, Kind::Remote);
					return Err(Outcome::Wait(TREE_AGAIN));
				}
				Ok((stable_id(uuid), uuid_text))
			}
			Ok(LocalEntry::Plain {
				kind: LocalKind::Dir { .. },
				..
			}) => Err(Outcome::Blocked(parent)),
			Ok(LocalEntry::Missing) => Err(Outcome::Done),
			// Under an item that is not a synced directory: nothing under it is synced.
			Ok(_) => Err(Outcome::Skipped),
			Err(error) => Err(local_failed(error)),
		}
	}

	/// Waits the settle time and checks the file `id` at `path` still has `stamp`: a file still
	/// being written is not uploaded. The file holds bytes to carry from here on until its job
	/// ends ([`Inner::holds_change`]).
	async fn settle(
		&self,
		path: &RootPath,
		id: LocalFileId,
		stamp: FileStamp,
	) -> Result<(), Outcome> {
		self.holds_change(path);
		sleep(self.config.settle).await;
		match self.inspect(path).await {
			Ok(
				LocalEntry::Plain {
					kind: LocalKind::File(now),
					id: now_id,
				}
				| LocalEntry::Placeholder {
					kind: LocalKind::File(now),
					id: now_id,
					..
				},
			) if now_id == id && now == stamp => Ok(()),
			// Written meanwhile: its writer is still at it, and is waited for as one holding it
			// open is, which never parks the file.
			Ok(
				LocalEntry::Plain {
					kind: LocalKind::File(_),
					id: now_id,
				}
				| LocalEntry::Placeholder {
					kind: LocalKind::File(_),
					id: now_id,
					..
				},
			) if now_id == id => Err(Outcome::Busy),
			Ok(LocalEntry::Missing) => Err(Outcome::Done),
			Ok(_) => Err(Outcome::Again(Duration::ZERO)),
			Err(error) => Err(local_failed(error)),
		}
	}

	/// Opens the file `id` at `path` so no writer can change it, and hashes it.
	async fn open_and_hash(
		&self,
		path: &RootPath,
		id: LocalFileId,
	) -> Result<(L::Settled, [u8; 32]), Outcome> {
		let path = path.clone();
		self.fs(move |local| {
			let mut file = local.open_settled(&path).map_err(local_failed)?;
			let (now_id, _) = local.settled_stamp(&file).map_err(local_failed)?;
			if now_id != id {
				return Err(Outcome::Again(Duration::ZERO));
			}
			let hash = local.hash(&mut file).map_err(local_failed)?;
			Ok((file, hash))
		})
		.await
	}

	/// Hashes the file `id` at `path` while no writer can change it, and lets go of it: a job asks
	/// the drive with no handle held, so no writer is kept out for as long as the drive answers.
	async fn hash_settled(&self, path: &RootPath, id: LocalFileId) -> Result<[u8; 32], Outcome> {
		self.open_and_hash(path, id).await.map(|(_, hash)| hash)
	}

	/// Opens the file `id` at `path` again so no writer can change it, while it still hashes to
	/// `hash`: the handle a job tags the file under once the drive answered. A file written since is
	/// waited for as one held open is.
	async fn reopen_hashed(
		&self,
		path: &RootPath,
		id: LocalFileId,
		hash: [u8; 32],
	) -> Result<L::Settled, Outcome> {
		let (file, now) = self.open_and_hash(path, id).await?;
		if now != hash {
			return Err(Outcome::Busy);
		}
		Ok(file)
	}

	/// Opens the file `id` at `path` again so no writer can change it, whatever it holds now.
	async fn reopen(&self, path: &RootPath, id: LocalFileId) -> Result<L::Settled, Outcome> {
		let path = path.clone();
		self.fs(move |local| {
			let file = local.open_settled(&path).map_err(local_failed)?;
			let (now_id, _) = local.settled_stamp(&file).map_err(local_failed)?;
			if now_id != id {
				return Err(Outcome::Again(Duration::ZERO));
			}
			Ok(file)
		})
		.await
	}

	/// Gives the file `file`, the item `id` at `path`, the identity `identity`: a convert for an
	/// ordinary file, an update for a placeholder.
	///
	/// The handle shares deleting, so the file may be renamed while the job holds it, and the
	/// convert then tags it wherever it went. A rename of a placeholder asks the provider first,
	/// but one of the file while it is still ordinary raises no callback, only the watcher's event,
	/// which may come long after: the file is read at `path` once converted, and when it is no
	/// longer there the convert is taken back ([`Tagged::Overtaken`]), so no placeholder ever
	/// stands where the job never looked (an editor's temporary file renamed over the file it saves
	/// would carry the upload's identity there, and the remote side would move it back to the
	/// name the drive has for it). Should the platform refuse to take it back, the file is detached
	/// through the same handle instead: the remote side leaves a detached placeholder to the local
	/// side, and the job where the file went uploads it as a file of its own. Either way a look at
	/// where it went is queued ([`Inner::follow_overtaken`]).
	async fn tag(
		self: &Arc<Self>,
		path: &RootPath,
		id: LocalFileId,
		file: L::Settled,
		was: Was,
		identity: PlaceholderIdentity,
		in_sync: bool,
	) -> Result<Tagged, Outcome> {
		let at = path.clone();
		let converted = self
			.fs(move |local| match was {
				Was::Placeholder => local
					.update_file(&file, identity, in_sync)
					.map(|()| Converted::Here),
				Was::Plain => {
					#[cfg(feature = "test-seams")]
					super::convert_hold::before_convert(&at);
					local.convert_file(&file, identity, in_sync)?;
					match local.inspect(&at) {
						Ok(
							LocalEntry::Placeholder { id: now, .. }
							| LocalEntry::Plain { id: now, .. },
						) if now == id => Ok(Converted::Here),
						// Unreadable: the file is taken to be where it was, as before the check.
						Err(error) => {
							warn!(item = %path_tag(&at), %error, "reading a file just converted failed");
							Ok(Converted::Here)
						}
						Ok(_) => {
							debug!(path = %at, "a rename overtook a file's convert; taken back");
							if let Err(error) = local.revert_settled(&file) {
								warn!(item = %path_tag(&at), %error, "taking back a convert a rename overtook failed; detaching it");
								let detached = PlaceholderIdentity::Detached {
									nonce: Uuid::new_v4(),
								};
								local
									.update_file(&file, detached, false)
									.inspect_err(|error| {
										warn!(item = %path_tag(&at), %error, "detaching a file whose convert a rename overtook failed");
									})?;
							}
							Ok(Converted::Overtaken(local.settled_path(&file)))
						}
					}
				}
			})
			.await
			.map_err(local_failed)?;
		Ok(self.follow_overtaken(path, converted))
	}

	/// What a convert at `path` left, as a job goes on with it: an overtaken one has a look queued
	/// where the item went, read through the job's handle. The job there may have looked in the
	/// instant the item was a placeholder of this job's identity, found it in sync, and stopped, and
	/// the watcher's events of the rename may all have come by then: nothing else would look before
	/// the next walk. Where it went unread, the root is walked; gone from the root, nothing is left
	/// to look at.
	fn follow_overtaken(self: &Arc<Self>, path: &RootPath, converted: Converted) -> Tagged {
		let went = match converted {
			Converted::Here => return Tagged::Here,
			Converted::Overtaken(went) => went,
		};
		let to = match went {
			Ok(Some(to)) => {
				debug!(%path, %to, "looking where an item whose convert a rename overtook went");
				self.push(&to, Delay::Now);
				Some(to)
			}
			Ok(None) => {
				debug!(%path, "an item whose convert a rename overtook left the root");
				None
			}
			Err(error) => {
				warn!(item = %path_tag(path), %error, "reading where an item whose convert a rename overtook went failed; walking the root");
				Self::request_walk(self);
				None
			}
		};
		Tagged::Overtaken { to }
	}

	/// Gives the directory `id` at `path` the identity `identity`: an update for a detached
	/// placeholder, a convert for an ordinary directory, marked `populated` or not.
	///
	/// The convert goes through a handle that shares deleting, so the directory may be renamed
	/// between the job's check of it and the convert, and while it is still ordinary that rename
	/// raises no callback, only the watcher's event: as for a file ([`Inner::tag`]), the directory is
	/// read at `path` once converted, and when it is no longer there the convert is taken back
	/// ([`Tagged::Overtaken`]). Left so, the directory where it went would stand for the drive's
	/// directory of the old name, which the remote side moves it back to, and nothing would take in
	/// what it holds, as the job there finds a directory placeholder in sync. Should the platform
	/// refuse to take it back, the directory is detached through the same handle instead, which the
	/// job where it went takes in as a directory of its own. Either way a look at where it went is
	/// queued ([`Inner::follow_overtaken`]). [`LocalError::NotFound`] says the directory was never
	/// converted: one converted and then neither taken back nor detached fails otherwise.
	async fn tag_dir(
		self: &Arc<Self>,
		path: &RootPath,
		id: LocalFileId,
		identity: PlaceholderIdentity,
		populated: bool,
	) -> Result<Tagged, LocalError> {
		let at = path.clone();
		let converted = self
			.fs(move |local| {
				if let Ok(LocalEntry::Placeholder { .. }) = local.inspect(&at) {
					// A placeholder's rename asks the provider first.
					return local
						.update_dir(&at, id, identity)
						.map(|()| Converted::Here);
				}
				let dir = local.open_dir(&at, id)?;
				#[cfg(feature = "test-seams")]
				super::convert_hold::before_convert(&at);
				local.convert_dir(&dir, identity, populated)?;
				match local.inspect(&at) {
					Ok(
						LocalEntry::Placeholder { id: now, .. } | LocalEntry::Plain { id: now, .. },
					) if now == id => Ok(Converted::Here),
					// Unreadable: the directory is taken to be where it was, as before the check.
					Err(error) => {
						warn!(item = %path_tag(&at), %error, "reading a directory just converted failed");
						Ok(Converted::Here)
					}
					Ok(_) => {
						debug!(path = %at, "a rename overtook a directory's convert; taken back");
						if let Err(error) = local.revert_dir(&dir) {
							warn!(item = %path_tag(&at), %error, "taking back a directory's convert a rename overtook failed; detaching it");
							let detached = PlaceholderIdentity::Detached {
								nonce: Uuid::new_v4(),
							};
							local.update_open_dir(&dir, detached).map_err(|error| {
								warn!(item = %path_tag(&at), %error, "detaching a directory whose convert a rename overtook failed");
								// Converted: never read as a directory that was not.
								if matches!(error, LocalError::NotFound) {
									LocalError::Modified
								} else {
									error
								}
							})?;
						}
						Ok(Converted::Overtaken(local.dir_path(&dir)))
					}
				}
			})
			.await?;
		Ok(self.follow_overtaken(path, converted))
	}

	/// As [`Inner::tag`] for a placeholder, only while the placeholder at `path` still holds
	/// `expect`: the remote side may have detached it meanwhile, and then the job looks again
	/// ([`LocalError::Modified`]).
	async fn retag(
		&self,
		path: &RootPath,
		file: L::Settled,
		expect: PlaceholderIdentity,
		identity: PlaceholderIdentity,
		in_sync: bool,
	) -> Result<(), Outcome> {
		let path = path.clone();
		self.fs(move |local| match local.inspect(&path)? {
			LocalEntry::Placeholder {
				identity: Ok(held), ..
			} if held == expect => local.update_file(&file, identity, in_sync),
			_ => Err(LocalError::Modified),
		})
		.await
		.map_err(local_failed)
	}

	/// Whether the directory `dir` (uuid text) is in the drive's tree as the cache holds it:
	/// neither it nor a directory above it in the trash. The root, and a directory the cache does
	/// not hold, count as in the tree.
	pub(super) async fn in_drive_tree(&self, dir: &str) -> Result<bool, Outcome> {
		let root = lock(&self.state).index.uuid_at(&RootPath::root());
		let mut at = Uuid::parse_str(dir).ok();
		for _ in 0..MAX_TREE_DEPTH {
			let Some(uuid) = at.filter(|uuid| Some(*uuid) != root) else {
				return Ok(true);
			};
			match self.drive.query_item(&stable_id(uuid)).await {
				Ok(Some(item @ FfiObject::Dir(_))) => {
					if is_trashed(&item) {
						return Ok(false);
					}
					at = parent_of(&item).and_then(|parent| Uuid::parse_str(parent).ok());
				}
				Ok(_) => return Ok(true),
				Err(error) => return Err(failed(&error)),
			}
		}
		Ok(true)
	}

	fn remember(&self, path: &RootPath, stable: Uuid, version: Uuid) {
		lock(&self.state)
			.lineage
			.remember(path, stable, version, Instant::now());
	}

	/// How a job ends that just marked the placeholder `id` at `path` in sync: done once it reads
	/// back in sync, and otherwise looked at again, as a look that came to nothing, so a mark the
	/// platform acknowledges and does not keep backs off and parks rather than coming back for ever
	/// on the events its own write raises. A file written since reads back not in sync too, and its
	/// next look sends it.
	async fn marked(&self, path: &RootPath, id: LocalFileId) -> Outcome {
		match self.inspect(path).await {
			Ok(LocalEntry::Placeholder { id: now, state, .. }) if now == id && !state.in_sync() => {
				debug!(%path, "a placeholder marked in sync reads back not in sync");
				Outcome::Again(Duration::ZERO)
			}
			_ => Outcome::Done,
		}
	}

	/// Records that the job on `path` found a local change there to carry: the path counts as
	/// holding what the drive lacks from now until its job ends.
	fn holds_change(&self, path: &RootPath) {
		lock(&self.state).queue.holds_change(path);
		self.looks
			.send_modify(|looks| *looks = looks.wrapping_add(1));
	}

	/// Records that the job on `path` sent the bytes hashed `hash` to the drive.
	fn sent(&self, path: &RootPath, hash: [u8; 32]) {
		lock(&self.state).queue.sent(path, hash);
	}

	fn report_skip(&self, path: &RootPath, reason: LocalSkip) -> Outcome {
		self.report(LocalReport::Skipped {
			path: path.clone(),
			reason,
		});
		Outcome::Skipped
	}
}

/// The job on `path`.
pub(super) async fn look<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
) -> Outcome {
	if path.is_root() {
		return Outcome::Done;
	}
	if inner.renaming_onto(path) {
		return Outcome::Wait(RENAME_TARGET_WAIT);
	}
	let entry = match inner.inspect(path).await {
		Ok(entry) => entry,
		Err(error) => return local_failed(error),
	};
	match entry {
		LocalEntry::Missing => {
			forget_upload(inner, path);
			// A path read back from a previous run claims no file once the ordinary file it claimed
			// is gone: its remote file gets its placeholder again.
			let mut state = lock(&inner.state);
			if state.lineage.get(path).is_some_and(|seen| seen.restored) {
				state.lineage.forget(path);
			}
			Outcome::Done
		}
		LocalEntry::Unsupported(kind) => inner.report_skip(path, LocalSkip::Unsupported(kind)),
		LocalEntry::Plain { kind, id } => {
			if let Some(reason) = skip_reason(path, matches!(kind, LocalKind::Dir { .. })) {
				return inner.report_skip(path, reason);
			}
			match kind {
				LocalKind::Dir { created } => take_in_dir(inner, path, id, created).await,
				LocalKind::File(stamp) => {
					let seen = {
						let mut state = lock(&inner.state);
						// A file being uploaded under another path: follow it, so the upload's end
						// converts it here.
						if let Some(at) = state.uploading.get_mut(&id) {
							*at = path.clone();
							return Outcome::Done;
						}
						// A save a previous run left may claim it: taken in only once that is known.
						match &state.restore {
							Restore::Loading => return Outcome::Wait(RESTORE_WAIT),
							Restore::Walking(saved) if saved.contains_key(&path.case_key()) => {
								return Outcome::Wait(RESTORE_WAIT);
							}
							Restore::Walking(_) | Restore::Done => {}
						}
						state.lineage.get(path).cloned()
					};
					match claim(inner, path, seen).await {
						Some(seen) => replace(inner, path, id, stamp, seen).await,
						None => create(inner, path, id, stamp, Was::Plain).await,
					}
				}
			}
		}
		LocalEntry::Placeholder {
			kind,
			id,
			identity,
			state,
		} => match (kind, identity) {
			(_, Ok(PlaceholderIdentity::Root { .. })) => Outcome::Done,
			(LocalKind::Dir { .. }, Ok(PlaceholderIdentity::Dir { uuid })) => {
				if state.in_sync() {
					if state.pin() != PinAttr::Unspecified {
						// Pinned or unpinned by the user: its items follow.
						inner.push_kind(path, Delay::Now, Kind::Hydration);
					}
					Outcome::Done
				} else {
					resync_dir(inner, path, id, uuid).await
				}
			}
			// Kept when its remote directory went: created on the drive anew.
			(LocalKind::Dir { created }, Ok(PlaceholderIdentity::Detached { .. })) => {
				if let Some(reason) = skip_reason(path, true) {
					return inner.report_skip(path, reason);
				}
				take_in_dir(inner, path, id, created).await
			}
			(
				LocalKind::File(stamp),
				Ok(identity @ PlaceholderIdentity::File { stable, version }),
			) => {
				let clean = state.in_sync() && state.modified_bytes() == 0;
				if remote::may_hold_edit(state, stamp.size)
					&& remote::detached_as_extra(inner, path, id, identity).await
				{
					// Another placeholder of the file stands where the drive has it: this edit is
					// uploaded as a file of its own, never over that one's version.
					return create(inner, path, id, stamp, Was::Placeholder).await;
				}
				inner.remember(path, stable, version);
				if !clean && state.pin() == PinAttr::Unpinned {
					// "Free up space" on a file holding local changes: kept and reported, and the
					// request cleared, as it is a one-shot action; the changes are uploaded below.
					hydration::clear_pin(inner, path, id, PinAttr::Unpinned).await;
					inner.report(LocalReport::KeptDownloaded { path: path.clone() });
				}
				if clean
					&& (hydration::partly_on_disk(kind, state)
						|| state.pin() != PinAttr::Unspecified)
				{
					// A reader left it partly on disk, or a close came after the last transfer, or
					// the user pinned or unpinned it: the hydration job brings it to that.
					inner.push_kind(path, Delay::Now, Kind::Hydration);
				}
				edit(inner, path, id, stamp, state, stable, version).await
			}
			(LocalKind::File(stamp), Ok(PlaceholderIdentity::Detached { .. }) | Err(_)) => {
				if let Some(reason) = skip_reason(path, false) {
					return inner.report_skip(path, reason);
				}
				create(inner, path, id, stamp, Was::Placeholder).await
			}
			_ => inner.report_skip(path, LocalSkip::UnknownPlaceholder),
		},
	}
}

/// The ordinary file `path` was uploaded as a new file and is gone before it became its
/// placeholder: deleted, so the remote file goes to the trash once that is confirmed.
fn forget_upload<D: Drive, L: LocalFs>(inner: &Arc<Inner<D, L>>, path: &RootPath) {
	let uploaded = lock(&inner.state)
		.lineage
		.get(path)
		.filter(|seen| seen.unconverted)
		.map(|seen| (seen.stable, seen.version));
	if let Some((stable, version)) = uploaded {
		veto::trash_later(inner, path, PlaceholderIdentity::File { stable, version });
	}
}

/// Whether `seen`, the file a placeholder at `path` last held, still claims the ordinary file now
/// there as its new version. A placeholder renamed away claims it only while the rename is recent
/// or the renamed placeholder is gone, as an editor's save leaves it: a file created at the old
/// name long after a rename is a new file, and the claim is forgotten.
async fn claim<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	seen: Option<Seen>,
) -> Option<Seen> {
	let seen = seen?;
	if still_claims(inner, &seen).await {
		return Some(seen);
	}
	lock(&inner.state).lineage.forget(path);
	None
}

/// [`claim`]'s test, with nothing forgotten: whether `seen` still claims an ordinary file at its
/// path. A renamed placeholder at a pending rename's target is never read: taken as still there.
pub(super) async fn still_claims<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	seen: &Seen,
) -> bool {
	let Some(moved_to) = &seen.moved_to else {
		return true;
	};
	if seen.at.elapsed() <= SAVE_WINDOW || inner.renaming_onto(moved_to) {
		return true;
	}
	!matches!(
		inner.inspect(moved_to).await,
		Ok(LocalEntry::Placeholder {
			identity: Ok(PlaceholderIdentity::File { stable, .. }),
			..
		}) if stable == seen.stable
	)
}

/// Whether a save is taking the file `stable` over: an ordinary file, not yet uploaded, stands at
/// a path other than `except` whose placeholder last held the file and still claims it, as an
/// editor's save leaves its new file where the old one was renamed away from or deleted. The
/// file's new version is that file's, and the remote side places or moves no placeholder of it
/// meanwhile. A file the local side ignores is never taken in, so it claims nothing; a pending
/// rename's target is never read ([`Inner::renaming_onto`]).
pub(super) async fn claimed_by_save<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	stable: Uuid,
	except: &RootPath,
) -> bool {
	let others: Vec<Seen> = {
		let state = lock(&inner.state);
		state
			.lineage
			.others(stable, except)
			.iter()
			.filter(|other| skip_reason(other, false).is_none())
			.filter_map(|other| state.lineage.get(other).cloned())
			.collect()
	};
	for seen in others {
		let other = seen.path();
		if inner.renaming_onto(other) {
			continue;
		}
		match inner.inspect(other).await {
			Ok(LocalEntry::Plain {
				kind: LocalKind::File(_),
				..
			}) if still_claims(inner, &seen).await => return true,
			Ok(_) => {}
			// Taken as no claim, so a path that keeps failing never holds the file back for good;
			// a second placeholder that results is healed as an extra one.
			Err(error) => {
				warn!(item = %path_tag(other), %error, "reading a path that may claim a file for a save failed");
			}
		}
	}
	false
}

/// A directory placeholder not in sync (a rename or an attribute change cleared it): marked in
/// sync again while its remote directory exists. One the drive no longer holds, or holds in its
/// trash, is removed with every clean item in it, as the remote side removes it.
async fn resync_dir<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	id: LocalFileId,
	uuid: Uuid,
) -> Outcome {
	match inner.drive.refresh_item(stable_id(uuid)).await {
		Ok(Some(FfiObject::Dir(dir))) if dir.original_parent.is_none() => {
			let target = path.clone();
			match inner
				.fs(move |local| local.set_dir_in_sync(&target, id))
				.await
			{
				Ok(()) => inner.marked(path, id).await,
				Err(error) => local_failed(error),
			}
		}
		Ok(Some(FfiObject::Dir(_)) | None) => remote::remove_gone_dir(inner, path, uuid).await,
		Ok(Some(_)) => Outcome::Park(Failure::Internal),
		Err(error) => failed(&error),
	}
}

/// A file placeholder whose data may have changed: not in sync, or holding bytes written since it
/// was last marked in sync (an append through an append-only handle leaves it in sync), or in sync
/// at a size the version it holds does not have.
///
/// Its bytes become the placeholder's content as they are: never downloaded again. Equal to the
/// remote head, the placeholder is re-marked in sync (a rename or an attribute change cleared it);
/// edited from the version the remote head still is, it is uploaded as a new version; edited while
/// the remote got a newer version, both are kept; its remote file gone, it is detached and uploaded
/// as a new file.
async fn edit<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	id: LocalFileId,
	stamp: FileStamp,
	state: PlaceholderState,
	stable: Uuid,
	version: Uuid,
) -> Outcome {
	if state.in_sync() && state.modified_bytes() == 0 {
		let resized = match inner.drive.query_item(&stable_id(stable)).await {
			Ok(Some(FfiObject::File(head))) => {
				Uuid::parse_str(&head.uuid).ok() == Some(version)
					&& u64::try_from(head.size).ok() != Some(stamp.size)
			}
			_ => false,
		};
		if !resized {
			return Outcome::Done;
		}
	}
	let edited = PlaceholderIdentity::File { stable, version };
	if state.on_disk() < stamp.size {
		if state.modified_bytes() == 0 {
			// A write hydrates the whole file first, so a file not all on disk with no byte written
			// had only its name or attributes changed.
			let marked = mark_in_sync(inner, path, id, edited).await;
			if marked == Outcome::Done
				&& !lock(&inner.state).lineage.others(stable, path).is_empty()
			{
				// Clean now, it may be an extra placeholder of its file, which the remote side
				// removes: one it handed over while it was not in sync.
				inner.push_kind(path, Delay::Now, Kind::Remote);
			}
			return marked;
		}
		if lock(&inner.state)
			.stalled
			.get(&path.case_key())
			.is_some_and(|stalls| *stalls >= HYDRATE_ATTEMPTS)
		{
			return Outcome::Park(Failure::Transient);
		}
		// Bytes were written: an edit, whatever the hydration takes.
		inner.holds_change(path);
		let target = path.clone();
		return match inner.fs(move |local| local.hydrate(&target, id)).await {
			// Looked at again at once, unless the hydration left the file partly on disk: then it
			// backs off, and after a few such hydrations waits for the next walk, rather than
			// hydrating again on every event its own hydration raises.
			Ok(()) => match inner.inspect(path).await {
				Ok(LocalEntry::Placeholder {
					kind: LocalKind::File(now),
					state,
					..
				}) if state.on_disk() < now.size => {
					*lock(&inner.state)
						.stalled
						.entry(path.case_key())
						.or_default() += 1;
					warn!(item = %path_tag(path), "hydrating a partly downloaded edit left it partly on disk");
					Outcome::Failed(Failure::Transient)
				}
				_ => {
					lock(&inner.state).stalled.remove(&path.case_key());
					Outcome::Again(Duration::ZERO)
				}
			},
			Err(LocalError::NotFound) => Outcome::Done,
			Err(LocalError::Busy) => Outcome::Busy,
			// An edit of a file partly on disk cannot be uploaded without its missing bytes.
			Err(error) => {
				warn!(item = %path_tag(path), %error, "hydrating a partly downloaded edit failed");
				Outcome::Park(Failure::Transient)
			}
		};
	}
	if let Err(outcome) = edits_may_upload(inner, path, stable) {
		return outcome;
	}
	if let Err(outcome) = inner.settle(path, id, stamp).await {
		return outcome;
	}
	let _upload = inner.uploads.acquire().await;
	let hash = match inner.hash_settled(path, id).await {
		Ok(hash) => hash,
		Err(outcome) => return outcome,
	};
	let head = match inner.drive.refresh_item(stable_id(stable)).await {
		Ok(Some(FfiObject::File(head))) if head.original_parent.is_none() => Some(head),
		Ok(Some(FfiObject::File(_)) | None) => None,
		Ok(Some(_)) => return Outcome::Park(Failure::Internal),
		Err(error) => return failed(&error),
	};
	let head = match head {
		Some(head) => match inner.in_drive_tree(&head.parent).await {
			Ok(true) => Some(head),
			Ok(false) => None,
			Err(outcome) => return outcome,
		},
		None => None,
	};
	let Some(head) = head else {
		// The remote file is gone or in the trash, alone or with a directory above it: the edit
		// becomes a new file, and the remote copy stays where it is.
		return keep_as_new(inner, path, id).await;
	};
	let Some((_, head_version)) = file_ids(&head) else {
		return Outcome::Park(Failure::Internal);
	};
	// A remote file declaring no hash cannot be told from these bytes by the drive: the platform's
	// count of bytes written since the placeholder was last in sync says whether they are its own,
	// read again below once no writer can change the file.
	let hashless_unchanged = declares_no_hash(&head)
		&& head_version == version
		&& u64::try_from(head.size).ok() == Some(stamp.size)
		&& state.modified_bytes() == 0;
	if same_hash(&head, &hash) || hashless_unchanged {
		let identity = PlaceholderIdentity::File {
			stable,
			version: head_version,
		};
		let file = match inner.reopen_hashed(path, id, hash).await {
			Ok(file) => file,
			Err(outcome) => return outcome,
		};
		if !same_hash(&head, &hash) && !unwritten(inner, path, id).await {
			// Written since the job read it: its next look sends it.
			return Outcome::Again(Duration::ZERO);
		}
		inner.remember(path, stable, head_version);
		return match inner.retag(path, file, edited, identity, true).await {
			Ok(()) => inner.marked(path, id).await,
			Err(outcome) => outcome,
		};
	}
	if head_version != version {
		return keep_edit_aside(inner, path, id, head).await;
	}
	// No handle is held over the upload, so a writer is never kept out for its length: the copy
	// the drive takes is refused unless it hashes to what was hashed here.
	let uploaded = match upload_version(inner, path, stable, hash).await {
		Ok(uploaded) => uploaded,
		Err(outcome) => return outcome,
	};
	inner.sent(path, hash);
	// The drive trashed a directory above the file while the upload ran: the new version went
	// into the trash with it, so the edit is kept as a new file.
	match inner.in_drive_tree(&uploaded.parent).await {
		Ok(true) => {}
		Ok(false) => return keep_as_new(inner, path, id).await,
		Err(outcome) => return outcome,
	}
	finish_upload(
		inner,
		path,
		id,
		&uploaded,
		Was::Placeholder,
		Sent::Version,
		Some(edited),
	)
	.await
}

/// Whether an edit of the file `stable` at `path` may go on to its upload, checked before its job
/// reads the remote head, so the version the edit is checked against is read after any wait: not
/// before the edits a previous run left marked in the cache are taken over, as the upload's copy
/// goes through the same cache slot and would overwrite a marked edit no placeholder holds, and not
/// while an unsent edit of this file is not kept yet. A job that may not ends, letting go of its
/// slot, and looks again; once the take-over is done it stays so, and the unkept edits only go.
fn edits_may_upload<D: Drive, L: LocalFs>(
	inner: &Inner<D, L>,
	path: &RootPath,
	stable: Uuid,
) -> Result<(), Outcome> {
	if !*inner.taken_over.borrow() {
		return Err(Outcome::Wait(TAKE_OVER_WAIT));
	}
	if lock(&inner.state).unkept.contains(&stable) {
		warn!(item = %path_tag(path), "an unsent edit of the file is not kept yet; its upload waits for it");
		return Err(Outcome::Park(Failure::Transient));
	}
	Ok(())
}

/// Uploads the file at `path`, hashed as `hash` while no writer had it open, as a new version of
/// the file `stable`, once [`edits_may_upload`] allowed it. A copy taken after a write since is
/// never uploaded: the job looks again once the writer is done, backing off as for a file held
/// open. The local file stays the edit's copy whatever happens: the cache keeps nothing of a failed
/// upload for a drain to send later.
///
/// The server takes a version with no check of the one it follows: one another device uploads
/// after the job read the head and before this upload ends becomes an older version of the file
/// (or, with versioning off, is gone). The job reads the head right before, which keeps that
/// window to the upload itself; nothing on the client closes it.
async fn upload_version<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	stable: Uuid,
	hash: [u8; 32],
) -> Result<FfiFile, Outcome> {
	let upload_path = inner.local.upload_path(path);
	match inner
		.drive
		.upload_version(
			stable_id(stable),
			upload_path.to_string_lossy().into_owned(),
			hash,
		)
		.await
	{
		Ok(CheckedUpload::Uploaded(uploaded)) => Ok(*uploaded),
		Ok(CheckedUpload::Changed) => {
			// Each try copies and hashes the whole file: a writer that keeps writing backs it off.
			debug!(%path, "a file written while it was copied for its upload is looked at again");
			Err(Outcome::Busy)
		}
		Err(error) => Err(failed(&error)),
	}
}

/// Whether the file placeholder `id` at `path` holds no byte written since it was last in sync.
async fn unwritten<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	id: LocalFileId,
) -> bool {
	matches!(
		inner.inspect(path).await,
		Ok(LocalEntry::Placeholder { id: now, state, .. }) if now == id && state.modified_bytes() == 0
	)
}

/// Detaches the edited file placeholder `id` at `path`, whose remote file the drive no longer holds
/// outside its trash, so it is uploaded as a new file; reported kept.
async fn keep_as_new<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	id: LocalFileId,
) -> Outcome {
	let (target, detached) = (
		path.clone(),
		PlaceholderIdentity::Detached {
			nonce: Uuid::new_v4(),
		},
	);
	match inner
		.fs(move |local| local.retag_unsynced(&target, id, detached))
		.await
	{
		Ok(()) => {
			inner.report(LocalReport::Remote(RemoteApplied::KeptLocal {
				path: path.clone(),
				reason: KeptReason::RemoteGone,
			}));
			Outcome::Again(Duration::ZERO)
		}
		Err(error) => local_failed(error),
	}
}

/// Marks the placeholder `id` at `path`, not all on disk and holding no written byte, in sync as
/// `identity`, its content unchanged and never read: a rename of a file that is not on disk
/// downloads nothing. One written or hydrated since the job looked is looked at again.
async fn mark_in_sync<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	id: LocalFileId,
	identity: PlaceholderIdentity,
) -> Outcome {
	let target = path.clone();
	let marked = inner
		.fs(move |local| local.mark_partial_in_sync(&target, id, identity))
		.await;
	match marked {
		Ok(()) => inner.marked(path, id).await,
		Err(error) => local_failed(error),
	}
}

/// After `uploaded` reached the drive from the file `id` at `path`: gives the file its identity,
/// marked in sync when its bytes are still the ones uploaded. A file written meanwhile stays not in
/// sync and is looked at again; one a writer holds is tagged without being marked, or, ordinary,
/// left for its next look, which finds the upload through the lineage. A placeholder that held
/// `expect` when the upload started is given its identity only while it still holds it. An
/// ordinary file renamed away before its convert ([`Tagged::Overtaken`]) is left to the job where
/// it went; a new file's upload, which only this job knew of, goes to the trash, as the file's
/// bytes are still on disk for that job to send. A new file gone from the path before it could be
/// opened has its upload trashed once that is confirmed, as one deleted while it uploaded.
async fn finish_upload<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	id: LocalFileId,
	uploaded: &FfiFile,
	was: Was,
	sent: Sent,
	expect: Option<PlaceholderIdentity>,
) -> Outcome {
	let Some((stable, version)) = file_ids(uploaded) else {
		return Outcome::Park(Failure::Internal);
	};
	let identity = PlaceholderIdentity::File { stable, version };
	inner.remember(path, stable, version);
	inner.hints.uploaded(FileVersion { stable, version });
	match inner.open_and_hash(path, id).await {
		Ok((file, hash)) => {
			let in_sync = same_hash(uploaded, &hash);
			let tagged = match expect {
				Some(expect) => inner
					.retag(path, file, expect, identity, in_sync)
					.await
					.map(|()| Tagged::Here),
				None => inner.tag(path, id, file, was, identity, in_sync).await,
			};
			if sent == Sent::Version && tagged.is_ok() {
				// The file's new version now has its local file, here or where a rename took it: a
				// delete of an old copy of the file must not trash it.
				veto::forget_deletes_of(inner, stable);
			}
			match tagged {
				Ok(Tagged::Here) => {}
				Ok(Tagged::Overtaken { .. }) => {
					overtaken_upload(inner, path, stable, sent);
					return Outcome::Done;
				}
				Err(outcome) => return outcome,
			}
			if in_sync {
				// Its bytes were just hashed equal to what the drive holds.
				inner.hints.verified(FileVersion { stable, version });
			}
			inner.report(LocalReport::Uploaded {
				path: path.clone(),
				identity,
			});
			if in_sync {
				inner.marked(path, id).await
			} else {
				Outcome::Again(Duration::ZERO)
			}
		}
		Err(Outcome::Busy) => {
			match was {
				Was::Placeholder => {
					let target = path.clone();
					if let Err(error) = inner
						.fs(move |local| {
							let held = match local.inspect(&target)? {
								LocalEntry::Placeholder {
									identity: Ok(held), ..
								} => Some(held),
								_ => None,
							};
							if expect.is_some_and(|expect| held != Some(expect)) {
								return Err(LocalError::Modified);
							}
							local.retag_unsynced(&target, id, identity)
						})
						.await
					{
						return local_failed(error);
					}
				}
				// Still ordinary: deleted before its next look, the remote file goes too.
				Was::Plain => lock(&inner.state).lineage.remember_upload(
					path,
					stable,
					version,
					Instant::now(),
				),
			}
			inner.report(LocalReport::Uploaded {
				path: path.clone(),
				identity,
			});
			Outcome::Busy
		}
		Err(outcome @ (Outcome::Done | Outcome::Again(_))) if sent == Sent::NewFile => {
			// Gone from the path before it got its identity (renamed away while still ordinary, or
			// deleted), or another file stands there now: as for a file deleted while it uploaded,
			// the new file's upload goes to the trash once the path is confirmed not to hold it,
			// and the job where the file went, if it went anywhere, sends it again.
			forget_claim(inner, path, stable);
			veto::trash_later(inner, path, identity);
			outcome
		}
		Err(outcome) => outcome,
	}
}

/// Forgets that `path` last held the file `stable`, if it does.
fn forget_claim<D: Drive, L: LocalFs>(inner: &Inner<D, L>, path: &RootPath, stable: Uuid) {
	let mut state = lock(&inner.state);
	if state
		.lineage
		.get(path)
		.is_some_and(|seen| seen.stable == stable)
	{
		state.lineage.forget(path);
	}
}

/// The upload `stable` of the file the job at `path` sent, renamed away while still ordinary before
/// its convert, which was taken back: `path` no longer claims it. A new file's upload, which only
/// this job knew of, is trashed, unless a rename of the file the platform announced after all (the
/// rename came between the convert and the check) is settling it: the job where the file went
/// uploads its bytes again, as a new file or, over a save's target, as that file's new version. A
/// version stays the file's latest.
fn overtaken_upload<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	stable: Uuid,
	sent: Sent,
) {
	forget_claim(inner, path, stable);
	let settling = {
		let mut state = lock(&inner.state);
		let settling = state
			.rename_records
			.iter()
			.any(|record| record.concerns(stable));
		if sent == Sent::NewFile && !settling {
			state.index.remove_id(stable);
		}
		settling
	};
	if sent == Sent::NewFile && !settling {
		veto::trash_transient(inner, stable);
	}
}

/// An ordinary file, or a detached placeholder, uploaded as a new file. A remote file of the same
/// name and content is adopted instead (an upload whose convert never happened, or a copy of what
/// the drive holds); another item of the name keeps both. A file renamed while it uploads is
/// converted where it went; one deleted meanwhile has its new remote file trashed.
///
/// The upload goes in under a name of its own ([`upload_name`]) and is then renamed to the file's
/// name, which the server refuses while another file holds it: an upload under the name itself
/// would make the upload the new version of a file another device created there meanwhile, its
/// content kept only as an old version, or not at all with versioning off. A file of the name
/// that turns up meanwhile keeps both. An upload that never got its name (the provider stopped
/// between the two) is found by its name and content and named instead of uploaded again.
async fn create<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	id: LocalFileId,
	stamp: FileStamp,
	was: Was,
) -> Outcome {
	let (parent_id, _) = match inner.remote_parent(path).await {
		Ok(parent) => parent,
		Err(outcome) => return outcome,
	};
	let Some(name) = server_name(path) else {
		return Outcome::Skipped;
	};
	if let Err(outcome) = inner.settle(path, id, stamp).await {
		return outcome;
	}
	let _upload = inner.uploads.acquire().await;
	let hash = match inner.hash_settled(path, id).await {
		Ok(hash) => hash,
		Err(outcome) => return outcome,
	};
	let listing = match inner.drive.list_children(parent_id.clone()).await {
		Ok(Some(listing)) => listing,
		Ok(None) => return Outcome::Failed(Failure::NotFound),
		Err(error) => return failed(&error),
	};
	match find_by_name(&listing, name.as_ref()) {
		Some(FfiNonRootObject::File(remote))
			if u64::try_from(remote.size).ok() == Some(stamp.size) && same_hash(remote, &hash) =>
		{
			let Some((stable, version)) = file_ids(remote) else {
				return Outcome::Park(Failure::Internal);
			};
			let identity = PlaceholderIdentity::File { stable, version };
			let file = match inner.reopen_hashed(path, id, hash).await {
				Ok(file) => file,
				Err(outcome) => return outcome,
			};
			match inner.tag(path, id, file, was, identity, true).await {
				Ok(Tagged::Here) => {}
				// Renamed away: the job where it went adopts it there.
				Ok(Tagged::Overtaken { .. }) => return Outcome::Done,
				Err(outcome) => return outcome,
			}
			inner.remember(path, stable, version);
			inner.report(LocalReport::Adopted {
				path: path.clone(),
				identity,
			});
			return inner.marked(path, id).await;
		}
		Some(taken) => {
			return keep_new_aside(inner, path, &listing, taken, ConflictKind::CreateVsExisting)
				.await;
		}
		None => {}
	}
	let (uploaded, current) = match unnamed_upload(&listing, id, &hash, stamp.size) {
		Some(earlier) => (earlier.clone(), path.clone()),
		None => {
			lock(&inner.state).uploading.insert(id, path.clone());
			let info = UploadFileInfo {
				name: upload_name(id.get()),
				creation: ticks_to_unix_millis(stamp.created),
				modification: ticks_to_unix_millis(stamp.modified),
				mime: None,
			};
			let upload_path = inner.local.upload_path(path);
			let result = inner
				.drive
				.upload_file(
					upload_path.to_string_lossy().into_owned(),
					parent_id.clone(),
					info,
				)
				.await;
			let current = lock(&inner.state)
				.uploading
				.remove(&id)
				.unwrap_or_else(|| path.clone());
			match result {
				Ok(uploaded) => {
					inner.sent(path, hash);
					// A new file the user made: an editor's temporary file may be one.
					if was == Was::Plain
						&& let Some((stable, version)) = file_ids(&uploaded)
					{
						lock(&inner.state)
							.fresh
							.uploaded(stable, version, Instant::now());
					}
					(uploaded, current)
				}
				Err(CacheError::InvalidName(error)) => {
					debug!(%path, %error, "the drive refused a name");
					return inner.report_skip(path, LocalSkip::DriveRefusedName);
				}
				Err(error) => return failed(&error),
			}
		}
	};
	let Some((stable, version)) = file_ids(&uploaded) else {
		return Outcome::Park(Failure::Internal);
	};
	match inner.in_drive_tree(&uploaded.parent).await {
		Ok(true) => {}
		// The drive trashed its directory while it uploaded: the file stays ordinary, uploaded
		// again once the remote side created the directory anew.
		Ok(false) => return Outcome::Wait(TREE_AGAIN),
		Err(outcome) => return outcome,
	}
	if current != *path {
		let saved = lock(&inner.state)
			.lineage
			.get(&current)
			.filter(|seen| !seen.unconverted && seen.stable != stable)
			.cloned();
		if let Some(seen) = saved
			&& still_claims(inner, &seen).await
		{
			// A save's temporary file renamed while it uploaded into the place of the file it saves,
			// over that file or where it was renamed away from: the job there takes its bytes as
			// that file's new version, and the upload, which never got its name, goes.
			veto::trash_transient(inner, stable);
			inner.push(&current, Delay::Now);
			return Outcome::Done;
		}
		// Renamed while it uploaded: the job at its new path moves the remote file after it, and
		// a delete there before that job trashes it.
		lock(&inner.state)
			.lineage
			.remember_upload(&current, stable, version, Instant::now());
		if matches!(inner.inspect(&current).await, Ok(LocalEntry::Missing)) {
			forget_upload(inner, &current);
		} else {
			inner.push(&current, Delay::Now);
		}
		return Outcome::Done;
	}
	if matches!(inner.inspect(path).await, Ok(LocalEntry::Missing)) {
		// Deleted while it uploaded: the remote file goes to the trash, where it can be restored.
		veto::trash_later(inner, path, PlaceholderIdentity::File { stable, version });
		return Outcome::Done;
	}
	let named = match give_name(inner, &uploaded, name.as_ref(), &parent_id).await {
		Ok(named) => named,
		Err(Naming::Taken(taken)) => {
			return keep_upload_aside(inner, path, &uploaded, &parent_id, &taken).await;
		}
		Err(Naming::Failed(outcome)) => return outcome,
	};
	finish_upload(inner, path, id, &named, was, Sent::NewFile, None).await
}

/// The upload of the local file `id` in `listing` that never got its name, when it holds the bytes
/// hashed `hash`, `size` long: never another file's, though it held the same bytes.
fn unnamed_upload<'a>(
	listing: &'a QueryChildrenResponse,
	id: LocalFileId,
	hash: &[u8; 32],
	size: u64,
) -> Option<&'a FfiFile> {
	let name = upload_name(id.get());
	listing.objects.iter().find_map(|object| match object {
		FfiNonRootObject::File(file)
			if object_name(object) == Some(name.as_str())
				&& u64::try_from(file.size).ok() == Some(size)
				&& same_hash(file, hash) =>
		{
			Some(file)
		}
		_ => None,
	})
}

/// Why a new file did not get its name.
enum Naming {
	/// Another item holds it: both are kept. Boxed, as the other variant is small.
	Taken(Box<FfiNonRootObject>),
	/// The rename failed otherwise; the job ends so.
	Failed(Outcome),
}

/// Renames the new file `uploaded` in the directory `parent` to `name`, which the server refuses
/// while another file holds it: then the item holding it, from a listing taken after the refusal.
async fn give_name<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	uploaded: &FfiFile,
	name: &str,
	parent: &FfiId,
) -> Result<FfiFile, Naming> {
	let Some((stable, _)) = file_ids(uploaded) else {
		return Err(Naming::Failed(Outcome::Park(Failure::Internal)));
	};
	let refused = match inner.drive.rename(stable_id(stable), name.to_owned()).await {
		Ok(Some(FfiObject::File(named))) => return Ok(named),
		Ok(None) => return Ok(uploaded.clone()),
		Ok(Some(_)) => return Err(Naming::Failed(Outcome::Park(Failure::Internal))),
		Err(error) => error,
	};
	match inner.drive.list_children(parent.clone()).await {
		Ok(Some(listing)) => match find_by_name(&listing, name) {
			Some(taken) if !same_uuid(object_uuid(taken), &uploaded.stable_uuid) => {
				Err(Naming::Taken(Box::new(taken.clone())))
			}
			_ => Err(Naming::Failed(failed(&refused))),
		},
		Ok(None) => Err(Naming::Failed(Outcome::Failed(Failure::NotFound))),
		Err(error) => Err(Naming::Failed(failed(&error))),
	}
}

/// Keeps both a new local file at `path`, uploaded as `uploaded`, and the remote item `taken`,
/// which took the name while it uploaded: the upload is renamed to a free `.old` name, the local
/// file moves aside to it, where its next look adopts the upload, and a placeholder of `taken`
/// takes the name. A rename refused again leaves the upload under its own name, for the next look
/// to find.
async fn keep_upload_aside<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	uploaded: &FfiFile,
	parent: &FfiId,
	taken: &FfiNonRootObject,
) -> Outcome {
	let listing = match inner.drive.list_children(parent.clone()).await {
		Ok(Some(listing)) => listing,
		Ok(None) => return Outcome::Failed(Failure::NotFound),
		Err(error) => return failed(&error),
	};
	let Some(aside) = choose_aside(inner, path, &listing.objects).await else {
		warn!(item = %path_tag(path), "no free name to keep a conflicting new file aside");
		return Outcome::Park(Failure::Internal);
	};
	let Some(aside_name) = aside.name() else {
		return Outcome::Park(Failure::Internal);
	};
	match give_name(inner, uploaded, aside_name.as_str(), parent).await {
		Ok(_) => {}
		// Taken too meanwhile: the upload keeps its own name, which the next look finds.
		Err(Naming::Taken(_)) => return Outcome::Again(Duration::ZERO),
		Err(Naming::Failed(outcome)) => return outcome,
	}
	if let Err(error) = set_aside(inner, path, aside, ConflictKind::CreateVsExisting).await {
		return local_failed(error);
	}
	place_remote(inner, path, taken).await;
	Outcome::Done
}

/// An ordinary file where a placeholder of the file `seen` was: an editor saved over it (a rename
/// over it, a rename of the old one aside, or a delete and a create). It becomes the new version of
/// that file rather than a new one, the remote file moved back to its name if the save renamed it
/// away, and the old copy left behind detached from it. Edited while the remote got a newer version,
/// both are kept.
async fn replace<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	id: LocalFileId,
	stamp: FileStamp,
	seen: Seen,
) -> Outcome {
	let (parent_id, parent_uuid) = match inner.remote_parent(path).await {
		Ok(parent) => parent,
		Err(outcome) => return outcome,
	};
	let Some(name) = server_name(path) else {
		return Outcome::Skipped;
	};
	if let Err(outcome) = edits_may_upload(inner, path, seen.stable) {
		return outcome;
	}
	if let Err(outcome) = inner.settle(path, id, stamp).await {
		return outcome;
	}
	let _upload = inner.uploads.acquire().await;
	let hash = match inner.hash_settled(path, id).await {
		Ok(hash) => hash,
		Err(outcome) => return outcome,
	};
	let forget_and_retry = |inner: &Inner<D, L>| {
		lock(&inner.state).lineage.forget(path);
		Outcome::Again(Duration::ZERO)
	};
	let head = match inner.drive.refresh_item(stable_id(seen.stable)).await {
		Ok(Some(FfiObject::File(head))) if head.original_parent.is_none() => head,
		Ok(_) => return forget_and_retry(inner),
		Err(error) => return failed(&error),
	};
	match inner.in_drive_tree(&head.parent).await {
		Ok(true) => {}
		Ok(false) => return forget_and_retry(inner),
		Err(outcome) => return outcome,
	}
	let at_place = same_uuid(&head.parent, &parent_uuid)
		&& head.meta.as_ref().map(|meta| collision_key(&meta.name))
			== Some(collision_key(name.as_ref()));
	// Read back from a previous run: the remote file must still be where this path, or the backup
	// the path's placeholder was renamed to, had it. Anywhere else, another device moved it since,
	// and this file is a new one.
	if seen.restored && !at_place && !at_backup(inner, path, &head, &seen).await {
		return forget_and_retry(inner);
	}
	if !at_place {
		let listing = match inner.drive.list_children(parent_id.clone()).await {
			Ok(Some(listing)) => listing,
			Ok(None) => return Outcome::Failed(Failure::NotFound),
			Err(error) => return failed(&error),
		};
		if find_by_name(&listing, name.as_ref())
			.is_some_and(|taken| !same_uuid(object_uuid(taken), &head.stable_uuid))
		{
			// Another item holds the name: this file is a new item after all.
			return forget_and_retry(inner);
		}
		let here = match veto::parent_id(inner, &head.parent).await {
			Ok(here) => here,
			Err(failure) => return Outcome::Failed(failure),
		};
		let from = Place {
			dir: &here,
			name: head.meta.as_ref().map(|meta| meta.name.as_str()),
		};
		let to = (!same_uuid(&head.parent, &parent_uuid)).then_some(&parent_id);
		if let Some(moved_to) = &seen.moved_to
			&& let Err(outcome) = hydrate_left_behind(inner, moved_to, &head).await
		{
			return outcome;
		}
		match relocate_file(inner.drive.as_ref(), seen.stable, from, to, name.as_ref()).await {
			Ok(Relocated::Done) => {}
			// Taken meanwhile, and the remote file back where it was: a new item after all.
			Ok(Relocated::Taken) => return forget_and_retry(inner),
			Err(error) => return failed(&error),
		}
		if let Some(moved_to) = &seen.moved_to {
			detach_left_behind(inner, moved_to, seen.stable).await;
		}
	}
	let Some((stable, head_version)) = file_ids(&head) else {
		return Outcome::Park(Failure::Internal);
	};
	if same_hash(&head, &hash) {
		let identity = PlaceholderIdentity::File {
			stable,
			version: head_version,
		};
		let file = match inner.reopen_hashed(path, id, hash).await {
			Ok(file) => file,
			Err(outcome) => return outcome,
		};
		match inner.tag(path, id, file, Was::Plain, identity, true).await {
			Ok(Tagged::Here) => {}
			// Renamed away: the job where it went takes it in there.
			Ok(Tagged::Overtaken { .. }) => return Outcome::Done,
			Err(outcome) => return outcome,
		}
		// This file is the item's new version: a delete of its old copy must not trash it.
		veto::forget_deletes_of(inner, stable);
		inner.remember(path, stable, head_version);
		inner.report(LocalReport::Adopted {
			path: path.clone(),
			identity,
		});
		return inner.marked(path, id).await;
	}
	if head_version != seen.version {
		let listing = match inner.drive.list_children(parent_id).await {
			Ok(Some(listing)) => listing,
			Ok(None) => return Outcome::Failed(Failure::NotFound),
			Err(error) => return failed(&error),
		};
		// No mark of an earlier edit is given up here: this file is a newer save, and an unsent
		// edit a previous run left was kept by the take-over before this job could get this far.
		let taken = FfiNonRootObject::File(head);
		return keep_new_aside(inner, path, &listing, &taken, ConflictKind::EditVsEdit).await;
	}
	lock(&inner.state).uploading.insert(id, path.clone());
	let result = upload_version(inner, path, stable, hash).await;
	let current = lock(&inner.state)
		.uploading
		.remove(&id)
		.unwrap_or_else(|| path.clone());
	let uploaded = match result {
		Ok(uploaded) => uploaded,
		Err(outcome) => return outcome,
	};
	inner.sent(path, hash);
	if let Some((_, version)) = file_ids(&uploaded) {
		// Deleted before it is taken over, the save goes with the file.
		veto::retarget_deletes_of(inner, stable, version);
	}
	match inner.in_drive_tree(&uploaded.parent).await {
		Ok(true) => {}
		Ok(false) => return forget_and_retry(inner),
		Err(outcome) => return outcome,
	}
	if current != *path {
		// Renamed while it uploaded: the job where it went takes it over, and its old path being
		// empty is no delete of the file.
		veto::forget_deletes_of(inner, stable);
		if let Some((stable, version)) = file_ids(&uploaded) {
			inner.remember(&current, stable, version);
		}
		inner.push(&current, Delay::Now);
		return Outcome::Done;
	}
	finish_upload(inner, path, id, &uploaded, Was::Plain, Sent::Version, None).await
}

/// Whether the remote file `head` is where a backup of `path`'s placeholder stands for, its
/// directory and its name: the path `seen` says the placeholder was renamed to, or the target of a
/// rename of it from `path` whose record a previous run left, which is put back meanwhile. Or it is
/// under its transit name, where a move of it a previous run stopped in the middle of left it.
async fn at_backup<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	head: &FfiFile,
	seen: &Seen,
) -> bool {
	if head.meta.as_ref().map(|meta| meta.name.as_str()) == Some(transit_name(seen.stable).as_str())
	{
		return true;
	}
	let mut backups: Vec<RootPath> = seen.moved_to.iter().cloned().collect();
	backups.extend(
		lock(&inner.state)
			.rename_records
			.iter()
			.filter(|record| {
				record.from.case_key() == path.case_key()
					&& matches!(record.identity, PlaceholderIdentity::File { stable, .. } if stable == seen.stable)
			})
			.map(|record| record.to.clone()),
	);
	for backup in backups {
		let Some(name) = backup.name() else {
			continue;
		};
		let Ok((_, parent_uuid)) = inner.remote_parent(&backup).await else {
			continue;
		};
		if same_uuid(&head.parent, &parent_uuid)
			&& head.meta.as_ref().map(|meta| collision_key(&meta.name))
				== Some(collision_key(name.as_str()))
		{
			return true;
		}
	}
	false
}

/// Brings every byte of the placeholder an editor renamed aside to `path` while saving onto disk,
/// while it still names `head`'s file at the version `head` is: once detached, nothing could fetch
/// them any more, and once the save is uploaded that version is no longer the one a hydration
/// reads. Done before the remote file moves back and the save's upload, so a failure leaves both
/// as they were for the next look, which tries again. A placeholder of another version than
/// `head`'s cannot be hydrated, and is left to the remote side, attached.
async fn hydrate_left_behind<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	head: &FfiFile,
) -> Result<(), Outcome> {
	let Some(head) = file_ids(head) else {
		return Ok(());
	};
	let id = match inner.inspect(path).await {
		Ok(LocalEntry::Placeholder {
			kind: LocalKind::File(stamp),
			identity: Ok(PlaceholderIdentity::File { stable, version }),
			id,
			state,
		}) if (stable, version) == head && state.on_disk() < stamp.size => id,
		Ok(_) | Err(LocalError::NotFound) => return Ok(()),
		Err(error) => return Err(local_failed(error)),
	};
	let target = path.clone();
	match inner.fs(move |local| local.hydrate(&target, id)).await {
		Ok(()) => {}
		// Gone, or another item there now: nothing left behind to keep.
		Err(LocalError::NotFound) => return Ok(()),
		Err(error) => {
			warn!(item = %path_tag(path), %error, "downloading a saved file's old copy failed");
			return Err(match error {
				LocalError::Busy => Outcome::Busy,
				_ => Outcome::Failed(Failure::Transient),
			});
		}
	}
	match inner.inspect(path).await {
		Ok(LocalEntry::Placeholder {
			kind: LocalKind::File(stamp),
			id: now,
			state,
			..
		}) if now == id && state.on_disk() < stamp.size => {
			warn!(item = %path_tag(path), "downloading a saved file's old copy left it partly on disk");
			Err(Outcome::Failed(Failure::Transient))
		}
		_ => Ok(()),
	}
}

/// The placeholder an editor renamed aside while saving still names the file `stable`, whose new
/// version now sits at the original name: it is detached, so deleting it (as the editor does next)
/// touches nothing remote, and kept, it is uploaded as a file of its own. Only once all its bytes
/// are on disk ([`hydrate_left_behind`]): a detached placeholder's missing bytes could never be
/// fetched, so one that is not stays attached, for the remote side to bring up to date.
async fn detach_left_behind<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	stable: Uuid,
) {
	let Ok(LocalEntry::Placeholder {
		kind: LocalKind::File(stamp),
		identity: Ok(PlaceholderIdentity::File { stable: held, .. }),
		id,
		state,
	}) = inner.inspect(path).await
	else {
		return;
	};
	if held != stable {
		return;
	}
	if state.on_disk() < stamp.size {
		warn!(item = %path_tag(path), "a saved file's old copy is not all on disk; it stays attached");
		return;
	}
	let target = path.clone();
	let detached = PlaceholderIdentity::Detached {
		nonce: Uuid::new_v4(),
	};
	match inner
		.fs(move |local| local.retag_unsynced(&target, id, detached))
		.await
	{
		Ok(()) => {
			lock(&inner.state).lineage.forget(path);
			inner.push(path, Delay::Debounce);
		}
		Err(error) => debug!(%path, %error, "detaching a saved file's old copy failed"),
	}
}

/// An ordinary directory: created on the drive and converted, or, when the drive holds a directory
/// of its name, converted into that one, which the platform then populates with what only the drive
/// holds. A remote file of its name keeps both. Its children are looked at next, after it.
pub(super) async fn take_in_dir<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	id: LocalFileId,
	created: i64,
) -> Outcome {
	let (parent_id, parent_uuid) = match inner.remote_parent(path).await {
		Ok(parent) => parent,
		Err(outcome) => return outcome,
	};
	let Some(name) = server_name(path).map(String::from) else {
		return Outcome::Skipped;
	};
	let listing = match inner.drive.list_children(parent_id.clone()).await {
		Ok(Some(listing)) => listing,
		Ok(None) => return Outcome::Failed(Failure::NotFound),
		Err(error) => return failed(&error),
	};
	let (uuid, populated, adopted) = match find_by_name(&listing, &name) {
		Some(FfiNonRootObject::Dir(dir)) => match Uuid::parse_str(&dir.uuid) {
			Ok(uuid) => (uuid, false, true),
			Err(_) => return Outcome::Park(Failure::Internal),
		},
		Some(taken) => {
			return keep_new_aside(inner, path, &listing, taken, ConflictKind::CreateVsExisting)
				.await;
		}
		None => {
			let dir = match inner
				.drive
				.create_dir(parent_id, name.clone(), ticks_to_unix_millis(created))
				.await
			{
				Ok(dir) => dir,
				Err(CacheError::InvalidName(error)) => {
					debug!(%path, %error, "the drive refused a name");
					return inner.report_skip(path, LocalSkip::DriveRefusedName);
				}
				Err(error) => return failed(&error),
			};
			match Uuid::parse_str(&dir.uuid) {
				Ok(uuid) => (uuid, true, false),
				Err(_) => return Outcome::Park(Failure::Internal),
			}
		}
	};
	match inner.in_drive_tree(&parent_uuid).await {
		Ok(true) => {}
		// The drive trashed the directory it is in meanwhile: taken in again once the remote side
		// created that one anew.
		Ok(false) => return Outcome::Wait(TREE_AGAIN),
		Err(outcome) => return outcome,
	}
	let identity = PlaceholderIdentity::Dir { uuid };
	match inner.tag_dir(path, id, identity, populated).await {
		Ok(Tagged::Here) => {
			if adopted {
				// A directory of the user's now stands for it.
				leftover::claimed(inner, uuid);
			}
		}
		// Renamed away once converted, and taken back: the job where it went takes it in afresh.
		// For a moment it stood for the directory this job created, and a job on an item in it may
		// have read it so and be sending an upload into that directory, which no listing shows
		// until it lands: the directory stays for now, and goes to the trash later, once nothing
		// can be sending into it and it is still empty ([`leftover`]).
		Ok(Tagged::Overtaken { to }) => {
			if !adopted {
				leftover::remember(
					inner,
					uuid,
					Leftover {
						from: path.clone(),
						went: to,
						parent: parent_uuid,
						name,
					},
				);
			}
			return Outcome::Done;
		}
		// Renamed away or deleted before its convert, or another directory took the path: never a
		// placeholder of the directory this job created, which goes again while nothing can be in
		// it. The job where the directory went, if it went anywhere, takes it in afresh.
		Err(LocalError::NotFound) => {
			if !adopted {
				drop_created_dir(inner, path, &parent_uuid, &name, uuid).await;
			}
			return Outcome::Done;
		}
		Err(error) => return local_failed(error),
	}
	inner.report(if adopted {
		LocalReport::Adopted {
			path: path.clone(),
			identity,
		}
	} else {
		LocalReport::DirCreated {
			path: path.clone(),
			identity,
		}
	});
	queue_children(inner, path).await;
	Outcome::Done
}

/// Trashes the directory `uuid` the job at `path` just created on the drive as `name` in the
/// directory `parent` (uuid text), for a local directory that left the path before it was ever
/// converted: nothing local stands for it or ever did, so no job on an item in the local directory
/// could have taken it for that directory, and the job where the local directory went
/// creates its own. Left, it would come back as an empty directory at the old name. Trashed only
/// while nothing can be in it or stand for it: no placeholder of it indexed, no rename of one
/// being settled, still where and as it was created, and empty as the server lists it. Otherwise
/// it stays on the drive, as anything another device may have put in it must. The trash is where
/// it goes, never a permanent delete.
async fn drop_created_dir<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	parent: &str,
	name: &str,
	uuid: Uuid,
) {
	let claimed = {
		let state = lock(&inner.state);
		state.index.path_of(uuid).is_some()
			|| state
				.rename_records
				.iter()
				.any(|record| record.concerns(uuid))
	};
	if claimed {
		debug!(%path, %uuid, "a directory created for a local one that left is claimed; it stays");
		return;
	}
	match leftover::as_created(inner, uuid, parent, name).await {
		AsCreated::Empty => {}
		AsCreated::Gone => return,
		AsCreated::Moved => {
			warn!(item = %path_tag(path), %uuid, "a directory created for a local one that left moved on the drive; it stays");
			return;
		}
		AsCreated::Holds | AsCreated::Unlisted => {
			warn!(item = %path_tag(path), %uuid, "a directory created for a local one that left holds items; it stays");
			return;
		}
		AsCreated::Unread(error) => {
			warn!(item = %path_tag(path), %uuid, error = %log_safe::error(&error), "reading a directory created for a local one that left failed; it stays");
			return;
		}
	}
	match inner.drive.trash(stable_id(uuid)).await {
		Ok(_) => debug!(%path, %uuid, "trashed a directory created for a local one that left"),
		Err(error) => {
			warn!(item = %path_tag(path), %uuid, error = %log_safe::error(&error), "trashing a directory created for a local one that left failed; it stays");
		}
	}
}

/// Queues every child of the directory `path` to be looked at: a tree moved in or created at once
/// may report its top directory alone.
async fn queue_children<D: Drive, L: LocalFs>(inner: &Arc<Inner<D, L>>, path: &RootPath) {
	let dir = path.clone();
	let children = match inner.fs(move |local| local.list_dir(&dir)).await {
		Ok(children) => children,
		Err(error) => {
			debug!(%path, %error, "listing a new directory failed");
			return;
		}
	};
	for child in children {
		match child {
			Ok(child) => inner.push(&child, Delay::Now),
			Err(_) => {
				inner.report(LocalReport::Skipped {
					path: path.clone(),
					reason: LocalSkip::Unrepresentable,
				});
			}
		}
	}
}

/// A free `.old` name beside `path`: free locally, free among the remote `listed` items in the
/// server's case folding, and a name both Windows and the server accept.
pub(super) async fn choose_aside<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	listed: &[FfiNonRootObject],
) -> Option<RootPath> {
	let name = path.name()?.as_str().to_owned();
	let taken: HashSet<String> = listed
		.iter()
		.filter_map(object_name)
		.map(collision_key)
		.collect();
	for candidate in aside_names(&name).take(ASIDE_ATTEMPTS) {
		let Ok(candidate_name) = PlaceholderName::parse(&candidate) else {
			continue;
		};
		if ValidatedName::try_from(candidate.as_str()).is_err()
			|| taken.contains(&collision_key(&candidate))
		{
			continue;
		}
		let aside = path.with_name(candidate_name)?;
		if matches!(inner.inspect(&aside).await, Ok(LocalEntry::Missing)) {
			return Some(aside);
		}
	}
	None
}

/// Keeps both a new local item at `path` and the remote item `taken` that holds its name: the local
/// item moves aside to a free `.old` name and is taken in from there, and a placeholder of the
/// remote item takes the name.
async fn keep_new_aside<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	listing: &QueryChildrenResponse,
	taken: &FfiNonRootObject,
	kind: ConflictKind,
) -> Outcome {
	let Some(aside) = choose_aside(inner, path, &listing.objects).await else {
		warn!(item = %path_tag(path), "no free name to keep a conflicting local item aside");
		return Outcome::Park(Failure::Internal);
	};
	if let Err(error) = set_aside(inner, path, aside, kind).await {
		return local_failed(error);
	}
	place_remote(inner, path, taken).await;
	Outcome::Done
}

/// Moves the local item at `path` aside to the free name `aside`, where it is taken in as a new
/// item, and reports the conflict `kind` that kept both.
pub(super) async fn set_aside<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	aside: RootPath,
	kind: ConflictKind,
) -> Result<(), LocalError> {
	let (from, to) = (path.clone(), aside.clone());
	inner
		.fs(move |local| local.rename_no_replace(&from, &to))
		.await?;
	lock(&inner.state).lineage.forget(path);
	inner.push(&aside, Delay::Now);
	inner.report(LocalReport::Conflict {
		path: path.clone(),
		kept_as: aside,
		kind,
	});
	Ok(())
}

/// Creates a placeholder of the remote item `remote` at `path`, freed by a conflict, when Windows
/// can hold its name; otherwise the remote item is left for the remote side to place. One the
/// sync root holds elsewhere is moved here by its remote job instead, so no item gets two
/// placeholders.
async fn place_remote<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	remote: &FfiNonRootObject,
) {
	let Some(parent) = path.parent() else {
		return;
	};
	let spec = match spec_for(remote) {
		Ok(spec) => spec,
		Err(reason) => {
			debug!(%path, ?reason, "the remote side of a conflict gets no placeholder");
			return;
		}
	};
	let Some(uuid) = remote::populated_uuid(spec.identity()) else {
		return;
	};
	let elsewhere = lock(&inner.state).index.path_of(uuid).cloned();
	if let Some(elsewhere) = elsewhere
		&& elsewhere.case_key() != path.case_key()
	{
		inner.push_kind(&elsewhere, Delay::Now, Kind::Remote);
		return;
	}
	let at = path.clone();
	match inner
		.fs(move |local| local.create_placeholder(&parent, &spec))
		.await
	{
		Ok(()) => lock(&inner.state).index.insert_if_populated(&at, uuid),
		Err(error) => debug!(%path, %error, "placing the remote side of a conflict failed"),
	}
}

/// Keeps both a local edit of the file placeholder `id` at `path` and `head`, the newer remote
/// version it was not made from: the local file is detached first, so nothing it does can reach the
/// remote file any more, then moved aside to a free `.old` name and uploaded from there as a new
/// file, and a placeholder of `head` takes the name.
async fn keep_edit_aside<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	id: LocalFileId,
	head: FfiFile,
) -> Outcome {
	let Some((stable, _)) = file_ids(&head) else {
		return Outcome::Park(Failure::Internal);
	};
	if let Err(outcome) = release_edit(inner, stable).await {
		return outcome;
	}
	let file = match inner.reopen(path, id).await {
		Ok(file) => file,
		Err(outcome) => return outcome,
	};
	let detached = PlaceholderIdentity::Detached {
		nonce: Uuid::new_v4(),
	};
	if let Err(outcome) = inner
		.tag(path, id, file, Was::Placeholder, detached, false)
		.await
	{
		return outcome;
	}
	lock(&inner.state).lineage.forget(path);
	let (parent_id, _) = match inner.remote_parent(path).await {
		Ok(parent) => parent,
		Err(outcome) => return outcome,
	};
	// A failure from here leaves a detached file at the name, which its next look keeps aside the
	// same way.
	let listing = match inner.drive.list_children(parent_id).await {
		Ok(Some(listing)) => listing,
		Ok(None) => return Outcome::Failed(Failure::NotFound),
		Err(error) => return failed(&error),
	};
	keep_new_aside(
		inner,
		path,
		&listing,
		&FfiNonRootObject::File(head),
		ConflictKind::EditVsEdit,
	)
	.await
}

/// Gives up the cache's copy of an edit of the file `stable` that is kept aside as a new file
/// instead, should an upload dropped before it returned have left one marked: the start-up
/// take-over would otherwise copy it out once more. Done before the edit moves aside, so a failure
/// leaves both where they were for the next look.
async fn release_edit<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	stable: Uuid,
) -> Result<(), Outcome> {
	inner
		.drive
		.drop_pending_upload(stable_id(stable))
		.await
		.map_err(|error| failed(&error))
}

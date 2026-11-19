//! The hydration job on one path: completes a clean file placeholder its readers left partly on
//! disk, and checks a whole one against the hash its version declares, once its reads went quiet.
//!
//! Windows asks for a file a window at a time, the next before the last is answered, so a file read
//! across several requests is never checked as it arrives: only a single-request whole-file read is
//! (the dispatcher records it verified). This job checks every other file once it is all on disk,
//! through a handle that holds no user's writer up, and comparing the file before and after the
//! hash, so a user's edit meanwhile is never taken for damage. A reader of a file read across
//! several requests gets damaged bytes before the job catches them: accepted, as no hash covers a
//! part of a file.
//!
//! | The clean file placeholder | Job |
//! |---|---|
//! | its version found damaged | dehydrated, reported damaged; its reads stay refused |
//! | read in the last quiet window, or a read in flight | looked at again once quiet |
//! | partly on disk, most of it there | completed (`CfHydratePlaceholder`), then checked |
//! | partly on disk, little of it there | left: a reader that seeks keeps only what it read |
//! | pinned, not all on disk | completed, whatever is on disk, then checked |
//! | all on disk, not yet checked | hashed and compared with its declared hash |
//! | a pinned directory | the job queued on each of its items |
//! | a pinned directory the platform has not populated | populated from the server, then as above |
//!
//! A pin is the nearest explicit one on the item or a directory above it (`attrib +P`, Explorer's
//! "Always keep on this device"): measured, the platform hydrates nothing for it, and a pin set on
//! a directory alone reaches no item in it, while one set recursively or inherited by a new item
//! marks each item; the nearest explicit pin covers both. A child's own pin wins, as on iOS.
//!
//! Nor does the platform populate a pinned directory nothing has listed (measured from Explorer:
//! its items waited for a listing, a subdirectory never listed for good), so the job populates it
//! itself, through the remote side's reconcile ([`populate_pinned`]), and then queues the job on
//! what it placed: a pinned tree ends on disk a directory at a time, each directory's items queued
//! as the queue has room. An unpin meanwhile stops it at the next directory, whose job then finds
//! no pin.
//!
//! A placeholder that is not clean is the local side's, which runs first on the same path; the
//! job leaves it.

use std::{sync::Arc, time::Duration};

use filen_mobile_native_cache::ffi::FfiObject;
use filen_sdk_rs::consts::CHUNK_SIZE_U64;
use filen_types::fs::Uuid;
use tokio::sync::OwnedSemaphorePermit;
use tracing::{debug, info, warn};

use super::{
	FileVersion, Inner,
	jobs::{failed, local_failed},
	lock,
	queue::{Delay, Kind, Outcome},
	remote::populate_pinned,
	report::LocalReport,
};
use crate::{
	drive::Drive,
	failure::Failure,
	identity::{PlaceholderIdentity, stable_id},
	log_safe::path_tag,
	os::{
		CheckedHash, IfPinned, LocalEntry, LocalError, LocalFileId, LocalFs, LocalKind, PinAttr,
		PlaceholderState,
	},
	root_path::RootPath,
};

/// How many completions in a row may leave a file partly on disk before it waits for its next
/// read or the next walk, and the waits between them.
const COMPLETION_BACKOFF: [Duration; 2] = [Duration::from_secs(2), Duration::from_secs(8)];

/// How a run of the job ended, as the log reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Settled {
	/// A partly hydrated file was completed.
	Completed,
	/// A whole file matched its declared hash.
	Verified,
	/// The file declares no hash to check against.
	Unverifiable,
	/// The file's content was damaged and was dehydrated.
	Damaged,
	/// The placeholder holds an older version than the drive's: the remote job updates it.
	Stale,
	/// The user changed the file while it was hashed: the local side takes it.
	Edited,
	/// The file's space was freed, as its pin asked.
	Freed,
}

impl Settled {
	fn as_str(self) -> &'static str {
		match self {
			Self::Completed => "completed",
			Self::Verified => "verified",
			Self::Unverifiable => "unverifiable",
			Self::Damaged => "damaged",
			Self::Stale => "stale",
			Self::Edited => "edited",
			Self::Freed => "freed",
		}
	}
}

/// A clean file placeholder the job works on.
#[derive(Debug, Clone, Copy)]
struct CleanFile {
	id: LocalFileId,
	size: u64,
	state: PlaceholderState,
	version: FileVersion,
}

impl CleanFile {
	/// The clean file placeholder `entry` is; `None` for anything else.
	fn from_entry(entry: LocalEntry) -> Option<Self> {
		match entry {
			LocalEntry::Placeholder {
				kind: LocalKind::File(stamp),
				id,
				identity: Ok(PlaceholderIdentity::File { stable, version }),
				state,
			} if is_clean(state) => Some(Self {
				id,
				size: stamp.size,
				state,
				version: FileVersion { stable, version },
			}),
			_ => None,
		}
	}

	fn identity(self) -> PlaceholderIdentity {
		PlaceholderIdentity::File {
			stable: self.version.stable,
			version: self.version.version,
		}
	}

	fn full(self) -> bool {
		self.state.on_disk() >= self.size
	}
}

/// Whether a placeholder in `state` holds nothing the drive lacks: in sync, with no byte written
/// since.
fn is_clean(state: PlaceholderState) -> bool {
	state.in_sync() && state.modified_bytes() == 0
}

/// Whether a file placeholder of `kind` in `state` holds some of its bytes on disk, not all.
pub(super) fn partly_on_disk(kind: LocalKind, state: PlaceholderState) -> bool {
	matches!(kind, LocalKind::File(stamp) if state.on_disk() > 0 && state.on_disk() < stamp.size)
}

/// Whether the job completes a file of `size` bytes holding `on_disk` of them: one most of whose
/// bytes a reader pulled. The race that leaves a whole read partly on disk loses the transfers in
/// flight as its reader closes, which the core sends a cache chunk (1 MiB) at a time, so such a
/// file misses about a chunk; completing a file at least nine tenths on disk costs at most a ninth
/// of what its reader pulled. A reader that seeks (a thumbnailer, a media player) leaves less, and
/// its file is left as it is: decision 9's progressive default.
fn completable(on_disk: u64, size: u64) -> bool {
	on_disk > 0 && size.saturating_sub(on_disk) <= CHUNK_SIZE_U64.max(size / 10)
}

/// Whether the file held the same bytes all through the hash `checked`: the same file of the same
/// size and write time, all on disk and clean before and after. A data write clears the in-sync
/// state, and one through a handle for appending only counts modified bytes, so a hash whose file
/// changed only otherwise (its pin, its attributes) still tells its content.
fn held_still(checked: &CheckedHash) -> bool {
	let (before, after) = (checked.before, checked.after);
	let whole_and_clean = |snapshot: crate::os::FileSnapshot| {
		is_clean(snapshot.state) && snapshot.state.on_disk() >= snapshot.stamp.size
	};
	before.id == after.id
		&& before.stamp.size == after.stamp.size
		&& before.stamp.modified == after.stamp.modified
		&& whole_and_clean(before)
		&& whole_and_clean(after)
}

fn log_settled(path: &RootPath, file: CleanFile, outcome: Settled) {
	info!(
		item = %path_tag(path),
		stable = %file.version.stable,
		version = %file.version.version,
		outcome = outcome.as_str(),
		"hydration settled"
	);
}

/// The pin `own`, an item's own, or when it has none the one it inherits, `inherited`.
pub(super) fn effective(own: PinAttr, inherited: PinAttr) -> PinAttr {
	match own {
		PinAttr::Unspecified => inherited,
		PinAttr::Pinned | PinAttr::Unpinned => own,
	}
}

/// Whether the hydration job has work on a clean file placeholder of `kind` in `state` whose pin,
/// own or inherited, is `pin`: one a reader left partly on disk, a pinned one not all on disk, or
/// an unpinned one holding data.
pub(super) fn wanted(kind: LocalKind, state: PlaceholderState, pin: PinAttr) -> bool {
	let LocalKind::File(stamp) = kind else {
		return false;
	};
	match pin {
		PinAttr::Pinned => state.on_disk() < stamp.size,
		PinAttr::Unpinned => state.on_disk() > 0,
		PinAttr::Unspecified => partly_on_disk(kind, state),
	}
}

impl<D: Drive, L: LocalFs> Inner<D, L> {
	/// The pin the directories above `path` pass down to it: the nearest explicit one, read up to
	/// the root (about a quarter of a millisecond per directory, measured for the state read). A
	/// directory whose state cannot be read fails the read: a pin decides whether a file stays on
	/// disk, so an unread one is never taken for none.
	pub(super) async fn inherited_pin(&self, path: &RootPath) -> Result<PinAttr, LocalError> {
		let mut at = path.parent();
		while let Some(dir) = at {
			match self.inspect(&dir).await? {
				LocalEntry::Placeholder { state, .. } if state.pin() != PinAttr::Unspecified => {
					return Ok(state.pin());
				}
				LocalEntry::Placeholder { .. } => {}
				// Under an item that is no placeholder (or gone): nothing above it is synced.
				LocalEntry::Missing | LocalEntry::Plain { .. } | LocalEntry::Unsupported(_) => {
					return Ok(PinAttr::Unspecified);
				}
			}
			at = dir.parent();
		}
		Ok(PinAttr::Unspecified)
	}

	/// The pin of the item at `path`, its own or the one it inherits; unread as
	/// [`Inner::inherited_pin`] says.
	pub(super) async fn pin_of(&self, path: &RootPath) -> Result<PinAttr, LocalError> {
		let own = match self.inspect(path).await? {
			LocalEntry::Placeholder { state, .. } => state.pin(),
			LocalEntry::Missing | LocalEntry::Plain { .. } | LocalEntry::Unsupported(_) => {
				return Ok(PinAttr::Unspecified);
			}
		};
		Ok(effective(own, self.inherited_pin(path).await?))
	}
}

/// The job on `path`; `hydration` the hydration permit the scheduler took for it, if it did.
pub(super) async fn run<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	mut hydration: Option<OwnedSemaphorePermit>,
) -> Outcome {
	let entry = match inner.inspect(path).await {
		Ok(entry) => entry,
		Err(error) => return local_failed(error),
	};
	if let LocalEntry::Placeholder {
		kind: LocalKind::Dir { .. },
		identity: Ok(PlaceholderIdentity::Dir { .. } | PlaceholderIdentity::Root { .. }),
		id,
		state,
	} = entry
	{
		if !state.in_sync() {
			return Outcome::Done;
		}
		if state.pin() == PinAttr::Unpinned {
			return unpin_dir(inner, path, id).await;
		}
		let inherited = match inner.inherited_pin(path).await {
			Ok(inherited) => inherited,
			Err(error) => return local_failed(error),
		};
		return settle_dir(
			inner,
			path,
			effective(state.pin(), inherited),
			!state.partial(),
		)
		.await;
	}
	let Some(file) = CleanFile::from_entry(entry) else {
		return Outcome::Done;
	};
	let inherited = match inner.inherited_pin(path).await {
		Ok(inherited) => inherited,
		Err(error) => return local_failed(error),
	};
	match effective(file.state.pin(), inherited) {
		// An unpin of its own under a pinned directory stays: cleared, the file would take the
		// directory's pin.
		PinAttr::Unpinned => free(inner, path, file, inherited != PinAttr::Pinned).await,
		pin => settle_file(inner, path, file, pin, &mut hydration).await,
	}
}

/// Clears the pin `from` of the placeholder `id` at `path`'s own, unless the user changed it
/// meanwhile, logging a failure: a user's "Free up space" is a one-shot action, after which the
/// item behaves as any other. Says whether it cleared it.
pub(super) async fn clear_pin<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	id: LocalFileId,
	from: PinAttr,
) -> bool {
	let target = path.clone();
	match inner
		.fs(move |local| local.swap_pin(&target, id, from, PinAttr::Unspecified))
		.await
	{
		Ok(cleared) => cleared,
		Err(error) => {
			warn!(item = %path_tag(path), %error, "clearing an item's unpin failed");
			false
		}
	}
}

/// Frees the space of the clean file placeholder `file` at `path`, unpinned by the user or under
/// an unpinned directory: dehydrated, then its own unpin cleared when `clear`, and reported. A
/// file holding local changes is never dehydrated (the call refuses it), nor one a read is in
/// flight for, nor one the user pinned since the job looked, which the platform refuses and the
/// job's next run brings to its pin.
async fn free<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	file: CleanFile,
	clear: bool,
) -> Outcome {
	let own_unpin = clear && file.state.pin() == PinAttr::Unpinned;
	if file.state.on_disk() == 0 {
		if own_unpin {
			clear_pin(inner, path, file.id, PinAttr::Unpinned).await;
		}
		return Outcome::Done;
	}
	if inner.hints.reads_in_flight(file.version.stable) > 0 {
		return Outcome::Wait(inner.config.hydration_quiet);
	}
	match inner
		.dehydrate_clean(path, file.identity(), IfPinned::Refuse)
		.await
	{
		Ok(()) => {
			if own_unpin {
				clear_pin(inner, path, file.id, PinAttr::Unpinned).await;
			}
			log_settled(path, file, Settled::Freed);
			inner.report(LocalReport::Freed { path: path.clone() });
			Outcome::Done
		}
		Err(LocalError::Pinned) => Outcome::Again(Duration::ZERO),
		// Written since: its next look keeps it and reports it.
		Err(LocalError::Modified | LocalError::NotFound) => Outcome::Done,
		Err(error) => local_failed(error),
	}
}

/// Passes the clean directory placeholder `id` at `path`'s own unpin down to each of its items that
/// has no pin of its own (an item pinned of its own stays), then clears the directory's own and
/// queues the job on each item: each item frees itself, a directory passing it on in turn, as a
/// recursive `CfSetPinState` would, and the directory behaves as any other afterwards. A user who
/// pins the directory again meanwhile keeps the pin, and its items get theirs back. An item whose
/// pin cannot be changed fails the job before the directory's own unpin is cleared, and the items
/// passed it so far get theirs back first: until the job's next run passes it down whole, they
/// follow the directory, so a pin the user gives it meanwhile reaches them.
async fn unpin_dir<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	id: LocalFileId,
) -> Outcome {
	let dir = path.clone();
	let children = match inner.fs(move |local| local.list_dir(&dir)).await {
		Ok(children) => inner.representable(path, children),
		Err(error) => return local_failed(error),
	};
	let mut passed = Vec::new();
	for child in &children {
		match inner.inspect(child).await {
			Ok(LocalEntry::Placeholder { id, state, .. })
				if state.pin() == PinAttr::Unspecified =>
			{
				let target = child.clone();
				match inner
					.fs(move |local| {
						local.swap_pin(&target, id, PinAttr::Unspecified, PinAttr::Unpinned)
					})
					.await
				{
					Ok(true) => passed.push((child.clone(), id)),
					Ok(false) => {}
					Err(error) => {
						warn!(child = %path_tag(child), %error, "passing a directory's unpin to an item failed");
						for (child, id) in passed {
							clear_pin(inner, &child, id, PinAttr::Unpinned).await;
						}
						return local_failed(error);
					}
				}
			}
			Ok(_) => {}
			// Its job looks at it again; the unpin is not passed to it meanwhile.
			Err(error) => {
				warn!(child = %path_tag(child), %error, "reading an item of an unpinned directory failed")
			}
		}
	}
	if !clear_pin(inner, path, id, PinAttr::Unpinned).await {
		// Pinned again (or its unpin not cleared): the items follow the directory as they did.
		for (child, id) in passed {
			clear_pin(inner, &child, id, PinAttr::Unpinned).await;
		}
	}
	inner.queue_waiting_for_room(children, Kind::Hydration);
	Outcome::Done
}

/// Queues the job on each item of the clean directory placeholder at `path` whose pin, own or
/// inherited, is `pin`, so each brings itself to the pin. Its items are listed only when the pin
/// asks for work, and a pinned directory not `populated` yet is populated first: a failed
/// population (the drive offline) ends the job, which the queue tries again with its backoff.
async fn settle_dir<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	pin: PinAttr,
	populated: bool,
) -> Outcome {
	if pin != PinAttr::Pinned {
		return Outcome::Done;
	}
	if !populated {
		match populate_pinned(inner, path).await {
			Outcome::Done => {}
			outcome => return outcome,
		}
	}
	let dir = path.clone();
	match inner.fs(move |local| local.list_dir(&dir)).await {
		Ok(children) => {
			let children = inner.representable(path, children);
			inner.queue_waiting_for_room(children, Kind::Hydration);
			Outcome::Done
		}
		Err(error) => local_failed(error),
	}
}

/// The clean file placeholder at `path`; `None` when something else is there.
async fn clean_file<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
) -> Result<Option<CleanFile>, Outcome> {
	inner
		.inspect(path)
		.await
		.map(CleanFile::from_entry)
		.map_err(local_failed)
}

/// Takes a hydration permit into `held` unless it holds one: the scheduler hands one to a job
/// waiting for the hydration job alone, and a job running more kinds waits for one here.
async fn hydration_permit<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	held: &mut Option<OwnedSemaphorePermit>,
) {
	if held.is_none() {
		// The semaphore is never closed.
		*held = Arc::clone(&inner.hydrations).acquire_owned().await.ok();
	}
}

/// Brings the clean file placeholder `file` at `path`, whose pin is `pin`, to what its pin, its
/// reads and its declared hash ask.
async fn settle_file<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	file: CleanFile,
	pin: PinAttr,
	hydration: &mut Option<OwnedSemaphorePermit>,
) -> Outcome {
	if inner.hints.is_damaged(file.version) {
		return free_damaged(inner, path, file).await;
	}
	if file.full() && inner.hints.is_verified(file.version) {
		return Outcome::Done;
	}
	if let Some(wait) = quiet_left(inner, file) {
		return Outcome::Wait(wait);
	}
	let file = if file.full() {
		file
	} else {
		match complete(inner, path, file, pin, hydration).await {
			Ok(Some(file)) => file,
			Ok(None) => return Outcome::Done,
			Err(outcome) => return outcome,
		}
	};
	verify(inner, path, file, hydration).await
}

/// How long the file must stay quiet still: a read in flight, or one that ended less than the
/// quiet window ago. The provider's own reads never count here.
fn quiet_left<D: Drive, L: LocalFs>(inner: &Arc<Inner<D, L>>, file: CleanFile) -> Option<Duration> {
	let quiet = inner.config.hydration_quiet;
	if inner.hints.reads_in_flight(file.version.stable) > 0 {
		return Some(quiet);
	}
	let since = inner.hints.last_read(file.version)?.elapsed();
	(since < quiet).then(|| quiet - since)
}

/// Completes the partly hydrated `file` at `path` when it is pinned (`pin`) or [`completable`],
/// and returns it as it is then; `Ok(None)` when it is left as it is. A completion that leaves it
/// partly on disk is tried again after [`COMPLETION_BACKOFF`], then waits for the file's next read
/// or the next walk.
async fn complete<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	file: CleanFile,
	pin: PinAttr,
	hydration: &mut Option<OwnedSemaphorePermit>,
) -> Result<Option<CleanFile>, Outcome> {
	if pin != PinAttr::Pinned && !completable(file.state.on_disk(), file.size) {
		debug!(%path, on_disk = file.state.on_disk(), size = file.size, "left a file mostly unread partly on disk");
		return Ok(None);
	}
	let key = path.case_key();
	let stalls = lock(&inner.state)
		.hydration_stalls
		.get(&key)
		.copied()
		.unwrap_or(0);
	let Some(&backoff) = COMPLETION_BACKOFF.get(usize::try_from(stalls).unwrap_or(usize::MAX))
	else {
		return Ok(None);
	};
	hydration_permit(inner, hydration).await;
	let (target, id) = (path.clone(), file.id);
	match inner.fs(move |local| local.hydrate(&target, id)).await {
		Ok(()) => {}
		Err(LocalError::NotFound | LocalError::Modified | LocalError::Pinned) => return Ok(None),
		Err(error) => return Err(local_failed(error)),
	}
	let Some(now) = clean_file(inner, path).await? else {
		return Ok(None);
	};
	if now.identity() != file.identity() {
		return Ok(None);
	}
	if !now.full() {
		let stalls = {
			let mut state = lock(&inner.state);
			let stalls = state.hydration_stalls.entry(key).or_default();
			*stalls += 1;
			*stalls
		};
		warn!(item = %path_tag(path), stalls, "completing a partly hydrated file left it partly on disk");
		return Err(
			if usize::try_from(stalls).is_ok_and(|n| n < COMPLETION_BACKOFF.len()) {
				Outcome::Wait(backoff)
			} else {
				Outcome::Done
			},
		);
	}
	lock(&inner.state).hydration_stalls.remove(&key);
	log_settled(path, now, Settled::Completed);
	Ok(Some(now))
}

/// Checks the whole `file` at `path` against the hash the drive declares for its version.
async fn verify<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	file: CleanFile,
	hydration: &mut Option<OwnedSemaphorePermit>,
) -> Outcome {
	let head = match inner
		.drive
		.query_item(&stable_id(file.version.stable))
		.await
	{
		Ok(Some(FfiObject::File(head))) => head,
		Ok(_) => {
			// The cache holds no such file: the remote job finds out what became of it.
			inner.push_kind(path, Delay::Now, Kind::Remote);
			return Outcome::Done;
		}
		Err(error) => return failed(&error),
	};
	if Uuid::parse_str(&head.uuid).ok() != Some(file.version.version) {
		inner.push_kind(path, Delay::Now, Kind::Remote);
		log_settled(path, file, Settled::Stale);
		return Outcome::Done;
	}
	let declared = head
		.meta
		.as_ref()
		.and_then(|meta| meta.hash.as_deref())
		.and_then(|hash| match <[u8; 32]>::try_from(hash) {
			Ok(declared) => Some(declared),
			Err(_) => {
				warn!(item = %path_tag(path), stable = %file.version.stable, version = %file.version.version, length = hash.len(), "a file declares a hash that is not 32 bytes long; its content stays unchecked");
				None
			}
		});
	let Some(declared) = declared else {
		inner.hints.verified(file.version);
		log_settled(path, file, Settled::Unverifiable);
		return Outcome::Done;
	};
	hydration_permit(inner, hydration).await;
	let (target, id, stop) = (path.clone(), file.id, Arc::clone(&inner.stop));
	let checked = match inner
		.fs(move |local| local.hash_checked(&target, id, &stop))
		.await
	{
		Ok(checked) => checked,
		Err(error) => return local_failed(error),
	};
	if !held_still(&checked) {
		let after = checked.after;
		if is_clean(after.state) && after.state.on_disk() >= after.stamp.size {
			// Changed with no byte written (its times, by another process): looked at again, as
			// its next hash tells its content.
			return Outcome::Again(inner.config.hydration_quiet);
		}
		log_settled(path, file, Settled::Edited);
		return Outcome::Done;
	}
	if checked.hash == declared {
		inner.hints.verified(file.version);
		log_settled(path, file, Settled::Verified);
		return Outcome::Done;
	}
	warn!(item = %path_tag(path), stable = %file.version.stable, version = %file.version.version, "a file's bytes on disk do not match its declared hash");
	inner.hints.record_damaged(file.version);
	free_damaged(inner, path, file).await
}

/// Dehydrates the clean `file` at `path`, whose version's content is damaged, a pin of the user's
/// overridden and kept, and reports it. A dehydration another handle refuses fails, so the queue
/// tries it again with its backoff and parks it after a few, which reports the damaged bytes still
/// on disk ([`Failure::ContentDamaged`]); reads of the version stay refused.
async fn free_damaged<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	path: &RootPath,
	file: CleanFile,
) -> Outcome {
	if file.state.on_disk() == 0 {
		return Outcome::Done;
	}
	let identity = file.identity();
	match inner
		.dehydrate_clean(path, identity, IfPinned::Override)
		.await
	{
		Ok(()) => {
			log_settled(path, file, Settled::Damaged);
			inner.report(LocalReport::Damaged {
				path: path.clone(),
				identity,
			});
			Outcome::Done
		}
		// Written since: the user's bytes are the local side's, no longer the damaged version.
		Err(LocalError::Modified) => {
			inner.hints.forget_damaged(file.version);
			Outcome::Done
		}
		Err(LocalError::NotFound) => Outcome::Done,
		Err(
			error
			@ (LocalError::Busy | LocalError::Pinned | LocalError::Exists | LocalError::Os(_)),
		) => {
			debug!(%path, %error, "dehydrating damaged content was refused; trying again");
			Outcome::Failed(Failure::ContentDamaged)
		}
	}
}

#[cfg(test)]
mod tests;

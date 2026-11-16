//! The reconcile of one populated directory with the cache's listing of it: what the listing holds
//! and the directory does not gets a placeholder, a placeholder whose item moved, changed or went is
//! queued for its remote job, and a name an ordinary item holds is left to the local side, which
//! adopts the item when it equals the remote one and keeps both otherwise.

use std::{
	collections::{HashMap, HashSet},
	sync::Arc,
	time::Duration,
};

use filen_mobile_native_cache::ffi::FfiObject;
use filen_sdk_rs::fs::name::collision_key;
use filen_types::fs::Uuid;
use tracing::{debug, warn};

use crate::{
	drive::Drive,
	failure::Failure,
	identity::PlaceholderIdentity,
	local::{
		Inner,
		jobs::{claimed_by_save, failed, is_trashed, local_failed, parent_of, same_uuid},
		lock,
		queue::{Delay, Kind, Outcome},
		remote::populated_uuid,
		report::{LocalReport, RemoteApplied},
	},
	log_safe::path_tag,
	os::{LocalEntry, LocalError, LocalFs, PinAttr},
	placeholder::PlaceholderSpec,
	populate::{SkippedItem, plan_entries},
	root_path::{CaseKey, RootPath},
};

/// How soon a reconcile that left something for another job looks again.
const DEFERRED_AGAIN: Duration = Duration::from_secs(1);
/// How soon a reconcile that waited for a rename into its directory looks again.
const RENAME_AGAIN: Duration = Duration::from_millis(250);
/// How many reconciles of a directory in a row may leave something for another job before it waits
/// for the next walk: an item the reconcile cannot read or place would have it look every second.
const MAX_DEFERRALS: u32 = 60;

/// The reconcile of the directory at `dir`. It waits while a rename into the directory is allowed
/// and not yet done as the drive has it: reading the children opens each, and the rename's target
/// may be a file the renaming process made only to be replaced, as `ReplaceFileW` reserves its
/// backup name. Measured (Windows 11, in the VM): while any other handle on that file is open,
/// even one for reading attributes only, the rename over it fails after the provider allowed it,
/// and `ReplaceFileW` with `ERROR_UNABLE_TO_REMOVE_REPLACED`. The provider's own rename on the
/// drive changes the directory's listing, which has it reconciled while the rename waits.
pub(in crate::local) async fn children<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	dir: &RootPath,
) -> Outcome {
	if renaming_into(inner, dir) {
		return Outcome::Wait(RENAME_AGAIN);
	}
	let identity = match inner.inspect(dir).await {
		Ok(LocalEntry::Placeholder {
			identity:
				Ok(identity @ (PlaceholderIdentity::Root { .. } | PlaceholderIdentity::Dir { .. })),
			state,
			..
		}) if !state.partial() => identity,
		// Not populated, or not a directory placeholder any more: nothing of the drive's in it.
		Ok(_) => return Outcome::Done,
		Err(error) => return local_failed(error),
	};
	let (Some(id), Some(dir_uuid)) = (identity.cache_id(), populated_uuid(identity)) else {
		return Outcome::Done;
	};
	if let Some(other) = other_placeholder(inner, dir, dir_uuid).await {
		// Two placeholders of one directory: their remote jobs find which one is where the drive
		// has it and remove the other, then the one left is reconciled. Reconciled meanwhile, the
		// extra one would take items' placeholders from the other, and back.
		inner.push_kind(dir, Delay::Now, Kind::Remote);
		inner.push_kind(&other, Delay::Now, Kind::Remote);
		return Outcome::Done;
	}
	let pinned = match inner.pin_of(dir).await {
		Ok(pin) => pin == PinAttr::Pinned,
		Err(error) => return local_failed(error),
	};
	let listing = match inner.drive.cached_children(&id).await {
		Ok(Some(listing)) => listing,
		// The cache does not hold the directory: listed from the server instead.
		Ok(None) => match inner.drive.list_children(id).await {
			Ok(Some(listing)) => listing,
			// Gone from the drive: the directory's own remote job deals with it.
			Ok(None) => return Outcome::Done,
			Err(error) => return failed(&error),
		},
		Err(error) => return failed(&error),
	};
	let (specs, skipped) = plan_entries(&listing);
	if !skipped.is_empty() {
		inner.report_listed_skips(identity, skipped);
	}
	let epoch = lock(&inner.state).index.epoch();
	let local = match local_children(inner, dir).await {
		Ok(local) => local,
		Err(outcome) => return outcome,
	};
	let mut by_uuid: HashMap<Uuid, &RootPath> = HashMap::with_capacity(local.len());
	let mut doubled = HashSet::new();
	for (path, entry) in local.values() {
		if let LocalEntry::Placeholder {
			identity: Ok(identity),
			..
		} = entry && let Some(uuid) = populated_uuid(*identity)
			&& let Some(first) = by_uuid.insert(uuid, path)
		{
			// Two placeholders of one item here: their remote jobs find which one goes.
			doubled.insert(uuid);
			inner.push_kind(first, Delay::Now, Kind::Remote);
			inner.push_kind(path, Delay::Now, Kind::Remote);
		}
	}
	{
		// What the directory holds keeps the index current, unless an item was removed or moved
		// since it was read: then the next reconcile reads it afresh. An item with two
		// placeholders here keeps the index's place until one of them goes.
		let mut state = lock(&inner.state);
		if state.index.epoch() == epoch {
			for (uuid, path) in &by_uuid {
				if !doubled.contains(uuid) {
					state.index.insert(path, *uuid);
				}
			}
		}
	}
	let mut deferred = false;
	let mut listed = HashSet::with_capacity(specs.len());
	for spec in &specs {
		let Some(uuid) = populated_uuid(spec.identity()) else {
			continue;
		};
		listed.insert(uuid);
		if let Some(path) = by_uuid.get(&uuid) {
			let renamed = path.name() != Some(spec.name());
			let changed = matches!(
				local.get(&path.case_key()),
				Some((_, LocalEntry::Placeholder { identity: Ok(held), .. })) if *held != spec.identity()
			);
			if renamed || changed {
				inner.push_kind(path, Delay::Now, Kind::Remote);
			}
			continue;
		}
		let at = dir.join(spec.name().clone());
		match local.get(&at.case_key()) {
			Some((path, LocalEntry::Plain { .. })) => {
				inner.push(path, Delay::Now);
				continue;
			}
			Some((path, LocalEntry::Placeholder { .. })) => {
				// Another item's placeholder holds the name: its own job moves it on first.
				inner.push_kind(path, Delay::Now, Kind::Remote);
				deferred = true;
				continue;
			}
			Some((_, LocalEntry::Unsupported(_) | LocalEntry::Missing)) | None => {}
		}
		match placed_elsewhere(inner, dir, uuid).await {
			// Its remote job moves it here.
			Placed::Elsewhere(path) => inner.push_kind(&path, Delay::Now, Kind::Remote),
			// The index may not know it yet: never two placeholders for one item.
			Placed::Unknown => {
				Inner::request_walk(inner);
				deferred = true;
			}
			Placed::Nowhere => {
				if !may_place(inner, dir_uuid, spec, uuid, &at).await {
					deferred = true;
					continue;
				}
				let (parent, placed) = (dir.clone(), spec.clone());
				match inner
					.fs(move |local| local.create_placeholder(&parent, &placed))
					.await
				{
					Ok(()) => {
						lock(&inner.state).index.insert(&at, uuid);
						if pinned {
							// Placed into a pinned directory: it arrives hydrated.
							inner.push_kind(&at, Delay::Now, Kind::Hydration);
						}
						inner.report(LocalReport::Remote(RemoteApplied::Created {
							path: at,
							identity: spec.identity(),
						}));
					}
					Err(LocalError::Exists) => deferred = true,
					Err(error) => {
						debug!(%at, %error, "placing a remote item failed");
						deferred = true;
					}
				}
			}
		}
	}
	for (path, entry) in local.values() {
		if let LocalEntry::Placeholder {
			identity:
				Ok(identity @ (PlaceholderIdentity::File { .. } | PlaceholderIdentity::Dir { .. })),
			..
		} = entry && populated_uuid(*identity).is_some_and(|uuid| !listed.contains(&uuid))
		{
			inner.push_kind(path, Delay::Now, Kind::Remote);
		}
	}
	let mut state = lock(&inner.state);
	if !deferred {
		state.deferrals.remove(&dir.case_key());
		return Outcome::Done;
	}
	let deferrals = state.deferrals.entry(dir.case_key()).or_default();
	*deferrals += 1;
	if *deferrals < MAX_DEFERRALS {
		return Outcome::Wait(DEFERRED_AGAIN);
	}
	state.deferrals.remove(&dir.case_key());
	drop(state);
	warn!(dir = %path_tag(dir), "a reconcile kept leaving items for other jobs; waiting for the next walk");
	Outcome::Park(Failure::Transient)
}

/// Where an item missing from a directory as just read has its placeholder, as the index says.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Placed {
	/// Nowhere in the root.
	Nowhere,
	/// The index cannot say: it is not complete.
	Unknown,
	/// At this path, in another directory or placed in this one since it was read.
	Elsewhere(RootPath),
}

/// Another placeholder of the directory `uuid`, whose placeholder `dir` is: where the index places
/// it, when a placeholder of it stands there.
async fn other_placeholder<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	dir: &RootPath,
	uuid: Uuid,
) -> Option<RootPath> {
	let other = lock(&inner.state)
		.index
		.path_of(uuid)
		.filter(|other| other.case_key() != dir.case_key())
		.cloned()?;
	if inner.renaming_onto(&other) {
		return None;
	}
	match inner.inspect(&other).await {
		Ok(LocalEntry::Placeholder {
			identity: Ok(held), ..
		}) if populated_uuid(held) == Some(uuid) => Some(other),
		_ => None,
	}
}

/// Where the index places the item `uuid`, missing from the directory `dir` as just read. A
/// placeholder the index places in `dir` itself is read again first: placed since the directory
/// was read (the upload of a save converting its new file, say), it is where the index says.
async fn placed_elsewhere<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	dir: &RootPath,
	uuid: Uuid,
) -> Placed {
	let indexed = lock(&inner.state).index.path_of(uuid).cloned();
	if let Some(path) = &indexed
		&& path.parent().as_ref() == Some(dir)
		&& !inner.renaming_onto(path)
		&& let Ok(LocalEntry::Placeholder {
			identity: Ok(identity),
			..
		}) = inner.inspect(path).await
		&& populated_uuid(identity) == Some(uuid)
	{
		return Placed::Elsewhere(path.clone());
	}
	let mut state = lock(&inner.state);
	match state.index.path_of(uuid) {
		// The index places it in this directory, which was just read without it: wrong.
		Some(path) if path.parent().as_ref() == Some(dir) => {
			state.index.remove_id(uuid);
			if state.index.is_complete() {
				Placed::Nowhere
			} else {
				Placed::Unknown
			}
		}
		Some(path) => Placed::Elsewhere(path.clone()),
		None if state.index.is_complete() => Placed::Nowhere,
		None => Placed::Unknown,
	}
}

/// Whether the item `spec` stands for (populated as `uuid`), missing locally, may get a placeholder
/// at `at` in the directory `dir_uuid`: no local delete of it waits for its trash, no rename of it
/// waits to be settled, no save is taking it over, and the cache still holds it live where the
/// listing had it. The listing the reconcile works from was read before the directory was, so a
/// delete allowed in between, and even trashed since, would otherwise be undone by placing the
/// item again; and a save that renamed the item aside and deleted its backup puts its new file in
/// the item's place, taking the item back from the backup name on the drive only when it uploads:
/// placed meanwhile at the backup name, the item would have two placeholders, the second at a name
/// the user deleted.
async fn may_place<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	dir_uuid: Uuid,
	spec: &PlaceholderSpec,
	uuid: Uuid,
	at: &RootPath,
) -> bool {
	let identity = spec.identity();
	// The pending deletes first: one that ends drops its entry only after trashing the item,
	// which the cache then holds trashed.
	{
		let state = lock(&inner.state);
		let deleting = state
			.deletes
			.values()
			.any(|pending| populated_uuid(pending.identity) == Some(uuid));
		// A save a previous run left takes its place under the name it had, and a rename it left
		// is put back first.
		let restoring = state.lineage.claims_restored(uuid)
			|| state
				.rename_records
				.iter()
				.any(|record| record.concerns(uuid));
		if deleting || restoring {
			return false;
		}
	}
	if let PlaceholderIdentity::File { stable, .. } = identity
		&& claimed_by_save(inner, stable, at).await
	{
		return false;
	}
	let Some(id) = identity.cache_id() else {
		return false;
	};
	let head = match inner.drive.query_item(&id).await {
		Ok(Some(head)) if !is_trashed(&head) => head,
		_ => return false,
	};
	// Moved or renamed since the listing was read: the next reconcile places it from a fresh one.
	let in_dir = parent_of(&head).is_some_and(|parent| same_uuid(parent, &dir_uuid.to_string()));
	let named = name_of(&head).map(collision_key) == Some(collision_key(spec.name().as_str()));
	if !in_dir || !named {
		return false;
	}
	// Placed meanwhile, by a save's upload converting its new file.
	lock(&inner.state).index.path_of(uuid).is_none()
}

/// The name the drive holds `object` under, when it decrypts.
fn name_of(object: &FfiObject) -> Option<&str> {
	match object {
		FfiObject::File(file) => file.meta.as_ref().map(|meta| meta.name.as_str()),
		FfiObject::Dir(dir) => dir.meta.as_ref().map(|meta| meta.name.as_str()),
		FfiObject::Root(_) => None,
	}
}

/// Whether a rename into the directory `dir` was allowed and has not happened as the drive has it.
fn renaming_into<D: Drive, L: LocalFs>(inner: &Inner<D, L>, dir: &RootPath) -> bool {
	let key = dir.case_key();
	lock(&inner.state).rename_records.iter().any(|record| {
		!record.confirmed
			&& record
				.to
				.parent()
				.is_some_and(|parent| parent.case_key() == key)
	})
}

/// What is in the directory at `dir`, by case key.
async fn local_children<D: Drive, L: LocalFs>(
	inner: &Arc<Inner<D, L>>,
	dir: &RootPath,
) -> Result<HashMap<CaseKey, (RootPath, LocalEntry)>, Outcome> {
	let listed = {
		let dir = dir.clone();
		inner
			.fs(move |local| local.list_dir(&dir))
			.await
			.map_err(local_failed)?
	};
	let mut local = HashMap::with_capacity(listed.len());
	for child in listed.into_iter().flatten() {
		if inner.renaming_onto(&child) {
			continue;
		}
		match inner.inspect(&child).await {
			Ok(entry) => {
				local.insert(child.case_key(), (child, entry));
			}
			Err(error) => {
				debug!(%child, %error, "reading a child of a reconciled directory failed")
			}
		}
	}
	Ok(local)
}

impl<D: Drive, L: LocalFs> Inner<D, L> {
	/// Reports the remote children of the directory `dir` that get no placeholder, each once.
	pub(in crate::local) fn report_listed_skips(
		&self,
		dir: PlaceholderIdentity,
		skipped: Vec<SkippedItem>,
	) {
		let Some(dir_uuid) = populated_uuid(dir) else {
			return;
		};
		let fresh: Vec<SkippedItem> = {
			let mut state = lock(&self.state);
			skipped
				.into_iter()
				.filter(|skip| {
					state
						.listed_skips_reported
						.insert((dir_uuid, skip.id().to_owned()))
				})
				.collect()
		};
		if !fresh.is_empty() {
			self.callback.on_skipped_items(dir, &fresh);
		}
	}
}

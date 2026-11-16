//! Population: answering a `FETCH_PLACEHOLDERS` request with a directory's children, from the
//! cache.

use std::{collections::HashMap, mem};

use filen_mobile_native_cache::{
	CacheError,
	ffi::{FfiNonRootObject, QueryChildrenResponse},
};
use filen_types::fs::Uuid;
use tracing::debug;

use crate::{
	drive::Drive,
	failure::{Answer, Failure, cache_answer},
	identity::{IdentityError, PlaceholderIdentity},
	log_safe::LoggedError,
	names::{NameRejection, PlaceholderName, is_upload_name},
	os::{EntryOutcome, OsError, PopulateRequest, PopulateResponder},
	placeholder::{FileSize, FileTime, NegativeSize, PlaceholderSpec, UnrepresentableTime},
};

/// Why a listed child got no placeholder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
	/// Windows cannot hold its name as spelled. Skipped and reported, never renamed: the default
	/// for the open decision on such names ("skip and report").
	Name(NameRejection),
	/// Another child's name is equal to its own in Windows's case folding, and only one of them
	/// can exist; `kept` is the one placed. The lowest uuid wins (a file's stable id, a
	/// directory's uuid), so every listing of the directory keeps the same one.
	CaseCollision { kept: PlaceholderIdentity },
	/// Its metadata does not decrypt, so it has no name. Skipped (a recorded default; the Apple
	/// File Provider shows it under a placeholder name instead), and a skipped directory hides its
	/// whole subtree.
	Undecryptable,
	/// The drive gives it a negative size.
	NegativeSize(NegativeSize),
	/// Neither its own date nor the server's timestamp is a time Windows holds.
	UnrepresentableTime(UnrepresentableTime),
	/// Its id is not a uuid.
	MalformedId,
	/// The OS refused its placeholder.
	Refused(OsError),
}

/// A listed child that got no placeholder, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedItem {
	id: String,
	name: Option<String>,
	reason: SkipReason,
}

impl SkippedItem {
	/// The child `id` (a file's stable id, a directory's uuid) named `name`, skipped for `reason`.
	pub(crate) fn new(id: String, name: Option<String>, reason: SkipReason) -> Self {
		Self { id, name, reason }
	}

	/// The child's id as the drive lists it: a file's stable id, a directory's uuid.
	pub fn id(&self) -> &str {
		&self.id
	}

	/// The child's name, when its metadata decrypts.
	pub fn name(&self) -> Option<&str> {
		self.name.as_deref()
	}

	/// Why it got no placeholder.
	pub fn reason(&self) -> &SkipReason {
		&self.reason
	}
}

/// What a population did.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct PopulateReport {
	/// Placeholders the OS created.
	pub(crate) created: usize,
	/// Children that got no placeholder.
	pub(crate) skipped: Vec<SkippedItem>,
}

/// Why a population failed as a whole.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PopulateError {
	/// The request's identity is not one this build writes.
	#[error(transparent)]
	Identity(#[from] IdentityError),
	/// The request names a file placeholder.
	#[error("a file placeholder has no children")]
	NotADirectory,
	/// The sync root was registered for another account than the signed-in one.
	#[error("root {placeholder} is not this account's root {account}")]
	OtherAccount { placeholder: Uuid, account: Uuid },
	/// The cache's account root is not a uuid.
	#[error("the account root {0:?} is not a uuid")]
	MalformedRoot(String),
	/// The cache holds no such directory.
	#[error("the cache does not hold the directory")]
	Missing,
	/// The cache failed. Display only: `CacheError` is no `std::error::Error`, so it cannot be a
	/// source.
	#[error("{0}")]
	Cache(CacheError),
	/// The OS answered another number of outcomes than placeholders were sent.
	#[error("the OS answered {answered} outcomes for {sent} placeholders")]
	OutcomeCount { sent: usize, answered: usize },
	/// The OS refused the population as a whole.
	#[error(transparent)]
	Os(#[from] OsError),
}

/// A failed population, with what it did before failing.
#[derive(Debug)]
pub(crate) struct PopulateFailed {
	pub(crate) report: PopulateReport,
	pub(crate) error: PopulateError,
}

impl PopulateError {
	/// What the OS is told.
	pub(crate) fn failure(&self) -> Failure {
		match self {
			Self::Identity(_) | Self::NotADirectory => Failure::InvalidRequest,
			// A sync root left by another account: the signed-in one cannot serve it.
			Self::OtherAccount { .. } => Failure::NotAuthenticated,
			Self::MalformedRoot(_) | Self::OutcomeCount { .. } => Failure::Internal,
			Self::Missing => Failure::NotFound,
			Self::Cache(error) => match cache_answer(error) {
				Answer::Fail(failure) => failure,
				// Nothing aborts a listing, and a listing checks no content hash.
				Answer::Cancelled | Answer::Damaged => Failure::Transient,
			},
			Self::Os(_) => Failure::Transient,
		}
	}
}

impl LoggedError for PopulateError {
	fn cache_error(&self) -> Option<&CacheError> {
		match self {
			Self::Cache(error) => Some(error),
			Self::Identity(_)
			| Self::NotADirectory
			| Self::OtherAccount { .. }
			| Self::MalformedRoot(_)
			| Self::Missing
			| Self::OutcomeCount { .. }
			| Self::Os(_) => None,
		}
	}
}

/// The uuid a collision is decided by: a file's stable id, a directory's uuid.
fn primary_uuid(identity: PlaceholderIdentity) -> Uuid {
	match identity {
		PlaceholderIdentity::Root { root } => root,
		PlaceholderIdentity::Dir { uuid } => uuid,
		PlaceholderIdentity::File { stable, .. } => stable,
		PlaceholderIdentity::Detached { nonce } => nonce,
	}
}

/// The time to show for an item: `own`, its own date, when it has one Windows holds, else the
/// server's `timestamp`, which every row has.
fn time_or_timestamp(own: Option<i64>, timestamp: i64) -> Result<FileTime, UnrepresentableTime> {
	own.and_then(|millis| FileTime::from_unix_millis(millis).ok())
		.map_or_else(|| FileTime::from_unix_millis(timestamp), Ok)
}

/// The placeholder for `child`, or why it gets none.
pub(crate) fn spec_for(child: &FfiNonRootObject) -> Result<PlaceholderSpec, SkipReason> {
	let identity = PlaceholderIdentity::from_child(child).map_err(|_| SkipReason::MalformedId)?;
	match (child, identity) {
		(FfiNonRootObject::File(file), PlaceholderIdentity::File { stable, version }) => {
			let meta = file.meta.as_ref().ok_or(SkipReason::Undecryptable)?;
			let name = PlaceholderName::parse(&meta.name).map_err(SkipReason::Name)?;
			let size = FileSize::from_i64(file.size).map_err(SkipReason::NegativeSize)?;
			let created = time_or_timestamp(meta.created, file.timestamp)
				.map_err(SkipReason::UnrepresentableTime)?;
			let modified = time_or_timestamp(Some(meta.modified), file.timestamp)
				.map_err(SkipReason::UnrepresentableTime)?;
			Ok(PlaceholderSpec::file(
				name, stable, version, size, created, modified,
			))
		}
		(FfiNonRootObject::Dir(dir), PlaceholderIdentity::Dir { uuid }) => {
			let meta = dir.meta.as_ref().ok_or(SkipReason::Undecryptable)?;
			let name = PlaceholderName::parse(&meta.name).map_err(SkipReason::Name)?;
			// A directory has no modification date of its own.
			let created = time_or_timestamp(meta.created, dir.timestamp)
				.map_err(SkipReason::UnrepresentableTime)?;
			Ok(PlaceholderSpec::dir(name, uuid, created, created))
		}
		// `from_child` gives a file a file identity and a directory a directory one.
		_ => Err(SkipReason::MalformedId),
	}
}

/// The child's id and name as the drive lists them, for a skip report.
pub(crate) fn listed_id_and_name(child: &FfiNonRootObject) -> (String, Option<String>) {
	match child {
		FfiNonRootObject::File(file) => (
			file.stable_uuid.clone(),
			file.meta.as_ref().map(|meta| meta.name.clone()),
		),
		FfiNonRootObject::Dir(dir) => (
			dir.uuid.clone(),
			dir.meta.as_ref().map(|meta| meta.name.clone()),
		),
	}
}

/// The skip report for a placeholder that lost a case collision to `kept`.
fn collision(lost: &PlaceholderSpec, kept: PlaceholderIdentity) -> SkippedItem {
	SkippedItem {
		id: primary_uuid(lost.identity()).to_string(),
		name: Some(lost.name().as_str().to_owned()),
		reason: SkipReason::CaseCollision { kept },
	}
}

/// The placeholders for `listing`'s children, in listing order, and the children that get none.
/// Linear: case collisions are found through one map of case keys.
pub(crate) fn plan_entries(
	listing: &QueryChildrenResponse,
) -> (Vec<PlaceholderSpec>, Vec<SkippedItem>) {
	let mut specs: Vec<PlaceholderSpec> = Vec::with_capacity(listing.objects.len());
	let mut skipped = Vec::new();
	let mut by_key: HashMap<String, usize> = HashMap::with_capacity(listing.objects.len());
	for child in &listing.objects {
		if listed_id_and_name(child)
			.1
			.as_deref()
			.is_some_and(is_upload_name)
		{
			// A new file on its way to its name: placed once it has it.
			continue;
		}
		let spec = match spec_for(child) {
			Ok(spec) => spec,
			Err(reason) => {
				let (id, name) = listed_id_and_name(child);
				skipped.push(SkippedItem { id, name, reason });
				continue;
			}
		};
		let key = spec.name().case_key();
		let Some(&at) = by_key.get(&key) else {
			by_key.insert(key, specs.len());
			specs.push(spec);
			continue;
		};
		let placed = specs
			.get_mut(at)
			.expect("by_key holds only indices of specs (should be impossible)");
		if primary_uuid(spec.identity()) < primary_uuid(placed.identity()) {
			let lost = mem::replace(placed, spec);
			skipped.push(collision(&lost, placed.identity()));
		} else {
			skipped.push(collision(&spec, placed.identity()));
		}
	}
	(specs, skipped)
}

/// Answers `request` through `responder` with the children of its directory, from `drive`.
///
/// The directory is listed after a server refresh. Offline, the population fails as unreachable
/// rather than placing what the cache may never have listed (a recorded default): Windows asks
/// again the next time the directory is opened. The whole directory is held in memory and sent in
/// one call, as the OS takes it: an accepted cost, linear in the number of children.
pub(crate) async fn populate<D: Drive, R: PopulateResponder>(
	drive: &D,
	request: PopulateRequest,
	responder: R,
) -> Result<PopulateReport, PopulateFailed> {
	let (specs, skipped) = match listing(drive, &request).await {
		Ok(listing) => plan_entries(&listing),
		Err(error) => {
			responder.fail(error.failure());
			return Err(PopulateFailed {
				report: PopulateReport::default(),
				error,
			});
		}
	};
	let mut report = PopulateReport {
		created: 0,
		skipped,
	};
	let outcomes = match responder.populate(&specs) {
		Ok(outcomes) => outcomes,
		Err(error) => {
			return Err(PopulateFailed {
				report,
				error: error.into(),
			});
		}
	};
	if outcomes.len() != specs.len() {
		report.created = outcomes
			.iter()
			.filter(|outcome| **outcome == EntryOutcome::Created)
			.count();
		return Err(PopulateFailed {
			report,
			error: PopulateError::OutcomeCount {
				sent: specs.len(),
				answered: outcomes.len(),
			},
		});
	}
	for (spec, outcome) in specs.iter().zip(outcomes) {
		match outcome {
			EntryOutcome::Created => report.created += 1,
			EntryOutcome::Exists => {
				debug!(name = ?spec.name(), "a local item holds a placeholder's name");
			}
			EntryOutcome::Rejected(error) => {
				debug!(name = ?spec.name(), %error, "the OS refused a placeholder");
				report.skipped.push(SkippedItem {
					id: primary_uuid(spec.identity()).to_string(),
					name: Some(spec.name().as_str().to_owned()),
					reason: SkipReason::Refused(error),
				});
			}
		}
	}
	Ok(report)
}

/// The children of the directory `request` names.
async fn listing<D: Drive>(
	drive: &D,
	request: &PopulateRequest,
) -> Result<QueryChildrenResponse, PopulateError> {
	let identity = PlaceholderIdentity::decode(request.identity())?;
	match identity {
		PlaceholderIdentity::File { .. } | PlaceholderIdentity::Detached { .. } => {
			return Err(PopulateError::NotADirectory);
		}
		PlaceholderIdentity::Root { root } => {
			let account = drive.root_uuid().await.map_err(PopulateError::Cache)?;
			let account =
				Uuid::parse_str(&account).map_err(|_| PopulateError::MalformedRoot(account))?;
			if account != root {
				return Err(PopulateError::OtherAccount {
					placeholder: root,
					account,
				});
			}
		}
		PlaceholderIdentity::Dir { .. } => {}
	}
	let id = identity.cache_id().ok_or(PopulateError::NotADirectory)?;
	drive
		.list_children(id)
		.await
		.map_err(PopulateError::Cache)?
		.ok_or(PopulateError::Missing)
}

#[cfg(test)]
mod tests;

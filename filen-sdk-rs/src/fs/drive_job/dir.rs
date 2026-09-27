//! Creating one directory: under the drive lock, with keep-both naming at the top level, its
//! color set and the destination's links and shares given it.

use std::{borrow::Cow, sync::Arc};

use chrono::{DateTime, Utc};
use filen_types::{api::v3::dir::color::DirColor, fs::Uuid};

use crate::{
	Error, ErrorKind,
	connect::ConnectedTargets,
	fs::name::keep_both::NameShape,
	fs::{categories::NonRootItemType, dir::RemoteDirectory, name::ValidatedName},
	job::{JobControl, Stopped, report::Ops},
};

use super::{
	backend::{CreatedDir, DriveBackend},
	lock::{LockWait, wait_for_lock},
	name_retry::NameRetry,
};

/// What [`create_dir`] needs.
pub(crate) struct DirTask<B> {
	pub(crate) backend: Arc<B>,
	pub(crate) control: JobControl,
	pub(crate) ops: Ops,
	pub(crate) targets: Arc<ConnectedTargets>,
	pub(crate) parent: Uuid,
	pub(crate) uuid: Uuid,
	pub(crate) name: ValidatedName,
	pub(crate) created: DateTime<Utc>,
	/// [`DirColor::Default`] sets none.
	pub(crate) color: DirColor<'static>,
	/// Created in a directory the job did not create, which may hold the name by now: a merge
	/// into an existing directory moves on to the next keep-both name instead of failing.
	pub(crate) top_level: bool,
	/// Check the name with the server before creating the directory.
	pub(crate) verify_name: bool,
	/// What the error giving up on a free name calls the directory (see [`NameRetry::new`]).
	pub(crate) subject: &'static str,
}

/// A created directory with the name it got, and what went wrong around it without undoing it.
pub(crate) struct CreatedDirOutcome {
	pub(crate) dir: RemoteDirectory,
	pub(crate) name: ValidatedName,
	pub(crate) color_error: Option<Error>,
	/// The links or shares it could not be added to.
	pub(crate) propagation_errors: Vec<Error>,
}

pub(crate) enum DirError {
	/// Paused or stopped before anything was sent; the create is tried again after a pause.
	NotStarted,
	Failed(Error),
}

/// Creates one directory. The shared lock a job holds is normally handed out at once; a fresh
/// acquisition (its lease was lost) can wait long. Nothing is sent before the lock is held, so a
/// pause or stop until then leaves the create to be tried again; once held, the create runs to
/// the end.
pub(crate) async fn create_dir<B: DriveBackend>(
	task: DirTask<B>,
) -> Result<CreatedDirOutcome, DirError> {
	let DirTask {
		backend,
		control,
		ops,
		targets,
		parent,
		uuid,
		mut name,
		created,
		color,
		top_level,
		verify_name,
		subject,
	} = task;
	let backend = &*backend;
	let _lock = match wait_for_lock(backend, &control, &ops).await {
		Ok(LockWait::Locked(held)) => held,
		Ok(LockWait::Paused) | Err(Stopped) => return Err(DirError::NotStarted),
		Ok(LockWait::Failed(error)) => return Err(DirError::Failed(error)),
	};
	let mut retry = NameRetry::new(NameShape::Dir, subject);
	let mut dir = loop {
		if verify_name {
			name = retry
				.free_name(backend, parent, name)
				.await
				.map_err(DirError::Failed)?;
		}
		match backend
			.create_dir_unpropagated(parent, uuid, &name, created)
			.await
			.map_err(DirError::Failed)?
		{
			CreatedDir::Created(created) => break created,
			// Someone created the same name at the destination after it was listed: keep both by
			// taking the next free name.
			CreatedDir::Merged if top_level => name = retry.next(name).map_err(DirError::Failed)?,
			CreatedDir::Merged => {
				return Err(DirError::Failed(Error::custom(
					ErrorKind::InvalidState,
					"a directory with this name already exists in the new directory",
				)));
			}
		}
	};
	let color_error = if color == DirColor::Default {
		None
	} else {
		backend.set_dir_color(&mut dir, color).await.err()
	};
	let propagation_errors = if targets.is_empty() {
		Vec::new()
	} else {
		backend
			.propagate(&targets, NonRootItemType::Dir(Cow::Borrowed(&dir)))
			.await
	};
	Ok(CreatedDirOutcome {
		dir,
		name,
		color_error,
		propagation_errors,
	})
}

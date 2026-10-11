//! Registering an uploaded file as a new file: under the drive lock, with its name checked
//! again when the parent may already hold it, and shared with the destination's links and
//! shares.

use std::borrow::Cow;

use filen_types::fs::Uuid;

use crate::{
	Error,
	connect::ConnectedTargets,
	fs::{
		categories::NonRootItemType,
		file::{
			RemoteFile,
			write::{RemoteFileInfo, UploadCompletion},
		},
		name::ValidatedName,
	},
	job::{JobControl, Stopped, report::Ops},
};

#[cfg(feature = "archive")]
use super::lock::{LockWait, wait_for_lock};
use super::{
	backend::DriveBackend,
	lock::{HeldLock, lock_after_pauses, propagate_after_write},
	name_retry::NameRetry,
};

/// What [`finalize_new_file`] needs.
pub(crate) struct FinalizeTask<'a, B: DriveBackend> {
	pub(crate) backend: &'a B,
	pub(crate) control: &'a JobControl,
	pub(crate) ops: &'a Ops,
	pub(crate) upload: &'a B::Upload,
	pub(crate) parent: Uuid,
	pub(crate) name: ValidatedName,
	/// Set when the parent was not created by this job, so it may hold the name by now: the
	/// name is checked again, and moved on to the next keep-both name when taken.
	pub(crate) recheck: Option<&'a mut NameRetry>,
	pub(crate) completion: UploadCompletion,
	pub(crate) info: RemoteFileInfo,
	pub(crate) targets: &'a ConnectedTargets,
}

/// A registered file, with the name it got and the links or shares it could not be added to.
pub(crate) struct Finalized {
	pub(crate) file: RemoteFile,
	pub(crate) name: ValidatedName,
	pub(crate) propagation_errors: Vec<Error>,
}

pub(crate) enum FinalizeError {
	/// Stopped before registering began.
	Stopped,
	Failed(Error),
	/// The server made the file a new version of an existing file with the same name instead of
	/// a new file (possible only if a client writing without the drive lock took the name at the
	/// last moment). It was still added to the targets it could be.
	RegisteredAsVersion {
		file: Box<RemoteFile>,
		propagation_errors: Vec<Error>,
	},
}

impl From<Stopped> for FinalizeError {
	fn from(_: Stopped) -> Self {
		Self::Stopped
	}
}

/// Registers an uploaded file. Registering is not started while paused, but once started it runs
/// to the end even on cancel, so a file that exists is always reported.
pub(crate) async fn finalize_new_file<B: DriveBackend>(
	task: FinalizeTask<'_, B>,
) -> Result<Finalized, FinalizeError> {
	let lock = lock_after_pauses(task.backend, task.control, task.ops)
		.await?
		.map_err(FinalizeError::Failed)?;
	register(task, lock).await
}

/// What became of a registration that gives way to a pause.
#[cfg(feature = "archive")]
#[expect(
	clippy::large_enum_variant,
	reason = "made once per file and moved once, from its registration to the driver: boxing \
	          would add an allocation to save a copy"
)]
pub(crate) enum UnlessPaused {
	/// It was attempted: how it went.
	Ran(Result<Finalized, FinalizeError>),
	/// A pause came before the drive lock was held: nothing was sent.
	Paused,
}

/// Registers an uploaded file like [`finalize_new_file`], except that a pause requested before
/// the drive lock is held ends it with nothing sent instead of being waited out, for the caller
/// to start it again once resumed: a job that pauses once its registrations are over is never
/// kept from pausing by one waiting for the lock. A pause requested while it waits for the lock
/// again, its lease not confirmed before anything was sent, is waited out.
#[cfg(feature = "archive")]
pub(crate) async fn finalize_new_file_unless_paused<B: DriveBackend>(
	task: FinalizeTask<'_, B>,
) -> UnlessPaused {
	let lock = match wait_for_lock(task.backend, task.control, task.ops).await {
		Ok(LockWait::Locked(held)) => held,
		Ok(LockWait::Paused) => return UnlessPaused::Paused,
		Ok(LockWait::Failed(error)) => {
			return UnlessPaused::Ran(Err(FinalizeError::Failed(error)));
		}
		Err(Stopped) => return UnlessPaused::Ran(Err(FinalizeError::Stopped)),
	};
	UnlessPaused::Ran(register(task, lock).await)
}

/// Registers the file under the drive lock `lock`, then adds it to `targets`: that is not
/// skipped for a lease that cannot be confirmed (see [`propagate_after_write`]).
async fn register<B: DriveBackend>(
	task: FinalizeTask<'_, B>,
	mut lock: HeldLock<B::DriveLock>,
) -> Result<Finalized, FinalizeError> {
	let FinalizeTask {
		backend,
		control,
		ops,
		upload,
		parent,
		mut name,
		mut recheck,
		completion,
		info,
		targets,
	} = task;
	loop {
		if let Some(retry) = recheck.as_deref_mut() {
			// Registering a file under a name the parent already holds would make it a new
			// version of that file instead of a new file, so the name is checked again here,
			// while holding the drive lock: clients that write under the lock cannot take it in
			// between.
			name = retry
				.free_name(backend, parent, name)
				.await
				.map_err(FinalizeError::Failed)?;
		}
		// The wait for the lock and the name check can take long. Nothing but the name was read
		// under the lock, so a lease that cannot be confirmed (lost, or the server answering
		// with an error; a failed request is asked again while the lease could stand) is not
		// the registration's end: the lock is waited for again and the name checked again under
		// the new lease.
		let Err(unconfirmed) = lock.confirm().await else {
			break;
		};
		tracing::warn!("{unconfirmed}; waiting for the drive lock again to register a file");
		drop(lock);
		lock = lock_after_pauses(backend, control, ops)
			.await?
			.map_err(FinalizeError::Failed)?;
	}
	let file = backend
		.finish_upload(upload, &name, completion, info)
		.await
		.map_err(FinalizeError::Failed)?;
	let registered_as_version = file.stable_uuid != file.uuid;
	if registered_as_version {
		tracing::error!(
			"new file {} was registered as a new version of the existing file {}",
			file.uuid,
			Uuid::from(file.stable_uuid)
		);
	}
	let propagation_errors = if targets.is_empty() {
		Vec::new()
	} else {
		propagate_after_write(
			backend,
			control,
			ops,
			lock,
			targets,
			NonRootItemType::File(Cow::Borrowed(&file)),
		)
		.await
	};
	if registered_as_version {
		return Err(FinalizeError::RegisteredAsVersion {
			file: Box::new(file),
			propagation_errors,
		});
	}
	Ok(Finalized {
		file,
		name,
		propagation_errors,
	})
}

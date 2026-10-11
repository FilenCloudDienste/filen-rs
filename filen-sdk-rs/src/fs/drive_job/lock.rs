//! The drive lock, waited for and held as one of a job's in-flight operations, so a pause never
//! leaves a job reported paused while it holds the lock.

use filen_types::traits::CowHelpers;

use crate::{
	Error,
	connect::ConnectedTargets,
	fs::{
		HasUUID,
		categories::{NonRootItemType, Normal},
	},
	job::{
		JobControl, Stopped,
		report::{JobState, OpGuard, Ops, Reporter},
	},
	sync::lock::Lease,
	util::MaybeArc,
};

use super::backend::DriveBackend;

/// The drive lock, held as one of the job's in-flight operations. The lock is dropped before
/// the operation ends, so the job is only reported paused once the lock is gone.
pub(crate) struct HeldLock<L> {
	lock: L,
	_op: OpGuard,
}

impl<L: Lease> HeldLock<L> {
	pub(crate) fn lock(&self) -> &L {
		&self.lock
	}

	/// See [`Lease::confirm`].
	pub(crate) async fn confirm(&self) -> Result<(), Error> {
		self.lock.confirm().await
	}
}

pub(crate) enum LockWait<L> {
	Locked(HeldLock<L>),
	/// A pause was requested while waiting; nothing is held.
	Paused,
	Failed(Error),
}

/// Waits for the drive lock, which another client may hold for a long time. A stop ends the
/// wait (`Err`), and a pause requested meanwhile ends it or drops the lock just acquired, so a
/// paused job holds no lock. After [`LockWait::Paused`] the caller waits out the pause before
/// trying again.
pub(crate) async fn wait_for_lock<B: DriveBackend>(
	backend: &B,
	control: &JobControl,
	ops: &Ops,
) -> Result<LockWait<B::DriveLock>, Stopped> {
	let op = ops.op();
	let result = tokio::select! {
		biased;
		() = control.stopping() => return Err(Stopped),
		() = control.pause_changed(false) => return Ok(LockWait::Paused),
		result = backend.acquire_drive_lock() => result,
	};
	Ok(match result {
		Ok(_) if control.is_pause_requested() => LockWait::Paused,
		Ok(lock) => LockWait::Locked(HeldLock { lock, _op: op }),
		Err(error) => LockWait::Failed(error),
	})
}

/// Waits for the drive lock like [`wait_for_lock`], waiting out a pause first and any pause
/// requested meanwhile. `Ok(Err)` when the acquisition fails.
pub(crate) async fn lock_after_pauses<B: DriveBackend>(
	backend: &B,
	control: &JobControl,
	ops: &Ops,
) -> Result<Result<HeldLock<B::DriveLock>, Error>, Stopped> {
	loop {
		control.checkpoint().await?;
		match wait_for_lock(backend, control, ops).await? {
			LockWait::Locked(held) => return Ok(Ok(held)),
			LockWait::Paused => {}
			LockWait::Failed(error) => return Ok(Err(error)),
		}
	}
}

/// Waits for the drive lock for a pass over items the job created, like
/// [`lock_after_pauses`], except that a pause is waited out through `reporter`, which reports
/// it. `Ok(Err)` when the acquisition fails.
pub(crate) async fn lock_reporting_pauses<B: DriveBackend, S: JobState>(
	backend: &B,
	control: &JobControl,
	reporter: &MaybeArc<Reporter<S>>,
) -> Result<Result<HeldLock<B::DriveLock>, Error>, Stopped> {
	loop {
		match wait_for_lock(backend, control, &reporter.ops()).await? {
			LockWait::Locked(held) => return Ok(Ok(held)),
			LockWait::Paused => reporter.checkpoint(control).await?,
			LockWait::Failed(error) => return Ok(Err(error)),
		}
	}
}

/// The drive lock waited for again ([`lock_after_pauses`]) for a step after a write that
/// landed, as the lease of the one held could not be confirmed (`unconfirmed` says why). `Err`
/// with what to report as the step's failure when the lock cannot be had: `unconfirmed` once
/// the job stops, or the acquisition's error.
async fn wait_again<B: DriveBackend>(
	backend: &B,
	control: &JobControl,
	ops: &Ops,
	unconfirmed: Error,
) -> Result<HeldLock<B::DriveLock>, Error> {
	tracing::warn!("{unconfirmed}; waiting for the drive lock again");
	match lock_after_pauses(backend, control, ops).await {
		Ok(Ok(held)) => Ok(held),
		Ok(Err(error)) => Err(error),
		Err(Stopped) => Err(unconfirmed),
	}
}

/// `held` once its lease is confirmed, for a step after a write that landed. Once it cannot
/// be (lost, or not confirmable), the step is not skipped: the lock is waited for again, a job
/// that stops meanwhile not kept waiting. `Err` with what to report as the step's failure when
/// the lock cannot be had (see [`wait_again`]).
pub(crate) async fn confirmed_after_write<B: DriveBackend>(
	backend: &B,
	control: &JobControl,
	ops: &Ops,
	held: HeldLock<B::DriveLock>,
) -> Result<HeldLock<B::DriveLock>, Error> {
	match held.confirm().await {
		Ok(()) => Ok(held),
		Err(unconfirmed) => {
			drop(held);
			wait_again(backend, control, ops, unconfirmed).await
		}
	}
}

/// Adds `item`, created by a write that landed, to `targets` under the drive lock `lock`. While
/// the lock's lease cannot be confirmed nothing is sent, and the lock is waited for again, a job
/// that stops meanwhile not kept waiting. Returns what failed: the operations, or what kept the
/// lock from being had (see [`wait_again`]).
pub(crate) async fn propagate_after_write<B: DriveBackend>(
	backend: &B,
	control: &JobControl,
	ops: &Ops,
	mut lock: HeldLock<B::DriveLock>,
	targets: &ConnectedTargets,
	item: NonRootItemType<'_, Normal>,
) -> Vec<Error> {
	loop {
		let unconfirmed = match backend
			.propagate(lock.lock(), targets, item.as_borrowed_cow())
			.await
		{
			Ok(errors) => return errors,
			Err(unconfirmed) => unconfirmed,
		};
		drop(lock);
		tracing::warn!("propagating {} waits for the drive lock again", item.uuid());
		lock = match wait_again(backend, control, ops, unconfirmed).await {
			Ok(held) => held,
			Err(error) => return vec![error],
		};
	}
}

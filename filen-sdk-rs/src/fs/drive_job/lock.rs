//! The drive lock, waited for and held as one of a job's in-flight operations, so a pause never
//! leaves a job reported paused while it holds the lock.

use crate::{
	Error,
	job::{
		JobControl, Stopped,
		report::{OpGuard, Ops},
	},
};

use super::backend::DriveBackend;

/// The drive lock, held as one of the job's in-flight operations. The lock is dropped before
/// the operation ends, so the job is only reported paused once the lock is gone.
pub(crate) struct HeldLock<L> {
	_lock: L,
	_op: OpGuard,
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
		Ok(lock) => LockWait::Locked(HeldLock {
			_lock: lock,
			_op: op,
		}),
		Err(error) => LockWait::Failed(error),
	})
}

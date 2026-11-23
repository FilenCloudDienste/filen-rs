//! A semaphore whose size changes while its permits are held.

use std::{
	pin::Pin,
	sync::{
		Arc, Mutex, MutexGuard, PoisonError,
		atomic::{AtomicUsize, Ordering},
	},
	task::{Context, Poll, Waker},
};

use futures::future::{AbortHandle, abortable};
use tokio::sync::{AcquireError, OwnedSemaphorePermit, Semaphore};

/// A [`Semaphore`] holding [`size`](Self::size) permits, resizable at any time without waiting for
/// its holders.
///
/// Growing adds the permits at once. Shrinking takes the free permits out of circulation at once
/// and queues for the rest, which it takes as holders give theirs back: the queue is first come,
/// first served, so from the resize on nobody new is admitted until less than the new size is
/// held. A holder keeps what it holds, and a waiter keeps its place in the queue; nothing is
/// cancelled. Waiting for what is still held needs a task of its own (see [`absorb`]), so a
/// shrink that has to wait is made from within the runtime.
pub(super) struct ResizableSemaphore {
	/// `Arc`: the permits are taken through owned acquisitions (the tower concurrency limits, the
	/// chunk reservations) that outlive any borrow of this struct, and the task taking permits
	/// back after a shrink holds it too.
	semaphore: Arc<Semaphore>,
	/// What the semaphore is sized to. Written only under `resize`, read without it.
	size: AtomicUsize,
	/// `Arc`: shared with the task taking permits back after a shrink.
	resize: Arc<Mutex<Resize>>,
}

/// What a shrink still has to take back.
#[derive(Default)]
struct Resize {
	/// Permits in circulation (free, held, or being handed to [`absorb`]) beyond the size: the
	/// semaphore circulates `size + surplus` permits.
	surplus: usize,
	/// The task taking `surplus` back, while it runs.
	absorber: Option<AbortHandle>,
	/// Bumped by every resize, so a superseded task never takes back what the new size counts on.
	generation: u64,
}

impl ResizableSemaphore {
	/// `size` must be at most [`Semaphore::MAX_PERMITS`].
	pub(super) fn new(size: usize) -> Self {
		Self {
			semaphore: Arc::new(Semaphore::new(size)),
			size: AtomicUsize::new(size),
			resize: Arc::default(),
		}
	}

	pub(super) fn semaphore(&self) -> &Arc<Semaphore> {
		&self.semaphore
	}

	/// How many permits the semaphore is sized to.
	pub(super) fn size(&self) -> usize {
		self.size.load(Ordering::Relaxed)
	}

	/// Sizes the semaphore to `size` permits, at most [`Semaphore::MAX_PERMITS`].
	pub(super) fn resize(&self, size: usize) {
		let mut resize = lock(&self.resize);
		// What the task was handed and has not taken out of circulation returns to the semaphore
		// when its future is dropped, so the surplus still counts it.
		if let Some(absorber) = resize.absorber.take() {
			absorber.abort();
		}
		resize.generation = resize.generation.wrapping_add(1);
		// Both were at most MAX_PERMITS - surplus and surplus when they were set: no overflow.
		let circulating = self.size() + resize.surplus;
		if let Some(missing) = size.checked_sub(circulating) {
			self.semaphore.add_permits(missing);
			resize.surplus = 0;
		} else {
			let excess = circulating - size;
			resize.surplus = excess - self.semaphore.forget_permits(excess);
			self.queue_for_surplus(&mut resize);
		}
		self.size.store(size, Ordering::Relaxed);
	}

	/// Queues for the surplus before `resize` returns, so no acquisition made after it is served
	/// first, and leaves the wait to a task.
	fn queue_for_surplus(&self, resize: &mut Resize) {
		while resize.surplus > 0 {
			let take = u32::try_from(resize.surplus).unwrap_or(u32::MAX);
			let mut acquire = Box::pin(Arc::clone(&self.semaphore).acquire_many_owned(take));
			// The first poll joins the queue; the task's polls replace the no-op waker with its own.
			match acquire
				.as_mut()
				.poll(&mut Context::from_waker(Waker::noop()))
			{
				Poll::Ready(Ok(permits)) => {
					// handed back since the free permits were taken
					permits.forget();
					resize.surplus -= take as usize;
				}
				// closed: there is nothing left to limit
				Poll::Ready(Err(_)) => return,
				Poll::Pending => {
					let (task, handle) = abortable(absorb(
						acquire,
						take,
						Arc::clone(&self.semaphore),
						Arc::clone(&self.resize),
						resize.generation,
					));
					resize.absorber = Some(handle);
					// Detached: it ends once it took the surplus back or a later resize aborted it.
					drop(crate::runtime::spawn_task_maybe_send(async move {
						let _ = task.await;
					}));
					return;
				}
			}
		}
	}
}

fn lock(resize: &Mutex<Resize>) -> MutexGuard<'_, Resize> {
	resize.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Takes the surplus out of circulation as holders give their permits back, starting with
/// `queued`, an acquisition of `take` permits already in the semaphore's queue. The waiters
/// behind it get nothing until it is served. Past `u32::MAX` permits the surplus is taken in
/// parts, each queueing again.
async fn absorb<F>(
	queued: Pin<Box<F>>,
	mut take: u32,
	semaphore: Arc<Semaphore>,
	resize: Arc<Mutex<Resize>>,
	generation: u64,
) where
	F: Future<Output = Result<OwnedSemaphorePermit, AcquireError>>,
{
	let mut acquired = queued.await;
	loop {
		// closed: there is nothing left to limit
		let Ok(permits) = acquired else {
			return;
		};
		{
			let mut state = lock(&resize);
			if state.generation != generation {
				// A later resize counted these as circulating; dropping them gives them back.
				return;
			}
			permits.forget();
			state.surplus -= take as usize;
			if state.surplus == 0 {
				state.absorber = None;
				return;
			}
			take = u32::try_from(state.surplus).unwrap_or(u32::MAX);
		}
		acquired = Arc::clone(&semaphore).acquire_many_owned(take).await;
	}
}

#[cfg(all(test, not(all(target_family = "wasm", target_os = "unknown"))))]
mod tests {
	use super::*;

	/// Lets the absorbing task run until it waits again.
	async fn settle() {
		for _ in 0..8 {
			tokio::task::yield_now().await;
		}
	}

	fn surplus(semaphore: &ResizableSemaphore) -> usize {
		lock(&semaphore.resize).surplus
	}

	#[tokio::test]
	async fn growing_adds_the_permits_at_once() {
		let semaphore = ResizableSemaphore::new(2);
		let _held = Arc::clone(semaphore.semaphore()).try_acquire_many_owned(2);
		semaphore.resize(5);
		assert_eq!(semaphore.size(), 5);
		assert_eq!(semaphore.semaphore().available_permits(), 3);
	}

	#[tokio::test]
	async fn shrinking_takes_the_free_permits_at_once() {
		let semaphore = ResizableSemaphore::new(6);
		semaphore.resize(2);
		assert_eq!(semaphore.size(), 2);
		assert_eq!(semaphore.semaphore().available_permits(), 2);
		assert_eq!(surplus(&semaphore), 0);
	}

	#[tokio::test]
	async fn shrinking_below_what_is_held_takes_the_rest_back_as_holders_return_it() {
		let semaphore = ResizableSemaphore::new(4);
		let first = Arc::clone(semaphore.semaphore())
			.try_acquire_owned()
			.expect("free");
		let rest = Arc::clone(semaphore.semaphore())
			.try_acquire_many_owned(3)
			.expect("free");
		semaphore.resize(1);
		settle().await;
		assert_eq!(surplus(&semaphore), 3);

		// a waiter queued behind the shrink gets nothing until the surplus is back
		let mut waiter = Box::pin(Arc::clone(semaphore.semaphore()).acquire_owned());
		assert!(futures::poll!(waiter.as_mut()).is_pending());

		drop(rest);
		settle().await;
		assert_eq!(surplus(&semaphore), 0);
		assert_eq!(semaphore.semaphore().available_permits(), 0);
		assert!(futures::poll!(waiter.as_mut()).is_pending());

		drop(first);
		let permit = waiter.await.expect("never closed");
		assert_eq!(semaphore.semaphore().available_permits(), 0);
		drop(permit);
		assert_eq!(semaphore.semaphore().available_permits(), 1);
	}

	#[tokio::test]
	async fn growing_during_a_shrink_cancels_what_it_still_owes() {
		let semaphore = ResizableSemaphore::new(4);
		let held = Arc::clone(semaphore.semaphore())
			.try_acquire_many_owned(4)
			.expect("free");
		semaphore.resize(1);
		settle().await;
		assert_eq!(surplus(&semaphore), 3);

		semaphore.resize(3);
		settle().await;
		assert_eq!(surplus(&semaphore), 1);
		drop(held);
		settle().await;
		assert_eq!(surplus(&semaphore), 0);
		assert_eq!(semaphore.semaphore().available_permits(), 3);

		semaphore.resize(6);
		assert_eq!(semaphore.semaphore().available_permits(), 6);
	}

	#[tokio::test]
	async fn growing_past_a_pending_shrink_adds_only_what_is_missing() {
		let semaphore = ResizableSemaphore::new(4);
		let held = Arc::clone(semaphore.semaphore())
			.try_acquire_many_owned(4)
			.expect("free");
		semaphore.resize(1);
		settle().await;
		semaphore.resize(8);
		settle().await;
		assert_eq!(surplus(&semaphore), 0);
		assert_eq!(semaphore.semaphore().available_permits(), 4);
		drop(held);
		settle().await;
		assert_eq!(semaphore.semaphore().available_permits(), 8);
	}
}

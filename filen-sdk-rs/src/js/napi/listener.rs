//! The callbacks of a napi call: one weak threadsafe function per callback object per call.
//!
//! Its calls reach JS on the JS thread in the order they were made, from any thread, without
//! waiting: the threadsafe function is NonBlocking on an unbounded queue (napi-rs's default
//! `max_queue_size` of 0), so a busy JS thread never holds up the transfer that reports to it,
//! unlike the sync engine's listener, which its own thread calls Blocking on a queue of 2. The
//! callback object's methods are read and bound once, when the call is made. A method that throws
//! is logged and loses only its own call.
//!
//! napi settles a promise through a deferred of its own, not through this queue, so nothing orders
//! the two: [`Listener::flushed`] sends a marker through the same queue and waits for the JS thread
//! to reach it, which a call awaits before it settles.

use std::{
	panic::{AssertUnwindSafe, catch_unwind},
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	},
};

use napi::{
	Env, Status,
	bindgen_prelude::Object,
	threadsafe_function::{ThreadsafeCallContext, ThreadsafeFunctionCallMode},
};
use tokio::sync::{oneshot, watch};

/// One call a [`Listener`] makes, and how it is made on the JS thread.
pub(crate) trait ListenerCall: Send + 'static {
	/// The callback object's methods, bound to it.
	type Methods: Send + 'static;

	/// Read and bind `callback`'s methods, on the JS thread. A missing method fails with napi's
	/// own error.
	fn bind(callback: &Object<'_>) -> napi::Result<Self::Methods>;

	/// Make this call on the JS thread.
	fn call(self, env: &Env, methods: &Self::Methods) -> napi::Result<()>;
}

/// What the queue carries: a call, or the marker [`Listener::flushed`] waits for.
enum Message<C> {
	Call(C),
	Flush(oneshot::Sender<()>),
}

type SendFn<C> = Box<dyn Fn(Message<C>) -> Status + Send + Sync>;

/// Makes the calls of one callback object. `Clone` (an `Arc` inside): one clone moves into the
/// `'static` closure the SDK calls on the commander runtime, while the call keeps one to await
/// [`flushed`](Self::flushed) once the SDK returns.
pub(crate) struct Listener<C: ListenerCall>(Arc<ListenerInner<C>>);

impl<C: ListenerCall> Clone for Listener<C> {
	fn clone(&self) -> Self {
		Self(Arc::clone(&self.0))
	}
}

struct ListenerInner<C> {
	send: SendFn<C>,
	/// Set once a send answers `Closing`: the environment is gone, and so is every later call.
	closed: AtomicBool,
	/// Changes (to closed) once the threadsafe function is finalized, as its environment goes
	/// away: a marker still queued then is dropped without its sender, so a flush waits on this too.
	env_gone: watch::Receiver<()>,
}

/// What the JS thread holds: the bound methods, and the sender whose drop with the threadsafe
/// function's callback tells a waiting flush that the environment is gone.
struct JsSide<C: ListenerCall> {
	methods: C::Methods,
	_env_alive: watch::Sender<()>,
}

impl<C: ListenerCall> Listener<C> {
	/// Bind `callback`'s methods and build the threadsafe function, on the JS thread. Weak: a
	/// call never keeps Node's event loop alive (its promise does).
	pub(crate) fn new(env: &Env, callback: Object<'_>) -> napi::Result<Self> {
		let (env_alive, env_gone) = watch::channel(());
		let js = JsSide::<C> {
			methods: C::bind(&callback)?,
			_env_alive: env_alive,
		};
		// The callback makes the calls itself, so the function the threadsafe function calls after
		// it does nothing.
		let tsfn = env
			.create_function_from_closure::<(), (), _>("callbackListener", |_| Ok(()))?
			.build_threadsafe_function::<Message<C>>()
			.weak::<true>()
			.build_callback(move |ctx: ThreadsafeCallContext<Message<C>>| {
				js.deliver(&ctx.env, ctx.value);
				Ok(())
			})?;
		Ok(Self::with_send(
			move |message| tsfn.call(message, ThreadsafeFunctionCallMode::NonBlocking),
			env_gone,
		))
	}

	fn with_send(
		send: impl Fn(Message<C>) -> Status + Send + Sync + 'static,
		env_gone: watch::Receiver<()>,
	) -> Self {
		Self(Arc::new(ListenerInner {
			send: Box::new(send),
			closed: AtomicBool::new(false),
			env_gone,
		}))
	}

	/// Queue `call` for the JS thread. Once the environment is gone this does nothing; any other
	/// failure loses only this call, logged.
	pub(crate) fn send(&self, call: C) {
		if !self.0.closed.load(Ordering::Relaxed) {
			self.handed_over(Message::Call(call));
		}
	}

	/// Resolves once the JS thread has made every call sent before it, or at once if the
	/// environment is gone or the marker could not be queued.
	pub(crate) async fn flushed(&self) {
		if self.0.closed.load(Ordering::Relaxed) {
			return;
		}
		let (done, reached) = oneshot::channel();
		if !self.handed_over(Message::Flush(done)) {
			return;
		}
		let mut env_gone = self.0.env_gone.clone();
		tokio::select! {
			// An error is the marker dropped unreached: the environment is gone.
			_ = reached => {}
			_ = env_gone.changed() => {}
		}
	}

	/// Send `message`, telling whether it was queued.
	fn handed_over(&self, message: Message<C>) -> bool {
		match (self.0.send)(message) {
			Status::Ok => true,
			Status::Closing => {
				self.0.closed.store(true, Ordering::Relaxed);
				tracing::debug!("the JS environment is gone; a callback listener stops");
				false
			}
			status => {
				tracing::warn!("a callback call was not handed to JS: {status}");
				false
			}
		}
	}
}

impl<C: ListenerCall> JsSide<C> {
	/// Make `message` on the JS thread. Never fails and never unwinds: it runs inside napi's
	/// `extern "C"` callback, where an error would be raised as an uncaught exception and an
	/// unwind would abort the process.
	fn deliver(&self, env: &Env, message: Message<C>) {
		match message {
			Message::Call(call) => {
				match catch_unwind(AssertUnwindSafe(|| call.call(env, &self.methods))) {
					Ok(Ok(())) => {}
					Ok(Err(e)) => tracing::warn!("a callback threw: {e}"),
					Err(_) => tracing::error!("a callback's dispatch panicked; its call was lost"),
				}
			}
			// The flush may have stopped waiting, as its environment went away.
			Message::Flush(done) => {
				let _ = done.send(());
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use std::sync::Mutex;

	use super::*;

	/// A call that only carries its number.
	#[derive(Debug, PartialEq)]
	struct Tick(u32);

	impl ListenerCall for Tick {
		type Methods = ();

		fn bind(_: &Object<'_>) -> napi::Result<()> {
			Ok(())
		}

		fn call(self, _: &Env, (): &()) -> napi::Result<()> {
			Ok(())
		}
	}

	/// What the fake JS thread has queued, in order.
	type Queue = Arc<Mutex<Vec<Message<Tick>>>>;

	/// A listener whose sends answer `answers` in turn (then `Ok`), queueing what they are handed
	/// when they answer `Ok`; and the sender whose drop is the environment going away.
	fn recording(answers: Vec<Status>) -> (Listener<Tick>, Queue, watch::Sender<()>) {
		let queue = Queue::default();
		let answers = Mutex::new(answers.into_iter());
		let record = Arc::clone(&queue);
		let (env_alive, env_gone) = watch::channel(());
		let listener = Listener::with_send(
			move |message| {
				let answer = answers.lock().unwrap().next().unwrap_or(Status::Ok);
				if answer == Status::Ok {
					record.lock().unwrap().push(message);
				}
				answer
			},
			env_gone,
		);
		(listener, queue, env_alive)
	}

	/// The numbers of the queued calls, and how many markers are queued.
	fn ticks(queue: &Queue) -> (Vec<u32>, usize) {
		let mut ticks = Vec::new();
		let mut markers = 0;
		for message in queue.lock().unwrap().iter() {
			match message {
				Message::Call(Tick(n)) => ticks.push(*n),
				Message::Flush(_) => markers += 1,
			}
		}
		(ticks, markers)
	}

	#[test]
	fn calls_are_sent_in_the_order_they_were_made() {
		let (listener, queue, _env_alive) = recording(Vec::new());

		listener.send(Tick(1));
		listener.clone().send(Tick(2));
		listener.send(Tick(3));

		assert_eq!(ticks(&queue), (vec![1, 2, 3], 0));
	}

	#[test]
	fn nothing_is_sent_after_the_environment_closes() {
		let (listener, queue, _env_alive) = recording(vec![Status::Ok, Status::Closing]);

		listener.send(Tick(1));
		listener.send(Tick(2));
		listener.send(Tick(3));

		assert_eq!(ticks(&queue), (vec![1], 0));
	}

	#[test]
	fn a_failed_send_loses_only_its_own_call() {
		let (listener, queue, _env_alive) = recording(vec![Status::GenericFailure]);

		listener.send(Tick(1));
		listener.send(Tick(2));

		assert_eq!(ticks(&queue), (vec![2], 0));
	}

	#[tokio::test]
	async fn a_flush_after_closing_resolves_at_once() {
		let (listener, queue, _env_alive) = recording(vec![Status::Closing]);
		listener.send(Tick(1));

		let mut flushed = std::pin::pin!(listener.flushed());

		assert!(futures::poll!(flushed.as_mut()).is_ready());
		assert_eq!(ticks(&queue), (Vec::new(), 0));
	}

	#[tokio::test]
	async fn a_flush_waits_for_every_earlier_call() {
		let (listener, queue, _env_alive) = recording(Vec::new());
		listener.send(Tick(1));
		listener.send(Tick(2));

		let mut flushed = std::pin::pin!(listener.flushed());
		assert!(futures::poll!(flushed.as_mut()).is_pending());
		assert_eq!(ticks(&queue), (vec![1, 2], 1));

		// The JS thread makes the calls in order, then reaches the marker.
		let mut made = Vec::new();
		for message in queue.lock().unwrap().drain(..) {
			match message {
				Message::Call(Tick(n)) => made.push(n),
				Message::Flush(done) => {
					assert_eq!(made, vec![1, 2]);
					done.send(()).unwrap();
				}
			}
		}

		assert!(futures::poll!(flushed.as_mut()).is_ready());
	}

	#[tokio::test]
	async fn a_flush_resolves_once_the_environment_goes_away_with_its_marker_queued() {
		let (listener, queue, env_alive) = recording(Vec::new());

		let mut flushed = std::pin::pin!(listener.flushed());
		assert!(futures::poll!(flushed.as_mut()).is_pending());
		// napi drops neither the queued marker nor its sender as the environment goes away; only
		// the threadsafe function's callback, which holds this.
		drop(env_alive);

		assert!(futures::poll!(flushed.as_mut()).is_ready());
		assert_eq!(ticks(&queue), (Vec::new(), 1));
	}
}

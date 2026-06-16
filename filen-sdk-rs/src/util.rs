use std::sync::{Arc, atomic::Ordering};

pub struct PathIterator<'a> {
	path: &'a str,
	split: std::str::MatchIndices<'a, char>,
	last_idx: usize,
}

impl<'a> PathIterator<'a> {
	fn new(path: &'a str) -> Self {
		let path = path.trim_start_matches('/');
		Self {
			path,
			split: path.match_indices('/'),
			last_idx: 0,
		}
	}
}

impl<'a> Iterator for PathIterator<'a> {
	type Item = (&'a str, &'a str);

	fn next(&mut self) -> Option<Self::Item> {
		match self.split.next() {
			None if self.last_idx == self.path.len() => None,
			None => {
				let slice = &self.path[self.last_idx..];
				self.last_idx = self.path.len();
				Some((slice, ""))
			}
			Some((idx, _)) => {
				let slice = &self.path[self.last_idx..idx];
				let rest = &self.path[idx + 1..];
				self.last_idx = idx + 1;
				Some((slice, rest))
			}
		}
	}
}

pub trait PathIteratorExt {
	fn path_iter(&self) -> PathIterator<'_>;
}

impl PathIteratorExt for str {
	fn path_iter(&self) -> PathIterator<'_> {
		PathIterator::new(self)
	}
}

pub async fn sleep(until: std::time::Duration) {
	#[cfg(not(target_family = "wasm"))]
	{
		tokio::time::sleep(until).await;
	}
	#[cfg(target_family = "wasm")]
	{
		wasmtimer::tokio::sleep(until).await;
	}
}

#[cfg(not(target_family = "wasm"))]
pub type MaybeSendBoxFuture<'a, T> = futures::future::BoxFuture<'a, T>;
#[cfg(target_family = "wasm")]
pub type MaybeSendBoxFuture<'a, T> = futures::future::LocalBoxFuture<'a, T>;

#[cfg(not(target_family = "wasm"))]
pub trait MaybeSendSync: Send + Sync {}
#[cfg(target_family = "wasm")]
pub trait MaybeSendSync {}

#[cfg(not(target_family = "wasm"))]
impl<T: Send + Sync> MaybeSendSync for T {}
#[cfg(target_family = "wasm")]
impl<T> MaybeSendSync for T {}

#[cfg(not(target_family = "wasm"))]
pub trait MaybeSend: Send {}
#[cfg(target_family = "wasm")]
pub trait MaybeSend {}

#[cfg(not(target_family = "wasm"))]
impl<T: Send> MaybeSend for T {}
#[cfg(target_family = "wasm")]
impl<T> MaybeSend for T {}

#[cfg(not(target_family = "wasm"))]
pub type MaybeSendCallback<'a, T> = std::sync::Arc<dyn Fn(T) + Send + Sync + 'a>;
#[cfg(target_family = "wasm")]
pub type MaybeSendCallback<'a, T> = std::rc::Rc<dyn Fn(T) + 'a>;

#[cfg(not(target_family = "wasm"))]
pub type MaybeArc<T> = std::sync::Arc<T>;
#[cfg(target_family = "wasm")]
pub type MaybeArc<T> = std::rc::Rc<T>;

#[cfg(not(target_family = "wasm"))]
pub type MaybeArcWeak<T> = std::sync::Weak<T>;
#[cfg(target_family = "wasm")]
pub type MaybeArcWeak<T> = std::rc::Weak<T>;

pub(crate) trait WasmResultExt<T> {
	fn unwrap_or_throw(self) -> T;
	fn expect_or_throw(self, msg: &str) -> T;
}

impl<T, E: std::fmt::Debug> WasmResultExt<T> for Result<T, E> {
	fn unwrap_or_throw(self) -> T {
		#[cfg(all(target_family = "wasm", target_os = "unknown"))]
		{
			use wasm_bindgen::UnwrapThrowExt;
			self.unwrap_throw()
		}
		#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
		{
			self.unwrap()
		}
	}
	fn expect_or_throw(self, msg: &str) -> T {
		#[cfg(all(target_family = "wasm", target_os = "unknown"))]
		{
			use wasm_bindgen::UnwrapThrowExt;
			self.expect_throw(msg)
		}
		#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
		{
			self.expect(msg)
		}
	}
}

impl<T> WasmResultExt<T> for Option<T> {
	fn unwrap_or_throw(self) -> T {
		#[cfg(all(target_family = "wasm", target_os = "unknown"))]
		{
			use wasm_bindgen::UnwrapThrowExt;
			self.unwrap_throw()
		}
		#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
		{
			self.unwrap()
		}
	}
	fn expect_or_throw(self, msg: &str) -> T {
		#[cfg(all(target_family = "wasm", target_os = "unknown"))]
		{
			use wasm_bindgen::UnwrapThrowExt;
			self.expect_throw(msg)
		}
		#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
		{
			self.expect(msg)
		}
	}
}

type DateTime = chrono::DateTime<chrono::Utc>;
#[cfg(feature = "uniffi")]
uniffi::custom_type!(DateTime, i64, {
	remote,
	lower: |dt: &DateTime| dt.timestamp_millis(),
	try_lift: |millis: i64| {
		chrono::DateTime::<chrono::Utc>::from_timestamp_millis(millis).ok_or_else(|| uniffi::deps::anyhow::anyhow!("invalid timestamp millis: {}", millis))
	},
});

#[cfg(feature = "multi-threaded-crypto")]
pub(crate) trait IntoMaybeParallelIterator: rayon::iter::IntoParallelIterator {
	fn into_maybe_par_iter(self) -> Self::Iter
	where
		Self: Sized,
	{
		Self::into_par_iter(self)
	}
}
#[cfg(feature = "multi-threaded-crypto")]
impl<T> IntoMaybeParallelIterator for T where T: rayon::iter::IntoParallelIterator {}

#[cfg(not(feature = "multi-threaded-crypto"))]
pub(crate) trait IntoMaybeParallelIterator: IntoIterator {
	fn into_maybe_par_iter(self) -> Self::IntoIter
	where
		Self: Sized,
	{
		Self::into_iter(self)
	}
}
#[cfg(not(feature = "multi-threaded-crypto"))]
impl<T> IntoMaybeParallelIterator for T where T: IntoIterator {}

pub(crate) struct AtomicDropCanceller {
	cancelled: Arc<std::sync::atomic::AtomicBool>,
}

impl AtomicDropCanceller {
	pub fn cancelled(&self) -> &Arc<std::sync::atomic::AtomicBool> {
		&self.cancelled
	}
}

impl Default for AtomicDropCanceller {
	fn default() -> Self {
		Self {
			cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
		}
	}
}

impl Drop for AtomicDropCanceller {
	fn drop(&mut self) {
		self.cancelled.store(true, Ordering::Relaxed);
	}
}

/// An [`UnboundedReceiver`](tokio::sync::mpsc::UnboundedReceiver) with a one-slot PEEK buffer:
/// [`peek`](Self::peek) reads the next message WITHOUT removing it from the logical queue, so a later
/// [`recv`](Self::recv)/[`try_recv`](Self::try_recv) still yields it. Useful when a `select!` arm must
/// react to "a message is waiting" — e.g. to abort a wait — while the message itself has to be CONSUMED
/// elsewhere: an mpsc receiver has neither a peek nor a push-front, so the only way to look without
/// losing is to pull and park. Peeking is IDEMPOTENT while a message is buffered (a second peek returns
/// the buffered one and pulls nothing new), so racing peeks can never clobber or reorder messages — the
/// rest stay queued in the channel, in order. A closed-and-empty channel surfaces as `peek`/`recv` →
/// `None`.
#[cfg(feature = "cache")]
pub(crate) struct PeekableReceiver<T> {
	inner: tokio::sync::mpsc::UnboundedReceiver<T>,
	buffered: Option<T>,
}

#[cfg(feature = "cache")]
impl<T> PeekableReceiver<T> {
	pub fn new(inner: tokio::sync::mpsc::UnboundedReceiver<T>) -> Self {
		Self {
			inner,
			buffered: None,
		}
	}

	/// Await the next message and park it WITHOUT consuming it; a following `recv`/`try_recv` returns it.
	/// Idempotent while a message is already buffered (returns the buffered one, pulls nothing new).
	/// Resolves to `None` once the channel is closed and empty.
	pub async fn peek(&mut self) -> Option<&T> {
		if self.buffered.is_none() {
			self.buffered = self.inner.recv().await;
		}
		self.buffered.as_ref()
	}

	/// Await the next message, draining the peek buffer first. `None` once the channel is closed and empty.
	pub async fn recv(&mut self) -> Option<T> {
		match self.buffered.take() {
			Some(msg) => Some(msg),
			None => self.inner.recv().await,
		}
	}

	/// Non-blocking take, draining the peek buffer first. `None` if nothing is buffered or queued.
	pub fn try_recv(&mut self) -> Option<T> {
		match self.buffered.take() {
			Some(msg) => Some(msg),
			None => self.inner.try_recv().ok(),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn path_iterator() {
		assert_eq!(
			"root/dir/file.txt".path_iter().collect::<Vec<_>>(),
			vec![
				("root", "dir/file.txt"),
				("dir", "file.txt"),
				("file.txt", "")
			]
		);
		assert_eq!(
			"root/dir/".path_iter().collect::<Vec<_>>(),
			vec![("root", "dir/"), ("dir", "")]
		);
		assert_eq!(
			"/root/dir/".path_iter().collect::<Vec<_>>(),
			vec![("root", "dir/"), ("dir", "")]
		);
		assert_eq!("root".path_iter().collect::<Vec<_>>(), vec![("root", "")]);
		assert_eq!("/".path_iter().collect::<Vec<_>>(), vec![]);
		assert_eq!("".path_iter().collect::<Vec<_>>(), vec![]);
	}
}

#[cfg(all(test, feature = "cache"))]
mod peekable_receiver_tests {
	use super::PeekableReceiver;

	#[tokio::test]
	async fn recv_drains_in_fifo_order() {
		let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
		let mut rx = PeekableReceiver::new(rx);
		tx.send(1).unwrap();
		tx.send(2).unwrap();
		drop(tx);
		assert_eq!(rx.recv().await, Some(1));
		assert_eq!(rx.recv().await, Some(2));
		assert_eq!(rx.recv().await, None, "closed + empty -> None");
	}

	/// Peeking is idempotent while a message is buffered: a second peek returns the buffered message and
	/// pulls nothing new, so two racing peeks never clobber or reorder the queue.
	#[tokio::test]
	async fn peek_is_idempotent_and_preserves_order() {
		let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
		let mut rx = PeekableReceiver::new(rx);
		tx.send(1).unwrap();
		tx.send(2).unwrap();
		assert_eq!(rx.peek().await, Some(&1));
		// Peeking again must NOT pull message 2 — it returns the already-buffered message 1.
		assert_eq!(rx.peek().await, Some(&1));
		// The buffered message is delivered first, then message 2 (never consumed by the peeks).
		assert_eq!(rx.recv().await, Some(1));
		assert_eq!(rx.recv().await, Some(2));
	}

	#[tokio::test]
	async fn try_recv_drains_buffer_then_channel() {
		let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
		let mut rx = PeekableReceiver::new(rx);
		tx.send(1).unwrap();
		tx.send(2).unwrap();
		// Buffer message 1 via peek; try_recv must return it before pulling message 2.
		assert_eq!(rx.peek().await, Some(&1));
		assert_eq!(rx.try_recv(), Some(1));
		assert_eq!(rx.try_recv(), Some(2));
		assert_eq!(rx.try_recv(), None, "drained -> None");
	}

	/// A closed channel surfaces as `peek`/`recv` -> `None` (the signal `CacheState::run` treats as the
	/// synthetic shutdown), with no sentinel value to construct.
	#[tokio::test]
	async fn closed_channel_yields_none() {
		let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<i32>();
		let mut rx = PeekableReceiver::new(rx);
		drop(tx);
		assert_eq!(rx.peek().await, None);
		assert_eq!(rx.recv().await, None);
	}
}

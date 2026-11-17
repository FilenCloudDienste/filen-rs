//! What archive readers keep between their calls (an archive's parsed index, the archive's chunks
//! read last), charged to one budget for the whole process, apart from the client's file-IO
//! budget: that one sizes transfers, which a reader left open must not starve.
//!
//! The budget is shared by every reader of every client. A reader that would pass it takes room
//! from the others, the one used least recently first: its chunks, then its index. A reader that
//! loses either keeps working, at the cost of reading them again when it needs them.

#![warn(unreachable_pub, unused_qualifications)]

use std::{
	sync::{
		Arc, LazyLock, Mutex, MutexGuard, PoisonError, Weak,
		atomic::{AtomicBool, AtomicU64, Ordering},
	},
	time::Duration,
};

use crate::{runtime::spawn_task_maybe_send, util::SeededMap, util::sleep};

use super::{
	extract::codec::HeldIndex,
	reader::{Resolved, Seeks, StreamKey},
};

/// Most heap one reader's index may take and still be held: a larger one is read again by every
/// job that needs it.
pub(crate) const MAX_HELD_INDEX_BYTES: u64 = 32 << 20;

/// Chunks of its archive a reader keeps: the two read last.
pub(crate) const READER_CHUNKS: usize = 2;

/// Streams a reader keeps decoder state for: the two read by range last.
pub(crate) const READER_SEEKS: usize = 2;

/// How long a reader keeps its chunks once nothing used it.
pub(crate) const CHUNKS_IDLE_DROP: Duration = Duration::from_secs(30);

/// The pool of the whole process.
static PROCESS_POOL: LazyLock<Arc<RetainedPool>> = LazyLock::new(Arc::default);

/// The pool every reader of the process draws on.
pub(crate) fn process_pool() -> Arc<RetainedPool> {
	Arc::clone(&PROCESS_POOL)
}

/// What one entry's access kept takes, a map's room to grow included.
const ACCESS_BYTES: u64 = 2 * size_of::<(u64, Resolved)>() as u64;

/// Every reader's retained state, and what it is charged in all.
#[derive(Default)]
pub(crate) struct RetainedPool {
	/// Bytes charged in all. Changed by a charge only while `holders` is locked; given back
	/// without the lock, by a reader that drops what it kept.
	charged: AtomicU64,
	/// A clock that ticks on every use of a reader, which orders them by when they were used.
	clock: AtomicU64,
	/// Every reader that may hold something.
	holders: Mutex<Vec<Weak<Retained>>>,
}

impl RetainedPool {
	/// Charges `bytes` for `requester`, whose client allows `budget` in all: room is taken from
	/// the other readers, least recently used first, when the budget has too little. Whether the
	/// bytes were charged; `false` leaves the pool as it was, but for what was taken.
	fn charge(&self, requester: &Retained, budget: u64, bytes: u64) -> bool {
		let mut holders = lock(&self.holders);
		holders.retain(|holder| holder.strong_count() > 0);
		let fits = |charged: u64| {
			charged
				.checked_add(bytes)
				.is_some_and(|total| total <= budget)
		};
		// what could never fit takes nothing from the others
		if bytes > budget {
			return false;
		}
		if !fits(self.charged.load(Ordering::Acquire)) {
			let mut others: Vec<Arc<Retained>> = holders
				.iter()
				.filter_map(Weak::upgrade)
				.filter(|holder| !std::ptr::eq(&**holder, requester))
				.collect();
			others.sort_by_key(|holder| holder.used.load(Ordering::Relaxed));
			// a tier of every reader before the next tier of any: an index read again costs a
			// job, the others only a fetch or a decode
			'take: for tier in [Tier::Checkpoints, Tier::Chunks, Tier::Index] {
				for other in &others {
					let freed = other.evict(tier);
					self.charged.fetch_sub(freed, Ordering::AcqRel);
					if fits(self.charged.load(Ordering::Acquire)) {
						break 'take;
					}
				}
			}
		}
		if !fits(self.charged.load(Ordering::Acquire)) {
			return false;
		}
		self.charged.fetch_add(bytes, Ordering::AcqRel);
		true
	}

	fn release(&self, bytes: u64) {
		if bytes > 0 {
			self.charged.fetch_sub(bytes, Ordering::AcqRel);
		}
	}

	fn register(&self, holder: Weak<Retained>) {
		let mut holders = lock(&self.holders);
		holders.retain(|holder| holder.strong_count() > 0);
		holders.push(holder);
	}
}

/// A mutex whose holder never leaves its data half-changed: a panic elsewhere does not poison it
/// for the readers that share the pool.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
	mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a reader keeps, in the order it is given up when room is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tier {
	/// Decoder cursors and their checkpoints.
	Checkpoints,
	Chunks,
	Index,
}

/// What one reader keeps, charged to the process-wide budget.
pub(crate) struct Retained {
	pool: Arc<RetainedPool>,
	/// The budget of the reader's client.
	budget: u64,
	/// When it was used last, on the pool's clock.
	used: AtomicU64,
	/// Keeps nothing more once set; read without the state's lock, which a JS thread never
	/// waits on.
	closed: AtomicBool,
	/// Whether `state` holds an index, read without its lock.
	index_held: AtomicBool,
	state: Mutex<State>,
}

#[derive(Default)]
struct State {
	index: Option<Charged<Arc<HeldIndex>>>,
	/// How each entry resolved so far is read, which goes with the index.
	accesses: SeededMap<u64, Resolved>,
	/// What `accesses` is charged.
	access_bytes: u64,
	/// Most recently used first.
	chunks: Vec<(u64, Charged<Arc<Vec<u8>>>)>,
	/// The decoder state of the streams read by range last, most recently used first.
	seeks: Vec<(StreamKey, Charged<Box<Seeks>>)>,
	/// A task drops the chunks once the reader was idle for [`CHUNKS_IDLE_DROP`].
	sweeping: bool,
}

/// Something kept, and the bytes it was charged.
struct Charged<T> {
	value: T,
	bytes: u64,
}

impl State {
	fn bytes(&self) -> u64 {
		self.index.as_ref().map_or(0, |index| index.bytes)
			+ self.access_bytes
			+ self
				.chunks
				.iter()
				.map(|(_, chunk)| chunk.bytes)
				.sum::<u64>()
			+ self.seeks.iter().map(|(_, seeks)| seeks.bytes).sum::<u64>()
	}
}

impl Retained {
	/// What a reader keeps, drawing on `pool` for a client allowing `budget` bytes in all.
	pub(crate) fn new(pool: Arc<RetainedPool>, budget: u64) -> Arc<Self> {
		let retained = Arc::new(Self {
			used: AtomicU64::new(pool.clock.fetch_add(1, Ordering::Relaxed)),
			pool,
			budget,
			closed: AtomicBool::new(false),
			index_held: AtomicBool::new(false),
			state: Mutex::new(State::default()),
		});
		retained.pool.register(Arc::downgrade(&retained));
		retained
	}

	/// Marks the reader used now.
	fn touch(&self) {
		self.used.store(
			self.pool.clock.fetch_add(1, Ordering::Relaxed),
			Ordering::Relaxed,
		);
	}

	/// The index it holds.
	pub(crate) fn index(&self) -> Option<Arc<HeldIndex>> {
		self.touch();
		lock(&self.state)
			.index
			.as_ref()
			.map(|index| Arc::clone(&index.value))
	}

	/// Holds `index` for later jobs, when it takes at most [`MAX_HELD_INDEX_BYTES`] and room can
	/// be made for it; whether it is held.
	pub(crate) fn hold_index(&self, index: Arc<HeldIndex>) -> bool {
		self.touch();
		let bytes = index.heap_bytes();
		if bytes > MAX_HELD_INDEX_BYTES || self.is_closed() {
			return false;
		}
		let replaced = lock(&self.state).index.take();
		self.pool
			.release(replaced.map_or(0, |replaced| replaced.bytes));
		if !self.pool.charge(self, self.budget, bytes) {
			return false;
		}
		let mut state = lock(&self.state);
		if self.is_closed() {
			drop(state);
			self.pool.release(bytes);
			return false;
		}
		let replaced = state.index.replace(Charged {
			value: index,
			bytes,
		});
		self.index_held.store(true, Ordering::Release);
		drop(state);
		self.pool
			.release(replaced.map_or(0, |replaced| replaced.bytes));
		true
	}

	/// How entry `ordinal` is read, when it was resolved before.
	pub(crate) fn access(&self, ordinal: u64) -> Option<Resolved> {
		self.touch();
		lock(&self.state).accesses.get(&ordinal).copied()
	}

	/// Keeps how entry `ordinal` is read, unless no room can be made for it.
	pub(crate) fn keep_access(&self, ordinal: u64, resolved: Resolved) {
		self.touch();
		if self.is_closed() || !self.pool.charge(self, self.budget, ACCESS_BYTES) {
			return;
		}
		let mut state = lock(&self.state);
		let kept = !self.is_closed() && state.accesses.insert(ordinal, resolved).is_none();
		if kept {
			state.access_bytes += ACCESS_BYTES;
		}
		drop(state);
		if !kept {
			self.pool.release(ACCESS_BYTES);
		}
	}

	/// Takes what is kept of stream `key`'s decoding, for a read that gives it back once done.
	pub(crate) fn take_seeks(&self, key: StreamKey) -> Option<Seeks> {
		self.touch();
		let mut state = lock(&self.state);
		let at = state.seeks.iter().position(|(kept, _)| *kept == key)?;
		let (_, seeks) = state.seeks.remove(at);
		drop(state);
		self.pool.release(seeks.bytes);
		Some(*seeks.value)
	}

	/// Keeps stream `key`'s decoding as the one used last, giving up the one used longest ago
	/// when [`READER_SEEKS`] are kept, unless no room can be made for it.
	pub(crate) fn keep_seeks(&self, key: StreamKey, seeks: Seeks) {
		self.touch();
		let bytes = seeks.bytes();
		let oldest = {
			let mut state = lock(&self.state);
			if self.is_closed() {
				return;
			}
			(state.seeks.len() >= READER_SEEKS).then(|| state.seeks.pop())
		};
		self.pool
			.release(oldest.flatten().map_or(0, |(_, oldest)| oldest.bytes));
		if !self.pool.charge(self, self.budget, bytes) {
			return;
		}
		let mut state = lock(&self.state);
		if self.is_closed() {
			drop(state);
			self.pool.release(bytes);
			return;
		}
		state.seeks.insert(
			0,
			(
				key,
				Charged {
					value: Box::new(seeks),
					bytes,
				},
			),
		);
		let keep = READER_SEEKS.min(state.seeks.len());
		let over: Vec<_> = state.seeks.drain(keep..).collect();
		drop(state);
		self.pool
			.release(over.iter().map(|(_, seeks)| seeks.bytes).sum());
	}

	/// Charges `bytes` a read holds while it is in flight, as anything kept is charged; whether
	/// there was room, taken from other readers if need be.
	pub(crate) fn reserve(&self, bytes: u64) -> bool {
		self.pool.charge(self, self.budget, bytes)
	}

	/// Gives back `bytes` [`Self::reserve`] charged.
	pub(crate) fn release_reserved(&self, bytes: u64) {
		self.pool.release(bytes);
	}

	/// Chunk `index` of the archive, when it is kept.
	pub(crate) fn chunk(&self, index: u64) -> Option<Arc<Vec<u8>>> {
		self.touch();
		let mut state = lock(&self.state);
		let at = state.chunks.iter().position(|(kept, _)| *kept == index)?;
		state.chunks[..=at].rotate_right(1);
		Some(Arc::clone(&state.chunks[0].1.value))
	}

	/// Keeps `data`, chunk `index` of the archive, as the one read last, giving up the one read
	/// longest ago when [`READER_CHUNKS`] are kept already, unless no room can be made for it.
	pub(crate) fn keep_chunk(self: &Arc<Self>, index: u64, data: Arc<Vec<u8>>) {
		self.touch();
		let bytes = data.capacity() as u64;
		let oldest = {
			let mut state = lock(&self.state);
			if self.is_closed() || state.chunks.iter().any(|(kept, _)| *kept == index) {
				return;
			}
			(state.chunks.len() >= READER_CHUNKS).then(|| state.chunks.pop())
		};
		self.pool
			.release(oldest.flatten().map_or(0, |(_, oldest)| oldest.bytes));
		if !self.pool.charge(self, self.budget, bytes) {
			return;
		}
		let mut state = lock(&self.state);
		if self.is_closed() {
			drop(state);
			self.pool.release(bytes);
			return;
		}
		state
			.chunks
			.insert(0, (index, Charged { value: data, bytes }));
		// another call may have kept one meanwhile
		let keep = READER_CHUNKS.min(state.chunks.len());
		let over: Vec<_> = state.chunks.drain(keep..).collect();
		let sweep = !std::mem::replace(&mut state.sweeping, true);
		drop(state);
		self.pool
			.release(over.iter().map(|(_, chunk)| chunk.bytes).sum());
		if sweep {
			self.sweep_when_idle();
		}
	}

	/// Drops the chunks once nothing used the reader for [`CHUNKS_IDLE_DROP`].
	fn sweep_when_idle(self: &Arc<Self>) {
		let retained = Arc::downgrade(self);
		drop(spawn_task_maybe_send(async move {
			loop {
				let Some(seen) = retained
					.upgrade()
					.map(|retained| retained.used.load(Ordering::Relaxed))
				else {
					return;
				};
				sleep(CHUNKS_IDLE_DROP).await;
				let Some(retained) = retained.upgrade() else {
					return;
				};
				if retained.drop_chunks_if_unused_since(seen) {
					return;
				}
			}
		}));
	}

	/// Drops the chunks when the reader was not used since `seen`; whether it was not.
	fn drop_chunks_if_unused_since(&self, seen: u64) -> bool {
		let mut state = lock(&self.state);
		if self.used.load(Ordering::Relaxed) != seen {
			return false;
		}
		let chunks = std::mem::take(&mut state.chunks);
		state.sweeping = false;
		drop(state);
		self.pool
			.release(chunks.iter().map(|(_, chunk)| chunk.bytes).sum());
		true
	}

	/// Gives up everything of `tier`, for the pool, which holds its lock; the bytes freed.
	fn evict(&self, tier: Tier) -> u64 {
		let mut state = lock(&self.state);
		match tier {
			Tier::Checkpoints => std::mem::take(&mut state.seeks)
				.iter()
				.map(|(_, seeks)| seeks.bytes)
				.sum(),
			Tier::Chunks => std::mem::take(&mut state.chunks)
				.iter()
				.map(|(_, chunk)| chunk.bytes)
				.sum(),
			Tier::Index => {
				self.index_held.store(false, Ordering::Release);
				state.accesses = SeededMap::default();
				std::mem::take(&mut state.access_bytes)
					+ state.index.take().map_or(0, |index| index.bytes)
			}
		}
	}

	/// Whether the reader closed.
	fn is_closed(&self) -> bool {
		self.closed.load(Ordering::Acquire)
	}

	/// Whether it holds an index, without waiting on anything.
	pub(crate) fn holds_index(&self) -> bool {
		self.index_held.load(Ordering::Acquire)
	}

	/// Gives up everything, and keeps nothing more: the reader closed. On the web it never waits
	/// on the state's lock, which a JS thread must not: when a call holds it, a task on that
	/// thread gives everything up once the call let go. Elsewhere it waits the moment a call
	/// holds the lock, from whatever thread it is called on, runtime or not.
	pub(crate) fn close(self: &Arc<Self>) {
		if self.closed.swap(true, Ordering::AcqRel) {
			return;
		}
		self.index_held.store(false, Ordering::Release);
		#[cfg(all(target_family = "wasm", target_os = "unknown"))]
		if !self.release_all(false) {
			let retained = Arc::clone(self);
			drop(spawn_task_maybe_send(async move {
				while !retained.release_all(false) {
					sleep(Duration::from_millis(1)).await;
				}
			}));
		}
		#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
		self.release_all(true);
	}

	/// Gives up everything it holds, waiting for the state's lock when `wait`, else only when it
	/// is free; whether it did.
	fn release_all(&self, wait: bool) -> bool {
		let mut state = if wait {
			lock(&self.state)
		} else {
			match self.state.try_lock() {
				Ok(state) => state,
				Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
				Err(std::sync::TryLockError::WouldBlock) => return false,
			}
		};
		// a hold that raced the close may have set it again
		self.index_held.store(false, Ordering::Release);
		let freed = state.bytes();
		let kept = std::mem::take(&mut *state);
		drop(state);
		drop(kept);
		self.pool.release(freed);
		true
	}
}

impl Drop for Retained {
	fn drop(&mut self) {
		let state = self.state.get_mut().unwrap_or_else(PoisonError::into_inner);
		let bytes = state.bytes();
		self.pool.release(bytes);
	}
}

#[cfg(test)]
pub(crate) mod test_support {
	use std::sync::atomic::Ordering;

	use super::{Retained, RetainedPool, Tier};

	impl RetainedPool {
		/// Bytes charged in all.
		pub(crate) fn charged_for_test(&self) -> u64 {
			self.charged.load(Ordering::Acquire)
		}
	}

	impl Retained {
		/// What it holds kept, as its state charges it.
		pub(crate) fn held_for_test(&self) -> u64 {
			super::lock(&self.state).bytes()
		}

		/// Gives up the index, as when another reader needed the room.
		pub(crate) fn evict_index_for_test(&self) {
			let freed = self.evict(Tier::Index);
			self.pool.release(freed);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::fs::archive::zip::read::{ZipEntry, ZipIndex};

	fn chunk(bytes: usize) -> Arc<Vec<u8>> {
		Arc::new(Vec::with_capacity(bytes))
	}

	fn kept(retained: &Retained) -> Vec<u64> {
		lock(&retained.state)
			.chunks
			.iter()
			.map(|(index, _)| *index)
			.collect()
	}

	fn holds_index(retained: &Retained) -> bool {
		lock(&retained.state).index.is_some()
	}

	/// An index of `entries` entries' room.
	fn index(entries: usize) -> Arc<HeldIndex> {
		Arc::new(HeldIndex::Zip(ZipIndex {
			entries: Vec::with_capacity(entries),
			overlapping: Vec::new(),
			duplicate_names: Vec::new(),
			duplicate_count: 0,
			prefix_bytes: 0,
			shift: 0,
			directory_slack: 0,
			trailing_bytes: 0,
		}))
	}

	#[tokio::test]
	async fn a_reader_over_the_budget_takes_room_from_the_one_used_least_recently() {
		let pool = Arc::new(RetainedPool::default());
		let (a, b, c) = (
			Retained::new(Arc::clone(&pool), 5000),
			Retained::new(Arc::clone(&pool), 5000),
			Retained::new(Arc::clone(&pool), 5000),
		);
		a.keep_chunk(0, chunk(1000));
		a.keep_chunk(1, chunk(1000));
		b.keep_chunk(0, chunk(1000));
		b.keep_chunk(1, chunk(1000));
		c.keep_chunk(0, chunk(1000));
		assert_eq!(pool.charged.load(Ordering::Acquire), 5000);
		// a was used longest ago, so its chunks go
		c.keep_chunk(1, chunk(1000));
		assert_eq!(kept(&a), Vec::<u64>::new());
		assert_eq!(kept(&b), [1, 0]);
		assert_eq!(kept(&c), [1, 0]);
		assert_eq!(pool.charged.load(Ordering::Acquire), 4000);
		// b used now: a later request takes from c
		assert!(b.chunk(0).is_some());
		a.keep_chunk(5, chunk(1000));
		a.keep_chunk(6, chunk(1000));
		assert_eq!(kept(&c), Vec::<u64>::new());
		assert_eq!(kept(&b), [0, 1]);
		assert_eq!(kept(&a), [6, 5]);
	}

	#[tokio::test]
	async fn a_readers_chunks_go_before_its_index() {
		let pool = Arc::new(RetainedPool::default());
		let index = index(0);
		let index_bytes = index.heap_bytes();
		let budget = index_bytes + 2000;
		let (a, b) = (
			Retained::new(Arc::clone(&pool), budget),
			Retained::new(Arc::clone(&pool), budget),
		);
		assert!(a.hold_index(Arc::clone(&index)));
		a.keep_chunk(0, chunk(1000));
		a.keep_chunk(1, chunk(1000));
		b.keep_chunk(0, chunk(1000));
		assert_eq!(kept(&a), Vec::<u64>::new());
		assert!(holds_index(&a));
		// and its index when that is not enough
		b.keep_chunk(1, chunk(1000));
		let larger = 1000 + usize::try_from(index_bytes).unwrap();
		b.keep_chunk(2, chunk(larger));
		assert!(!holds_index(&a));
		assert!(a.index().is_none());
		assert_eq!(kept(&b), [2, 1]);
		assert_eq!(pool.charged.load(Ordering::Acquire), 2000 + index_bytes);
	}

	#[tokio::test]
	async fn a_tier_of_every_reader_goes_before_the_next_tier_of_any() {
		let pool = Arc::new(RetainedPool::default());
		let index = index(0);
		let index_bytes = index.heap_bytes();
		let budget = index_bytes + 2000;
		let (a, b, c) = (
			Retained::new(Arc::clone(&pool), budget),
			Retained::new(Arc::clone(&pool), budget),
			Retained::new(Arc::clone(&pool), budget),
		);
		assert!(a.hold_index(index));
		b.keep_chunk(0, chunk(1000));
		// a was used longest ago, but its index costs a job to read again: b's chunk goes first
		c.keep_chunk(0, chunk(1500));
		assert!(holds_index(&a));
		assert!(a.holds_index());
		assert_eq!(kept(&b), Vec::<u64>::new());
		assert_eq!(kept(&c), [0]);
	}

	#[tokio::test]
	async fn closing_waits_out_a_call_holding_the_state_off_any_runtime() {
		let pool = Arc::new(RetainedPool::default());
		let retained = Retained::new(Arc::clone(&pool), 5000);
		retained.keep_chunk(0, chunk(1000));
		// a call on another thread holds the state a moment; this thread has no runtime
		let (held, release) = std::sync::mpsc::channel::<()>();
		let holder = {
			let retained = Arc::clone(&retained);
			let (locked, wait) = std::sync::mpsc::channel();
			let holder = std::thread::spawn(move || {
				let state = lock(&retained.state);
				locked.send(()).unwrap();
				release.recv().unwrap();
				drop(state);
			});
			wait.recv().unwrap();
			holder
		};
		let releaser = std::thread::spawn(move || {
			std::thread::sleep(Duration::from_millis(20));
			held.send(()).unwrap();
		});
		// closed from a thread of no runtime, as a foreign caller's
		let closer = {
			let retained = Arc::clone(&retained);
			std::thread::spawn(move || retained.close())
		};
		closer.join().unwrap();
		holder.join().unwrap();
		releaser.join().unwrap();
		assert!(retained.is_closed());
		assert!(!retained.holds_index());
		assert_eq!(pool.charged.load(Ordering::Acquire), 0);
		retained.keep_chunk(1, chunk(1000));
		assert_eq!(kept(&retained), Vec::<u64>::new());
	}

	#[tokio::test]
	async fn a_readers_decoder_state_goes_before_its_chunks() {
		let pool = Arc::new(RetainedPool::default());
		let seeks = Seeks::new().bytes();
		let budget = seeks + 2000;
		let (a, b) = (
			Retained::new(Arc::clone(&pool), budget),
			Retained::new(Arc::clone(&pool), budget),
		);
		a.keep_seeks(StreamKey::Entry(0), Seeks::new());
		a.keep_chunk(0, chunk(1000));
		a.keep_chunk(1, chunk(1000));
		b.keep_chunk(0, chunk(usize::try_from(seeks).unwrap()));
		assert!(a.take_seeks(StreamKey::Entry(0)).is_none());
		assert_eq!(kept(&a), [1, 0]);
		assert_eq!(pool.charged.load(Ordering::Acquire), budget);
	}

	#[tokio::test]
	async fn what_no_room_can_be_made_for_is_not_kept() {
		let pool = Arc::new(RetainedPool::default());
		let a = Retained::new(Arc::clone(&pool), 1500);
		a.keep_chunk(0, chunk(1000));
		// a reader never takes room from itself
		a.keep_chunk(1, chunk(1000));
		assert_eq!(kept(&a), [0]);
		// nor does what can never fit
		let nothing = Retained::new(Arc::clone(&pool), 0);
		nothing.keep_chunk(0, chunk(1));
		assert!(!nothing.hold_index(index(0)));
		assert_eq!(kept(&nothing), Vec::<u64>::new());
		assert_eq!(pool.charged.load(Ordering::Acquire), 1000);
	}

	#[tokio::test]
	async fn an_index_over_32_mib_is_never_held() {
		let pool = Arc::new(RetainedPool::default());
		let a = Retained::new(Arc::clone(&pool), u64::MAX);
		let entry = size_of::<ZipEntry>();
		let fits = index(usize::try_from(MAX_HELD_INDEX_BYTES).unwrap() / entry - 1);
		assert!(fits.heap_bytes() <= MAX_HELD_INDEX_BYTES);
		assert!(a.hold_index(fits));
		let over = index(usize::try_from(MAX_HELD_INDEX_BYTES).unwrap() / entry + 1);
		assert!(over.heap_bytes() > MAX_HELD_INDEX_BYTES);
		let held = pool.charged.load(Ordering::Acquire);
		assert!(!a.hold_index(over));
		// the one it held stays
		assert!(holds_index(&a));
		assert_eq!(pool.charged.load(Ordering::Acquire), held);
	}

	#[tokio::test(start_paused = true)]
	async fn chunks_are_dropped_once_the_reader_was_idle_for_30_seconds() {
		let pool = Arc::new(RetainedPool::default());
		let a = Retained::new(Arc::clone(&pool), u64::MAX);
		a.keep_chunk(0, chunk(1000));
		tokio::time::sleep(Duration::from_secs(20)).await;
		// a use puts the drop off
		assert!(a.chunk(0).is_some());
		tokio::time::sleep(Duration::from_secs(20)).await;
		assert_eq!(kept(&a), [0]);
		tokio::time::sleep(Duration::from_secs(25)).await;
		assert_eq!(kept(&a), Vec::<u64>::new());
		assert_eq!(pool.charged.load(Ordering::Acquire), 0);
		// and kept again afterwards, to be dropped again
		a.keep_chunk(1, chunk(1000));
		tokio::time::sleep(Duration::from_secs(31)).await;
		assert_eq!(kept(&a), Vec::<u64>::new());
	}

	#[tokio::test]
	async fn closing_or_dropping_a_reader_gives_back_what_it_kept() {
		let pool = Arc::new(RetainedPool::default());
		let a = Retained::new(Arc::clone(&pool), u64::MAX);
		assert!(a.hold_index(index(0)));
		a.keep_chunk(0, chunk(1000));
		a.close();
		assert_eq!(pool.charged.load(Ordering::Acquire), 0);
		a.keep_chunk(1, chunk(1000));
		assert!(!a.hold_index(index(0)));
		assert_eq!(pool.charged.load(Ordering::Acquire), 0);
		let b = Retained::new(Arc::clone(&pool), u64::MAX);
		b.keep_chunk(0, chunk(1000));
		drop(b);
		assert_eq!(pool.charged.load(Ordering::Acquire), 0);
	}
}

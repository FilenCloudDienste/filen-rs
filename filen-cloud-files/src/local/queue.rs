//! The queue of paths to look at: one entry per path in NTFS's case folding, debounced as the sync
//! engine debounces its watcher, retried with its backoff, and parked after repeated failures. An
//! entry carries the kinds of job its path waits for ([`Kinds`]), so the jobs on one path never
//! run at once. Pure bookkeeping on a given clock; the engine runs the jobs.
//!
//! A job never spins: past a few looks in a row that end asking for another one
//! ([`Outcome::Again`]) without the path ever coming to rest or sending bytes it had not sent
//! before ([`Queue::sent`]), each further one counts as a failure,
//! backing off and parking as failures do, and no event, the job's own included, brings the path
//! back before its backoff runs out. Waits for something outside the job ([`Outcome::Wait`]), and a
//! writer holding the file ([`Outcome::Busy`]), are bounded by what they wait for instead, and are
//! reported, and looked at once a minute, once they have lasted ten minutes: what they wait for
//! (the drive, the local side, a start-up step) may never end.
//!
//! The debounce takes the sync engine's window and burst cap ([`WATCH_DEBOUNCE`],
//! [`WATCH_MAX_DEBOUNCE_BURST`]) and its rule, a look once the path has been quiet for the window
//! or the cap is reached, but applies it per path over due times, where the engine waits once for
//! the whole tree.
//!
//! [`WATCH_DEBOUNCE`]: filen_sdk_rs::sync_engine::WATCH_DEBOUNCE
//! [`WATCH_MAX_DEBOUNCE_BURST`]: filen_sdk_rs::sync_engine::WATCH_MAX_DEBOUNCE_BURST

use std::{collections::HashMap, time::Duration};

use filen_sdk_rs::sync_engine::watch_backoff;
use tokio::time::Instant;

use crate::{
	failure::Failure,
	local::report::LocalReport,
	root_path::{CaseKey, RootPath},
};

/// How soon a path whose writer closed it is looked at: the platform's close notification says the
/// write is over, so only a short wait for the watcher's trailing events.
pub(super) const CLOSE_DELAY: Duration = Duration::from_millis(200);
/// How many failures in a row park a path.
pub(super) const PARK_AFTER: u32 = 8;
/// How many looks in a row may end asking for another look before each further one counts as a
/// failure: a job that changed something, or saw it change, comes to rest within a few, and one
/// that keeps coming back to the same step (an upload the drive answers with the version the file
/// had, a mark in sync that keeps failing) backs off and parks instead of spinning.
pub(super) const AGAIN_FREE: u32 = 4;
/// How long a file may stay open for writing before it is reported busy.
const BUSY_REPORT_AFTER: Duration = Duration::from_secs(10 * 60);
/// The shortest wait before a job's path is looked at again, so a job that keeps asking for another
/// look at once cannot spin.
const MIN_AGAIN: Duration = Duration::from_millis(50);
/// The longest wait between two looks at a file open for writing, before it is reported busy.
const BUSY_MAX_DELAY: Duration = Duration::from_secs(30);
/// The wait between two looks at a file reported busy.
const BUSY_SLOW_DELAY: Duration = Duration::from_secs(60);
/// How long a path may wait for something outside its job ([`Outcome::Wait`]) before it is
/// reported: what it waits for has not ended, and the change it holds is not reaching the drive.
pub(super) const WAIT_REPORT_AFTER: Duration = Duration::from_secs(10 * 60);
/// The shortest wait between two looks at a path reported waiting.
pub(super) const WAIT_SLOW_DELAY: Duration = Duration::from_secs(60);

/// A kind of job on a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
	/// What the user changed at the path, carried to the drive.
	Local,
	/// What changed on the drive, applied to the item at the path.
	Remote,
	/// The directory at the path reconciled with the drive's listing of it.
	Children,
	/// The clean file placeholder at the path completed and checked once its reads went quiet,
	/// or brought to what its pin asks for.
	Hydration,
}

impl Kind {
	const fn bit(self) -> u8 {
		match self {
			Self::Local => 1,
			Self::Remote => 2,
			Self::Children => 4,
			Self::Hydration => 8,
		}
	}
}

/// A set of [`Kind`]s: a bitset, `Copy` in every queue entry, its bits private to this module.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Kinds(u8);

impl Kinds {
	/// No kind.
	pub(super) const NONE: Self = Self(0);

	/// The set holding `kind` alone.
	pub(super) const fn of(kind: Kind) -> Self {
		Self(kind.bit())
	}

	/// Whether the set holds `kind`.
	pub(super) fn has(self, kind: Kind) -> bool {
		self.0 & kind.bit() != 0
	}

	/// The set with `kind` added.
	pub(super) fn with(self, kind: Kind) -> Self {
		Self(self.0 | kind.bit())
	}

	/// The set without `kind`.
	pub(super) fn without(self, kind: Kind) -> Self {
		Self(self.0 & !kind.bit())
	}

	/// The kinds in both sets.
	pub(super) fn intersect(self, other: Self) -> Self {
		Self(self.0 & other.0)
	}

	/// Both sets together.
	pub(super) fn union(self, other: Self) -> Self {
		Self(self.0 | other.0)
	}

	/// Whether the set holds no kind.
	pub(super) fn is_empty(self) -> bool {
		self.0 == 0
	}
}

/// When a path should be looked at after an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Delay {
	/// Once the path has been quiet for the debounce window, or the burst cap is reached.
	Debounce,
	/// After [`CLOSE_DELAY`]: its writer closed it.
	Close,
	/// At once.
	Now,
	/// After the wait given, or sooner if the path was due sooner already.
	After(Duration),
}

/// How a job on a path ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Outcome {
	/// Nothing more to do.
	Done,
	/// Look again after the wait given: the item changed under the job, or the job changed it.
	/// Past [`AGAIN_FREE`] in a row, counted as a failure.
	Again(Duration),
	/// Look again after the wait given, as the job waits for something outside it to end (another
	/// job on the path, a read, the start-up's restore or take-over, the remote side): bounded by
	/// what it waits for, never counted as a failure.
	Wait(Duration),
	/// A writer still holds the file.
	Busy,
	/// The item's directory must be taken in first.
	Blocked(RootPath),
	/// The job failed and may pass on a retry.
	Failed(Failure),
	/// The job failed in a way a retry does not mend.
	Park(Failure),
	/// The item is not synced; what sits under it is not either.
	Skipped,
}

/// A look [`Queue::finish`] counts as a failure.
struct Failed {
	failure: Failure,
	kind: FailedKind,
}

/// How a failed look is backed off from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailedKind {
	/// Retried after its backoff, sooner on an event; parked after [`PARK_AFTER`] in a row.
	Retried,
	/// Parked at once: a retry does not mend it.
	Parked,
	/// A look that came to nothing past [`AGAIN_FREE`]: retried as a failure is, but no event
	/// brings it back before its backoff runs out.
	CameToNothing,
}

/// Whether an event was queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Admit {
	/// Queued, or merged into the path's entry.
	Queued,
	/// The queue is full: drop it and walk the root instead.
	Overflow,
}

#[derive(Debug)]
enum State {
	/// Waits for its due time; `busy` while a writer holds the file.
	Waiting {
		busy: Option<Busy>,
	},
	/// Its job runs the kinds `running`; `again` once an event arrived meanwhile, to look again
	/// after it.
	Running {
		again: bool,
		busy: Option<Busy>,
		running: Kinds,
	},
	/// Waits for the job on its directory.
	Blocked(CaseKey),
	Parked(Failure),
}

/// A path whose looks wait for something outside their job.
#[derive(Debug, Clone, Copy)]
struct Waited {
	/// Since when its looks ended so.
	since: Instant,
	/// How many looks ended so.
	looks: u32,
	/// Whether it was reported.
	reported: bool,
}

/// A file a writer holds.
#[derive(Debug, Clone, Copy)]
struct Busy {
	/// Since when a look found it so.
	since: Instant,
	/// How many looks found it so.
	looks: u32,
	/// Whether it was reported.
	reported: bool,
}

#[derive(Debug)]
struct Entry {
	path: RootPath,
	first_seen: Instant,
	due: Instant,
	/// Failures in a row.
	attempts: u32,
	/// Looks in a row that ended [`Outcome::Again`], since the path last came to rest or sent bytes
	/// it had not sent before.
	agains: u32,
	/// Set while the path backs off from looks that came to nothing: no event makes it due before
	/// this, as the job's own writes may raise the events.
	hold: Option<Instant>,
	/// The hash of the bytes the path's last upload sent, since it last came to rest.
	sent: Option<[u8; 32]>,
	/// Set while its looks wait for something outside their job, one after the other.
	waited: Option<Waited>,
	/// The kinds of job the path waits for, beside those running.
	kinds: Kinds,
	/// Whether the path is known to hold a local change the drive lacks: a job on it found bytes to
	/// carry, or ended without carrying its change, since the change was queued. Unset while its
	/// change was only named by an event or a walk, which may have changed nothing.
	looked: bool,
	state: State,
}

/// The queue.
#[derive(Debug)]
pub(super) struct Queue {
	entries: HashMap<CaseKey, Entry>,
	cap: usize,
	debounce: Duration,
	max_burst: u32,
}

impl Queue {
	/// A queue of at most `cap` paths, debouncing over `debounce` for at most `max_burst` windows.
	pub(super) fn new(cap: usize, debounce: Duration, max_burst: u32) -> Self {
		Self {
			entries: HashMap::new(),
			cap,
			debounce,
			max_burst,
		}
	}

	/// Whether the queue holds as many paths as it may.
	fn is_full(&self) -> bool {
		self.entries.len() >= self.cap
	}

	/// Queues a `kind` job on `path` after `delay`, merged into its entry when it has one: a
	/// running job looks again when it ends, a parked path is retried, a blocked one keeps waiting.
	pub(super) fn push_kind(
		&mut self,
		path: &RootPath,
		now: Instant,
		delay: Delay,
		kind: Kind,
	) -> Admit {
		self.push_kinds(path, now, delay, Kinds::of(kind))
	}

	fn push_kinds(&mut self, path: &RootPath, now: Instant, delay: Delay, kinds: Kinds) -> Admit {
		let key = path.case_key();
		let (debounce, max_burst) = (self.debounce, self.max_burst);
		if let Some(entry) = self.entries.get_mut(&key) {
			let due = due_after(debounce, max_burst, entry.first_seen, now, delay);
			entry.path = path.clone();
			if kinds.has(Kind::Local) && !entry.has_local() {
				entry.looked = false;
			}
			entry.kinds = entry.kinds.union(kinds);
			match &mut entry.state {
				State::Running { again, .. } => *again = true,
				State::Blocked(_) => {}
				State::Parked(_) => {
					entry.state = State::Waiting { busy: None };
					entry.due = held(now.max(due), entry.hold);
				}
				State::Waiting { busy } => {
					let due = match delay {
						// A writer still at it raises these as it writes, quiet spells between
						// included: the look stays where the wait for it put it, as each may copy
						// and hash the whole file, and only the writer's close brings it nearer.
						Delay::Debounce if busy.is_some() => entry.due,
						Delay::Debounce => due,
						Delay::Close | Delay::Now | Delay::After(_) => entry.due.min(due),
					};
					entry.due = held(due, entry.hold);
				}
			}
			return Admit::Queued;
		}
		if self.is_full() {
			return Admit::Overflow;
		}
		let due = due_after(self.debounce, self.max_burst, now, now, delay);
		self.entries.insert(
			key,
			Entry {
				path: path.clone(),
				first_seen: now,
				due,
				attempts: 0,
				agains: 0,
				hold: None,
				sent: None,
				waited: None,
				kinds,
				looked: false,
				state: State::Waiting { busy: None },
			},
		);
		Admit::Queued
	}

	/// Up to `limit` paths due at `now`, shallowest first so a directory goes before what it holds,
	/// marked running, each with the kinds of job to run on it. Of the paths waiting for the
	/// hydration job alone, at most `hydrations`: the others wait for a hydration to end, so they
	/// never hold a job slot a user's change could run in.
	pub(super) fn take_due(
		&mut self,
		now: Instant,
		limit: usize,
		mut hydrations: usize,
	) -> Vec<(RootPath, Kinds)> {
		let mut due: Vec<(&CaseKey, &Entry)> = self
			.entries
			.iter()
			.filter(|(_, entry)| matches!(entry.state, State::Waiting { .. }) && entry.due <= now)
			.collect();
		due.sort_by_key(|(_, entry)| (entry.path.depth(), entry.due));
		let keys: Vec<CaseKey> = due
			.into_iter()
			.filter(|(_, entry)| {
				if entry.kinds != Kinds::of(Kind::Hydration) {
					return true;
				}
				let take = hydrations > 0;
				hydrations = hydrations.saturating_sub(1);
				take
			})
			.take(limit)
			.map(|(key, _)| key.clone())
			.collect();
		keys.into_iter()
			.filter_map(|key| {
				let entry = self.entries.get_mut(&key)?;
				let State::Waiting { busy } = entry.state else {
					return None;
				};
				let running = std::mem::take(&mut entry.kinds);
				entry.state = State::Running {
					again: false,
					busy,
					running,
				};
				Some((entry.path.clone(), running))
			})
			.collect()
	}

	/// When the next waiting path is due, leaving out those waiting for the hydration job alone
	/// unless `hydrations` may start.
	pub(super) fn next_due(&self, hydrations: bool) -> Option<Instant> {
		self.entries
			.values()
			.filter(|entry| {
				matches!(entry.state, State::Waiting { .. })
					&& (hydrations || entry.kinds != Kinds::of(Kind::Hydration))
			})
			.map(|entry| entry.due)
			.min()
	}

	/// Records how the job on `path` ended at `now`, and returns what to report. `owed` are the
	/// kinds the job did not finish, which wait again unless it ended [`Outcome::Done`] or
	/// [`Outcome::Skipped`]. `settle` is the first wait on a file open for writing. A blocked job's
	/// directory is not queued here: the caller pushes it.
	pub(super) fn finish(
		&mut self,
		path: &RootPath,
		outcome: &Outcome,
		owed: Kinds,
		now: Instant,
		settle: Duration,
	) -> Vec<LocalReport> {
		let key = path.case_key();
		let mut reports = Vec::new();
		let Some(entry) = self.entries.get_mut(&key) else {
			return reports;
		};
		let State::Running {
			again,
			busy,
			running,
		} = entry.state
		else {
			return reports;
		};
		let waiting = |busy: Option<Busy>| State::Waiting { busy };
		let mut parked = None;
		if !matches!(outcome, Outcome::Done | Outcome::Skipped) {
			entry.kinds = entry.kinds.union(owed);
			// Its local change was looked at, or held up by the job's other kinds, and is not on the
			// drive yet.
			entry.looked |= running.has(Kind::Local);
		}
		let waited = entry.waited.take();
		let failed = match outcome {
			Outcome::Done | Outcome::Skipped => {
				if again && !entry.kinds.is_empty() {
					entry.state = waiting(None);
					entry.first_seen = now;
					entry.due = now + self.debounce;
					entry.attempts = 0;
					entry.agains = 0;
					entry.hold = None;
					// Named again while the job ran: nothing says yet that it changed.
					entry.looked = false;
				} else {
					self.entries.remove(&key);
				}
				self.release_blocked(&key, now, *outcome == Outcome::Skipped);
				return reports;
			}
			Outcome::Again(wait) => {
				entry.agains = entry.agains.saturating_add(1);
				if entry.agains > AGAIN_FREE {
					Some(Failed {
						failure: Failure::Transient,
						kind: FailedKind::CameToNothing,
					})
				} else {
					entry.state = waiting(busy);
					entry.due = held(now + (*wait).max(MIN_AGAIN), entry.hold);
					None
				}
			}
			Outcome::Wait(wait) => {
				let mut waited = waited.unwrap_or(Waited {
					since: now,
					looks: 0,
					reported: false,
				});
				waited.looks = waited.looks.saturating_add(1);
				let mut wait = (*wait).max(MIN_AGAIN);
				if now.saturating_duration_since(waited.since) >= WAIT_REPORT_AFTER {
					if !waited.reported {
						waited.reported = true;
						reports.push(LocalReport::Parked {
							path: entry.path.clone(),
							failure: Failure::Transient,
							attempts: waited.looks,
						});
					}
					wait = wait.max(WAIT_SLOW_DELAY);
				}
				entry.waited = Some(waited);
				entry.state = waiting(busy);
				entry.due = held(now + wait, entry.hold);
				None
			}
			Outcome::Busy => {
				let mut busy = busy.unwrap_or(Busy {
					since: now,
					looks: 0,
					reported: false,
				});
				busy.looks += 1;
				let wait = if now.saturating_duration_since(busy.since) >= BUSY_REPORT_AFTER {
					if !busy.reported {
						busy.reported = true;
						reports.push(LocalReport::Busy {
							path: entry.path.clone(),
						});
					}
					BUSY_SLOW_DELAY
				} else {
					let doublings = (busy.looks - 1).min(16);
					(settle * 2u32.pow(doublings)).min(BUSY_MAX_DELAY)
				};
				entry.state = waiting(Some(busy));
				entry.due = held(now + wait, entry.hold);
				None
			}
			Outcome::Blocked(parent) => {
				entry.state = State::Blocked(parent.case_key());
				None
			}
			Outcome::Failed(failure) => Some(Failed {
				failure: *failure,
				kind: FailedKind::Retried,
			}),
			Outcome::Park(failure) => Some(Failed {
				failure: *failure,
				kind: FailedKind::Parked,
			}),
		};
		if let Some(Failed { failure, kind }) = failed {
			entry.attempts = entry.attempts.saturating_add(1);
			let backoff =
				watch_backoff(entry.attempts).expect("a failed job counts at least one failure");
			entry.hold = (kind == FailedKind::CameToNothing).then_some(now + backoff);
			if kind == FailedKind::Parked || entry.attempts >= PARK_AFTER {
				entry.state = State::Parked(failure);
				reports.push(LocalReport::Parked {
					path: entry.path.clone(),
					failure,
					attempts: entry.attempts,
				});
				parked = Some(failure);
			} else {
				entry.state = waiting(busy);
				entry.due = now + backoff;
			}
		}
		// An event while it ran brings the next look nearer, unless a writer still at it raised it.
		if again && *outcome != Outcome::Busy && matches!(entry.state, State::Waiting { .. }) {
			entry.due = held(entry.due.min(now + self.debounce), entry.hold);
		}
		if let Some(failure) = parked {
			self.park_blocked(&key, failure);
		}
		reports
	}

	/// Wakes the paths blocked on `parent`, or drops them when it was skipped.
	fn release_blocked(&mut self, parent: &CaseKey, now: Instant, skipped: bool) {
		if skipped {
			self.entries.retain(
				|_, entry| !matches!(&entry.state, State::Blocked(blocker) if blocker == parent),
			);
			return;
		}
		for entry in self.entries.values_mut() {
			if matches!(&entry.state, State::Blocked(blocker) if blocker == parent) {
				entry.state = State::Waiting { busy: None };
				entry.due = held(now, entry.hold);
			}
		}
	}

	/// Parks the paths blocked on `parent`, which parked with `failure`: they wait with it, for the
	/// next event, walk or the drive coming back, rather than for a job that is not coming.
	fn park_blocked(&mut self, parent: &CaseKey, failure: Failure) {
		for entry in self.entries.values_mut() {
			if matches!(&entry.state, State::Blocked(blocker) if blocker == parent) {
				entry.state = State::Parked(failure);
			}
		}
	}

	/// Retries every path parked as unreachable, its counts started afresh: the drive answered
	/// again.
	pub(super) fn unpark_unreachable(&mut self, now: Instant) {
		for entry in self.entries.values_mut() {
			if matches!(entry.state, State::Parked(Failure::Unreachable)) {
				entry.state = State::Waiting { busy: None };
				entry.due = now;
				entry.attempts = 0;
				entry.agains = 0;
				entry.hold = None;
			}
		}
	}

	/// Moves every entry at or under `from` that no job runs on to the same place under `to`, as a
	/// directory rename moves its subtree; a running one is looked at again under its new path.
	/// [`Admit::Overflow`] when the queue had no room for such a new path.
	pub(super) fn rebase(&mut self, from: &RootPath, to: &RootPath, now: Instant) -> Admit {
		let mut admit = Admit::Queued;
		let moved: Vec<CaseKey> = self
			.entries
			.iter()
			.filter(|(_, entry)| entry.path.starts_with(from))
			.map(|(key, _)| key.clone())
			.collect();
		for key in moved {
			let running = self.entries.get(&key).and_then(|entry| match entry.state {
				State::Running { running, .. } => Some(entry.kinds.union(running)),
				_ => None,
			});
			let path = self.entries.get(&key).map(|entry| entry.path.clone());
			let Some(new_path) = path.and_then(|path| path.rebase(from, to)) else {
				continue;
			};
			if let Some(kinds) = running {
				// Its job holds the old path and ends on it; the new path is looked at afresh.
				let kinds = kinds.with(Kind::Local);
				if self.push_kinds(&new_path, now, Delay::Debounce, kinds) == Admit::Overflow {
					admit = Admit::Overflow;
				}
				continue;
			}
			let Some(mut entry) = self.entries.remove(&key) else {
				continue;
			};
			entry.path = new_path.clone();
			let new_key = new_path.case_key();
			match self.entries.get_mut(&new_key) {
				Some(existing) => {
					existing.attempts = existing.attempts.max(entry.attempts);
					existing.agains = existing.agains.max(entry.agains);
					existing.hold = existing.hold.max(entry.hold);
					existing.due = held(existing.due.min(entry.due), existing.hold);
					existing.looked = existing.looked && entry.looked;
					existing.kinds = existing.kinds.union(entry.kinds);
				}
				None => {
					self.entries.insert(new_key, entry);
				}
			}
		}
		admit
	}

	/// Records that the job running on `path` sent the bytes hashed `hash` to the drive. Bytes it
	/// had not sent before are progress, made by whoever writes the file: the path's count of looks
	/// that came to nothing, and of failures, starts afresh. The same bytes again are not, so a drive
	/// that keeps answering an upload with the version the file had is still backed off from.
	pub(super) fn sent(&mut self, path: &RootPath, hash: [u8; 32]) {
		let Some(entry) = self.entries.get_mut(&path.case_key()) else {
			return;
		};
		if entry.sent == Some(hash) {
			return;
		}
		entry.sent = Some(hash);
		entry.agains = 0;
		entry.attempts = 0;
		entry.hold = None;
	}

	/// Puts the running `path` back to wait, due at `now`: its job never started.
	pub(super) fn put_back(&mut self, path: &RootPath, now: Instant) {
		let Some(entry) = self.entries.get_mut(&path.case_key()) else {
			return;
		};
		if let State::Running { busy, running, .. } = entry.state {
			entry.state = State::Waiting { busy };
			entry.kinds = entry.kinds.union(running);
			entry.due = now;
		}
	}

	/// Whether a job runs at or under `path`, the path `except` left out.
	pub(super) fn busy_at_or_under(&self, path: &RootPath, except: &RootPath) -> bool {
		let except = except.case_key();
		self.entries.iter().any(|(key, entry)| {
			*key != except
				&& matches!(entry.state, State::Running { .. })
				&& entry.path.starts_with(path)
		})
	}

	/// Whether a job runs at or under `path`, or, with `waiting`, one waits there for its due time
	/// or for its directory's job; a parked path waits for nothing.
	pub(super) fn active_under(&self, path: &RootPath, waiting: bool) -> bool {
		self.entries.values().any(|entry| {
			entry.path.starts_with(path)
				&& match entry.state {
					State::Running { .. } => true,
					State::Waiting { .. } | State::Blocked(_) => waiting,
					State::Parked(_) => false,
				}
		})
	}

	/// The paths that wait for, or run, a look at what the user changed there: in the debounce or
	/// the settle wait, held by a writer, backing off after a failure, blocked on their directory,
	/// or parked. Each path once, however many kinds it waits for.
	pub(super) fn local_paths(&self) -> impl Iterator<Item = &RootPath> {
		self.entries
			.values()
			.filter(|entry| entry.has_local())
			.map(|entry| &entry.path)
	}

	/// Whether a path waits for, or runs, a look at a local change that no job has found to hold
	/// anything yet ([`Queue::holds_change`]): until its look, the change it stands for may be
	/// nothing, as the provider's own writes raise events too.
	pub(super) fn local_unlooked(&self) -> bool {
		self.entries
			.values()
			.any(|entry| entry.has_local() && !entry.looked)
	}

	/// Records that the job running on `path` found a local change there to carry: the path holds
	/// what the drive lacks until the job ends, whatever kind of job it started as.
	pub(super) fn holds_change(&mut self, path: &RootPath) {
		let Some(entry) = self.entries.get_mut(&path.case_key()) else {
			return;
		};
		if let State::Running { running, .. } = &mut entry.state {
			*running = running.with(Kind::Local);
			entry.looked = true;
		}
	}

	/// Records that the job running on `path` goes on to look at the local change there, as a
	/// remote job hands its path to the local side.
	pub(super) fn runs_local(&mut self, path: &RootPath) {
		let Some(entry) = self.entries.get_mut(&path.case_key()) else {
			return;
		};
		let had_local = entry.has_local();
		if let State::Running { running, .. } = &mut entry.state {
			*running = running.with(Kind::Local);
			if !had_local {
				entry.looked = false;
			}
		}
	}

	/// Whether the job running on `path` runs `kind` alone.
	pub(super) fn runs_only(&self, path: &RootPath, kind: Kind) -> bool {
		self.entries.get(&path.case_key()).is_some_and(
			|entry| matches!(entry.state, State::Running { running, .. } if running == Kinds::of(kind)),
		)
	}

	/// Whether a job runs or waits on `path`; a parked path waits for nothing.
	pub(super) fn holds(&self, path: &RootPath) -> bool {
		self.entries
			.get(&path.case_key())
			.is_some_and(|entry| !matches!(entry.state, State::Parked(_)))
	}

	/// Drops every entry at or under `path` that no job runs on.
	pub(super) fn remove_under(&mut self, path: &RootPath) {
		self.entries.retain(|_, entry| {
			matches!(entry.state, State::Running { .. }) || !entry.path.starts_with(path)
		});
	}

	/// Drops every entry no job runs on, as an overflow does before the root is walked.
	pub(super) fn clear(&mut self) {
		self.entries
			.retain(|_, entry| matches!(entry.state, State::Running { .. }));
	}
}

impl Entry {
	/// Whether the path waits for, or runs, a look at what the user changed there.
	fn has_local(&self) -> bool {
		let running = match self.state {
			State::Running { running, .. } => running,
			State::Waiting { .. } | State::Blocked(_) | State::Parked(_) => Kinds::NONE,
		};
		self.kinds.union(running).has(Kind::Local)
	}
}

/// `due`, or the end of the backoff `hold`, whichever is later.
fn held(due: Instant, hold: Option<Instant>) -> Instant {
	hold.map_or(due, |hold| due.max(hold))
}

/// When a path pushed at `now` after `delay` is due, its entry first seen at `first_seen`.
fn due_after(
	debounce: Duration,
	max_burst: u32,
	first_seen: Instant,
	now: Instant,
	delay: Delay,
) -> Instant {
	match delay {
		Delay::Debounce => (now + debounce).min(first_seen + debounce * max_burst),
		Delay::Close => now + CLOSE_DELAY,
		Delay::Now => now,
		Delay::After(wait) => now + wait,
	}
}

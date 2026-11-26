//! An in-memory OS: directory and file placeholders, the requests Windows sends for them, and
//! responders that check every answer against the rules cfapi enforces, recording what happened
//! as [`FakeEvent`]s.

use std::{
	collections::{HashMap, HashSet},
	ops::{Deref, DerefMut, Range},
	pin::pin,
	sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use tokio::sync::Notify;

use crate::{
	failure::Failure,
	identity::{IdentityBlob, MAX_IDENTITY_LEN, PlaceholderIdentity},
	names::PlaceholderName,
	os::{
		AlignedChunk, CancelRequest, DataResponder, EntryOutcome, FetchDataRequest, OptionalRange,
		OsError, PopulateRequest, PopulateResponder, RequestedRange, TransferKey,
	},
	placeholder::{PlaceholderSpec, SpecKind},
	sealed,
};

/// cfapi's transfer granularity, in bytes.
const ALIGN: u64 = 4096;

/// The error the fake answers a refused operation with.
const REFUSED: OsError = OsError::Rejected { code: -1 };

/// What happened on the fake, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeEvent {
	/// `entries` were created under `dir`, which no longer asks for population.
	Populated {
		dir: PlaceholderIdentity,
		entries: Vec<PlaceholderSpec>,
	},
	/// A population was failed with the failure given.
	PopulateFailed(Failure),
	/// A population responder was dropped unanswered.
	PopulateAbandoned { dir: PlaceholderIdentity },
	/// `len` bytes at `offset` were transferred under `key`.
	Transferred {
		key: TransferKey,
		offset: u64,
		len: u64,
	},
	/// A transfer cfapi would refuse (or one a switch refused), and why.
	RejectedTransfer {
		key: TransferKey,
		offset: u64,
		len: u64,
		why: &'static str,
	},
	/// The request was completed with its required range hydrated.
	Completed(TransferKey),
	/// The request was completed without its required range hydrated.
	Incomplete(TransferKey),
	/// The request was failed with the failure given.
	Failed(TransferKey, Failure),
	/// The request was ended as cancelled.
	Cancelled(TransferKey),
	/// A data responder was dropped unanswered.
	Abandoned(TransferKey),
}

/// The fake OS. Clones share one state, which every responder holds too.
#[derive(Debug, Clone)]
pub struct FakeOs(
	// Reason for the Arc: the test driver and the responders it hands out (which must be 'static
	// to cross into tasks) observe one state.
	Arc<Shared>,
);

#[derive(Debug)]
struct Shared {
	state: Mutex<FakeState>,
	/// Woken whenever the state may have changed, for [`FakeOs::wait_for`].
	changed: Notify,
}

/// The locked state, which wakes every [`FakeOs::wait_for`] when released.
struct StateGuard<'a> {
	state: MutexGuard<'a, FakeState>,
	changed: &'a Notify,
}

impl Deref for StateGuard<'_> {
	type Target = FakeState;

	fn deref(&self) -> &FakeState {
		&self.state
	}
}

impl DerefMut for StateGuard<'_> {
	fn deref_mut(&mut self) -> &mut FakeState {
		&mut self.state
	}
}

impl Drop for StateGuard<'_> {
	fn drop(&mut self) {
		self.changed.notify_waiters();
	}
}

#[derive(Debug, Default)]
struct FakeState {
	dirs: HashMap<PlaceholderIdentity, FakeDir>,
	files: HashMap<PlaceholderIdentity, FakeFile>,
	next_transfer: i64,
	cancelled: Vec<Cancel>,
	events: Vec<FakeEvent>,
	fail_populate: Option<OsError>,
	refused_names: HashSet<String>,
	/// Names a local item holds in every directory, as one the user made before a population.
	local_names: HashSet<String>,
	fail_next_transfer: Option<OsError>,
}

/// A cancel the OS sent: `length` bytes at `offset` of the transfer `key`, as sent.
#[derive(Debug)]
struct Cancel {
	key: TransferKey,
	offset: i64,
	length: i64,
}

#[derive(Debug, Default)]
struct FakeDir {
	populated: bool,
	children: Vec<PlaceholderSpec>,
}

#[derive(Debug)]
struct FakeFile {
	size: u64,
	/// The bytes transferred, from offset 0 to the end of the last one.
	data: Vec<u8>,
	/// The ranges transferred: sorted, disjoint and not touching.
	hydrated: Vec<Range<u64>>,
}

impl FakeOs {
	/// An OS with the sync root `root` registered and not yet populated.
	pub fn with_root(root: PlaceholderIdentity) -> Self {
		let mut state = FakeState::default();
		state.dirs.insert(root, FakeDir::default());
		Self(Arc::new(Shared {
			state: Mutex::new(state),
			changed: Notify::new(),
		}))
	}

	fn state(&self) -> StateGuard<'_> {
		StateGuard {
			state: self.0.state.lock().unwrap_or_else(PoisonError::into_inner),
			changed: &self.0.changed,
		}
	}

	/// Fails the next population call as a whole with `error`.
	pub fn fail_populate(&self, error: OsError) {
		self.state().fail_populate = Some(error);
	}

	/// Refuses every entry named `name` in later populations.
	pub fn refuse_entry(&self, name: &str) {
		self.state().refused_names.insert(name.to_owned());
	}

	/// Answers every later entry named `name` as one a local item already holds.
	pub fn hold_locally(&self, name: &str) {
		self.state().local_names.insert(name.to_owned());
	}

	/// Refuses the next transfer with `error`.
	pub fn fail_next_transfer(&self, error: OsError) {
		self.state().fail_next_transfer = Some(error);
	}

	/// The OS asks for the children of `dir`, which must be a directory placeholder not yet
	/// populated: Windows never asks again once a population turned it off.
	pub fn fetch_placeholders(&self, dir: &PlaceholderIdentity) -> (PopulateRequest, FakePopulate) {
		self.populate_request(dir, false)
	}

	/// As [`FakeOs::fetch_placeholders`], raised by the provider's own process, as its own rename
	/// into a directory not populated yet does.
	pub fn fetch_placeholders_from_self(
		&self,
		dir: &PlaceholderIdentity,
	) -> (PopulateRequest, FakePopulate) {
		self.populate_request(dir, true)
	}

	fn populate_request(
		&self,
		dir: &PlaceholderIdentity,
		own_process: bool,
	) -> (PopulateRequest, FakePopulate) {
		let state = self.state();
		let known = state
			.dirs
			.get(dir)
			.unwrap_or_else(|| panic!("{dir:?} is not a directory placeholder"));
		assert!(
			!known.populated,
			"{dir:?} is already populated, so Windows does not ask again"
		);
		drop(state);
		(
			PopulateRequest::new(dir.encode(), own_process),
			FakePopulate {
				os: Some(self.clone()),
				dir: *dir,
			},
		)
	}

	/// The OS asks for `required` (offset, length) of the file placeholder `file` under a new
	/// transfer key, offering `optional` too, with the numbers cfapi sends (length 0 for none). The
	/// request carries the placeholder's size.
	pub fn fetch_data(
		&self,
		file: &PlaceholderIdentity,
		required: (i64, i64),
		optional: (i64, i64),
	) -> (FetchDataRequest, FakeData) {
		let key = self.next_key();
		self.fetch_data_under(key, file, required, optional)
	}

	/// As [`FakeOs::fetch_data`], raised by the provider's own process, as its own hydration of the
	/// file does.
	pub fn fetch_data_from_self(
		&self,
		file: &PlaceholderIdentity,
		required: (i64, i64),
		optional: (i64, i64),
	) -> (FetchDataRequest, FakeData) {
		let key = self.next_key();
		let (request, responder) = self.fetch_data_under(key, file, required, optional);
		let range = request.range();
		let identity = request.identity().clone();
		(FetchDataRequest::new(identity, key, range, true), responder)
	}

	/// As [`FakeOs::fetch_data`], under the transfer key `key` of an earlier request, as one
	/// transfer of several ranges would be sent. Whether Windows ever sends two requests under one
	/// key is unmeasured; this lets a test drive the core either way.
	pub fn fetch_data_under(
		&self,
		key: TransferKey,
		file: &PlaceholderIdentity,
		required: (i64, i64),
		optional: (i64, i64),
	) -> (FetchDataRequest, FakeData) {
		let size = self
			.state()
			.files
			.get(file)
			.unwrap_or_else(|| panic!("{file:?} is not a file placeholder"))
			.size;
		let size = i64::try_from(size).expect("a created placeholder's size fits an i64");
		let range = RequestedRange::new(
			size,
			required.0,
			required.1,
			OptionalRange::new(optional.0, optional.1),
		);
		self.request(key, file.encode(), range)
	}

	/// The OS sends `range` for the placeholder `identity` as given, under a new transfer key,
	/// which need be no placeholder the fake holds, for malformed-input tests. A transfer for an
	/// unknown file is refused.
	pub fn fetch_data_raw(
		&self,
		identity: IdentityBlob,
		range: RequestedRange,
	) -> (FetchDataRequest, FakeData) {
		let key = self.next_key();
		self.request(key, identity, range)
	}

	fn next_key(&self) -> TransferKey {
		let mut state = self.state();
		state.next_transfer += 1;
		TransferKey::new(state.next_transfer)
	}

	fn request(
		&self,
		key: TransferKey,
		identity: IdentityBlob,
		range: RequestedRange,
	) -> (FetchDataRequest, FakeData) {
		let file = PlaceholderIdentity::decode(&identity).ok();
		(
			FetchDataRequest::new(identity, key, range, false),
			FakeData {
				os: Some(self.clone()),
				key,
				file,
				range,
			},
		)
	}

	/// The OS cancels `length` bytes at `offset` of the transfer `key`: every later transfer of a
	/// request under `key` whose required range is exactly the cancelled one is answered
	/// [`OsError::Cancelled`], as Windows was measured to send one cancel per request carrying its
	/// required range; and of every request under `key` when the cancel's range holds no byte or
	/// its end overflows, a cancel Windows was not seen to send.
	pub fn cancel(&self, key: TransferKey, offset: i64, length: i64) -> CancelRequest {
		self.state().cancelled.push(Cancel {
			key,
			offset,
			length,
		});
		CancelRequest::new(key, offset, length)
	}

	/// The bytes transferred into `file` (from offset 0 to the end of the last transfer, zeros in
	/// the gaps) and the ranges they cover.
	pub fn contents(&self, file: &PlaceholderIdentity) -> (Vec<u8>, Vec<Range<u64>>) {
		let state = self.state();
		let file = state
			.files
			.get(file)
			.unwrap_or_else(|| panic!("{file:?} is not a file placeholder"));
		(file.data.clone(), file.hydrated.clone())
	}

	/// The placeholders created under `dir`, in creation order.
	pub fn children(&self, dir: &PlaceholderIdentity) -> Vec<PlaceholderSpec> {
		self.state()
			.dirs
			.get(dir)
			.map(|dir| dir.children.clone())
			.unwrap_or_default()
	}

	/// Every event so far, in order.
	pub fn events(&self) -> Vec<FakeEvent> {
		self.state().events.clone()
	}

	/// Waits until an event `matches`, and returns the first that does. For tests whose requests
	/// run on real I/O; the others drain their tasks instead.
	pub async fn wait_for(&self, matches: impl Fn(&FakeEvent) -> bool) -> FakeEvent {
		loop {
			let mut changed = pin!(self.0.changed.notified());
			// Registered before the check, so an event recorded after it still wakes this.
			changed.as_mut().enable();
			// Locked without a StateGuard, whose release would wake this very waiter.
			let found = self
				.0
				.state
				.lock()
				.unwrap_or_else(PoisonError::into_inner)
				.events
				.iter()
				.find(|event| matches(event))
				.cloned();
			if let Some(event) = found {
				return event;
			}
			changed.await;
		}
	}

	/// Panics when a request was abandoned, completed short, or sent a transfer cfapi refuses.
	pub fn assert_clean(&self) {
		let events = self.events();
		let dirty = events.iter().any(|event| {
			matches!(
				event,
				FakeEvent::PopulateAbandoned { .. }
					| FakeEvent::Abandoned(_)
					| FakeEvent::Incomplete(_)
					| FakeEvent::RejectedTransfer { .. }
			)
		});
		assert!(!dirty, "the fake OS saw a broken request: {events:?}");
	}
}

/// The answer to one fake `FETCH_PLACEHOLDERS`.
#[derive(Debug)]
pub struct FakePopulate {
	/// `None` once answered.
	os: Option<FakeOs>,
	dir: PlaceholderIdentity,
}

impl sealed::Sealed for FakePopulate {}

impl PopulateResponder for FakePopulate {
	/// Refuses, per entry, what Windows would: an identity longer than cfapi stores, a name
	/// [`PlaceholderName::parse`] refuses (the SDK's Windows name rule stands in for the OS here),
	/// a name refused by a switch, and a name equal to an earlier entry's in Windows's case
	/// folding.
	fn populate(mut self, entries: &[PlaceholderSpec]) -> Result<Vec<EntryOutcome>, OsError> {
		let os = self.os.take().expect("a responder is answered once");
		let mut state = os.state();
		if let Some(error) = state.fail_populate.take() {
			return Err(error);
		}
		let mut seen = HashSet::new();
		let mut created = Vec::new();
		let outcomes = entries
			.iter()
			.map(|entry| {
				let name = entry.name().as_str();
				let refused = entry.identity().encode().as_bytes().len() > MAX_IDENTITY_LEN
					|| PlaceholderName::parse(name).is_err()
					|| state.refused_names.contains(name)
					|| !seen.insert(entry.name().case_key());
				if refused {
					return EntryOutcome::Rejected(REFUSED);
				}
				if state.local_names.contains(name) {
					return EntryOutcome::Exists;
				}
				match entry.kind() {
					SpecKind::File { size } => {
						state.files.insert(
							entry.identity(),
							FakeFile {
								size: size.get(),
								data: Vec::new(),
								hydrated: Vec::new(),
							},
						);
					}
					SpecKind::Dir => {
						state.dirs.insert(entry.identity(), FakeDir::default());
					}
				}
				created.push(entry.clone());
				EntryOutcome::Created
			})
			.collect();
		let dir = state.dirs.entry(self.dir).or_default();
		dir.populated = true;
		dir.children.extend(created.iter().cloned());
		state.events.push(FakeEvent::Populated {
			dir: self.dir,
			entries: created,
		});
		Ok(outcomes)
	}

	fn fail(mut self, failure: Failure) {
		let os = self.os.take().expect("a responder is answered once");
		os.state().events.push(FakeEvent::PopulateFailed(failure));
	}
}

impl Drop for FakePopulate {
	fn drop(&mut self) {
		if let Some(os) = self.os.take() {
			os.state()
				.events
				.push(FakeEvent::PopulateAbandoned { dir: self.dir });
		}
	}
}

/// The answer to one fake `FETCH_DATA`.
#[derive(Debug)]
pub struct FakeData {
	/// `None` once answered.
	os: Option<FakeOs>,
	key: TransferKey,
	/// The file placeholder the request names, when its identity decodes.
	file: Option<PlaceholderIdentity>,
	range: RequestedRange,
}

impl FakeData {
	fn os(&self) -> &FakeOs {
		self.os.as_ref().expect("a responder is answered once")
	}

	fn finish(&mut self, event: FakeEvent) {
		let os = self.os.take().expect("a responder is answered once");
		os.state().events.push(event);
	}

	/// Why cfapi (or this stricter fake) refuses `len` bytes at `offset` of a file of `size` bytes,
	/// or `None` when it takes them.
	///
	/// Stricter than cfapi on purpose: a piece must lie inside the required range widened to
	/// 4096-byte alignment (or the end of the file), or inside the optional range with its end so
	/// widened, where cfapi takes any aligned piece of the file.
	fn refusal(&self, offset: u64, len: u64, size: u64) -> Option<&'static str> {
		if len == 0 {
			return Some("an empty transfer");
		}
		if !offset.is_multiple_of(ALIGN) {
			return Some("an unaligned offset");
		}
		let Some(end) = offset.checked_add(len).filter(|end| *end <= size) else {
			return Some("a transfer past the end of the file");
		};
		if !end.is_multiple_of(ALIGN) && end != size {
			return Some("an unaligned length before the end of the file");
		}
		let mut allowed = Vec::new();
		let required = window(
			self.range.required_offset(),
			self.range.required_len(),
			size,
			true,
		);
		let optional = self
			.range
			.optional()
			.and_then(|optional| window(optional.offset(), optional.length(), size, false));
		for range in required.into_iter().chain(optional) {
			merge(&mut allowed, range);
		}
		(!covers(&allowed, &(offset..end))).then_some("a transfer outside the request")
	}
}

impl sealed::Sealed for FakeData {}

impl DataResponder for FakeData {
	fn transfer(&mut self, chunk: AlignedChunk<'_>) -> Result<(), OsError> {
		let key = self.key;
		let offset = chunk.offset();
		let bytes = chunk.bytes();
		let len = u64::try_from(bytes.len()).expect("a slice's length fits a u64");
		let mut state = self.os().state();
		let required = (self.range.required_offset(), self.range.required_len());
		if state
			.cancelled
			.iter()
			.any(|cancel| cancel.key == key && names((cancel.offset, cancel.length), required))
		{
			return Err(OsError::Cancelled);
		}
		let reject = |state: &mut FakeState, why| {
			state.events.push(FakeEvent::RejectedTransfer {
				key,
				offset,
				len,
				why,
			});
			REFUSED
		};
		if let Some(error) = state.fail_next_transfer.take() {
			reject(&mut state, "refused by a switch");
			return Err(error);
		}
		let Some(size) = self
			.file
			.and_then(|file| state.files.get(&file))
			.map(|file| file.size)
		else {
			return Err(reject(&mut state, "not a file placeholder"));
		};
		if let Some(why) = self.refusal(offset, len, size) {
			return Err(reject(&mut state, why));
		}
		let file = self
			.file
			.and_then(|file| state.files.get_mut(&file))
			.expect("the file was found above");
		let start = usize::try_from(offset).expect("a fake file fits in memory");
		let end = usize::try_from(offset + len).expect("a fake file fits in memory");
		if file.data.len() < end {
			file.data.resize(end, 0);
		}
		file.data
			.get_mut(start..end)
			.expect("resized to hold the piece")
			.copy_from_slice(bytes);
		merge(&mut file.hydrated, offset..offset + len);
		state
			.events
			.push(FakeEvent::Transferred { key, offset, len });
		Ok(())
	}

	fn complete(mut self) {
		let key = self.key;
		let state = self.os().state();
		let covered = self
			.file
			.and_then(|file| state.files.get(&file))
			.is_some_and(|file| {
				window(
					self.range.required_offset(),
					self.range.required_len(),
					file.size,
					true,
				)
				.is_some_and(|required| covers(&file.hydrated, &required))
			});
		drop(state);
		self.finish(if covered {
			FakeEvent::Completed(key)
		} else {
			FakeEvent::Incomplete(key)
		});
	}

	fn fail(mut self, failure: Failure) {
		let key = self.key;
		self.finish(FakeEvent::Failed(key, failure));
	}

	fn cancelled(mut self) {
		let key = self.key;
		self.finish(FakeEvent::Cancelled(key));
	}
}

impl Drop for FakeData {
	fn drop(&mut self) {
		if let Some(os) = self.os.take() {
			os.state().events.push(FakeEvent::Abandoned(self.key));
		}
	}
}

/// `len` bytes at `offset` as a range of a file of `size` bytes: a negative start clamped to 0,
/// the start aligned down to [`ALIGN`] when `align_start`, and the end aligned up to it or to the
/// end of the file. `None` when nothing of the file is left.
fn window(offset: i64, len: i64, size: u64, align_start: bool) -> Option<Range<u64>> {
	if len <= 0 {
		return None;
	}
	let end = u64::try_from(offset.checked_add(len)?).ok()?;
	let start = u64::try_from(offset.max(0)).ok()?;
	let start = if align_start {
		start - start % ALIGN
	} else {
		start
	};
	let end = end
		.checked_next_multiple_of(ALIGN)
		.map_or(size, |end| end.min(size));
	(start < end).then_some(start..end)
}

/// `[offset, offset + len)`, or `None` when it holds no byte or its end overflows.
fn span((offset, len): (i64, i64)) -> Option<(i64, i64)> {
	if len <= 0 {
		return None;
	}
	Some((offset, offset.checked_add(len)?))
}

/// Whether the cancelled (offset, length) range `cancel` names the request that required
/// `required`: the same range, or a cancel naming no range at all.
fn names(cancel: (i64, i64), required: (i64, i64)) -> bool {
	span(cancel).is_none() || cancel == required
}

/// Adds `new` to `ranges`, kept sorted, disjoint and not touching.
fn merge(ranges: &mut Vec<Range<u64>>, new: Range<u64>) {
	let mut merged = new;
	ranges.retain(|range| {
		let apart = range.end < merged.start || merged.end < range.start;
		if !apart {
			merged = merged.start.min(range.start)..merged.end.max(range.end);
		}
		apart
	});
	let at = ranges.partition_point(|range| range.start < merged.start);
	ranges.insert(at, merged);
}

/// Whether one of `ranges` (as [`merge`] keeps them) holds all of `range`.
fn covers(ranges: &[Range<u64>], range: &Range<u64>) -> bool {
	ranges
		.iter()
		.any(|held| held.start <= range.start && range.end <= held.end)
}

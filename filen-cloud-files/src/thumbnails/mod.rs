//! File Explorer's thumbnails for the items of the served root, answered from the drive's bytes
//! and never by reading a placeholder's data: a path is resolved to the placeholder's identity
//! through the OS seam without opening its content, a file all on disk is thumbnailed from its
//! local bytes, and any other from the remote file's chunks through the cache. Nothing here
//! hydrates a placeholder, queues a hydration, takes the dispatcher's read permits, sets the local
//! engine's "reads in flight" hints or touches a pin: the service holds the root's file system and
//! the drive, and nothing of the engine's.
//!
//! The COM layer (`os::win::com`) calls [`ThumbnailService::thumbnail_for`] from a thread of its
//! own for each request, with a [`ThumbnailCancel`] it sets once the caller cancelled the call.
//! Explorer has up to two requests in flight and waits for each as long as it takes, never
//! cancelling one (measured), so a request held up stalls its whole view. So a request is answered
//! from what is kept first, before it waits for anything: the cache's thumbnail of the drive
//! file's version, or one of local bytes this service finished, at the size asked for or at any
//! other kept size of the same version. A smaller one is answered at once as it is, while the size
//! asked for is made in the background: File Explorer replaces a thumbnail it shows with the
//! type's generic icon when a larger size is answered Pending (measured), and shows blank tiles
//! while it waits. Anything else is made by a job of its own, in one of a bounded number of slots
//! shared by every job running, so the reads of remote files in flight stay bounded too; with
//! every slot held, the job waits in a bounded queue, and the newest waiting starts first once a
//! slot is free. A request whose thumbnail is not made within its deadline is answered
//! [`ThumbnailRefusal::Pending`], and its job goes on where it is, running or queued, with what it
//! did so far: it keeps what it makes and tells File Explorer the item changed, so the next ask is
//! answered from what is kept. Nothing started is thrown away while it is wanted, so a slow drive
//! is read once per item, not once per ask. One job makes each item version at each size: a
//! request for one already running or queued waits for it rather than make it again.
//!
//! Every item answered Pending is eventually either told it changed, with a kept thumbnail the
//! next ask is answered from, or its work ended with no thumbnail to show (a settled refusal,
//! offline, signed out, stopped, the item gone, a failed read, or a thumbnail the cache could
//! not keep), which the next ask answers or tries again. Pending is answered only for a job that
//! runs or is queued; with every slot held and the queue full, a request for anything else is
//! answered [`ThumbnailRefusal::Busy`] at once, and nothing answered Pending is dropped from the
//! queue.

#[cfg(test)]
mod tests;

use std::{
	collections::{HashMap, VecDeque},
	io::Cursor,
	panic::AssertUnwindSafe,
	path::Path,
	pin::pin,
	sync::{
		Arc, Mutex, MutexGuard, PoisonError,
		atomic::{AtomicBool, AtomicU64, Ordering},
	},
	time::Duration,
};

use filen_mobile_native_cache::{
	CacheError,
	ffi::FfiId,
	thumbnail::{FittedOutcome, FittedThumbnail},
};
use filen_types::fs::Uuid;
use futures::FutureExt;
use image_webp::WebPDecoder;
use tokio::{
	runtime::Handle,
	sync::{Notify, OwnedSemaphorePermit, Semaphore, watch},
	task::AbortHandle,
	time::{Instant, sleep_until, timeout_at},
};
use tracing::{debug, warn};

use crate::{
	drive::Drive,
	failure::{Answer, Failure, cache_answer},
	helper::{HOST, RootDir, below_root, blocking},
	identity::PlaceholderIdentity,
	local::Recent,
	log_safe::Tag,
	os::{LocalEntry, LocalError, LocalFileId, LocalFs, LocalKind},
	root_path::RootPath,
};

/// The smallest and largest thumbnail side asked for that is served as asked; others are clamped
/// into this range.
const MIN_SIDE: u32 = 16;
const MAX_SIDE: u32 = 1024;
/// The side every icon view's thumbnail is made at. File Explorer asks each item at 96 and then at
/// 256 in quick succession in icon views, and at 256 again once told the item changed (measured),
/// so both of its asks share one job and one read of the remote file, and the ask after the
/// notification is answered from what that job kept. Smaller sides are scaled down from it.
const ICON_SIDE: u32 = 256;
/// The sides thumbnails are made at, smallest first: a request is made at the smallest one at
/// least as large as it, and scaled down to the request, so the cache keeps two sizes of an item
/// rather than one per size Explorer asks for (it asks for 32, 48, 96 and 256 in icon views, 768
/// in the details pane and 1280 in the preview pane, measured). The largest is made only when
/// asked for.
const BUCKETS: [u32; 2] = [ICON_SIDE, MAX_SIDE];
/// How many settled refusals are remembered at most; past it the oldest goes.
const MAX_VERDICTS: usize = 256;
/// How many thumbnails of local bytes finished in the background are kept for the next ask, the
/// oldest dropped past it: the cache keeps none of local bytes, which have no remote version to key
/// them by. A few MB at most.
const MAX_KEPT_LOCAL: usize = 8;

/// How near a notification sent for another item an answer with a thumbnail must come, before or
/// after, for its item to be sent a notification of its own. Measured (VM runs t4, t5 and t6,
/// finding F6): items answered with a thumbnail, never Pending, while the notifications for the
/// items the background finished went out (25 in 0.3 s in t4, 30 at the release in t6) stayed
/// blank until a refresh, every one of them answered inside such a burst, and a notification of
/// its own painted each within 3 s with no new request; no item answered outside a burst stayed
/// blank. The bursts lasted well under a second: a few seconds takes in the answers just before
/// and after one, with room for a loaded machine.
const BURST_WINDOW: Duration = Duration::from_secs(3);
/// How long no notification for an item the background finished must have gone out before the
/// items answered inside the burst are sent theirs, so they land after it.
const BURST_SETTLE: Duration = Duration::from_millis(500);
/// How many item versions File Explorer was told of are remembered, so the burst remedy never
/// sends one a notification of its own after it was told; past it the least recently told goes.
const MAX_TOLD: usize = 4096;
/// How many items answered lately, and how many owed a notification, are held at most; past it the
/// oldest goes, its tile left as File Explorer leaves it.
const MAX_BURST_ITEMS: usize = 256;

/// The deadline of a request File Explorer makes through the COM layer
/// ([`ThumbnailService::thumbnail_for`]), when shorter than the service's: past it the request is
/// answered [`ThumbnailRefusal::Pending`] and its thumbnail finished in the background, so nothing
/// is lost by answering early. Explorer never cancels a call (measured: no cancel in fast
/// scrolling, navigating away or closing the window) and shows blank tiles while it waits, with at
/// most two calls in flight, so a request held up stalls the whole view; the slowest answer
/// measured took 105 ms, the median 20 ms, with a fast drive.
pub(crate) const EXPLORER_DEADLINE: Duration = Duration::from_millis(1500);

/// Why a thumbnail is not given. Explorer shows the type's generic icon for each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ThumbnailRefusal {
	/// The path is not inside the served root, or not a path this OS spells.
	#[error("the path is not inside the served root")]
	NotInRoot,
	/// The item is not a file the drive or the disk can give bytes of: a link, a placeholder whose
	/// identity does not decode, or one standing for no file.
	#[error("the item is not a file of the drive")]
	NotAPlaceholder,
	/// The item is a directory.
	#[error("the item is a directory")]
	Directory,
	/// Not an image this build decodes, or one whose bytes the decoder refused.
	#[error("the file is not an image this build thumbnails")]
	Unsupported,
	/// An image too large to thumbnail within the memory budget, or one that would need more of a
	/// remote file than is fetched for a thumbnail.
	#[error("the image is too large to thumbnail")]
	TooLarge,
	/// The server could not be reached.
	#[error("the server could not be reached")]
	Offline,
	/// No session is signed in, or the root is not served.
	#[error("no session is signed in")]
	SignedOut,
	/// Not made within the request's deadline: it is being finished in the background, kept once
	/// made, and File Explorer told the item changed, so it asks again and is answered from what is
	/// kept.
	#[error("the thumbnail is still being made")]
	Pending,
	/// No room to make it: every slot was held and the queue full, the file was busy on disk, or
	/// the request was cancelled.
	#[error("the thumbnail could not be made now")]
	Busy,
	/// Anything else: the item vanished, or a read or the cache failed. Worth asking again.
	#[error("making the thumbnail failed")]
	Failed,
}

/// Whether a thumbnail's pixels carry transparency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThumbnailAlpha {
	/// Every pixel is opaque; the alpha bytes are all 255 (`WTSAT_RGB`).
	Opaque,
	/// Some pixels are not opaque; the colour bytes are straight, not premultiplied by alpha
	/// (`WTSAT_ARGB`): File Explorer composites such a bitmap as straight alpha (measured:
	/// premultiplied pixels came out darkened by their alpha a second time).
	Straight,
}

/// A thumbnail as File Explorer takes it: 32-bit pixels, blue, green, red and alpha bytes in that
/// order, the alpha straight, rows top-down with no padding, its longer side no larger than the
/// size asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
	width: u32,
	height: u32,
	bgra: Vec<u8>,
	alpha: ThumbnailAlpha,
}

impl Thumbnail {
	/// Its width in pixels; at least 1.
	pub fn width(&self) -> u32 {
		self.width
	}

	/// Its height in pixels; at least 1.
	pub fn height(&self) -> u32 {
		self.height
	}

	/// Its pixels: `width * height * 4` bytes, BGRA, top-down.
	pub fn bgra(&self) -> &[u8] {
		&self.bgra
	}

	/// Whether it carries transparency.
	pub fn alpha(&self) -> ThumbnailAlpha {
		self.alpha
	}

	/// A thumbnail of `width` by `height` pixels `bgra`, as given, for the COM layer's tests.
	#[cfg(all(test, windows))]
	pub(crate) fn for_test(width: u32, height: u32, bgra: Vec<u8>, alpha: ThumbnailAlpha) -> Self {
		Self {
			width,
			height,
			bgra,
			alpha,
		}
	}
}

/// What the COM layer is handed: the service's answer for a path, the size asked for, in pixels,
/// and the request's cancellation, which the caller sets once the answer is no longer wanted. It
/// must answer within a bounded time, and soon once cancelled: the COM layer waits for its answer
/// with no limit of its own, holding the call's thread. The service's handler answers within
/// `EXPLORER_DEADLINE`.
pub type ThumbnailHandler =
	Arc<dyn Fn(&Path, u32, &ThumbnailCancel) -> Result<Thumbnail, ThumbnailRefusal> + Send + Sync>;

/// One request's cancellation: once set, the service answers the request
/// [`ThumbnailRefusal::Busy`] at once. A job the request started goes on where it runs, as past
/// its deadline, though File Explorer is not told of it; one still queued that nothing else waits
/// for nor was promised leaves the queue. Clones share it. Set from any thread.
#[derive(Debug, Clone, Default)]
pub struct ThumbnailCancel(Arc<CancelState>);

#[derive(Debug, Default)]
struct CancelState {
	/// Set once cancelled.
	flag: AtomicBool,
	/// Wakes the request waiting on [`ThumbnailCancel::cancelled`].
	woken: Notify,
}

impl ThumbnailCancel {
	/// Cancels the request; a repeat does nothing more.
	pub fn cancel(&self) {
		self.0.flag.store(true, Ordering::Release);
		self.0.woken.notify_waiters();
	}

	/// Whether the request was cancelled.
	pub fn is_cancelled(&self) -> bool {
		self.0.flag.load(Ordering::Acquire)
	}

	/// Ready once the request is cancelled.
	async fn cancelled(&self) {
		loop {
			let mut woken = pin!(self.0.woken.notified());
			// Registered before the flag is read, so a cancel between the two still wakes it.
			woken.as_mut().enable();
			if self.is_cancelled() {
				return;
			}
			woken.await;
		}
	}
}

/// How the service runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ThumbnailConfig {
	/// How many jobs make a thumbnail at once, those a request waits for and those it no longer
	/// does alike; at least 1. A job keeps its slot until it ends, so this bounds the remote reads
	/// in flight. Default 4: the two requests File Explorer has in flight at most (measured), and
	/// two jobs left to finish after their requests were answered Pending; the cache's decode gate
	/// bounds memory across all of them anyway.
	pub(crate) running: usize,
	/// How long a request may take, its wait for a slot included, before it is answered
	/// [`ThumbnailRefusal::Pending`] and its thumbnail finished in the background. Default 10 s; a
	/// request File Explorer makes has [`EXPLORER_DEADLINE`] when that is shorter.
	pub(crate) deadline: Duration,
	/// How many jobs wait for a slot, newest first; past it a request for anything not running or
	/// queued is answered [`ThumbnailRefusal::Busy`] at once rather than Pending, as nothing
	/// answered Pending is dropped. Default 1024: File Explorer has two requests in flight and asks
	/// each item twice (measured), so a stalled drive gets about one new item a second, each a few
	/// hundred bytes here.
	pub(crate) queued: usize,
	/// How long a settled "no thumbnail" for an item at a size is answered from memory, so
	/// Explorer asking again does not download the item again. Default 60 s.
	pub(crate) remember: Duration,
}

impl Default for ThumbnailConfig {
	fn default() -> Self {
		Self {
			running: 4,
			deadline: Duration::from_secs(10),
			queued: 1024,
			remember: Duration::from_secs(60),
		}
	}
}

/// The root the service answers for, while it is served.
struct Target<L, D> {
	root: RootDir,
	local: Arc<L>,
	drive: Arc<D>,
}

/// What a thumbnail is made of: a version of a drive file, or local bytes as they were.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Bytes {
	/// The version `version` of a drive file, as its placeholder names it.
	Remote { version: Uuid },
	/// The local file `id` of `size` bytes last written at `modified` (FILETIME ticks).
	Local {
		id: LocalFileId,
		size: u64,
		modified: i64,
	},
}

/// What one job makes: the bytes, and the side made at.
type JobKey = (Bytes, u32);

/// Where a thumbnail's bytes come from.
enum Plan {
	/// The local file, all on disk.
	Local {
		path: RootPath,
		id: LocalFileId,
		key: Bytes,
	},
	/// The drive's file, by its `stable/<uuid>` id.
	Remote { id: FfiId, key: Bytes },
}

impl Plan {
	fn key(&self) -> &Bytes {
		match self {
			Self::Local { key, .. } | Self::Remote { key, .. } => key,
		}
	}

	fn kind(&self) -> &'static str {
		match self {
			Self::Local { .. } => "local",
			Self::Remote { .. } => "remote",
		}
	}
}

/// How a job ended.
enum Finished {
	/// The thumbnail, at the job's side.
	Made(Arc<FittedThumbnail>),
	/// A settled verdict about the bytes, remembered.
	Settled(ThumbnailRefusal),
	/// Anything else, which the next ask tries again.
	Refused(ThumbnailRefusal),
}

/// What the requests waiting on a job are told once it ended.
type Made = Result<Arc<FittedThumbnail>, ThumbnailRefusal>;

/// What a job needs to make its thumbnail, taken when it was asked for.
struct Work<L, D> {
	plan: Plan,
	made_at: u32,
	/// The item, for telling File Explorer once the background finished it.
	at: RootPath,
	local: Arc<L>,
	drive: Arc<D>,
	/// The served root's generation it was asked for under.
	generation: u64,
	/// The item's tag in the log ([`ThumbnailService::item_tag`]).
	tag: Tag,
}

/// One thumbnail being made, or waiting for a slot.
struct Job<L, D> {
	/// Tells it from a later job of the same key.
	id: u64,
	/// What it ended with, once it did; each request waiting on it holds a receiver.
	made: watch::Sender<Option<Made>>,
	run: Run<L, D>,
	/// How many requests wait on it now: what it makes is answered to them. It runs in the
	/// background once none does.
	waiting: usize,
	/// Whether a request was answered Pending for it, or from a smaller kept size while it makes
	/// the one asked for: File Explorer is told the item changed once it is made and kept, even
	/// when a later request waiting on it is answered with it, as File Explorer leaves the tile of
	/// an item answered Pending blank, whatever a later ask is answered, until it is told
	/// (measured). Such a job is never dropped but by a stop.
	promised: bool,
}

/// Where a job is.
enum Run<L, D> {
	/// Waiting for a slot.
	Queued(Arc<Work<L, D>>),
	/// Being made in a task of its own, holding a slot until it ends, whether or not a request
	/// still waits for it.
	Running {
		task: AbortHandle,
		/// Held only to be given back with the job.
		_slot: OwnedSemaphorePermit,
	},
}

/// The service's work, shared with the tasks doing it.
struct Lane<L, D> {
	runtime: Handle,
	/// One per job running. Only ever taken without waiting, and given back only as a job ends
	/// ([`Lane::finish`]) or by a stop, so a slot freed always goes to the newest queued job.
	slots: Arc<Semaphore>,
	/// How many jobs may wait for a slot.
	queued_max: usize,
	/// How long a settled refusal is remembered.
	remember: Duration,
	state: Mutex<LaneState<L, D>>,
}

struct LaneState<L, D> {
	target: Option<Target<L, D>>,
	/// Counts the roots served and stopped: work asked for under an earlier one starts nothing.
	generation: u64,
	/// The jobs running or waiting for a slot, by what each makes: at most one per slot, and the
	/// queued ones.
	jobs: HashMap<JobKey, Job<L, D>>,
	/// The keys of the jobs waiting for a slot, oldest first.
	queued: VecDeque<JobKey>,
	/// Thumbnails of local bytes the background finished, oldest first.
	kept: VecDeque<(JobKey, Arc<FittedThumbnail>)>,
	/// Settled refusals, with when they were reached.
	verdicts: HashMap<JobKey, (ThumbnailRefusal, Instant)>,
	next_job: u64,
	burst: Burst<L>,
}

/// An item a request was answered with a thumbnail for, as the burst remedy holds it.
struct Answered<L> {
	bytes: Bytes,
	at: RootPath,
	local: Arc<L>,
	/// The served root's generation it was answered under.
	generation: u64,
	when: Instant,
}

/// The remedy for the tiles File Explorer leaves blank once it is answered with a thumbnail inside
/// a burst of notifications for other items ([`BURST_WINDOW`]): each such item version is sent one
/// notification of its own once the burst settles. The notifications it sends are no burst of
/// their own and go only to item versions never told of, so the asks they bring end it: one per
/// item version at most.
struct Burst<L> {
	/// When the last notification went out for an item the background finished.
	last_tell: Option<Instant>,
	/// Items answered with a thumbnail with no such notification since, oldest first, those of the
	/// last [`BURST_WINDOW`] at least.
	answered: VecDeque<Answered<L>>,
	/// Items owed a notification of their own, oldest first.
	owed: VecDeque<Answered<L>>,
	/// Item versions File Explorer was told of, by the background or by this remedy.
	told: Recent<Bytes, ()>,
	/// The task that sends what is owed once the burst settles, while one waits.
	sender: Option<AbortHandle>,
}

impl<L> Burst<L> {
	fn new() -> Self {
		Self {
			last_tell: None,
			answered: VecDeque::new(),
			owed: VecDeque::new(),
			told: Recent::new(MAX_TOLD, "item versions told"),
			sender: None,
		}
	}

	/// Whether the item version `bytes` was told of, or is owed a notification.
	fn told_or_owed(&self, bytes: &Bytes) -> bool {
		self.told.get(bytes).is_some() || self.owed.iter().any(|item| item.bytes == *bytes)
	}
}

/// Appends `item` to `items`, dropping the oldest past [`MAX_BURST_ITEMS`].
fn push_bounded<L>(items: &mut VecDeque<Answered<L>>, item: Answered<L>) {
	items.push_back(item);
	if items.len() > MAX_BURST_ITEMS {
		items.pop_front();
	}
}

/// How a request is joined to the job making its thumbnail.
enum Joined<L: LocalFs, D: Drive> {
	/// Waiting on the job, running or queued.
	Waiting(Waiter<L, D>),
	/// Refused before any work.
	Refused(ThumbnailRefusal),
}

/// A request waiting on a job, which goes on where it is once the request stops waiting.
struct Waiter<L: LocalFs, D: Drive> {
	made: watch::Receiver<Option<Made>>,
	leave: Leave<L, D>,
}

impl<L: LocalFs, D: Drive> Waiter<L, D> {
	/// Stops waiting, as the request is answered Pending: whether the job still runs or waits for
	/// a slot, promised, so the answer may be Pending.
	fn promise(&mut self) -> Left {
		self.leave.armed = false;
		self.leave.lane.promise(self.leave.key, self.leave.id)
	}

	/// What the job ended with, once it did.
	fn made(&self) -> Option<Made> {
		self.made.borrow().clone()
	}
}

/// Stops the request waiting on the job `id` making `key` once dropped, unless it stopped already
/// ([`Lane::leave`]).
struct Leave<L: LocalFs, D: Drive> {
	lane: Arc<Lane<L, D>>,
	key: JobKey,
	id: u64,
	armed: bool,
}

impl<L: LocalFs, D: Drive> Drop for Leave<L, D> {
	fn drop(&mut self) {
		if self.armed {
			self.lane.leave(self.key, self.id);
		}
	}
}

/// What became of a job a request stopped waiting on, to answer it Pending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Left {
	/// It runs, or waits for a slot, and File Explorer is told once it is made.
	Promised,
	/// It ended, or was dropped, before the request stopped waiting: its waiters were told how.
	Ended,
}

/// How a request was answered, for its log line.
#[derive(Debug, Clone, Copy)]
struct Asked {
	/// The item's tag ([`ThumbnailService::item_tag`]), never the path.
	item: Option<Tag>,
	/// Which bytes it is made of.
	bytes: &'static str,
	/// What answered it.
	via: &'static str,
}

impl Default for Asked {
	fn default() -> Self {
		Self {
			item: None,
			bytes: "none",
			via: "none",
		}
	}
}

/// Answers File Explorer's thumbnail requests for the served root (see the module's docs). One per
/// helper run; the helper tells it which root it serves, and while none is, every request is
/// [`ThumbnailRefusal::SignedOut`].
pub(crate) struct ThumbnailService<L, D> {
	config: ThumbnailConfig,
	runtime: Handle,
	lane: Arc<Lane<L, D>>,
	/// Numbers the requests in the log.
	requests: AtomicU64,
}

/// `side` clamped into [`MIN_SIDE`]`..=`[`MAX_SIDE`].
fn clamp_side(side: u32) -> u32 {
	side.clamp(MIN_SIDE, MAX_SIDE)
}

/// The size a thumbnail of no more than `side` pixels is made at: the smallest bucket at least as
/// large as the clamped side.
fn bucket(side: u32) -> u32 {
	let side = clamp_side(side);
	BUCKETS
		.into_iter()
		.find(|bucket| *bucket >= side)
		.unwrap_or(MAX_SIDE)
}

/// What the drive's `error` tells Explorer, through the same mapping the provider's own requests
/// use.
fn cache_refusal(error: &CacheError) -> ThumbnailRefusal {
	match cache_answer(error) {
		Answer::Fail(Failure::NotAuthenticated) => ThumbnailRefusal::SignedOut,
		Answer::Fail(Failure::Unreachable) => ThumbnailRefusal::Offline,
		Answer::Fail(Failure::Unsupported) => ThumbnailRefusal::Unsupported,
		Answer::Cancelled => ThumbnailRefusal::Busy,
		Answer::Fail(
			Failure::NotFound
			| Failure::ContentDamaged
			| Failure::Undecryptable
			| Failure::StalePlaceholder
			| Failure::InvalidRequest
			| Failure::Transient
			| Failure::Internal,
		)
		| Answer::Damaged => ThumbnailRefusal::Failed,
	}
}

/// `thumbnail` as pixels fitted inside a `side` pixel square; one that does not decode fails.
fn pixels(thumbnail: &FittedThumbnail, side: u32) -> Result<Thumbnail, ThumbnailRefusal> {
	to_pixels(thumbnail, side).map_err(|error| {
		warn!(%error, "a thumbnail the cache made does not decode");
		ThumbnailRefusal::Failed
	})
}

/// Decodes `thumbnail` and scales it to fit inside a `side` pixel square.
fn to_pixels(
	thumbnail: &FittedThumbnail,
	side: u32,
) -> Result<Thumbnail, image_webp::DecodingError> {
	let mut decoder = WebPDecoder::new(Cursor::new(thumbnail.webp.as_slice()))?;
	let (width, height) = decoder.dimensions();
	let channels = if decoder.has_alpha() { 4 } else { 3 };
	let size = decoder
		.output_buffer_size()
		.ok_or(image_webp::DecodingError::ImageTooLarge)?;
	let mut decoded = vec![0; size];
	decoder.read_image(&mut decoded)?;
	let (bgra, alpha) = to_bgra(&decoded, channels);
	Ok(fit_within(
		Thumbnail {
			width,
			height,
			bgra,
			alpha,
		},
		side,
	))
}

/// RGB or RGBA pixels, `channels` bytes each, as BGRA with the alpha straight, as File Explorer
/// composites them, and whether any is not opaque.
fn to_bgra(rgba: &[u8], channels: usize) -> (Vec<u8>, ThumbnailAlpha) {
	let mut bgra = Vec::with_capacity(rgba.len() / channels * 4);
	let mut alpha = ThumbnailAlpha::Opaque;
	for pixel in rgba.chunks_exact(channels) {
		let [red, green, blue, ref rest @ ..] = *pixel else {
			continue;
		};
		let a = rest.first().copied().unwrap_or(u8::MAX);
		if a != u8::MAX {
			alpha = ThumbnailAlpha::Straight;
		}
		bgra.extend_from_slice(&[blue, green, red, a]);
	}
	(bgra, alpha)
}

/// `thumbnail` scaled down, by averaging the pixels each new one covers, so that its longer side
/// is `side`; as it is when it fits already. The alpha is straight, so each colour is averaged
/// weighted by its pixel's alpha: a transparent pixel's colour, which shows nowhere, does not
/// bleed into its opaque neighbours'.
fn fit_within(thumbnail: Thumbnail, side: u32) -> Thumbnail {
	let longer = thumbnail.width.max(thumbnail.height);
	if longer <= side {
		return thumbnail;
	}
	let scaled = |length: u32| {
		let length =
			(u64::from(length) * u64::from(side) + u64::from(longer) / 2) / u64::from(longer);
		u32::try_from(length).unwrap_or(side).clamp(1, side)
	};
	let (width, height) = (scaled(thumbnail.width), scaled(thumbnail.height));
	// The source pixels the new pixel `n` of `new` covers along a side of `old`.
	let span = |n: u32, new: u32, old: u32| {
		let start = u64::from(n) * u64::from(old) / u64::from(new);
		let end = (u64::from(n + 1) * u64::from(old) / u64::from(new)).max(start + 1);
		(start, end)
	};
	let source_width = u64::from(thumbnail.width);
	let mut bgra =
		Vec::with_capacity(usize::try_from(u64::from(width) * u64::from(height) * 4).unwrap_or(0));
	for y in 0..height {
		let (top, bottom) = span(y, height, thumbnail.height);
		for x in 0..width {
			let (left, right) = span(x, width, thumbnail.width);
			// Blue, green and red, each times its pixel's alpha, then the alpha.
			let mut sums = [0_u64; 4];
			for row in top..bottom {
				for column in left..right {
					let at =
						usize::try_from((row * source_width + column) * 4).unwrap_or(usize::MAX);
					if let Some(&[blue, green, red, a]) = thumbnail.bgra.get(at..at + 4) {
						let a = u64::from(a);
						let weighted = [blue, green, red].map(|channel| u64::from(channel) * a);
						for (sum, value) in sums.iter_mut().zip(weighted.into_iter().chain([a])) {
							*sum += value;
						}
					}
				}
			}
			let count = (bottom - top) * (right - left);
			let [blue, green, red, alpha] = sums;
			// A pixel transparent throughout has no colour to show.
			let colour = |sum: u64| {
				sum.checked_div(alpha)
					.map_or(0, |colour| u8::try_from(colour).unwrap_or(u8::MAX))
			};
			bgra.extend([
				colour(blue),
				colour(green),
				colour(red),
				u8::try_from(alpha / count).unwrap_or(u8::MAX),
			]);
		}
	}
	Thumbnail {
		width,
		height,
		bgra,
		alpha: thumbnail.alpha,
	}
}

impl<L: LocalFs, D: Drive> ThumbnailService<L, D> {
	/// A service running its requests on `runtime`, serving no root yet.
	pub(crate) fn new(config: ThumbnailConfig, runtime: Handle) -> Self {
		Self {
			lane: Arc::new(Lane {
				runtime: runtime.clone(),
				slots: Arc::new(Semaphore::new(
					config.running.clamp(1, Semaphore::MAX_PERMITS),
				)),
				queued_max: config.queued,
				remember: config.remember,
				state: Mutex::new(LaneState {
					target: None,
					generation: 0,
					jobs: HashMap::new(),
					queued: VecDeque::new(),
					kept: VecDeque::new(),
					verdicts: HashMap::new(),
					next_job: 0,
					burst: Burst::new(),
				}),
			}),
			config,
			runtime,
			requests: AtomicU64::new(0),
		}
	}

	/// Answers for the root at `root`, whose file system is `local`, from `drive`.
	pub(crate) fn serve(&self, root: RootDir, local: Arc<L>, drive: Arc<D>) {
		self.lane.reset(Some(Target { root, local, drive }));
	}

	/// Answers every later request [`ThumbnailRefusal::SignedOut`], cancels the work running or
	/// waiting in the background at once, and forgets what it remembered and kept: the next session
	/// may be another account's.
	pub(crate) fn stop_serving(&self) {
		self.lane.reset(None);
	}

	/// The handler the COM layer is handed: [`ThumbnailService::thumbnail_for`] on this service.
	pub(crate) fn handler(self: &Arc<Self>) -> ThumbnailHandler {
		let service = Arc::clone(self);
		Arc::new(move |path, size, cancel| service.thumbnail_for(path, size, cancel))
	}

	/// The thumbnail of the item at `path`, its longer side at most `size_px` (clamped to
	/// 16..=1024), or why none is given; within [`ThumbnailService::answer_for_explorer`]'s
	/// deadline, or until `cancel` is set, blocking the calling thread. For a thread outside the
	/// service's runtime, as the COM layer's is: called on a thread of the runtime, it refuses with
	/// [`ThumbnailRefusal::Busy`] rather than block the runtime.
	pub(crate) fn thumbnail_for(
		&self,
		path: &Path,
		size_px: u32,
		cancel: &ThumbnailCancel,
	) -> Result<Thumbnail, ThumbnailRefusal> {
		if Handle::try_current().is_ok() {
			warn!("a thumbnail was asked for on the helper's runtime; refused");
			return Err(ThumbnailRefusal::Busy);
		}
		self.runtime
			.block_on(self.answer_for_explorer(path, size_px, cancel))
	}

	/// [`ThumbnailService::answer`] for a request File Explorer made, within [`EXPLORER_DEADLINE`]
	/// or the service's deadline, whichever is shorter.
	async fn answer_for_explorer(
		&self,
		path: &Path,
		size_px: u32,
		cancel: &ThumbnailCancel,
	) -> Result<Thumbnail, ThumbnailRefusal> {
		let deadline = EXPLORER_DEADLINE.min(self.config.deadline);
		self.answer_within(path, size_px, cancel, deadline).await
	}

	/// [`ThumbnailService::answer`] with a cancellation nothing sets.
	#[cfg(test)]
	pub(crate) async fn request(
		&self,
		path: &Path,
		size_px: u32,
	) -> Result<Thumbnail, ThumbnailRefusal> {
		self.answer(path, size_px, &ThumbnailCancel::default())
			.await
	}

	/// One request, within the service's deadline, logged with its number, the item's tag, timing
	/// and outcome, never with the path. Once `cancel` is set, the request is answered at once. The
	/// tests'; File Explorer's requests come through [`ThumbnailService::answer_for_explorer`].
	#[cfg(test)]
	pub(crate) async fn answer(
		&self,
		path: &Path,
		size_px: u32,
		cancel: &ThumbnailCancel,
	) -> Result<Thumbnail, ThumbnailRefusal> {
		self.answer_within(path, size_px, cancel, self.config.deadline)
			.await
	}

	/// [`ThumbnailService::answer`] within `deadline`, its wait for a slot included.
	async fn answer_within(
		&self,
		path: &Path,
		size_px: u32,
		cancel: &ThumbnailCancel,
		deadline: Duration,
	) -> Result<Thumbnail, ThumbnailRefusal> {
		let number = self.requests.fetch_add(1, Ordering::Relaxed);
		let started = Instant::now();
		let side = clamp_side(size_px);
		let (asked, answer) = tokio::select! {
			biased;
			// What the request waited on goes on, or goes, as a request past its deadline's does.
			() = cancel.cancelled() => (
				Asked { via: "cancelled", ..Asked::default() },
				Err(ThumbnailRefusal::Busy),
			),
			answered = self.answer_until(path, side, started + deadline) => answered,
		};
		debug!(
			request = number,
			item = %asked.item.map_or_else(|| "-".to_owned(), |item| item.to_string()),
			size = size_px,
			made_at = bucket(side),
			bytes = asked.bytes,
			source = asked.via,
			elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
			outcome = %match &answer {
				Ok(thumbnail) => format!("{}x{}", thumbnail.width, thumbnail.height),
				Err(refusal) => format!("{refusal:?}"),
			},
			"thumbnail request"
		);
		answer
	}

	/// The answer for the item at `path` fitted inside a `side` square, by `until`, and how it was
	/// reached: from the path's spelling alone, then from what is kept and remembered, before any
	/// slot is taken, then from the job making it, joined or started
	/// ([`ThumbnailService::answer_work`]). An item answered with a thumbnail inside a burst of
	/// notifications for others is sent one of its own ([`Lane::answered`]). A kept size smaller than the
	/// one asked for is answered at once, as it is, while the background makes the one asked for
	/// and tells File Explorer once it is kept; a larger one is scaled down to the request.
	async fn answer_until(
		&self,
		path: &Path,
		side: u32,
		until: Instant,
	) -> (Asked, Result<Thumbnail, ThumbnailRefusal>) {
		let mut asked = Asked::default();
		let Some((root, local, drive, generation)) = self.lane.target() else {
			return (asked, Err(ThumbnailRefusal::SignedOut));
		};
		let at = match locate(&root, path) {
			Ok(at) => at,
			Err(refusal) => return (asked, Err(refusal)),
		};
		let plan = match timeout_at(until, plan(&local, &at)).await {
			Ok(Ok(plan)) => plan,
			Ok(Err(refusal)) => return (asked, Err(refusal)),
			Err(_) => {
				asked.via = "deadline";
				return (asked, Err(ThumbnailRefusal::Busy));
			}
		};
		let tag = Self::item_tag(&plan);
		asked.item = Some(tag);
		asked.bytes = plan.kind();
		let work = Arc::new(Work {
			plan,
			made_at: bucket(side),
			at,
			local,
			drive,
			generation,
			tag,
		});
		let (asked, answer) = self
			.answer_work(asked, side, Arc::clone(&work), until)
			.await;
		// Answered from a job promised to tell File Explorer, the item is told by it.
		if answer.is_ok() && matches!(asked.via, "kept" | "made") {
			self.lane.answered(&work);
		}
		(asked, answer)
	}

	/// [`ThumbnailService::answer_until`] once the item's bytes are known: from what is kept and
	/// remembered, before any slot is taken, then from the job making `work`, joined or started.
	async fn answer_work(
		&self,
		mut asked: Asked,
		side: u32,
		work: Arc<Work<L, D>>,
		until: Instant,
	) -> (Asked, Result<Thumbnail, ThumbnailRefusal>) {
		let made_at = work.made_at;
		let bytes = *work.plan.key();
		let key = (bytes, made_at);
		let kept = self.best_kept(bytes, made_at, &work.drive).await;
		let remembered = self.lane.remembered(&key);
		if let Some((kept_at, thumbnail)) = kept {
			asked.via = "kept";
			if kept_at < made_at && remembered.is_none() {
				asked.via = "kept smaller";
				// Nothing is promised with every slot held and the queue full: the smaller one
				// stands.
				self.lane.queue(key, work);
			}
			return (asked, pixels(&thumbnail, side));
		}
		if let Some(refusal) = remembered {
			asked.via = "remembered";
			return (asked, Err(refusal));
		}
		let drive = Arc::clone(&work.drive);
		let mut waiter = match self.lane.join(key, work) {
			Joined::Waiting(waiter) => waiter,
			Joined::Refused(refusal) => {
				asked.via = "refused";
				return (asked, Err(refusal));
			}
		};
		let waited = timeout_at(until, waiter.made.wait_for(Option::is_some))
			.await
			.map(|made| made.ok().and_then(|made| made.clone()));
		let made = match waited {
			// A job is dropped only once it says how it ended.
			Ok(made) => made,
			Err(_) => match waiter.promise() {
				Left::Promised => {
					asked.via = "pending";
					return self.pending(asked, bytes, side, &drive).await;
				}
				// It ended as the deadline passed: what it made is answered.
				Left::Ended => waiter.made(),
			},
		};
		asked.via = "made";
		match made {
			Some(Ok(thumbnail)) => (asked, pixels(&thumbnail, side)),
			Some(Err(refusal)) => (asked, Err(refusal)),
			None => (asked, Err(ThumbnailRefusal::Failed)),
		}
	}

	/// The answer for a request whose thumbnail the background goes on making: Pending, unless a
	/// size of the same bytes was kept meanwhile, which is answered instead, as Explorer would
	/// replace a thumbnail it shows with the generic icon.
	async fn pending(
		&self,
		mut asked: Asked,
		bytes: Bytes,
		side: u32,
		drive: &D,
	) -> (Asked, Result<Thumbnail, ThumbnailRefusal>) {
		match self.best_kept(bytes, bucket(side), drive).await {
			Some((_, thumbnail)) => {
				asked.via = "kept meanwhile";
				(asked, pixels(&thumbnail, side))
			}
			None => (asked, Err(ThumbnailRefusal::Pending)),
		}
	}

	/// The best thumbnail of `bytes` kept for a request made at `made_at`, and the side it was
	/// made at: the one made at `made_at`, else the smallest larger one, which is scaled down,
	/// else the largest smaller one, which is answered as it is (File Explorer scales it).
	async fn best_kept(
		&self,
		bytes: Bytes,
		made_at: u32,
		drive: &D,
	) -> Option<(u32, Arc<FittedThumbnail>)> {
		let larger = BUCKETS.into_iter().filter(|side| *side > made_at);
		let smaller = BUCKETS.into_iter().rev().filter(|side| *side < made_at);
		for side in std::iter::once(made_at).chain(larger).chain(smaller) {
			if let Some(thumbnail) = self.lane.kept(&(bytes, side), drive).await {
				return Some((side, thumbnail));
			}
		}
		None
	}

	/// The item's tag in the log: of what identifies it across renames (the drive file's stable
	/// id, the local file's id), never its path.
	fn item_tag(plan: &Plan) -> Tag {
		match plan {
			Plan::Remote { id, .. } => Tag::of(&id.0),
			Plan::Local { id, .. } => Tag::of(id),
		}
	}
}

impl<L: LocalFs, D: Drive> Lane<L, D> {
	fn state(&self) -> MutexGuard<'_, LaneState<L, D>> {
		self.state.lock().unwrap_or_else(PoisonError::into_inner)
	}

	/// The served root, its file system, its drive and its generation, if one is served.
	fn target(&self) -> Option<(RootDir, Arc<L>, Arc<D>, u64)> {
		let state = self.state();
		state.target.as_ref().map(|target| {
			(
				target.root.clone(),
				Arc::clone(&target.local),
				Arc::clone(&target.drive),
				state.generation,
			)
		})
	}

	/// Serves `target`, or none: every job is cancelled at once, and its waiters answered
	/// [`ThumbnailRefusal::SignedOut`]; what was remembered and kept goes.
	fn reset(&self, target: Option<Target<L, D>>) {
		let mut state = self.state();
		state.target = target;
		state.generation += 1;
		for (_, job) in state.jobs.drain() {
			if let Run::Running { task, .. } = &job.run {
				task.abort();
			}
			job.made
				.send_replace(Some(Err(ThumbnailRefusal::SignedOut)));
		}
		state.queued.clear();
		state.kept.clear();
		state.verdicts.clear();
		if let Some(sender) = state.burst.sender.take() {
			sender.abort();
		}
		state.burst = Burst::new();
	}

	/// The settled refusal remembered for `key`, if it is recent enough.
	fn remembered(&self, key: &JobKey) -> Option<ThumbnailRefusal> {
		let mut state = self.state();
		let (refusal, at) = *state.verdicts.get(key)?;
		if at.elapsed() < self.remember {
			return Some(refusal);
		}
		state.verdicts.remove(key);
		None
	}

	/// Remembers the settled `refusal` for `key`, forgetting the expired ones, and the oldest past
	/// [`MAX_VERDICTS`].
	fn remember(&self, state: &mut LaneState<L, D>, key: JobKey, refusal: ThumbnailRefusal) {
		if state.verdicts.len() >= MAX_VERDICTS {
			state
				.verdicts
				.retain(|_, (_, at)| at.elapsed() < self.remember);
		}
		if state.verdicts.len() >= MAX_VERDICTS
			&& let Some(oldest) = state
				.verdicts
				.iter()
				.min_by_key(|(_, (_, at))| *at)
				.map(|(key, _)| *key)
		{
			state.verdicts.remove(&oldest);
		}
		state.verdicts.insert(key, (refusal, Instant::now()));
	}

	/// The thumbnail `key` names, if one is kept: the cache's for a drive file's version, read
	/// from disk, or one of local bytes the background finished. A cache that cannot be read keeps
	/// nothing.
	async fn kept(&self, key: &JobKey, drive: &D) -> Option<Arc<FittedThumbnail>> {
		match key {
			(Bytes::Remote { version }, made_at) => {
				match drive.cached_fitted_thumbnail(*version, *made_at).await {
					Ok(kept) => kept.map(Arc::new),
					Err(error) => {
						debug!(%error, "reading the kept thumbnail failed");
						None
					}
				}
			}
			(Bytes::Local { .. }, _) => self.kept_local(key),
		}
	}

	/// The thumbnail of local bytes `key` names, if the background finished it.
	fn kept_local(&self, key: &JobKey) -> Option<Arc<FittedThumbnail>> {
		self.state()
			.kept
			.iter()
			.find(|(kept, _)| kept == key)
			.map(|(_, thumbnail)| Arc::clone(thumbnail))
	}

	/// Joins the request for `key` to the job making it, running or queued, or starts one with
	/// `work`: in a slot free now, or queued for one at the newest end. A queued job asked for again
	/// moves to the newest end, so the item asked for now starts next. With every slot held and
	/// the queue full, nothing is started and the request is refused [`ThumbnailRefusal::Busy`]
	/// at once: waiting would hold up File Explorer's view for nothing.
	fn join(self: &Arc<Self>, key: JobKey, work: Arc<Work<L, D>>) -> Joined<L, D> {
		let mut state = self.state();
		if state.generation != work.generation {
			return Joined::Refused(ThumbnailRefusal::SignedOut);
		}
		if let Some(job) = state.jobs.get_mut(&key) {
			let waiter = self.waiter(key, job);
			if matches!(job.run, Run::Queued(_)) {
				self.enqueue(&mut state, key);
			}
			return Joined::Waiting(waiter);
		}
		let Some(mut job) = self.new_job(&mut state, key, work, false) else {
			return Joined::Refused(ThumbnailRefusal::Busy);
		};
		let waiter = self.waiter(key, &mut job);
		self.insert(&mut state, key, job);
		Joined::Waiting(waiter)
	}

	/// Promises File Explorer to tell it once the thumbnail of `key` is made, as the request was
	/// answered from a smaller kept size: a job making `key` already is promised, else one is
	/// started with `work`, or queued. With every slot held and the queue full, nothing is started
	/// or promised.
	fn queue(self: &Arc<Self>, key: JobKey, work: Arc<Work<L, D>>) {
		let mut state = self.state();
		if state.generation != work.generation {
			return;
		}
		if let Some(job) = state.jobs.get_mut(&key) {
			job.promised = true;
			return;
		}
		if let Some(job) = self.new_job(&mut state, key, work, true) {
			self.insert(&mut state, key, job);
		}
	}

	/// A new job making `key` with `work`, promised or not, running in a slot free now or else
	/// queued; `None` with every slot held and the queue full.
	fn new_job(
		self: &Arc<Self>,
		state: &mut LaneState<L, D>,
		key: JobKey,
		work: Arc<Work<L, D>>,
		promised: bool,
	) -> Option<Job<L, D>> {
		let slot = Arc::clone(&self.slots).try_acquire_owned().ok();
		if slot.is_none() && state.queued.len() >= self.queued_max {
			debug!("every slot is held and the queue is full; the thumbnail is not made");
			return None;
		}
		let id = state.next_job;
		state.next_job += 1;
		let run = match slot {
			Some(slot) => self.run(key, id, work, slot, promised),
			None => Run::Queued(work),
		};
		Some(Job {
			id,
			made: watch::Sender::new(None),
			run,
			waiting: 0,
			promised,
		})
	}

	/// Records the new `job` for `key`, queuing it when it waits for a slot.
	fn insert(self: &Arc<Self>, state: &mut LaneState<L, D>, key: JobKey, job: Job<L, D>) {
		let queued = matches!(job.run, Run::Queued(_));
		state.jobs.insert(key, job);
		if queued {
			self.enqueue(state, key);
		}
	}

	/// A waiter on `job`, which makes `key`.
	fn waiter(self: &Arc<Self>, key: JobKey, job: &mut Job<L, D>) -> Waiter<L, D> {
		job.waiting += 1;
		Waiter {
			made: job.made.subscribe(),
			leave: Leave {
				lane: Arc::clone(self),
				key,
				id: job.id,
				armed: true,
			},
		}
	}

	/// Starts making `work`, the job `id` for `key`, in a task of its own holding `slot`;
	/// `background` when no request waits for it, for the log.
	fn run(
		self: &Arc<Self>,
		key: JobKey,
		id: u64,
		work: Arc<Work<L, D>>,
		slot: OwnedSemaphorePermit,
		background: bool,
	) -> Run<L, D> {
		let lane = Arc::clone(self);
		let making = Arc::clone(&work);
		debug!(
			item = %work.tag,
			made_at = work.made_at,
			bytes = work.plan.kind(),
			background,
			"making a thumbnail"
		);
		let task = self.runtime.spawn(async move {
			// A flag of its own each run: dropping a run's future, as an abort does, sets it.
			let cancel = Arc::new(AtomicBool::new(false));
			let finished = AssertUnwindSafe(making.make(&cancel))
				.catch_unwind()
				.await
				.unwrap_or_else(|_| {
					// The panic's message went to the log through the panic hook.
					warn!("making a thumbnail panicked");
					Finished::Refused(ThumbnailRefusal::Failed)
				});
			if lane.finish(key, id, finished, &making) {
				lane.tell(&making).await;
			}
		});
		Run::Running {
			task: task.abort_handle(),
			_slot: slot,
		}
	}

	/// Ends the job `id` for `key` with `finished`, unless it was dropped meanwhile: tells its
	/// waiters, remembers a settled refusal, and whether File Explorer is to be told the item
	/// changed ([`Lane::tell`]): for a thumbnail made of a promised job, whether or not a request
	/// waiting on it is answered with it too (the ask the notification may bring meanwhile is
	/// answered from what is kept). Keeps such a thumbnail of local bytes for the ask that
	/// follows. Its slot goes to the newest queued job: a job gives its slot back nowhere else.
	fn finish(
		self: &Arc<Self>,
		key: JobKey,
		id: u64,
		finished: Finished,
		work: &Work<L, D>,
	) -> bool {
		let mut state = self.state();
		if state.jobs.get(&key).is_none_or(|job| job.id != id) {
			// Dropped by a stop.
			return false;
		}
		let Some(job) = state.jobs.remove(&key) else {
			return false;
		};
		let tell = job.promised;
		let made = match finished {
			Finished::Made(thumbnail) => {
				if tell && matches!(work.plan, Plan::Local { .. }) {
					state.kept.push_back((key, Arc::clone(&thumbnail)));
					if state.kept.len() > MAX_KEPT_LOCAL {
						state.kept.pop_front();
					}
				}
				Ok(thumbnail)
			}
			Finished::Settled(refusal) => {
				self.remember(&mut state, key, refusal);
				Err(refusal)
			}
			Finished::Refused(refusal) => Err(refusal),
		};
		let tell = tell && made.is_ok();
		job.made.send_replace(Some(made));
		// Its slot with it.
		drop(job);
		self.pump(&mut state);
		tell
	}

	/// Tells File Explorer the item `work` made a thumbnail of changed, so it asks again, once the
	/// thumbnail is kept where the next ask looks first, and while the root it was made for is
	/// still served. One the cache did not keep is not told: the ask that followed would find
	/// nothing kept, be answered Pending again and be told again, a loop with no end.
	async fn tell(self: &Arc<Self>, work: &Work<L, D>) {
		let key = (*work.plan.key(), work.made_at);
		if self.kept(&key, &work.drive).await.is_none() {
			debug!(
				"a thumbnail finished in the background was not kept; not telling File Explorer"
			);
			return;
		}
		{
			let mut state = self.state();
			if state.generation != work.generation {
				return;
			}
			self.burst_tell(&mut state, key.0);
		}
		debug!(
			item = %work.tag,
			made_at = work.made_at,
			"a thumbnail was finished in the background; telling File Explorer"
		);
		let (local, at) = (Arc::clone(&work.local), work.at.clone());
		// Not awaited: a hint, which may block.
		drop(
			self.runtime
				.spawn_blocking(move || local.notify_item_updated(&at)),
		);
	}

	/// Notes that a request for the item `work` makes a thumbnail of was answered with one, from a
	/// foreground request: owed a notification of its own once a notification for another item the
	/// background finished goes out within [`BURST_WINDOW`] of the answer, before or after it
	/// ([`Lane::burst_tell`]). An item version told of, or owed, already is left as it is.
	fn answered(self: &Arc<Self>, work: &Work<L, D>) {
		let mut state = self.state();
		if state.generation != work.generation {
			return;
		}
		let bytes = *work.plan.key();
		if state.burst.told_or_owed(&bytes) {
			return;
		}
		let now = Instant::now();
		let item = Answered {
			bytes,
			at: work.at.clone(),
			local: Arc::clone(&work.local),
			generation: work.generation,
			when: now,
		};
		let burst = &mut state.burst;
		burst
			.answered
			.retain(|held| held.bytes != bytes && now.duration_since(held.when) <= BURST_WINDOW);
		if burst
			.last_tell
			.is_some_and(|at| now.duration_since(at) <= BURST_WINDOW)
		{
			push_bounded(&mut burst.owed, item);
			self.send_owed(&mut state);
		} else {
			push_bounded(&mut burst.answered, item);
		}
	}

	/// Notes that File Explorer is told the background finished the item version `bytes`: it is
	/// told of, owed nothing more, and every other item answered within [`BURST_WINDOW`] before is
	/// owed a notification of its own.
	fn burst_tell(self: &Arc<Self>, state: &mut LaneState<L, D>, bytes: Bytes) {
		let now = Instant::now();
		let burst = &mut state.burst;
		burst.told.insert(bytes, ());
		burst.last_tell = Some(now);
		burst.owed.retain(|item| item.bytes != bytes);
		for item in std::mem::take(&mut burst.answered) {
			if item.bytes != bytes && now.duration_since(item.when) <= BURST_WINDOW {
				push_bounded(&mut burst.owed, item);
			}
		}
		if !burst.owed.is_empty() {
			self.send_owed(state);
		}
	}

	/// Starts the task that sends what is owed once the burst settles, unless one waits already.
	fn send_owed(self: &Arc<Self>, state: &mut LaneState<L, D>) {
		if state.burst.sender.is_some() {
			return;
		}
		let lane = Arc::clone(self);
		let sender = self.runtime.spawn(async move { lane.follow_up().await });
		state.burst.sender = Some(sender.abort_handle());
	}

	/// Sends each item owed one its notification, once no notification for an item the background
	/// finished went out for [`BURST_SETTLE`]: one per item version, never to one told of since, and
	/// none for a root no longer served. A stop aborts it ([`Lane::reset`]).
	async fn follow_up(self: Arc<Self>) {
		let owed = loop {
			let due = self.state().burst.last_tell.map(|at| at + BURST_SETTLE);
			if let Some(due) = due {
				sleep_until(due).await;
			}
			let mut state = self.state();
			if state
				.burst
				.last_tell
				.is_some_and(|at| at + BURST_SETTLE > Instant::now())
			{
				continue;
			}
			let generation = state.generation;
			let burst = &mut state.burst;
			let mut owed = Vec::with_capacity(burst.owed.len());
			for item in std::mem::take(&mut burst.owed) {
				if item.generation == generation && burst.told.get(&item.bytes).is_none() {
					burst.told.insert(item.bytes, ());
					owed.push(item);
				}
			}
			burst.sender = None;
			break owed;
		};
		for Answered { at, local, .. } in owed {
			debug!(
				"an item answered inside a burst of notifications for others; telling File Explorer"
			);
			// Not awaited: a hint, which may block.
			drop(
				self.runtime
					.spawn_blocking(move || local.notify_item_updated(&at)),
			);
		}
	}

	/// The request waiting on the job `id` for `key` stops waiting, to be answered Pending: the job
	/// goes on where it is, running in its slot with what it did so far or queued, promised, so
	/// File Explorer is told once it is made. [`Left::Ended`] when it ended first.
	fn promise(&self, key: JobKey, id: u64) -> Left {
		let mut state = self.state();
		let Some(job) = state.jobs.get_mut(&key).filter(|job| job.id == id) else {
			return Left::Ended;
		};
		job.waiting = job.waiting.saturating_sub(1);
		job.promised = true;
		if job.waiting == 0 && matches!(job.run, Run::Running { .. }) {
			debug!("the thumbnail goes on in the background");
		}
		Left::Promised
	}

	/// The request waiting on the job `id` for `key` stops waiting with nothing promised, as a
	/// cancelled one does: a running job goes on, holding its slot, as what it read so far would
	/// be thrown away; a queued one that no request waits on any more and none was promised leaves
	/// the queue, as nothing of it started.
	fn leave(&self, key: JobKey, id: u64) {
		let mut state = self.state();
		let Some(job) = state.jobs.get_mut(&key).filter(|job| job.id == id) else {
			return;
		};
		job.waiting = job.waiting.saturating_sub(1);
		if job.waiting == 0
			&& !job.promised
			&& matches!(job.run, Run::Queued(_))
			&& let Some(job) = state.jobs.remove(&key)
		{
			state.queued.retain(|queued| *queued != key);
			job.made.send_replace(Some(Err(ThumbnailRefusal::Busy)));
		}
	}

	/// Puts `key`, queued, at the newest end of the queue, then starts what the free slots have
	/// room for. Nothing queued is ever dropped but by a stop or, wanted by nothing, by
	/// [`Lane::leave`]: what may be queued is bounded where it is queued ([`Lane::new_job`]).
	fn enqueue(self: &Arc<Self>, state: &mut LaneState<L, D>, key: JobKey) {
		state.queued.retain(|queued| *queued != key);
		state.queued.push_back(key);
		self.pump(state);
	}

	/// Starts the newest queued jobs while a slot is free.
	///
	/// Newest first on purpose, for scrolling: the items File Explorer asks for last are the ones
	/// on screen now, and a job queued earlier was most often asked for by a view since scrolled
	/// past. The cost is that nothing ages: a job already promised to File Explorer (its request
	/// answered Pending) waits for as long as newer asks keep every slot busy, and its tile stays
	/// blank until traffic eases and the queue drains back to it. Only a stop drops it meanwhile
	/// (see [`Lane::enqueue`]).
	fn pump(self: &Arc<Self>, state: &mut LaneState<L, D>) {
		while let Some(&key) = state.queued.back() {
			let Ok(slot) = Arc::clone(&self.slots).try_acquire_owned() else {
				return;
			};
			state.queued.pop_back();
			let Some(job) = state.jobs.get_mut(&key) else {
				continue;
			};
			let Run::Queued(work) = &job.run else {
				continue;
			};
			let (work, background) = (Arc::clone(work), job.waiting == 0);
			job.run = self.run(key, job.id, work, slot, background);
		}
	}
}

impl<L: LocalFs, D: Drive> Work<L, D> {
	/// Makes the thumbnail: from the local bytes, opened without asking the provider for any, or
	/// from the drive's, which the cache keeps. Dropping the future stops the decode at its next
	/// chunk.
	async fn make(&self, cancel: &Arc<AtomicBool>) -> Finished {
		let outcome = match &self.plan {
			Plan::Local { path, id, .. } => {
				let name = path
					.name()
					.map(|name| name.as_str().to_owned())
					.unwrap_or_default();
				let opened = {
					let (local, path, id) = (Arc::clone(&self.local), path.clone(), *id);
					blocking(move || local.open_on_disk(&path, id)).await
				};
				let source = match opened {
					Ok(source) => source,
					Err(LocalError::Busy) => return Finished::Refused(ThumbnailRefusal::Busy),
					Err(error) => {
						debug!(%error, "the local file changed before its thumbnail");
						return Finished::Refused(ThumbnailRefusal::Failed);
					}
				};
				self.drive
					.fitted_thumbnail_of_local(source, &name, self.made_at, cancel)
					.await
			}
			Plan::Remote { id, .. } => self.drive.fitted_thumbnail(id, self.made_at, cancel).await,
		};
		match outcome {
			Ok(FittedOutcome::Thumbnail(thumbnail)) => Finished::Made(Arc::new(thumbnail)),
			Ok(FittedOutcome::Unsupported | FittedOutcome::Corrupt(_)) => {
				Finished::Settled(ThumbnailRefusal::Unsupported)
			}
			Ok(FittedOutcome::OverBudget) => Finished::Settled(ThumbnailRefusal::TooLarge),
			Err(error) => {
				debug!(%error, "the drive could not make the thumbnail");
				Finished::Refused(cache_refusal(&error))
			}
		}
	}
}

/// The item at `path` in the root at `root`, from its spelling alone: nothing is read.
fn locate(root: &RootDir, path: &Path) -> Result<RootPath, ThumbnailRefusal> {
	let text = path.to_str().ok_or(ThumbnailRefusal::NotInRoot)?;
	let components = below_root(root.as_str(), text, HOST).ok_or(ThumbnailRefusal::NotInRoot)?;
	if components.is_empty() {
		return Err(ThumbnailRefusal::Directory);
	}
	// A component Windows cannot hold under its spelling is no placeholder's.
	RootPath::parse(components).map_err(|_| ThumbnailRefusal::NotAPlaceholder)
}

/// Where the bytes of the item at `at` come from, read through `local` without opening the
/// item's data: a file all on disk from its local bytes (an ordinary file the provider has not
/// taken in, or a placeholder, edited locally or not), a placeholder of a drive file in sync from
/// the drive.
async fn plan<L: LocalFs>(local: &Arc<L>, at: &RootPath) -> Result<Plan, ThumbnailRefusal> {
	let entry = {
		let local = Arc::clone(local);
		let at = at.clone();
		blocking(move || local.inspect(&at)).await
	};
	let local_bytes = |id: LocalFileId, size: u64, modified: i64| Plan::Local {
		path: at.clone(),
		id,
		key: Bytes::Local { id, size, modified },
	};
	match entry {
		Ok(LocalEntry::Missing) | Err(LocalError::NotFound) => {
			debug!("the item asked for is gone");
			Err(ThumbnailRefusal::Failed)
		}
		Err(LocalError::Busy) => Err(ThumbnailRefusal::Busy),
		Err(error) => {
			debug!(%error, "reading the item's state failed");
			Err(ThumbnailRefusal::Failed)
		}
		Ok(LocalEntry::Unsupported(_)) => Err(ThumbnailRefusal::NotAPlaceholder),
		Ok(
			LocalEntry::Plain {
				kind: LocalKind::Dir { .. },
				..
			}
			| LocalEntry::Placeholder {
				kind: LocalKind::Dir { .. },
				..
			},
		) => Err(ThumbnailRefusal::Directory),
		Ok(LocalEntry::Plain {
			kind: LocalKind::File(stamp),
			id,
		}) => Ok(local_bytes(id, stamp.size, stamp.modified)),
		Ok(LocalEntry::Placeholder {
			identity: Err(_), ..
		}) => Err(ThumbnailRefusal::NotAPlaceholder),
		Ok(LocalEntry::Placeholder {
			kind: LocalKind::File(stamp),
			id,
			identity: Ok(identity),
			state,
		}) => {
			let on_disk = state.on_disk() >= stamp.size;
			match identity {
				PlaceholderIdentity::File { version, .. } if !on_disk => {
					if state.in_sync() {
						let id = identity
							.cache_id()
							.ok_or(ThumbnailRefusal::NotAPlaceholder)?;
						Ok(Plan::Remote {
							id,
							key: Bytes::Remote { version },
						})
					} else {
						// Edited locally and not all on disk: neither side holds the bytes the
						// file shows, and reading them would hydrate it.
						debug!("an edited file not all on disk has no thumbnail");
						Err(ThumbnailRefusal::Failed)
					}
				}
				PlaceholderIdentity::File { .. } | PlaceholderIdentity::Detached { .. }
					if on_disk =>
				{
					Ok(local_bytes(id, stamp.size, stamp.modified))
				}
				PlaceholderIdentity::File { .. }
				| PlaceholderIdentity::Detached { .. }
				| PlaceholderIdentity::Root { .. }
				| PlaceholderIdentity::Dir { .. } => Err(ThumbnailRefusal::NotAPlaceholder),
			}
		}
	}
}

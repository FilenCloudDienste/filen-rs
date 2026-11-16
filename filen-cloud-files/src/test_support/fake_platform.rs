//! A fake platform over a [`FakeLocal`] root, and a drive opener handing out one [`FakeDrive`]:
//! the seams the helper's command loop runs on in the tests.

use std::{
	collections::{HashMap, HashSet},
	fmt,
	path::{Path, PathBuf},
	sync::{
		Arc, Condvar, Mutex, MutexGuard, PoisonError,
		atomic::{AtomicUsize, Ordering},
	},
	time::{Duration, SystemTime},
};

use filen_types::fs::Uuid;

use crate::{
	auth::{KeyAccount, KeyStore, KeyStoreError, SealingKey, SessionStore},
	branding::Branding,
	dispatch::Dispatcher,
	drive::Drive,
	helper::{
		BrandingScope, Cleared, DriveOpener, ExistingRoot, Platform, PlatformError, PlatformFailed,
		PlatformOp, RootRefusal, UserId,
	},
	local::LocalEngine,
	os::{WatchSignal, WatchSink},
	sealed,
	test_support::{FakeDrive, FakeEntry, FakeLocal, MemoryKeyStore},
};

/// The time the fake platform's clock always reads, in seconds since the Unix epoch: 2026-11-12
/// 12:00 UTC.
const FAKE_NOW_SECS: u64 = 1_794_484_800;

/// What the fake platform was asked to do, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlatformCall {
	/// A root was registered for an account.
	Register { root: PathBuf, account_root: Uuid },
	/// A root was connected to serve.
	Connect(PathBuf),
	/// A root was connected to refuse every request.
	ConnectRefusing(PathBuf),
	/// A root was disconnected.
	Disconnect(PathBuf),
	/// A root was unregistered.
	Unregister(PathBuf),
	/// A watcher started.
	Watch,
	/// A watcher was dropped.
	Unwatch,
	/// The unregistered root was moved aside to the path.
	SetAside(PathBuf),
	/// Nothing is left at the unregistered root's path.
	RootRemoved(PathBuf),
}

/// A platform step an unregister takes, where a test kills the helper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FakeStep {
	/// Connecting the root to refuse every request.
	ConnectRefusing,
	/// Disconnecting it.
	Disconnect,
	/// The OS's unregister.
	Unregister,
	/// Removing the unregistered root's empty directories.
	Clear,
	/// Renaming the root aside.
	SetAside,
	/// Deleting the sealing key.
	KeyDelete,
}

/// Where a test kills the helper: just before a step, or just after it took effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillPoint {
	/// Before the step does anything.
	Before(FakeStep),
	/// Once the step took effect, before the helper sees its result.
	After(FakeStep),
}

/// A registered root of the fake platform.
#[derive(Debug)]
pub struct FakeRegistered(PathBuf);

/// A connected root of the fake platform.
#[derive(Debug)]
pub struct FakeConnected(PathBuf);

/// A running fake watcher; dropping it stops it.
pub struct FakeWatcher {
	id: u64,
	state: Arc<Mutex<PlatformState>>,
}

impl Drop for FakeWatcher {
	fn drop(&mut self) {
		let mut state = lock(&self.state);
		state.watchers.retain(|watcher| watcher.id != self.id);
		state.calls.push(PlatformCall::Unwatch);
	}
}

struct LiveWatcher {
	id: u64,
	sink: WatchSink,
	/// Delivers nothing, as a watcher `notify` stopped silently.
	deaf: bool,
}

#[derive(Default)]
struct PlatformState {
	/// The roots registered, with their account root; `None` for another provider's.
	registered: HashMap<PathBuf, Option<Uuid>>,
	connected: HashSet<PathBuf>,
	calls: Vec<PlatformCall>,
	watchers: Vec<LiveWatcher>,
	next_watcher: u64,
	fail_watch_starts: u32,
	watch_gate: Option<WatchGate>,
	refusal: Option<RootRefusal>,
	fail_connect: bool,
	panic_check_root: bool,
	elevated: bool,
	keys: HashMap<String, Arc<MemoryKeyStore>>,
	/// What each path the root was renamed to holds; also paths taken by something else.
	asides: HashMap<PathBuf, Vec<FakeEntry>>,
	fail_set_asides: u32,
	fail_unregisters: u32,
	kill: Option<KillPoint>,
	/// What a test runs when an unregister reaches a point, once each.
	hooks: Vec<(KillPoint, Box<dyn FnOnce() + Send>)>,
	/// The machine-wide sync root list entries, by root: whose root each names.
	machine_branding: HashMap<PathBuf, UserId>,
	/// The per-user navigation-pane entries, by root.
	user_branding: HashMap<PathBuf, FakeBranding>,
	branding_calls: Vec<BrandingCall>,
	fail_brands: u32,
	fail_unbrands: u32,
	/// Whether the root was connected at each brand, in order.
	connected_when_branded: Vec<bool>,
}

/// What a fake navigation-pane entry shows, and for whom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeBranding {
	/// Whose root it shows.
	pub user: UserId,
	/// What it shows.
	pub branding: Branding,
}

/// What the fake platform was asked to show in File Explorer, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrandingCall {
	/// A root was shown.
	Brand {
		root: PathBuf,
		user: UserId,
		branding: Branding,
	},
	/// A root's entries were removed, within a scope.
	Unbrand {
		root: PathBuf,
		user: UserId,
		scope: BrandingScope,
	},
}

/// Runs what `state` was told to run at `point`, then panics, as a killed helper stops, when it
/// was told to kill there; once.
fn kill_at(state: &Mutex<PlatformState>, point: KillPoint) {
	let hooks: Vec<Box<dyn FnOnce() + Send>> = {
		let mut state = lock(state);
		let (due, kept) = std::mem::take(&mut state.hooks)
			.into_iter()
			.partition(|(at, _)| *at == point);
		state.hooks = kept;
		due.into_iter().map(|(_, hook)| hook).collect()
	};
	for hook in hooks {
		hook();
	}
	let mut state = lock(state);
	if state.kill == Some(point) {
		state.kill = None;
		drop(state);
		panic!("the fake platform killed the helper at {point:?}");
	}
}

fn lock(state: &Mutex<PlatformState>) -> MutexGuard<'_, PlatformState> {
	state.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Default)]
struct GateState {
	/// Starts that reached the gate.
	entered: usize,
	open: bool,
}

/// Holds watcher starts until it opens, as an OS slow to start a watcher does. Clones share one
/// gate.
#[derive(Clone, Default)]
pub struct WatchGate(Arc<(Mutex<GateState>, Condvar)>);

impl WatchGate {
	fn gate(&self) -> MutexGuard<'_, GateState> {
		self.0.0.lock().unwrap_or_else(PoisonError::into_inner)
	}

	/// Blocks until a start reached the gate.
	pub fn wait_entered(&self) {
		let mut gate = self.gate();
		while gate.entered == 0 {
			gate = self.0.1.wait(gate).unwrap_or_else(PoisonError::into_inner);
		}
	}

	/// Lets every held start and every later one through.
	pub fn open(&self) {
		self.gate().open = true;
		self.0.1.notify_all();
	}

	/// Blocks the calling start until the gate opens.
	fn pass(&self) {
		let mut gate = self.gate();
		gate.entered += 1;
		self.0.1.notify_all();
		while !gate.open {
			gate = self.0.1.wait(gate).unwrap_or_else(PoisonError::into_inner);
		}
	}
}

/// The fake platform: one [`FakeLocal`] stands for whichever root is served, sync roots are
/// recorded by path, and every user's sealing key is kept in memory. Clones share one state.
#[derive(Clone)]
pub struct FakePlatform {
	local: FakeLocal,
	// Reason for the Arc: the test and the watchers handed out observe one state.
	state: Arc<Mutex<PlatformState>>,
}

impl fmt::Debug for FakePlatform {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("FakePlatform").finish_non_exhaustive()
	}
}

impl FakePlatform {
	/// A platform whose sync root is `local`.
	pub fn new(local: FakeLocal) -> Self {
		Self {
			local,
			state: Arc::new(Mutex::new(PlatformState::default())),
		}
	}

	fn state(&self) -> MutexGuard<'_, PlatformState> {
		lock(&self.state)
	}

	/// The root's file system.
	pub fn local(&self) -> &FakeLocal {
		&self.local
	}

	/// What the platform was asked to do, in order.
	pub fn calls(&self) -> Vec<PlatformCall> {
		self.state().calls.clone()
	}

	/// The account root `root` is registered for, `Some(None)` when another provider has it.
	pub fn registration(&self, root: &Path) -> Option<Option<Uuid>> {
		self.state().registered.get(root).copied()
	}

	/// Registers `root` as an earlier run of this provider left it, for `account_root`.
	pub fn register_left_over(&self, root: &Path, account_root: Uuid) {
		self.state()
			.registered
			.insert(root.to_path_buf(), Some(account_root));
	}

	/// Registers `root` for another provider.
	pub fn register_foreign(&self, root: &Path) {
		self.state().registered.insert(root.to_path_buf(), None);
	}

	/// Whether `root` is connected.
	pub fn is_connected(&self, root: &Path) -> bool {
		self.state().connected.contains(root)
	}

	/// Refuses every root with `refusal`.
	pub fn refuse_roots(&self, refusal: RootRefusal) {
		self.state().refusal = Some(refusal);
	}

	/// Fails every later connect.
	pub fn fail_connect(&self) {
		self.state().fail_connect = true;
	}

	/// Makes every later root check panic, as a bug in a request's handling would.
	pub fn panic_on_check_root(&self) {
		self.state().panic_check_root = true;
	}

	/// Makes the process count as elevated.
	pub fn set_elevated(&self, elevated: bool) {
		self.state().elevated = elevated;
	}

	/// Fails the next `count` watcher starts.
	pub fn fail_watch_starts(&self, count: u32) {
		self.state().fail_watch_starts = count;
	}

	/// Holds every later watcher start, before it does anything, until the gate returned opens.
	pub fn hold_watch_starts(&self) -> WatchGate {
		let gate = WatchGate::default();
		self.state().watch_gate = Some(gate.clone());
		gate
	}

	/// How many watchers run.
	pub fn live_watchers(&self) -> usize {
		self.state().watchers.len()
	}

	/// How many watchers were started.
	pub fn watch_starts(&self) -> usize {
		self.state()
			.calls
			.iter()
			.filter(|call| **call == PlatformCall::Watch)
			.count()
	}

	/// Hands every watcher that still delivers the root's watcher events since the last call.
	pub fn deliver(&self) {
		let events = self.local.take_watch_events();
		let sinks: Vec<WatchSink> = self
			.state()
			.watchers
			.iter()
			.filter(|watcher| !watcher.deaf)
			.map(|watcher| Arc::clone(&watcher.sink))
			.collect();
		for event in events {
			for sink in &sinks {
				sink(WatchSignal::Event(event.clone()));
			}
		}
	}

	/// Makes the newest watcher report a failure.
	pub fn fail_watcher(&self) {
		let sink = self
			.state()
			.watchers
			.last()
			.map(|watcher| Arc::clone(&watcher.sink));
		if let Some(sink) = sink {
			sink(WatchSignal::Failed("the fake watcher failed".to_owned()));
		}
	}

	/// Makes every running watcher stop delivering, silently, as `notify` does on Windows.
	pub fn deafen_watchers(&self) {
		for watcher in &mut self.state().watchers {
			watcher.deaf = true;
		}
	}

	/// The in-memory key store of `account`.
	pub fn keys(&self, account: &str) -> Arc<MemoryKeyStore> {
		Arc::clone(self.state().keys.entry(account.to_owned()).or_default())
	}

	/// Kills the helper, by a panic, at `point` of the next unregister that reaches it.
	pub fn kill_at(&self, point: KillPoint) {
		self.state().kill = Some(point);
	}

	/// Whether the helper was told to be killed and has not reached the point yet.
	pub fn kill_pending(&self) -> bool {
		self.state().kill.is_some()
	}

	/// What the root renamed to `aside` holds, if it was.
	pub fn aside(&self, aside: &Path) -> Option<Vec<FakeEntry>> {
		self.state().asides.get(aside).cloned()
	}

	/// Every path the root was renamed to, and every path taken by [`FakePlatform::occupy`].
	pub fn asides(&self) -> Vec<PathBuf> {
		let mut asides: Vec<PathBuf> = self.state().asides.keys().cloned().collect();
		asides.sort();
		asides
	}

	/// Puts something at `path`, so no root is renamed there.
	pub fn occupy(&self, path: &Path) {
		self.state().asides.insert(path.to_path_buf(), Vec::new());
	}

	/// Runs `hook` when the next unregister reaches `point`.
	pub fn run_at(&self, point: KillPoint, hook: impl FnOnce() + Send + 'static) {
		self.state().hooks.push((point, Box::new(hook)));
	}

	/// Refuses the next `count` unregisters of a root, as the OS refusing one does.
	pub fn fail_unregisters(&self, count: u32) {
		self.state().fail_unregisters = count;
	}

	/// Refuses the next `count` renames of a root aside, as access denied does.
	pub fn fail_set_asides(&self, count: u32) {
		self.state().fail_set_asides = count;
	}

	fn kill_at_point(&self, point: KillPoint) {
		kill_at(&self.state, point);
	}

	/// Whose root the machine-wide sync root list entry of `root` names, if there is one.
	pub fn machine_branding(&self, root: &Path) -> Option<UserId> {
		self.state().machine_branding.get(root).copied()
	}

	/// The per-user navigation-pane entry of `root`, if there is one.
	pub fn user_branding(&self, root: &Path) -> Option<FakeBranding> {
		self.state().user_branding.get(root).cloned()
	}

	/// How many entries of either kind File Explorer holds.
	pub fn branding_entries(&self) -> usize {
		let state = self.state();
		state.machine_branding.len() + state.user_branding.len()
	}

	/// What the platform was asked to show in File Explorer, in order.
	pub fn branding_calls(&self) -> Vec<BrandingCall> {
		self.state().branding_calls.clone()
	}

	/// Leaves both entries of `user`'s root at `root`, as a run killed before removing them does.
	pub fn leave_branding(&self, root: &Path, user: UserId) {
		let mut state = self.state();
		state.machine_branding.insert(root.to_path_buf(), user);
		state.user_branding.insert(
			root.to_path_buf(),
			FakeBranding {
				user,
				branding: Branding::default(),
			},
		);
	}

	/// Fails the next `count` brands, having written the sync root list entry only, as a registry
	/// write refused part way does.
	pub fn fail_brands(&self, count: u32) {
		self.state().fail_brands = count;
	}

	/// Whether the root was connected when each brand ran, in order.
	pub fn connected_when_branded(&self) -> Vec<bool> {
		self.state().connected_when_branded.clone()
	}

	/// Fails the next `count` unbrands, having removed nothing.
	pub fn fail_unbrands(&self, count: u32) {
		self.state().fail_unbrands = count;
	}
}

impl sealed::Sealed for FakePlatform {}

/// A user's key store on the fake platform, shared with the test.
pub struct FakeKeys {
	keys: Arc<MemoryKeyStore>,
	state: Arc<Mutex<PlatformState>>,
}

impl fmt::Debug for FakeKeys {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("FakeKeys").finish_non_exhaustive()
	}
}

impl sealed::Sealed for FakeKeys {}

impl KeyStore for FakeKeys {
	fn load(&self) -> Result<Option<SealingKey>, KeyStoreError> {
		self.keys.load()
	}

	fn store(&self, key: &SealingKey) -> Result<(), KeyStoreError> {
		self.keys.store(key)
	}

	fn delete(&self) -> Result<(), KeyStoreError> {
		kill_at(&self.state, KillPoint::Before(FakeStep::KeyDelete));
		let deleted = self.keys.delete();
		kill_at(&self.state, KillPoint::After(FakeStep::KeyDelete));
		deleted
	}
}

/// A failure the fake platform was told to answer with.
#[derive(Debug, thiserror::Error)]
#[error("the fake platform was told to fail")]
struct Switched;

impl Platform for FakePlatform {
	type Local = FakeLocal;
	type Keys = FakeKeys;
	type Registered = FakeRegistered;
	type Connected = FakeConnected;
	type Watcher = FakeWatcher;

	fn platform_build(&self) -> Option<u32> {
		Some(26300)
	}

	fn check_root(&self, _root: &Path) -> Result<(), RootRefusal> {
		let state = self.state();
		assert!(
			!state.panic_check_root,
			"the fake platform was told to panic"
		);
		state.refusal.clone().map_or(Ok(()), Err)
	}

	fn existing_registration(&self, root: &Path) -> Result<Option<ExistingRoot>, PlatformError> {
		Ok(self
			.state()
			.registered
			.get(root)
			.map(|account_root| ExistingRoot {
				ours: account_root.is_some(),
				account_root: *account_root,
			}))
	}

	fn register(&self, root: &Path, account_root: Uuid) -> Result<FakeRegistered, PlatformError> {
		let mut state = self.state();
		state
			.registered
			.insert(root.to_path_buf(), Some(account_root));
		state.calls.push(PlatformCall::Register {
			root: root.to_path_buf(),
			account_root,
		});
		Ok(FakeRegistered(root.to_path_buf()))
	}

	fn local_fs(&self, _root: &Path) -> Arc<FakeLocal> {
		// Reason for the Arc: the engine's blocking calls each take a handle.
		Arc::new(self.local.clone())
	}

	fn connect<D: Drive>(
		&self,
		registered: FakeRegistered,
		_dispatcher: Arc<Dispatcher<D>>,
		_engine: Arc<LocalEngine<D, FakeLocal>>,
	) -> Result<FakeConnected, PlatformFailed<FakeRegistered>> {
		let mut state = self.state();
		if state.fail_connect {
			return Err(PlatformFailed {
				held: registered,
				error: PlatformError::Failed {
					op: PlatformOp::Connect,
					source: Box::new(Switched),
				},
			});
		}
		state.connected.insert(registered.0.clone());
		state
			.calls
			.push(PlatformCall::Connect(registered.0.clone()));
		Ok(FakeConnected(registered.0))
	}

	fn connect_refusing(
		&self,
		registered: FakeRegistered,
	) -> Result<FakeConnected, PlatformFailed<FakeRegistered>> {
		self.kill_at_point(KillPoint::Before(FakeStep::ConnectRefusing));
		{
			let mut state = self.state();
			state.connected.insert(registered.0.clone());
			state
				.calls
				.push(PlatformCall::ConnectRefusing(registered.0.clone()));
		}
		self.kill_at_point(KillPoint::After(FakeStep::ConnectRefusing));
		Ok(FakeConnected(registered.0))
	}

	fn disconnect(
		&self,
		connected: FakeConnected,
	) -> Result<FakeRegistered, PlatformFailed<FakeConnected>> {
		self.kill_at_point(KillPoint::Before(FakeStep::Disconnect));
		{
			let mut state = self.state();
			state.connected.remove(&connected.0);
			state
				.calls
				.push(PlatformCall::Disconnect(connected.0.clone()));
		}
		self.kill_at_point(KillPoint::After(FakeStep::Disconnect));
		Ok(FakeRegistered(connected.0))
	}

	fn unregister(&self, registered: FakeRegistered) -> Result<(), PlatformFailed<FakeRegistered>> {
		self.kill_at_point(KillPoint::Before(FakeStep::Unregister));
		{
			let mut state = self.state();
			if state.fail_unregisters > 0 {
				state.fail_unregisters -= 1;
				return Err(PlatformFailed {
					held: registered,
					error: PlatformError::Failed {
						op: PlatformOp::Unregister,
						source: Box::new(Switched),
					},
				});
			}
		}
		self.local.unregister_root();
		{
			let mut state = self.state();
			state.registered.remove(&registered.0);
			state.calls.push(PlatformCall::Unregister(registered.0));
		}
		self.kill_at_point(KillPoint::After(FakeStep::Unregister));
		Ok(())
	}

	fn clear_unregistered(&self, root: &Path) -> Result<Cleared, PlatformError> {
		self.kill_at_point(KillPoint::Before(FakeStep::Clear));
		let cleared = match self.local.remove_empty_dirs() {
			None => {
				self.state()
					.calls
					.push(PlatformCall::RootRemoved(root.to_path_buf()));
				Cleared::Gone
			}
			Some(left) => Cleared::Left(left),
		};
		self.kill_at_point(KillPoint::After(FakeStep::Clear));
		Ok(cleared)
	}

	fn exists(&self, path: &Path) -> bool {
		self.state().asides.iter().any(|(aside, entries)| {
			path == aside
				|| path.strip_prefix(aside).is_ok_and(|inside| {
					entries
						.iter()
						.any(|entry| Path::new(&entry.path.to_string()) == inside)
				})
		})
	}

	fn set_aside(&self, _root: &Path, aside: &Path) -> Result<(), PlatformError> {
		self.kill_at_point(KillPoint::Before(FakeStep::SetAside));
		{
			let mut state = self.state();
			if state.fail_set_asides > 0 || state.asides.contains_key(aside) {
				state.fail_set_asides = state.fail_set_asides.saturating_sub(1);
				return Err(PlatformError::Failed {
					op: PlatformOp::SetAside,
					source: Box::new(Switched),
				});
			}
			let moved = self.local.take_all();
			state.asides.insert(aside.to_path_buf(), moved);
			state
				.calls
				.push(PlatformCall::SetAside(aside.to_path_buf()));
		}
		self.kill_at_point(KillPoint::After(FakeStep::SetAside));
		Ok(())
	}

	fn watch(&self, _root: &Path, sink: WatchSink) -> Result<FakeWatcher, PlatformError> {
		let gate = self.state().watch_gate.clone();
		if let Some(gate) = gate {
			gate.pass();
		}
		let mut state = self.state();
		if state.fail_watch_starts > 0 {
			state.fail_watch_starts -= 1;
			return Err(PlatformError::Failed {
				op: PlatformOp::Watch,
				source: Box::new(Switched),
			});
		}
		let id = state.next_watcher;
		state.next_watcher += 1;
		state.watchers.push(LiveWatcher {
			id,
			sink,
			deaf: false,
		});
		state.calls.push(PlatformCall::Watch);
		Ok(FakeWatcher {
			id,
			state: Arc::clone(&self.state),
		})
	}

	fn key_store(&self, account: &KeyAccount) -> Result<FakeKeys, KeyStoreError> {
		Ok(FakeKeys {
			keys: self.keys(account.as_str()),
			state: Arc::clone(&self.state),
		})
	}

	fn elevated(&self) -> bool {
		self.state().elevated
	}

	/// Always [`FAKE_NOW_SECS`], so a test knows the date an unregister sets a root aside under.
	fn now(&self) -> SystemTime {
		SystemTime::UNIX_EPOCH + Duration::from_secs(FAKE_NOW_SECS)
	}

	fn brand(&self, root: &Path, user: UserId, branding: &Branding) -> Result<(), PlatformError> {
		let mut state = self.state();
		let connected = state.connected.contains(root);
		state.connected_when_branded.push(connected);
		state.branding_calls.push(BrandingCall::Brand {
			root: root.to_path_buf(),
			user,
			branding: branding.clone(),
		});
		state.machine_branding.insert(root.to_path_buf(), user);
		if state.fail_brands > 0 {
			state.fail_brands -= 1;
			return Err(PlatformError::Failed {
				op: PlatformOp::Brand,
				source: Box::new(Switched),
			});
		}
		state.user_branding.insert(
			root.to_path_buf(),
			FakeBranding {
				user,
				branding: branding.clone(),
			},
		);
		Ok(())
	}

	fn unbrand(
		&self,
		root: &Path,
		user: UserId,
		scope: BrandingScope,
	) -> Result<(), PlatformError> {
		let mut state = self.state();
		state.branding_calls.push(BrandingCall::Unbrand {
			root: root.to_path_buf(),
			user,
			scope,
		});
		if state.fail_unbrands > 0 {
			state.fail_unbrands -= 1;
			return Err(PlatformError::Failed {
				op: PlatformOp::Unbrand,
				source: Box::new(Switched),
			});
		}
		if state.machine_branding.get(root) == Some(&user) {
			state.machine_branding.remove(root);
		}
		if scope == BrandingScope::User
			&& state
				.user_branding
				.get(root)
				.is_some_and(|entry| entry.user == user)
		{
			state.user_branding.remove(root);
		}
		Ok(())
	}
}

/// Opens one [`FakeDrive`], the same each time, and counts the opens.
pub struct FakeOpener {
	// Reason for the Arc: the test inspects the drive the helper serves.
	drive: Arc<FakeDrive>,
	opens: AtomicUsize,
}

impl FakeOpener {
	/// An opener handing out `drive`.
	pub fn new(drive: Arc<FakeDrive>) -> Self {
		Self {
			drive,
			opens: AtomicUsize::new(0),
		}
	}

	/// How many times the drive was opened.
	pub fn opens(&self) -> usize {
		self.opens.load(Ordering::Relaxed)
	}
}

impl fmt::Debug for FakeOpener {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("FakeOpener")
			.field("opens", &self.opens())
			.finish_non_exhaustive()
	}
}

impl sealed::Sealed for FakeOpener {}

impl DriveOpener for FakeOpener {
	type Drive = FakeDrive;

	fn open(&self, _store: &SessionStore) -> Arc<FakeDrive> {
		self.opens.fetch_add(1, Ordering::Relaxed);
		Arc::clone(&self.drive)
	}
}

#[cfg(test)]
mod tests {
	use crate::{
		identity::PlaceholderIdentity,
		os::LocalEvent,
		test_support::{FakeLocal, root_path},
	};

	use super::*;

	fn recording() -> (WatchSink, Arc<Mutex<Vec<WatchSignal>>>) {
		let seen = Arc::new(Mutex::new(Vec::new()));
		let sink_seen = Arc::clone(&seen);
		let sink: WatchSink = Arc::new(move |signal| sink_seen.lock().unwrap().push(signal));
		(sink, seen)
	}

	fn platform() -> FakePlatform {
		FakePlatform::new(FakeLocal::new(PlaceholderIdentity::Root {
			root: Uuid::from_u128(0x100),
		}))
	}

	#[test]
	fn a_watcher_hands_on_the_roots_events_until_it_is_deafened_or_dropped() {
		let platform = platform();
		let (sink, seen) = recording();
		let watcher = platform.watch(Path::new("/r"), sink).unwrap();
		platform.local().user_create(&root_path("a.txt"), b"a");
		platform.deliver();
		assert_eq!(
			*seen.lock().unwrap(),
			[
				WatchSignal::Event(LocalEvent::Created(root_path("a.txt"))),
				WatchSignal::Event(LocalEvent::Modified(root_path("a.txt"))),
			]
		);
		platform.deafen_watchers();
		platform.local().user_create(&root_path("b.txt"), b"b");
		platform.deliver();
		assert_eq!(seen.lock().unwrap().len(), 2);
		drop(watcher);
		assert_eq!(platform.live_watchers(), 0);
		assert_eq!(
			platform.calls(),
			[PlatformCall::Watch, PlatformCall::Unwatch]
		);
	}

	#[test]
	fn a_failing_watcher_start_fails_as_many_times_as_told() {
		let platform = platform();
		platform.fail_watch_starts(1);
		let (sink, _seen) = recording();
		assert!(platform.watch(Path::new("/r"), Arc::clone(&sink)).is_err());
		assert!(platform.watch(Path::new("/r"), sink).is_ok());
		assert_eq!(platform.watch_starts(), 1);
	}

	#[test]
	fn registering_records_the_account_root_and_unregistering_forgets_it() {
		let platform = platform();
		let root = Path::new("/r");
		let registered = platform.register(root, Uuid::from_u128(7)).unwrap();
		assert_eq!(
			platform.existing_registration(root).unwrap(),
			Some(ExistingRoot {
				ours: true,
				account_root: Some(Uuid::from_u128(7)),
			})
		);
		platform.unregister(registered).unwrap();
		assert_eq!(platform.existing_registration(root).unwrap(), None);
	}
}

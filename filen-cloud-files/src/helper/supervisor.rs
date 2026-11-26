//! The helper's command loop: it reads requests from the parent one at a time, in order, while it
//! goes on reading, and moves the helper between its phases: idle, a root registered, a root
//! served with a session.

use std::{
	mem,
	panic::AssertUnwindSafe,
	path::PathBuf,
	pin::pin,
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	},
	time::Duration,
};

use filen_mobile_native_cache::CacheError;
use filen_types::fs::Uuid;
use futures::FutureExt;
use tokio::{
	io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
	runtime::Handle,
	sync::{mpsc, oneshot, watch},
	task::JoinHandle,
	time::{Instant, sleep},
};
use tracing::{debug, info, warn};

use crate::{
	auth::{AuthPayload, DataDir, KeyStore, KeyStoreError, SessionError, SessionStore},
	branding::Branding,
	dispatch::{CloudFilesCallback, DispatchConfig, Dispatcher},
	drive::Drive,
	helper::{
		Stop, blocking,
		events::{self, HelperCallback, Out},
		framing::{Frame, next_frame},
		identify::{self, HELD, NOT_SERVING},
		index::{RootEntry, RootsIndex, StateDir, index_error},
		platform::{DriveOpener, Platform, PlatformError, RootRefusal, platform_error},
		protocol::{
			ErrorCode, FatalCode, Hello, IdentifiedItem, InMessage, ItemPath, KeptId, OutMessage,
			ParseError, PhaseName, Registration, ReplyError, ReplyValue, Request, RootDir,
			RootListing, StateReport, StatusEvent, UnregisterReport, UserId, parse_line,
		},
		unregister::{
			self, EntryUnregistered, Held, Mode, NOT_HELD, Stopped, Stopping, WalkError,
			shutting_down, unregister_entry, unsynced,
		},
		validate,
		watch_keeper::{Keeper, Keeping, keep_watching},
	},
	local::{LocalConfig, LocalEngine},
	log_safe,
	os::LocalFs,
	root_path::RootPath,
	thumbnails::{ThumbnailConfig, ThumbnailHandler, ThumbnailService},
};

/// How the thumbnail server's start, which runs in the background, is answered: the server, `None`
/// on a build that serves none, or why it did not start; a start that panicked drops its sender.
type ThumbnailStart<T> = oneshot::Receiver<Result<Option<T>, PlatformError>>;

/// How many requests wait, read and parsed, for the command loop.
const INCOMING_LEN: usize = 16;
/// How long `getState` waits for the walk its count of unsent items follows: the desktop app gives
/// the answer 5 s, and takes none as "there may be some".
const STATE_WALK_LIMIT: Duration = Duration::from_secs(4);

/// How the helper runs.
#[derive(Debug, Clone)]
pub struct HelperConfig {
	/// The state directory: the roots index.
	pub state_dir: StateDir,
	/// How long a new session has to sign the cache in. Default 30 s.
	pub auth_wait: Duration,
	/// How long a helper starting with a held root waits for its sealed session to sign the cache
	/// in before it takes the parent's first request; the root stays registered, held, past it, and
	/// a later `register` or `unregister` serves it. Default 5 s, as the desktop app gives its
	/// first requests little more.
	pub resume_wait: Duration,
	/// How often the cache is asked whether it signed in meanwhile. Default 250 ms: it re-reads
	/// auth.json at most once a second while signed out.
	pub auth_poll: Duration,
	/// How often the pending uploads are counted; a changed count is reported. Default 5 s.
	pub pending_poll: Duration,
	/// How often the served session is checked to still be accepted. Default 30 s.
	pub revoke_probe: Duration,
	/// How long activity waits to be written with what follows it. Default 1 s.
	pub coalesce: Duration,
	/// How often the root's watcher is replaced by a new one, which bounds how long a watcher that
	/// stopped silently goes unnoticed. Default 10 min.
	pub watch_rearm: Duration,
	/// How the local engine runs.
	pub local: LocalConfig,
	/// How the dispatcher runs.
	pub dispatch: DispatchConfig,
}

impl HelperConfig {
	/// The defaults, with the state directory `state_dir`.
	pub fn new(state_dir: StateDir) -> Self {
		Self {
			state_dir,
			auth_wait: Duration::from_secs(30),
			resume_wait: Duration::from_secs(5),
			auth_poll: Duration::from_millis(250),
			pending_poll: Duration::from_secs(5),
			revoke_probe: Duration::from_secs(30),
			coalesce: Duration::from_secs(1),
			watch_rearm: Duration::from_secs(600),
			local: LocalConfig::default(),
			dispatch: DispatchConfig::default(),
		}
	}
}

/// Why the helper's command loop ended. Each stops the helper in order: the root stays registered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
	/// The parent asked it to stop.
	Shutdown,
	/// The parent closed its stdin, or reading it failed.
	InputClosed,
	/// Writing to the parent failed: it closed its end of stdout.
	OutputClosed,
	/// The state directory's roots index could not be read.
	StateDirInvalid,
	/// A request panicked; it was answered `internal`.
	Internal,
}

fn session_error(error: SessionError) -> ReplyError {
	match error {
		SessionError::Keys(e) => ReplyError::new(ErrorCode::CredentialStore, e.to_string()),
		e @ (SessionError::Seal(_) | SessionError::Encode(_)) => {
			ReplyError::new(ErrorCode::Internal, e.to_string())
		}
		e @ SessionError::NotOurs(_) => ReplyError::new(ErrorCode::DataDirNotEmpty, e.to_string()),
		e @ SessionError::Io { .. } => ReplyError::new(ErrorCode::Io, e.to_string()),
	}
}

fn key_error(error: &KeyStoreError) -> ReplyError {
	ReplyError::new(ErrorCode::CredentialStore, error.to_string())
}

/// `error`'s code, for a line at INFO or WARN, after its message was logged at DEBUG: a reply's
/// message may quote a path in the root (an unregister names the first file it could not settle),
/// which those lines never show ([`log_safe`]).
fn reply_code(error: &ReplyError) -> ErrorCode {
	debug!(code = ?error.code, message = %error.message, "a reply's message");
	error.code
}

impl ExitReason {
	/// The process's exit code: 0 for an orderly stop, 1 after a request panicked, and the fatal
	/// code's for an unusable state directory.
	pub fn exit_code(self) -> u8 {
		match self {
			Self::Shutdown | Self::InputClosed | Self::OutputClosed => 0,
			Self::Internal => 1,
			Self::StateDirInvalid => FatalCode::StateDirInvalid.exit_code(),
		}
	}
}

/// A root registered with the helper, not served.
struct Registered<P: Platform> {
	user: UserId,
	root: RootDir,
	data_dir: DataDir,
	/// The OS's registration, once registered or taken over in this run.
	cf: Option<P::Registered>,
	/// What File Explorer shows for the root; `None` for the default.
	branding: Option<Branding>,
}

/// A root served with a session.
struct Serving<P: Platform, D: Drive> {
	user: UserId,
	root: RootDir,
	data_dir: DataDir,
	/// The account the root is registered for and the drive signed in as.
	account_root: Uuid,
	/// What File Explorer shows for the root; `None` for the default.
	branding: Option<Branding>,
	/// Whether File Explorer was last told what to show for the root without a failure.
	branded: bool,
	store: SessionStore,
	keys: P::Keys,
	drive: Arc<D>,
	dispatcher: Arc<Dispatcher<D>>,
	engine: Arc<LocalEngine<D, P::Local>>,
	/// The root's file system, for the thumbnail service a lifted hold serves again.
	thumbnail_local: Arc<P::Local>,
	connected: P::Connected,
	/// Keeps the root's watcher running; stopped before the engine stops.
	keeper: Keeping,
	watching: Arc<AtomicBool>,
	/// Whether the root is held: other processes' downloads refused, no thumbnail served. Reason
	/// for the Arc: the watch keeper reads it for the status it sends.
	held: Arc<AtomicBool>,
	/// The pending-upload poll and the revocation probe.
	probes: Vec<JoinHandle<()>>,
}

enum Phase<P: Platform, D: Drive> {
	Idle,
	Registered(Registered<P>),
	Serving(Box<Serving<P, D>>),
}

/// The command loop's state.
struct Helper<P: Platform, O: DriveOpener> {
	// Reason for the Arc: each blocking call takes a handle.
	platform: Arc<P>,
	// Reason for the Arc: each blocking open takes a handle.
	opener: Arc<O>,
	config: HelperConfig,
	index: RootsIndex,
	out: Out,
	callback: Arc<HelperCallback>,
	/// Set once the parent asked the helper to stop or closed its input: a sign-in still waited
	/// for gives up, and an unregister stops at its next safe point.
	stopping: Stop,
	/// Set only while the helper serves a held root of its own accord, as it starts: set in turn
	/// once the parent's first line is read, so a sign-in waited for then gives up and holds up no
	/// request.
	yield_to: Option<Stop>,
	phase: Phase<P, O::Drive>,
	/// The kept reports the last reply carried, by user: the parent's next request says it read
	/// them, and their records go.
	reported: Vec<(UserId, Vec<KeptId>)>,
	/// Answers File Explorer's thumbnail requests for the served root, and refuses them while none
	/// is. Reason for the Arc: the thumbnail server's handler holds it too.
	thumbnails: Arc<ThumbnailService<P::Local, O::Drive>>,
	/// The thumbnail server, from its start's success to the end of the run; `None` before, on a
	/// build that serves none, and after a failed start.
	thumbnail_server: Option<P::Thumbnails>,
}

/// One line from the parent, as the reader hands it on.
enum Incoming {
	Message(InMessage),
	Refused(ParseError),
	Unframed(ErrorCode),
}

/// Reads frames from `input` and hands them on until the input ends or the loop closes its end.
/// Sets `stop` as soon as it reads a `shutdown` or the input ends, so a request still waiting gives
/// up, and `arrived` as it hands on the first line. Returns the line it read but could not hand
/// on, as the loop closed its end meanwhile, so the loop answers it.
async fn read_input<R: AsyncRead + Unpin + Send>(
	input: R,
	tx: mpsc::Sender<Incoming>,
	stop: watch::Sender<bool>,
	arrived: watch::Sender<bool>,
) -> Option<Incoming> {
	let mut input = BufReader::new(input);
	let mut buf = Vec::new();
	loop {
		// A line cut off here was not read whole: the parent gets no answer to it.
		let frame = tokio::select! {
			frame = next_frame(&mut input, &mut buf) => frame,
			() = tx.closed() => return None,
		};
		let incoming = match frame {
			Ok(Frame::Line(line)) => match parse_line(&line) {
				Ok(message) => {
					if matches!(message.request, Request::Shutdown) {
						stop.send_replace(true);
					}
					Incoming::Message(message)
				}
				Err(error) => Incoming::Refused(error),
			},
			Ok(Frame::TooLong) => Incoming::Unframed(ErrorCode::LineTooLong),
			Ok(Frame::InvalidUtf8) => Incoming::Unframed(ErrorCode::InvalidUtf8),
			Ok(Frame::Eof) => {
				stop.send_replace(true);
				return None;
			}
			Err(error) => {
				warn!(%error, "reading the parent's input failed");
				stop.send_replace(true);
				return None;
			}
		};
		arrived.send_replace(true);
		if let Err(mpsc::error::SendError(incoming)) = tx.send(incoming).await {
			return Some(incoming);
		}
	}
}

/// Writes every line `lines` brings to `output`, flushing each; ends when writing fails or every
/// sender is gone.
async fn write_output<W: AsyncWrite + Unpin + Send>(
	mut output: W,
	mut lines: mpsc::UnboundedReceiver<OutMessage>,
) -> std::io::Result<()> {
	while let Some(message) = lines.recv().await {
		let mut line = message.to_line();
		line.push('\n');
		output.write_all(line.as_bytes()).await?;
		output.flush().await?;
	}
	Ok(())
}

/// Runs the helper over `input` and `output` until the parent asks it to stop, closes either end,
/// or the state directory is unusable: writes `hello`, then answers each request in order. Never
/// unregisters or wipes anything on its way out: a served root is disconnected and stays
/// registered. Needs a multi-thread runtime on Windows, as the Windows layer refuses to connect
/// on another.
///
/// `security` is what [`Platform::secure_thumbnails`] answered, called by the process's start
/// before it built the runtime this runs on, as [`serve_stdio`] does: the thumbnail server starts
/// only with its proof, `None` starts none, and a failure is answered as the server's failed start
/// is, the class withdrawn from every root.
///
/// [`serve_stdio`]: crate::helper::serve_stdio
pub async fn run_helper<P, O, R, W>(
	platform: P,
	security: Result<Option<P::ThumbnailSecurity>, PlatformError>,
	opener: O,
	config: HelperConfig,
	input: R,
	output: W,
) -> ExitReason
where
	P: Platform,
	O: DriveOpener,
	R: AsyncRead + Unpin + Send + 'static,
	W: AsyncWrite + Unpin + Send + 'static,
{
	let (out, lines) = mpsc::unbounded_channel();
	let mut writer = tokio::spawn(write_output(output, lines));
	let platform = Arc::new(platform);
	// Read before `hello`, so a helper that cannot start writes its `fatal` line alone.
	let index = {
		let dir = config.state_dir.clone();
		blocking(move || RootsIndex::load(&dir)).await
	};
	let index = match index {
		Ok(index) => index,
		Err(error) => {
			warn!(%error, "the roots index cannot be read");
			send(
				&out,
				OutMessage::Fatal {
					code: FatalCode::StateDirInvalid,
					message: error.to_string(),
				},
			);
			drop(out);
			let _ = writer.await;
			return ExitReason::StateDirInvalid;
		}
	};
	let build = {
		let platform = Arc::clone(&platform);
		blocking(move || platform.platform_build()).await
	};
	send(&out, OutMessage::Hello(Hello::new(build)));
	// What a run stopped between the OS's unregister and its own clean-up left in File Explorer.
	unregister::sweep_branding(&platform).await;
	// Bounded, so a parent writing faster than the helper answers waits on its pipe.
	let (tx, mut incoming) = mpsc::channel(INCOMING_LEN);
	let (stop, stopping) = watch::channel(false);
	let (arrived, first_line) = watch::channel(false);
	let reader = tokio::spawn(read_input(input, tx, stop, arrived));
	let callback = events::reporting(out.clone(), config.coalesce);
	let thumbnails = Arc::new(ThumbnailService::new(
		ThumbnailConfig::default(),
		Handle::current(),
	));
	// In the background, so a COM start that hangs holds up no request of the parent's; answered
	// in the loop below (`Helper::thumbnails_started`).
	let mut thumbnail_start = start_thumbnails(&platform, security, thumbnails.handler());
	let mut thumbnails_starting = true;
	let mut helper = Helper {
		platform,
		opener: Arc::new(opener),
		config,
		index,
		out,
		callback,
		stopping: Stop::new(stopping),
		yield_to: None,
		phase: Phase::Idle,
		reported: Vec::new(),
		thumbnails,
		thumbnail_server: None,
	};
	helper.status();
	// A held root goes on uploading what it holds without waiting for the parent; guarded as a
	// request is.
	let resumed = AssertUnwindSafe(helper.resume_held(Stop::new(first_line)))
		.catch_unwind()
		.await;
	let reason = if resumed.is_err() {
		ExitReason::Internal
	} else {
		loop {
			tokio::select! {
				biased;
				_ = &mut writer => break ExitReason::OutputClosed,
				started = &mut thumbnail_start, if thumbnails_starting => {
					thumbnails_starting = false;
					// Guarded as a request is: a panic stops the helper in order.
					let answered = AssertUnwindSafe(helper.thumbnails_started(started.ok()))
						.catch_unwind()
						.await;
					if answered.is_err() {
						break ExitReason::Internal;
					}
				}
				next = incoming.recv() => match next {
					None => break ExitReason::InputClosed,
					Some(Incoming::Unframed(code)) => helper.send(OutMessage::ProtocolError { code }),
					Some(Incoming::Refused(error)) => match error.id {
						Some(id) => {
							// A request after a reply: the parent read it.
							helper.acknowledge();
							helper.send(OutMessage::Reply { id, outcome: Err(error.into()) });
						}
						None => {
							warn!(code = ?error.code, message = %error.message, "refused a line");
							helper.send(OutMessage::ProtocolError { code: error.code });
						}
					},
					Some(Incoming::Message(InMessage { id, request })) => {
						helper.acknowledge();
						let shutdown = matches!(request, Request::Shutdown);
						let outcome = match AssertUnwindSafe(helper.handle(request)).catch_unwind().await {
							Ok(outcome) => outcome,
							Err(_) => {
								// The panic's message went to the log through the panic hook.
								helper.send(OutMessage::Reply {
									id,
									outcome: Err(ReplyError::new(
										ErrorCode::Internal,
										"the request failed inside the helper, which stops",
									)),
								});
								break ExitReason::Internal;
							}
						};
						if let Err(error) = &outcome {
							info!(id = id.get(), code = ?reply_code(error), "request refused");
						}
						helper.send(OutMessage::Reply { id, outcome });
						if shutdown {
							break ExitReason::Shutdown;
						}
					}
				},
			}
		}
	};
	info!(?reason, "stopping");
	// Every request read but not handled is answered, in the order read: the helper no longer
	// takes it. Closing the queue stops the reader, which hands back the line it holds, if any.
	incoming.close();
	let mut unanswered = Vec::new();
	while let Ok(next) = incoming.try_recv() {
		unanswered.push(next);
	}
	match reader.await {
		Ok(held) => unanswered.extend(held),
		Err(error) => warn!(%error, "the input reader ended abnormally"),
	}
	for next in unanswered {
		let id = match next {
			Incoming::Message(InMessage { id, .. }) => Some(id),
			Incoming::Refused(error) => error.id,
			Incoming::Unframed(_) => None,
		};
		if let Some(id) = id {
			helper.send(OutMessage::Reply {
				id,
				outcome: Err(ReplyError::new(
					ErrorCode::ShuttingDown,
					"the helper is stopping",
				)),
			});
		}
	}
	helper
		.stop(thumbnails_starting.then_some(thumbnail_start))
		.await;
	let Helper { out, callback, .. } = helper;
	drop(callback);
	drop(out);
	if reason != ExitReason::OutputClosed {
		// The coalescer and the writer end once their senders are gone; the parent kills a
		// helper that takes too long.
		let _ = writer.await;
	}
	reason
}

fn send(out: &Out, message: OutMessage) {
	// The writer is gone only once the parent closed stdout, which ends the loop.
	let _ = out.send(message);
}

/// Starts the thumbnail server under `security` answering with `handler` on the blocking pool,
/// detached, as the start blocks (on Windows up to the COM layer's own bound): what it answers
/// comes through the receiver. Once the receiver is closed, as the helper's stop closes one still
/// waiting, a server that starts after all is stopped at once, on the same thread. Without the
/// proof nothing starts: the receiver answers at once with `None`, or with why the security was not
/// set.
fn start_thumbnails<P: Platform>(
	platform: &Arc<P>,
	security: Result<Option<P::ThumbnailSecurity>, PlatformError>,
	handler: ThumbnailHandler,
) -> ThumbnailStart<P::Thumbnails> {
	let (started, start) = oneshot::channel();
	let security = match security {
		Ok(Some(security)) => security,
		unsecured => {
			// The receiver is still held here.
			let _ = started.send(unsecured.map(|_| None));
			return start;
		}
	};
	let platform = Arc::clone(platform);
	// Not awaited: the helper's stop does not wait for a start in progress.
	drop(tokio::task::spawn_blocking(move || {
		let served = platform.serve_thumbnails(security, handler).map(Some);
		match started.send(served) {
			Err(Ok(Some(server))) => {
				info!("the thumbnail server started after the helper stopped; stopping it");
				platform.stop_thumbnails(server);
			}
			// Logged here, as nothing takes it now.
			Err(Err(error)) => {
				warn!(%error, "serving File Explorer's thumbnails failed after the helper stopped");
			}
			Ok(()) | Err(Ok(None)) => {}
		}
	}));
	start
}

impl<P: Platform, O: DriveOpener> Helper<P, O> {
	fn send(&self, message: OutMessage) {
		send(&self.out, message);
	}

	/// Forgets the kept reports the last reply carried, which the parent read.
	fn acknowledge(&mut self) {
		for (user, ids) in mem::take(&mut self.reported) {
			if let Err(error) = self.index.acknowledge(user, &ids) {
				warn!(%error, "forgetting what was reported as kept failed; it is reported again");
			}
		}
	}

	/// Notes that the reply about to go carries `ids`, `user`'s kept reports.
	fn reporting(&mut self, user: UserId, ids: Vec<KeptId>) {
		if !ids.is_empty() {
			self.reported.push((user, ids));
		}
	}

	/// Whether `user`'s root is held, as the index records it.
	fn held(&self, user: UserId) -> bool {
		self.index.entry(user).is_some_and(|entry| entry.held)
	}

	/// The phase as reports name it: [`PhaseName::Held`] for a root this run holds registered
	/// whose entry is held, or serves held.
	fn phase_name(&self) -> PhaseName {
		match &self.phase {
			Phase::Idle => PhaseName::Idle,
			Phase::Registered(registered) if self.held(registered.user) => PhaseName::Held,
			Phase::Registered(_) => PhaseName::Registered,
			Phase::Serving(serving) if serving.held.load(Ordering::Relaxed) => PhaseName::Held,
			Phase::Serving(_) => PhaseName::Serving,
		}
	}

	fn status(&self) {
		let state = self.phase_name();
		let (user, root, watching) = match &self.phase {
			Phase::Idle => (None, None, false),
			Phase::Registered(registered) => {
				(Some(registered.user), Some(registered.root.clone()), false)
			}
			Phase::Serving(serving) => (
				Some(serving.user),
				Some(serving.root.clone()),
				serving.watching.load(Ordering::Relaxed),
			),
		};
		self.send(OutMessage::Status(StatusEvent {
			state,
			user,
			root,
			watching,
		}));
	}

	async fn handle(&mut self, request: Request) -> Result<ReplyValue, ReplyError> {
		match request {
			Request::Register(registration) => self.register(registration).await,
			Request::SetAuth { user, payload } => self.set_auth(user, payload).await,
			Request::ClearAuth { user } => self.clear_auth(user).await,
			Request::Unregister {
				user,
				delete_hydrated,
				hold,
			} => self.unregister(user, delete_hydrated, hold).await,
			Request::Abandon {
				user,
				delete_hydrated,
			} => self.abandon(user, delete_hydrated).await,
			Request::Identify { path } => self.identify(&path).await.map(ReplyValue::Identified),
			Request::ListRoots => Ok(self.list_roots().await),
			Request::GetState => Ok(ReplyValue::State(self.state().await)),
			Request::Shutdown => Ok(ReplyValue::Empty {}),
		}
	}

	/// The drive item at `path` in the served root (see [`identify::identify`]).
	async fn identify(&self, path: &ItemPath) -> Result<IdentifiedItem, ReplyError> {
		match &self.phase {
			Phase::Idle => Err(ReplyError::new(
				ErrorCode::NotRegistered,
				"no root is registered",
			)),
			Phase::Registered(_) => Err(ReplyError::new(ErrorCode::NotServing, NOT_SERVING)),
			Phase::Serving(serving) if serving.held.load(Ordering::Relaxed) => {
				Err(ReplyError::new(ErrorCode::NotServing, HELD))
			}
			Phase::Serving(serving) => {
				identify::identify(
					&self.platform,
					&serving.root,
					serving.account_root,
					&*serving.drive,
					path,
				)
				.await
			}
		}
	}

	/// The helper's state as `getState` reports it.
	async fn state(&self) -> StateReport {
		let state = self.phase_name();
		match &self.phase {
			Phase::Idle => StateReport {
				state: PhaseName::Idle,
				user: None,
				root: None,
				watching: false,
				pending_uploads: None,
			},
			Phase::Registered(registered) => StateReport {
				state,
				user: Some(registered.user),
				root: Some(registered.root.clone()),
				watching: false,
				pending_uploads: None,
			},
			Phase::Serving(serving) => StateReport {
				state,
				user: Some(serving.user),
				root: Some(serving.root.clone()),
				watching: serving.watching.load(Ordering::Relaxed),
				pending_uploads: {
					// Not past a stop the parent asked for: its shutdown budget is the parent's.
					tokio::select! {
						counted = serving.engine.unsynced_count_after_walk(STATE_WALK_LIMIT) => counted,
						() = self.stopping.asked() => None,
					}
				},
			},
		}
	}

	async fn list_roots(&mut self) -> ReplyValue {
		let mut roots = Vec::new();
		let mut reported = Vec::new();
		for entry in self.index.entries() {
			let platform = Arc::clone(&self.platform);
			let root = entry.root.clone();
			let registered = match blocking(move || platform.existing_registration(&root)).await {
				Ok(existing) => existing.is_some_and(|existing| existing.ours),
				Err(error) => {
					warn!(%error, root = ?entry.root, "reading a root's registration failed");
					false
				}
			};
			let serving = match &self.phase {
				Phase::Serving(serving) => serving.user == entry.user,
				Phase::Idle | Phase::Registered(_) => false,
			};
			let kept_earlier = entry.earlier(None);
			reported.push((
				entry.user,
				kept_earlier.iter().map(|earlier| earlier.id).collect(),
			));
			roots.push(RootListing {
				user: entry.user,
				root: entry.root.clone(),
				registered,
				serving,
				held: entry.held,
				kept_earlier,
			});
		}
		for (user, ids) in reported {
			self.reporting(user, ids);
		}
		ReplyValue::Roots { roots }
	}

	async fn register(&mut self, registration: Registration) -> Result<ReplyValue, ReplyError> {
		self.register_waiting(registration, self.config.auth_wait)
			.await
	}

	/// [`Helper::register`], a sealed session it resumes given `wait` to sign the cache in.
	async fn register_waiting(
		&mut self,
		registration: Registration,
		wait: Duration,
	) -> Result<ReplyValue, ReplyError> {
		let Registration {
			root,
			user,
			data_dir,
			branding,
		} = registration;
		let data_dir = DataDir::new(data_dir)
			.map_err(|e| ReplyError::new(ErrorCode::DataDirInvalid, e.to_string()))?;
		// Held for changes not on the drive yet, whatever this run holds of it: only its user's
		// sign-in or its unregister ends that.
		if self
			.index
			.entries()
			.iter()
			.any(|entry| entry.held && entry.user != user)
		{
			return Err(ReplyError::new(
				ErrorCode::AlreadyRegistered,
				"another user's root is held for changes not on the drive yet; unregister it first",
			));
		}
		let state = self.phase_name();
		let held = match &self.phase {
			Phase::Idle => None,
			Phase::Registered(registered) => Some((
				registered.user,
				registered.root.as_path(),
				&registered.data_dir,
			)),
			Phase::Serving(serving) => {
				Some((serving.user, serving.root.as_path(), &serving.data_dir))
			}
		};
		if let Some((held, held_root, held_data)) = held {
			if held == user
				&& validate::same_path(held_root, &root)
				&& validate::same_path(held_data.path(), data_dir.path())
			{
				return self.registered_again(user, branding, state).await;
			}
			return Err(ReplyError::new(
				ErrorCode::AlreadyRegistered,
				"another root, user or data directory is registered; unregister it first",
			));
		}
		// The index keeps a root an earlier run registered, with what the user changed in it: a
		// register elsewhere would forget it, still registered with the OS.
		if let Some(entry) = self.index.entry(user)
			&& !entry.unregistered
			&& !(validate::same_path(&entry.root, &root)
				&& validate::same_path(&entry.data_dir, data_dir.path()))
		{
			return Err(ReplyError::new(
				ErrorCode::AlreadyRegistered,
				"the user's root is registered at another path or with another data directory; \
				 unregister it first",
			));
		}
		{
			let platform = Arc::clone(&self.platform);
			let checked = root.clone();
			blocking(move || platform.check_root(&checked))
				.await
				.map_err(|refusal| match refusal {
					RootRefusal::Invalid(_) | RootRefusal::DriveRoot => {
						ReplyError::new(ErrorCode::RootInvalid, refusal.to_string())
					}
					RootRefusal::NotNtfs(_) => {
						ReplyError::new(ErrorCode::NotNtfs, refusal.to_string())
					}
				})?;
		}
		{
			let (checked, data, state) = (
				root.clone(),
				data_dir.path().to_path_buf(),
				self.config.state_dir.path().to_path_buf(),
			);
			// An unregistered root kept only for its report holds no path any more.
			let entries: Vec<RootEntry> = self
				.index
				.entries()
				.iter()
				.filter(|entry| !entry.unregistered)
				.cloned()
				.collect();
			blocking(move || validate::check_overlaps(&checked, &data, &state, user, &entries))
				.await?;
		}
		{
			let claimed = data_dir.clone();
			blocking(move || SessionStore::claim_dir(&claimed))
				.await
				.map_err(session_error)?;
		}
		// An unregister stopped after the OS let the root go is finished first: what it kept is
		// still in the root, which is moved aside before its path is used again.
		if let Some(entry) = self.index.entry(user).cloned() {
			unregister::finish_stopped(&self.platform, &mut self.index, &entry).await?;
		}
		unregister::sweep_branding(&self.platform).await;
		let existing = {
			let platform = Arc::clone(&self.platform);
			let checked = root.clone();
			blocking(move || platform.existing_registration(&checked))
				.await
				.map_err(|e| platform_error(&e))?
		};
		if existing.is_some_and(|existing| !existing.ours) {
			return Err(ReplyError::new(
				ErrorCode::RootInUse,
				"another provider registered the root",
			));
		}
		let ours = existing.is_some();
		{
			let checked = root.clone();
			blocking(move || validate::prepare_root(&checked, ours)).await?;
		}
		let known = self.index.entry(user).and_then(|entry| entry.account_root);
		let account_root = existing.and_then(|existing| existing.account_root);
		self.index
			.upsert(RootEntry {
				user,
				root: root.clone(),
				data_dir: data_dir.path().to_path_buf(),
				account_root: account_root.or(known),
				kept: Vec::new(),
				unregistered: false,
				branding: branding.clone(),
				// Kept as the index has it: only a sign-in or the unregister lifts a hold.
				held: false,
			})
			.map_err(|e| index_error(&e))?;
		let asked = branding.is_some();
		let registered = Registered {
			user,
			root,
			data_dir,
			cf: None,
			branding,
		};
		// Resumes a session sealed by an earlier run: the root is this provider's, for the account
		// the index says the user owns, and the session is on disk.
		let resumable = account_root.filter(|account| Some(*account) == known);
		let resume = resumable.is_some() && SessionStore::has_session(&registered.data_dir);
		let (phase, resumed) = match (resume, resumable) {
			(true, Some(account_root)) => {
				match self.serve(registered, None, account_root, wait).await {
					Ok(serving) => (Phase::Serving(Box::new(serving)), true),
					Err((registered, error)) => {
						warn!(code = ?reply_code(&error), "resuming the sealed session failed");
						(Phase::Registered(registered), false)
					}
				}
			}
			_ => (Phase::Registered(registered), false),
		};
		self.phase = phase;
		self.status();
		let state = self.phase_name();
		let branded = match &self.phase {
			Phase::Serving(serving) => asked.then_some(serving.branded),
			Phase::Idle | Phase::Registered(_) => None,
		};
		let kept_earlier = self
			.index
			.entry(user)
			.map(|entry| entry.earlier(None))
			.unwrap_or_default();
		self.reporting(
			user,
			kept_earlier.iter().map(|earlier| earlier.id).collect(),
		);
		Ok(ReplyValue::Registered {
			state,
			resumed,
			kept_earlier,
			branded,
		})
	}

	/// A `register` of the root already held, by the same user and data directory: answered as it
	/// stands, after rewriting what File Explorer shows when the request names a branding (shown at
	/// once when the root is served, else when it is).
	async fn registered_again(
		&mut self,
		user: UserId,
		branding: Option<Branding>,
		state: PhaseName,
	) -> Result<ReplyValue, ReplyError> {
		let mut branded = None;
		if let Some(branding) = branding {
			self.index
				.set_branding(user, Some(branding.clone()))
				.map_err(|e| index_error(&e))?;
			match &mut self.phase {
				Phase::Serving(serving) => {
					serving.branded =
						brand(&self.platform, &serving.root, user, Some(&branding)).await;
					serving.branding = Some(branding);
					branded = Some(serving.branded);
				}
				Phase::Registered(registered) => registered.branding = Some(branding),
				Phase::Idle => {}
			}
		}
		Ok(ReplyValue::Registered {
			state,
			resumed: false,
			kept_earlier: Vec::new(),
			branded,
		})
	}

	async fn set_auth(
		&mut self,
		user: UserId,
		payload: Box<AuthPayload>,
	) -> Result<ReplyValue, ReplyError> {
		let Some(owner) = payload.owner() else {
			return Err(ReplyError::new(
				ErrorCode::PayloadInvalid,
				"the session is missing",
			));
		};
		if owner.user_id != user.get() {
			return Err(ReplyError::new(
				ErrorCode::OwnerMismatch,
				"the session belongs to another user",
			));
		}
		let held = match &self.phase {
			Phase::Idle => None,
			Phase::Registered(registered) => Some(registered.user),
			Phase::Serving(serving) => Some(serving.user),
		};
		match held {
			None => {
				return Err(ReplyError::new(
					ErrorCode::NotRegistered,
					"no root is registered",
				));
			}
			Some(held) if held != user => {
				return Err(ReplyError::new(
					ErrorCode::OwnerMismatch,
					"another user's root is registered",
				));
			}
			Some(_) => {}
		}
		if let Some(known) = self.index.entry(user).and_then(|entry| entry.account_root)
			&& known != owner.account_root
		{
			return Err(ReplyError::new(
				ErrorCode::OwnerMismatch,
				"the session is of another account than the root's",
			));
		}
		match mem::replace(&mut self.phase, Phase::Idle) {
			Phase::Serving(serving) if serving.account_root != owner.account_root => {
				self.phase = Phase::Serving(serving);
				Err(ReplyError::new(
					ErrorCode::OwnerMismatch,
					"the session is of another account than the served root's",
				))
			}
			Phase::Serving(serving) => {
				let sealed = serving.store.seal(&payload).map_err(session_error);
				let lifted = match sealed {
					Ok(()) if serving.held.load(Ordering::Relaxed) => {
						Some(self.lift_hold(&serving).await)
					}
					Ok(()) => None,
					Err(error) => Some(Err(error)),
				};
				self.phase = Phase::Serving(serving);
				if let Some(lifted) = lifted {
					lifted?;
					self.status();
				}
				Ok(ReplyValue::Serving {
					adopted: true,
					branded: None,
				})
			}
			Phase::Registered(registered) => {
				let asked = registered.branding.is_some();
				match self
					.serve(
						registered,
						Some(payload),
						owner.account_root,
						self.config.auth_wait,
					)
					.await
				{
					Ok(serving) => {
						let branded = asked.then_some(serving.branded);
						// The drive signed in with the session as the serve started.
						let lifted = if serving.held.load(Ordering::Relaxed) {
							self.lift_hold(&serving).await
						} else {
							Ok(())
						};
						self.phase = Phase::Serving(Box::new(serving));
						self.status();
						lifted?;
						Ok(ReplyValue::Serving {
							adopted: false,
							branded,
						})
					}
					Err((registered, error)) => {
						self.phase = Phase::Registered(registered);
						Err(error)
					}
				}
			}
			Phase::Idle => Err(ReplyError::new(
				ErrorCode::NotRegistered,
				"no root is registered",
			)),
		}
	}

	/// Serves `registered` with the sealed session, after sealing `payload` into it when given:
	/// registers the root for `account_root` if this run has not, opens the drive, waits at most
	/// `wait` for it to sign in as that account, connects the root and starts the engine and the
	/// watcher. A root the index holds is served held from the first request on: the dispatcher
	/// and the engine hold it before it is connected, and no thumbnail is served. Hands the root
	/// back, as it was, on a failure.
	async fn serve(
		&mut self,
		mut registered: Registered<P>,
		payload: Option<Box<AuthPayload>>,
		account_root: Uuid,
		wait: Duration,
	) -> Result<Serving<P, O::Drive>, (Registered<P>, ReplyError)> {
		let opened = {
			let platform = Arc::clone(&self.platform);
			let dir = registered.data_dir.clone();
			let account = registered.user.key_account();
			blocking(move || {
				let keys = platform.key_store(&account).map_err(|e| key_error(&e))?;
				let store = SessionStore::open(dir, &keys).map_err(session_error)?;
				if let Some(payload) = payload {
					store.seal(&payload).map_err(session_error)?;
				}
				Ok::<_, ReplyError>((keys, store))
			})
			.await
		};
		let (keys, store) = match opened {
			Ok(opened) => opened,
			Err(error) => return Err((registered, error)),
		};
		let (store, drive) = {
			let opener = Arc::clone(&self.opener);
			blocking(move || {
				let drive = opener.open(&store);
				(store, drive)
			})
			.await
		};
		if let Err(error) = self.signed_in(&drive, &store, account_root, wait).await {
			drive.stop().await;
			return Err((registered, error));
		}
		// Registered only once the drive signed in as the account, so no root is registered for an
		// account the session does not belong to; the index records the account before anything is
		// served, as it is what a later session is checked against.
		let cf = match registered.cf.take() {
			Some(cf) => cf,
			None => {
				let platform = Arc::clone(&self.platform);
				let root = registered.root.clone();
				match blocking(move || platform.register(&root, account_root)).await {
					Ok(cf) => cf,
					Err(error) => {
						drive.stop().await;
						return Err((registered, platform_error(&error)));
					}
				}
			}
		};
		if let Err(error) = self.index.set_account_root(registered.user, account_root) {
			drive.stop().await;
			registered.cf = Some(cf);
			return Err((registered, index_error(&error)));
		}
		// Before connecting, so the first window Explorer opens on the root shows it branded.
		let branded = brand(
			&self.platform,
			&registered.root,
			registered.user,
			registered.branding.as_ref(),
		)
		.await;
		let callback = Arc::clone(&self.callback) as Arc<dyn CloudFilesCallback>;
		let local = {
			let platform = Arc::clone(&self.platform);
			let root = registered.root.clone();
			blocking(move || platform.local_fs(&root)).await
		};
		// The engine keeps what it needs across a restart in the root's data directory.
		let thumbnail_local = Arc::clone(&local);
		let engine = Arc::new(LocalEngine::new_keeping_records(
			Arc::clone(&drive),
			local,
			Handle::current(),
			self.config.local,
			Arc::clone(&callback),
			registered.data_dir.path().to_path_buf(),
		));
		let dispatcher = Arc::new(
			Dispatcher::new(
				Arc::clone(&drive),
				Handle::current(),
				self.config.dispatch,
				callback,
			)
			.with_hints(engine.hints()),
		);
		let held = self.held(registered.user);
		if held {
			dispatcher.hold();
			engine.set_held(true);
		}
		let connected = {
			let platform = Arc::clone(&self.platform);
			let (dispatcher, engine) = (Arc::clone(&dispatcher), Arc::clone(&engine));
			blocking(move || platform.connect(cf, dispatcher, engine)).await
		};
		let connected = match connected {
			Ok(connected) => connected,
			Err(failed) => {
				drive.stop().await;
				registered.cf = Some(failed.held);
				return Err((registered, platform_error(&failed.error)));
			}
		};
		engine.start();
		if !held {
			self.thumbnails.serve(
				registered.root.clone(),
				Arc::clone(&thumbnail_local),
				Arc::clone(&drive),
			);
		}
		let watching = Arc::new(AtomicBool::new(false));
		let held = Arc::new(AtomicBool::new(held));
		let keeper = {
			let engine = Arc::clone(&engine);
			keep_watching(Keeper {
				platform: Arc::clone(&self.platform),
				root: registered.root.to_path_buf(),
				on_event: Arc::new(move |event| engine.on_local_event(event)),
				watching: Arc::clone(&watching),
				status: StatusEvent {
					state: PhaseName::Serving,
					user: Some(registered.user),
					root: Some(registered.root.clone()),
					watching: false,
				},
				held: Arc::clone(&held),
				out: self.out.clone(),
				rearm: self.config.watch_rearm,
			})
		};
		let probes = vec![
			tokio::spawn(poll_pending(
				Arc::clone(&engine),
				self.out.clone(),
				self.config.pending_poll,
			)),
			tokio::spawn(probe_revocation(
				Arc::clone(&drive),
				registered.data_dir.clone(),
				registered.user,
				self.out.clone(),
				self.config.revoke_probe,
			)),
		];
		let Registered {
			user,
			root,
			data_dir,
			branding,
			..
		} = registered;
		Ok(Serving {
			user,
			root,
			data_dir,
			account_root,
			branding,
			branded,
			store,
			keys,
			drive,
			dispatcher,
			engine,
			thumbnail_local,
			connected,
			keeper,
			watching,
			held,
			probes,
		})
	}

	/// Holds `user`'s root, recorded first: a write that fails answers `io` and changes nothing
	/// else. A root this run serves is held at once, and the change reported. Nothing for a user
	/// the index does not know, or whose root is unregistered.
	fn hold(&mut self, user: UserId) -> Result<(), ReplyError> {
		let Some(was_held) = self
			.index
			.entry(user)
			.filter(|entry| !entry.unregistered)
			.map(|entry| entry.held)
		else {
			return Ok(());
		};
		if !was_held {
			self.index
				.set_held(user, true)
				.map_err(|e| index_error(&e))?;
		}
		let changed = match &self.phase {
			Phase::Serving(serving) if serving.user == user => {
				let was = serving.held.load(Ordering::Relaxed);
				self.enter_hold(serving);
				!was
			}
			Phase::Registered(registered) if registered.user == user => !was_held,
			Phase::Idle | Phase::Registered(_) | Phase::Serving(_) => false,
		};
		if changed {
			self.status();
		}
		Ok(())
	}

	/// Holds the served root: other processes' downloads are refused from now on, those in flight
	/// failed, only local changes go up to the drive, and no thumbnail is served.
	fn enter_hold(&self, serving: &Serving<P, O::Drive>) {
		serving.dispatcher.hold();
		serving.engine.set_held(true);
		self.thumbnails.stop_serving();
		serving.held.store(true, Ordering::Relaxed);
	}

	/// Lifts the hold of the served root, once the drive answers as the root's account: records it,
	/// then serves every process again, the engine walking the root for what the hold left undone.
	/// A failure leaves the root held. What lifts it is the parent's `setAuth` of the held user for
	/// the root's account, which it sends once the user signed in again: the drive of a served root
	/// goes on with the session it signed in with, which the sealed payload replaces only at its
	/// next open, so the check proves the account, not that the payload just sealed signs in.
	async fn lift_hold(&mut self, serving: &Serving<P, O::Drive>) -> Result<(), ReplyError> {
		self.signed_in(
			&serving.drive,
			&serving.store,
			serving.account_root,
			self.config.auth_wait,
		)
		.await?;
		self.index
			.set_held(serving.user, false)
			.map_err(|e| index_error(&e))?;
		serving.dispatcher.release();
		serving.engine.set_held(false);
		self.thumbnails.serve(
			serving.root.clone(),
			Arc::clone(&serving.thumbnail_local),
			Arc::clone(&serving.drive),
		);
		serving.held.store(false, Ordering::Relaxed);
		info!("the root's hold is lifted");
		Ok(())
	}

	/// Serves the held root the index names, as an unregister would before it fetches, so its
	/// changes go on up to the drive without waiting for the parent; at most
	/// [`HelperConfig::resume_wait`] for the sign-in, and only until `first_line` is set (the
	/// parent's first line read), after which it stays registered, held, for a later request to
	/// serve. Nothing for a run that serves no root, or no held one. Opening the drive and connecting
	/// the root are not cut short, as for a `register`.
	async fn resume_held(&mut self, first_line: Stop) {
		let held: Vec<RootEntry> = self
			.index
			.entries()
			.iter()
			.filter(|entry| entry.held)
			.cloned()
			.collect();
		let Some(entry) = held.first() else {
			return;
		};
		if held.len() > 1 {
			warn!(
				count = held.len(),
				"the index holds more than one held root; only the first is served"
			);
		}
		info!("serving the held root");
		self.yield_to = Some(first_line);
		self.serve_to_unregister(entry, self.config.resume_wait)
			.await;
		self.yield_to = None;
	}

	/// Waits at most `wait` for `drive` to sign in with `store`'s session as the account whose root
	/// is `account_root`, and no longer than until a stop is asked for or, while the helper serves
	/// a held root of its own accord, until the parent's first line is read. Another account's
	/// session is cleared.
	async fn signed_in(
		&self,
		drive: &Arc<O::Drive>,
		store: &SessionStore,
		account_root: Uuid,
		wait: Duration,
	) -> Result<(), ReplyError> {
		let deadline = Instant::now() + wait;
		loop {
			match drive.root_uuid().await {
				Ok(root) if Uuid::parse_str(&root).ok() == Some(account_root) => return Ok(()),
				Ok(_) => {
					if let Err(error) = store.clear() {
						warn!(%error, "clearing another account's session failed");
					}
					return Err(ReplyError::new(
						ErrorCode::OwnerMismatch,
						"the session signed in as another account than the root's",
					));
				}
				Err(CacheError::Unauthenticated(_) | CacheError::Disabled(_)) => {}
				Err(error) => {
					warn!(error = %log_safe::error(&error), "the cache could not name its root yet");
				}
			}
			if self.stopping.requested() {
				return Err(ReplyError::new(
					ErrorCode::ShuttingDown,
					"the helper is stopping; the session stays for its next start",
				));
			}
			if self.yield_to.as_ref().is_some_and(Stop::requested) {
				return Err(ReplyError::new(
					ErrorCode::SessionRejected,
					"the cache did not sign in before the parent's first request",
				));
			}
			if Instant::now() >= deadline {
				return Err(ReplyError::new(
					ErrorCode::SessionRejected,
					"the cache did not sign in with the session",
				));
			}
			tokio::select! {
				() = sleep(self.config.auth_poll) => {}
				() = self.stopping.asked() => {}
				() = async {
					match &self.yield_to {
						Some(first_line) => first_line.asked().await,
						None => std::future::pending().await,
					}
				} => {}
			}
		}
	}

	/// Stops serving: the watcher, the probes, the engine and the dispatcher, then disconnects the
	/// root and stops the drive. The session stays on disk.
	async fn stop_serving(
		&self,
		serving: Serving<P, O::Drive>,
	) -> (Registered<P>, SessionStore, P::Keys) {
		let Serving {
			user,
			root,
			data_dir,
			store,
			keys,
			drive,
			dispatcher,
			engine,
			connected,
			keeper,
			probes,
			branding,
			..
		} = serving;
		self.thumbnails.stop_serving();
		// Awaited, a watcher still starting included, so no watcher outlives the engine.
		keeper.stop().await;
		for probe in probes {
			probe.abort();
		}
		engine.shutdown().await;
		dispatcher.shutdown().await;
		let platform = Arc::clone(&self.platform);
		let cf = match blocking(move || platform.disconnect(connected)).await {
			Ok(registered) => Some(registered),
			Err(failed) => {
				warn!(error = %failed.error, "disconnecting the sync root failed");
				drop(failed.held);
				None
			}
		};
		drive.stop().await;
		(
			Registered {
				user,
				root,
				data_dir,
				cf,
				branding,
			},
			store,
			keys,
		)
	}

	/// Signs `user` out: stops serving their root, when served, then deletes the session and the
	/// sealing key, and withdraws the thumbnail class from the root's entry
	/// ([`Platform::withdraw_root_thumbnails`]), so File Explorer shows its own thumbnails there while
	/// no session is; a sign-in names it again.
	async fn clear_auth(&mut self, user: UserId) -> Result<ReplyValue, ReplyError> {
		match mem::replace(&mut self.phase, Phase::Idle) {
			Phase::Serving(serving) if serving.user == user => {
				let (registered, store, keys) = self.stop_serving(*serving).await;
				let dir = registered.data_dir.clone();
				self.withdraw_root_thumbnails(registered.root.to_path_buf(), user)
					.await;
				self.phase = Phase::Registered(registered);
				self.status();
				// The cache is stopped, so nothing reads auth.json to wipe what it cached on its own.
				// An edit whose upload is pending is still in the sync root, not in sync, and goes up
				// again once a session serves the root.
				blocking(move || {
					drop(store);
					SessionStore::wipe_dir(&dir).map_err(session_error)?;
					keys.delete().map_err(|e| key_error(&e))
				})
				.await?;
				Ok(ReplyValue::Cleared { cleared: true })
			}
			phase => {
				let (dir, root) = match &phase {
					Phase::Registered(registered) if registered.user == user => (
						Some(registered.data_dir.clone()),
						Some(registered.root.to_path_buf()),
					),
					Phase::Idle | Phase::Registered(_) | Phase::Serving(_) => {
						match self.index.entry(user) {
							Some(entry) => (
								DataDir::new(entry.data_dir.clone()).ok(),
								// An unregistered root's entry is gone, its value with it.
								(!entry.unregistered).then(|| entry.root.to_path_buf()),
							),
							None => (None, None),
						}
					}
				};
				self.phase = phase;
				// The value a helper that only stopped left, named while a session served the root.
				if let Some(root) = root {
					self.withdraw_root_thumbnails(root, user).await;
				}
				let Some(dir) = dir else {
					return Ok(ReplyValue::Cleared { cleared: false });
				};
				let platform = Arc::clone(&self.platform);
				blocking(move || {
					SessionStore::wipe_dir(&dir).map_err(session_error)?;
					let keys = platform
						.key_store(&user.key_account())
						.map_err(|e| key_error(&e))?;
					keys.delete().map_err(|e| key_error(&e))
				})
				.await?;
				Ok(ReplyValue::Cleared { cleared: true })
			}
		}
	}

	/// Unregisters `user`'s root without losing local data, then forgets it and deletes the user's
	/// data and sealing key. A served root first fetches the data of its changed files not all on
	/// disk, while a session can. A stop asked for before the root's items are settled answers
	/// `shuttingDown` at the next safe point, leaving the root registered with its session, and
	/// the loop then stops as for any shutdown. With `hold`, the root is held first and stays so
	/// unless the unregister succeeds; a held root whose fetches leave a changed file not all on
	/// disk answers `unsyncedChanges` at once, still served, so its uploads go on.
	async fn unregister(
		&mut self,
		user: UserId,
		delete_hydrated: bool,
		hold: bool,
	) -> Result<ReplyValue, ReplyError> {
		if hold {
			self.hold(user)?;
		}
		let Some(entry) = self.index.entry(user).cloned() else {
			return Ok(ReplyValue::Unregistered(UnregisterReport {
				unregistered: false,
				deleted_hydrated: 0,
				kept: Vec::new(),
				kept_count: 0,
				kept_in: None,
				root_removed: false,
				key_deleted: false,
				data_deleted: false,
				kept_id: None,
				kept_earlier: Vec::new(),
				abandoned: None,
			}));
		};
		self.unregister_entry_as(entry, delete_hydrated, Mode::Unregister)
			.await
	}

	/// Unregisters `user`'s held root as [`Helper::unregister`] does, but saves what is on disk of
	/// each changed file it cannot fetch instead of stopping on it (see `Request::Abandon`). The
	/// fetches are tried a second time when the first left a file not all on disk while the drive
	/// answers. Refused `notHeld` for a root that is not held, and `notRegistered` for a user the
	/// index does not know; a root an earlier abandon or unregister finished is reported as
	/// `unregister` reports it.
	async fn abandon(
		&mut self,
		user: UserId,
		delete_hydrated: bool,
	) -> Result<ReplyValue, ReplyError> {
		let entry = match self.index.entry(user) {
			None => {
				return Err(ReplyError::new(
					ErrorCode::NotRegistered,
					"the index knows no root of the user's",
				));
			}
			Some(entry) if !entry.held && !entry.unregistered => {
				return Err(ReplyError::new(ErrorCode::NotHeld, NOT_HELD));
			}
			Some(entry) => entry.clone(),
		};
		self.unregister_entry_as(entry, delete_hydrated, Mode::Abandon)
			.await
	}

	/// The unregister of `entry`'s root, in `mode`, once it is known (see [`Helper::unregister`]
	/// and [`Helper::abandon`]).
	async fn unregister_entry_as(
		&mut self,
		entry: RootEntry,
		delete_hydrated: bool,
		mode: Mode,
	) -> Result<ReplyValue, ReplyError> {
		let user = entry.user;
		self.serve_to_unregister(&entry, self.config.auth_wait)
			.await;
		// A sign-in the serve waited for gave up on the stop, or the stop came after it.
		if self.stopping.requested() {
			return Err(shutting_down());
		}
		// What this run held of the root: its registration, and the account it served, so a served
		// root that cannot be unregistered is served again; and how many changes the engine had
		// not carried to the drive yet.
		let (held, served, pending) = match mem::replace(&mut self.phase, Phase::Idle) {
			Phase::Serving(serving) if serving.user == user => {
				let Ok(mut unfetched) = self.fetch_kept(&serving).await else {
					// Left served: the loop's own stop stops it.
					self.phase = Phase::Serving(serving);
					return Err(shutting_down());
				};
				// Given up only on files the drive does not serve now either.
				if mode == Mode::Abandon
					&& !unfetched.is_empty()
					&& serving.drive.root_uuid().await.is_ok()
				{
					let Ok(again) = self.fetch_kept(&serving).await else {
						self.phase = Phase::Serving(serving);
						return Err(shutting_down());
					};
					unfetched = again;
				}
				// Only the provider's own fetches fill such a file, so the walk under the refusing
				// connection could only find it again; the root stays served for its uploads.
				if entry.held
					&& mode == Mode::Unregister
					&& let Some(first) = unfetched.first()
				{
					self.phase = Phase::Serving(serving);
					return Err(unsynced(unfetched.len(), &first.to_string()));
				}
				let account_root = serving.account_root;
				let pending = serving.engine.unsynced_count();
				let (registered, _, _) = self.stop_serving(*serving).await;
				(Some(registered), Some(account_root), pending)
			}
			Phase::Registered(registered) if registered.user == user => {
				(Some(registered), None, None)
			}
			phase => {
				self.phase = phase;
				(None, None, None)
			}
		};
		let active = held.is_some();
		let (cf, data_dir) = match held {
			Some(Registered { cf, data_dir, .. }) => (cf, Some(data_dir)),
			None => (None, None),
		};
		match unregister_entry(
			&self.platform,
			&mut self.index,
			&entry,
			cf,
			delete_hydrated,
			&self.stopping,
			mode,
		)
		.await
		{
			Ok(EntryUnregistered { mut report, .. }) => {
				if active {
					self.status();
				}
				// Only an abandon that saved in this run reports it: one that finishes what a killed
				// run left, or found the root unregistered, reports the copies among what was kept.
				if let Some(abandoned) = report.abandoned.as_mut() {
					abandoned.pending_operations = pending;
				}
				self.reporting(user, report.kept_ids());
				Ok(ReplyValue::Unregistered(report))
			}
			Err(Stopped { held, error }) => {
				let registered = match held {
					Held::Registered(registered) => Some(registered),
					Held::Unknown => None,
					// Nothing to serve: what was kept waits in the index for the next unregister.
					Held::Released => {
						if active {
							self.status();
						}
						return Err(error);
					}
				};
				if let Some(data_dir) = data_dir {
					let registered = Registered {
						user,
						root: entry.root.clone(),
						data_dir,
						cf: registered,
						branding: entry.branding.clone(),
					};
					self.phase = match served {
						// Not while stopping: the serve would wait for a sign-in.
						Some(account_root) if !self.stopping.requested() => {
							match self
								.serve(registered, None, account_root, self.config.auth_wait)
								.await
							{
								Ok(serving) => Phase::Serving(Box::new(serving)),
								Err((registered, resume)) => {
									warn!(code = ?reply_code(&resume), "serving the root again after its unregister stopped failed");
									Phase::Registered(registered)
								}
							}
						}
						Some(_) | None => Phase::Registered(registered),
					};
					self.status();
				}
				Err(error)
			}
		}
	}

	/// Walks the served root and fetches the data of its changed files not all on disk, before it
	/// is unregistered. A stop asked for meanwhile closes the dispatcher, which fails the fetch in
	/// flight and every later one, so the platform's hydrate returns; then it waits for the walk to
	/// give up. The changed files it could not fetch; [`Stopping`] once a stop was asked for,
	/// whenever it came.
	async fn fetch_kept(&self, serving: &Serving<P, O::Drive>) -> Result<Vec<RootPath>, Stopping> {
		let fetching = {
			let platform = Arc::clone(&self.platform);
			let root = serving.root.clone();
			let stop = self.stopping.clone();
			blocking(move || {
				let local = platform.local_fs(&root);
				match unregister::walk(&*local, &stop) {
					Ok(walked) => unregister::hydrate_kept(&*local, &walked, &stop),
					Err(WalkError::Stopping(stopping)) => Err(stopping),
					// The unregister's own walk fails on it in turn.
					Err(WalkError::Local(error)) => {
						warn!(%error, "walking the root before unregistering failed");
						Ok(Vec::new())
					}
				}
			})
		};
		let mut fetching = pin!(fetching);
		let fetched = tokio::select! {
			fetched = &mut fetching => fetched,
			() = self.stopping.asked() => {
				serving.dispatcher.close().await;
				fetching.await
			}
		};
		// A stop that came just after the fetches is honoured too: the root is not settled yet.
		if self.stopping.requested() {
			return Err(Stopping);
		}
		fetched
	}

	/// Serves `entry`'s root with the session sealed in its data directory before it is
	/// unregistered, when this run does not serve it: the changed files not all on disk can be
	/// fetched only while it is served, and an unregister that stopped on them leaves the root
	/// registered with its session, which the parent keeps for a later try (a run started anew
	/// included). Only while the OS holds this provider's registration of the root for its
	/// account: nothing for an elevated run, a root with no session or no known account, an
	/// unregister the OS let go of (its record is finished as it is), or while another user's root
	/// is held; a serve that fails leaves the unregister as it was. A sealed session is given
	/// `wait` to sign the cache in.
	async fn serve_to_unregister(&mut self, entry: &RootEntry, wait: Duration) {
		if entry.unregistered || entry.unsettled().is_some() || self.platform.elevated() {
			return;
		}
		let Some(account_root) = entry.account_root else {
			return;
		};
		let held = {
			let platform = Arc::clone(&self.platform);
			let root = entry.root.clone();
			blocking(move || platform.existing_registration(&root)).await
		};
		match held {
			Ok(Some(existing)) if existing.ours && existing.account_root == Some(account_root) => {}
			Ok(_) => return,
			Err(error) => {
				warn!(%error, "reading the root's registration before unregistering it failed");
				return;
			}
		}
		let data_dir = match DataDir::new(entry.data_dir.clone()) {
			Ok(data_dir) => data_dir,
			Err(error) => {
				warn!(%error, "the root's data directory is not usable; it is not served to unregister it");
				return;
			}
		};
		let sealed = {
			let dir = data_dir.clone();
			blocking(move || SessionStore::has_session(&dir)).await
		};
		if !sealed {
			return;
		}
		match mem::replace(&mut self.phase, Phase::Idle) {
			Phase::Idle => {
				// What the register would report goes with the unregister's reply instead.
				let reported = self.reported.len();
				let registration = Registration {
					root: entry.root.clone(),
					user: entry.user,
					data_dir: entry.data_dir.clone(),
					branding: entry.branding.clone(),
				};
				if let Err(error) = self.register_waiting(registration, wait).await {
					warn!(code = ?reply_code(&error), "serving the root before unregistering it failed");
				}
				self.reported.truncate(reported);
			}
			Phase::Registered(registered) if registered.user == entry.user => {
				self.phase = match self.serve(registered, None, account_root, wait).await {
					Ok(serving) => Phase::Serving(Box::new(serving)),
					Err((registered, error)) => {
						warn!(code = ?reply_code(&error), "serving the root before unregistering it failed");
						Phase::Registered(registered)
					}
				};
				self.status();
			}
			phase => self.phase = phase,
		}
	}

	/// Removes the value naming the thumbnail class from the entry of `user`'s root at `root`, on a
	/// build that serves thumbnails; a failure is logged.
	async fn withdraw_root_thumbnails(&self, root: PathBuf, user: UserId) {
		let platform = Arc::clone(&self.platform);
		blocking(move || platform.withdraw_root_thumbnails(&root, user)).await;
	}

	/// Takes what the thumbnail server's start answered (`None` when it panicked). A server that
	/// started names the class for the root served now, which its brand left without it while no
	/// server ran; a root served later is branded with it anyway. A start that failed, as one
	/// without the process's thumbnail security does, withdraws the class from every root.
	async fn thumbnails_started(
		&mut self,
		started: Option<Result<Option<P::Thumbnails>, PlatformError>>,
	) {
		match started {
			Some(Ok(None)) => return,
			Some(Ok(Some(server))) => {
				self.thumbnail_server = Some(server);
				if let Phase::Serving(serving) = &mut self.phase {
					serving.branded = brand(
						&self.platform,
						&serving.root,
						serving.user,
						serving.branding.as_ref(),
					)
					.await;
				}
				return;
			}
			Some(Err(error)) => warn!(%error, "serving File Explorer's thumbnails failed"),
			None => warn!("serving File Explorer's thumbnails failed: its start panicked"),
		}
		// The value naming the class goes from every root, and no root is branded with it without
		// a server, so File Explorer keeps its own thumbnails instead of the class's fallback,
		// which refuses every request. Here, not in the platform's start, which the instance COM
		// starts runs too: that one brands nothing, and may fail while a helper serves.
		let platform = Arc::clone(&self.platform);
		blocking(move || platform.withdraw_thumbnails()).await;
	}

	/// Stops whatever runs, leaving the root registered and its session on disk. `start` is the
	/// thumbnail server's start when it was not answered yet: no longer waited for, it stops the
	/// server itself should one start after all.
	async fn stop(&mut self, start: Option<ThumbnailStart<P::Thumbnails>>) {
		if let Phase::Serving(serving) = mem::replace(&mut self.phase, Phase::Idle) {
			self.send(OutMessage::Status(StatusEvent {
				state: PhaseName::Stopping,
				user: Some(serving.user),
				root: Some(serving.root.clone()),
				watching: false,
			}));
			self.stop_serving(*serving).await;
		}
		if let Some(mut start) = start {
			// Closed first, so a server is either taken here or stopped by its start.
			start.close();
			if let Ok(started) = start.try_recv() {
				self.thumbnails_started(Some(started)).await;
			}
		}
		if let Some(server) = self.thumbnail_server.take() {
			let platform = Arc::clone(&self.platform);
			blocking(move || platform.stop_thumbnails(server)).await;
		}
	}
}

/// Tells File Explorer what to show for `user`'s root at `root`: `branding`, or the default. A
/// failure is logged and changes nothing else, as the root is served all the same: whether it
/// worked.
async fn brand<P: Platform>(
	platform: &Arc<P>,
	root: &RootDir,
	user: UserId,
	branding: Option<&Branding>,
) -> bool {
	let platform = Arc::clone(platform);
	let root = root.to_path_buf();
	let branding = branding.cloned().unwrap_or_default();
	blocking(move || platform.brand(&root, user, &branding))
		.await
		.inspect_err(|error| warn!(%error, "showing the root in File Explorer failed"))
		.is_ok()
}

/// Reports how many items in the root hold what the drive does not have yet
/// ([`LocalEngine::unsynced_count`]) every `every` when the count changed, the first count
/// included; nothing while the engine cannot say yet.
async fn poll_pending<D: Drive, L: LocalFs>(
	engine: Arc<LocalEngine<D, L>>,
	out: Out,
	every: Duration,
) {
	let mut last = None;
	loop {
		if let Some(count) = engine.unsynced_count()
			&& last != Some(count)
		{
			last = Some(count);
			send(&out, OutMessage::PendingUploads { count });
		}
		sleep(every).await;
	}
}

/// Reports, once, that the served session stopped being accepted: the cache answers as signed out
/// while auth.json is still there. Checked every `every`.
async fn probe_revocation<D: Drive>(
	drive: Arc<D>,
	dir: DataDir,
	user: UserId,
	out: Out,
	every: Duration,
) {
	loop {
		sleep(every).await;
		if let Err(CacheError::Unauthenticated(_) | CacheError::Disabled(_)) =
			drive.root_uuid().await
			&& SessionStore::has_session(&dir)
		{
			send(&out, OutMessage::AuthRevoked { user });
			return;
		}
	}
}

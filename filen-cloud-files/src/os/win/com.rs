//! File Explorer's thumbnail requests over COM: the helper exe as an out-of-process server of this
//! provider's thumbnail class ([`THUMBNAIL_CLSID`]), registered with `CoRegisterClassObject` on a
//! thread of its own, answering `IThumbnailProvider::GetThumbnail` through the handler it is
//! started with.
//!
//! The thread joins the multithreaded apartment, so each call runs on one of COM's RPC threads and
//! no message loop is needed. `GetThumbnail` runs the handler on a thread of its own while the RPC
//! thread, the one servicing the call, checks whether the caller cancelled it (`CoTestCancel`) and
//! then cancels the handler's request. A class factory makes one thumbnail object per request.
//! The object offers `IInitializeWithItem`, which hands it the item's path, and
//! `IThumbnailProvider`. It offers neither `IInitializeWithStream` nor `IInitializeWithFile`, which
//! would make Explorer read the file's content and so hydrate the placeholder. The interfaces are
//! declared here by hand, since `windows-sys` declares only `IUnknown`'s.
//!
//! Who may call the server is the process's COM security, which a process sets once, and which COM
//! sets to its defaults the first time anything in the process marshals an interface or makes or
//! takes a COM call. So it is set first, by [`ComSecurity::establish`], which the helper calls
//! before any COM or shell use and before it builds its runtime; [`ThumbnailServer::start`] takes
//! the [`ComSecurity`] that returns, so no server runs in a process whose COM security this layer
//! did not set.
//!
//! The COM layer's own `unsafe` blocks are in this file: the apartment, the process's COM
//! security, the class's registration, the objects COM calls and the calls they make on the
//! interfaces they are handed. The Win32 calls around them that are not COM's, the security
//! descriptor the access is built from and the bitmaps handed out, are behind safe functions in
//! `ffi`. A panic must not unwind into COM, so
//! every method COM calls that can panic catches it: one returning an `HRESULT` answers
//! `E_UNEXPECTED`, `AddRef` and `Release` answer 0. `LockServer` does nothing.
//!
//! [`THUMBNAIL_CLSID`]: crate::os::cfapi::explorer::THUMBNAIL_CLSID

use std::{
	ffi::{OsString, c_void},
	fmt, io,
	marker::PhantomData,
	mem::{self, offset_of},
	os::windows::ffi::OsStringExt,
	panic::{self, AssertUnwindSafe},
	path::{Path, PathBuf},
	ptr,
	sync::{
		Arc, OnceLock,
		atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
		mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender},
	},
	thread::{self, JoinHandle},
	time::{Duration, Instant},
};

use tracing::{debug, field, info, warn};
use windows_sys::{
	Win32::{
		Foundation::{
			CLASS_E_NOAGGREGATION, CO_E_SERVER_STOPPING, E_NOINTERFACE, E_OUTOFMEMORY, E_POINTER,
			E_UNEXPECTED, ERROR_ALREADY_INITIALIZED, RPC_E_CALL_CANCELED, S_OK,
		},
		Graphics::Gdi::HBITMAP,
		System::Com::{
			CLSCTX_LOCAL_SERVER, COINIT_MULTITHREADED, CoInitializeEx, CoInitializeSecurity,
			CoRegisterClassObject, CoRevokeClassObject, CoTaskMemFree, CoTestCancel,
			CoUninitialize, EOAC_NONE, REGCLS_MULTIPLEUSE, RPC_C_AUTHN_LEVEL_DEFAULT,
			RPC_C_IMP_LEVEL_IDENTIFY,
		},
		UI::Shell::{
			SIGDN, SIGDN_FILESYSPATH, WTS_ALPHATYPE, WTS_E_DATAFILEUNAVAILABLE,
			WTS_E_EXTRACTIONPENDING, WTS_E_EXTRACTIONTIMEDOUT, WTS_E_FAILEDEXTRACTION, WTSAT_ARGB,
			WTSAT_RGB, WTSAT_UNKNOWN,
		},
	},
	core::{BOOL, GUID, HRESULT, IUnknown_Vtbl},
};

use super::{
	LOG_TARGET,
	ffi::{AbsoluteDescriptor, DibError, current_user_sid, dib_section, read_wide},
	run_blocking,
};
use crate::{
	os::cfapi::{E_FAIL, E_INVALIDARG, Hresult, explorer::BracedGuid, gate::Gate},
	thumbnails::{Thumbnail, ThumbnailAlpha, ThumbnailCancel, ThumbnailHandler, ThumbnailRefusal},
};

/// How long [`ThumbnailServer::start`] waits for the server's thread to report whether it serves:
/// past it, the helper goes on without thumbnails rather than stalling its start on a COM call.
const START_WAIT: Duration = Duration::from_secs(30);

/// How long stopping waits for the requests in progress: past the deadline of Explorer's requests
/// (1.5 s, the service's `EXPLORER_DEADLINE`), which bounds a `GetThumbnail`, though stopping cancels
/// each, which ends it within [`CANCEL_POLL`] and the cache's next chunk. An `Initialize` may wait
/// on a call into its caller (when the item it is handed is a proxy), which nothing bounds; one
/// still running then is left to the process's exit, and the server's thread does not leave the
/// apartment, as nothing documents whether `CoUninitialize` waits for a call in progress.
const STOP_WAIT: Duration = Duration::from_secs(15);

/// How often the thread servicing a `GetThumbnail` checks, while the handler runs, whether the
/// call was cancelled or the server is stopping.
const CANCEL_POLL: Duration = Duration::from_millis(100);

/// How many [`ThumbnailServer`]s of this process have the class registered.
static SERVING: AtomicUsize = AtomicUsize::new(0);

/// Whether this process serves File Explorer's thumbnail requests now: the one switch the helper's
/// registration of the class and of the root's `ThumbnailProvider` value follows, so a server that
/// failed to start leaves Explorer its own thumbnails instead of the refusing fallback.
pub(crate) fn is_serving() -> bool {
	SERVING.load(Ordering::Acquire) > 0
}

/// How long [`ComSecurity::establish`] waits for its thread to report whether it set the process's
/// COM security: past it, the process serves no thumbnails rather than stalling its start. The
/// helper sets it before `hello`, so this also bounds how long the security holds `hello` up, well
/// under the 30 s the desktop app waits for it.
const SECURE_WAIT: Duration = Duration::from_secs(10);

/// A thread of the COM layer, for errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ComThread {
	/// The thread setting the process's COM security ([`ComSecurity::establish`]).
	Security,
	/// The server's thread ([`ThumbnailServer::start`]).
	Server,
}

impl fmt::Display for ComThread {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(match self {
			Self::Security => "COM security's",
			Self::Server => "thumbnail server's",
		})
	}
}

/// Why the process's COM security was not set, or the thumbnail server did not start. Not logged
/// here: whoever is answered it logs it, once.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ComError {
	/// The thread could not be spawned.
	#[error("the {0} thread could not be started: {1}")]
	Spawn(ComThread, #[source] io::Error),
	/// The thread panicked before it reported.
	#[error("the {0} thread panicked while starting")]
	Panicked(ComThread),
	/// The thread did not report within its wait ([`SECURE_WAIT`], [`START_WAIT`]).
	#[error("the {0} thread did not report in time")]
	Timeout(ComThread),
	/// The security descriptor limiting who may call the server could not be built.
	#[error("the access the thumbnail server grants could not be built: {0}")]
	Security(#[source] io::Error),
	/// A COM call failed.
	#[error("{call} failed with {code}")]
	Com {
		/// The function that failed.
		call: &'static str,
		/// What it returned.
		code: Hresult,
	},
}

/// Proof that this process's COM security is the one [`ComSecurity::establish`] set: only this user
/// and SYSTEM may call its COM objects. Made only by `establish`, and taken by
/// [`ThumbnailServer::start`], so a server never runs under COM's default access or under another
/// caller's.
#[derive(Debug, Clone, Copy)]
pub struct ComSecurity {
	/// Private, so nothing outside this module makes one.
	_private: (),
}

impl ComSecurity {
	/// Sets this process's COM security (see [`secure_process`]) on a thread of its own, which
	/// joins the multithreaded apartment first and, once the security is set, stays in it, parked,
	/// for the life of the process: nothing documents that the process's security outlives the
	/// last thread leaving COM, so the apartment it was set in is never left. Waits at most
	/// [`SECURE_WAIT`] for the thread's report.
	///
	/// Called once per process, first: before any COM or shell use and before the helper builds its
	/// runtime, as COM sets the defaults itself the first time anything in the process marshals an
	/// interface or makes or takes a COM call, after which this fails with `RPC_E_TOO_LATE`, as a
	/// second call does. A failure leaves the process without a [`ComSecurity`], so serving no
	/// thumbnails.
	pub(crate) fn establish() -> Result<Self, ComError> {
		let (ready_tx, ready_rx) = mpsc::sync_channel(1);
		let thread = thread::Builder::new()
			.name("filen-com-security".to_owned())
			.spawn(move || hold_security(&ready_tx))
			.map_err(|error| ComError::Spawn(ComThread::Security, error))?;
		Self::await_report(thread, &ready_rx, SECURE_WAIT)
	}

	/// What [`ComSecurity::establish`] answers once `thread`, the COM security's thread, reports on
	/// `ready`, waiting at most `wait`.
	fn await_report(
		thread: JoinHandle<()>,
		ready: &Receiver<Result<(), ComError>>,
		wait: Duration,
	) -> Result<Self, ComError> {
		let error = match ready.recv_timeout(wait) {
			Ok(Ok(())) => {
				info!(target: LOG_TARGET, "set the process's COM security");
				// The thread stays, parked in the apartment, for the life of the process.
				drop(thread);
				return Ok(Self { _private: () });
			}
			Ok(Err(error)) => error,
			// The thread ended without a word: it panicked.
			Err(RecvTimeoutError::Disconnected) => ComError::Panicked(ComThread::Security),
			Err(RecvTimeoutError::Timeout) => {
				// The thread is left to finish: should it set the security after all, it is this
				// process's, which then serves nothing under it.
				return Err(ComError::Timeout(ComThread::Security));
			}
		};
		// The thread left the apartment and has ended, or is about to. It panicked only when it
		// ended without a word, which `error` says already.
		let _ = thread.join();
		Err(error)
	}
}

/// The COM security's thread: joins the multithreaded apartment, sets the process's COM security
/// and reports to `ready`. Once set, never returns: the thread stays in the apartment, parked, for
/// the life of the process. Otherwise it leaves the apartment and returns.
fn hold_security(ready: &SyncSender<Result<(), ComError>>) {
	let secured = Apartment::enter().and_then(|apartment| {
		secure_process()?;
		Ok(apartment)
	});
	let apartment = match secured {
		Ok(apartment) => apartment,
		Err(error) => {
			// Fails only when `establish` stopped waiting, which has nothing to report then.
			let _ = ready.send(Err(error));
			return;
		}
	};
	// Fails only when `establish` stopped waiting: the security is set all the same, and held.
	let _ = ready.send(Ok(()));
	let _apartment = apartment;
	loop {
		thread::park();
	}
}

/// The thumbnail class, registered with COM for the life of the value. [`ThumbnailServer::stop`]
/// revokes it, and so does dropping it.
#[derive(Debug)]
pub struct ThumbnailServer {
	/// `Some` until the value drops.
	running: Option<Running>,
}

/// The server's thread, the sender whose drop tells it to stop, and the receiver it reports to
/// once stopped.
#[derive(Debug)]
struct Running {
	/// Nothing is ever sent: dropping it ends the thread's wait.
	stop: Sender<()>,
	/// Whether the thread left the apartment, sent once it stopped serving.
	left: Receiver<bool>,
	thread: JoinHandle<()>,
}

impl ThumbnailServer {
	/// Serves the class `clsid` from this process, a COM local server, answering each
	/// `GetThumbnail` with `handler` called on one of COM's RPC threads, never on the helper's
	/// runtime. Only in a process whose COM security [`ComSecurity::establish`] set, which
	/// `_security` proves: the access the server grants is that security's. Returns once the class
	/// is registered, or with why it was not, within [`START_WAIT`]: past it the thread is left to
	/// revoke the class itself, as no stop is waited on.
	pub(crate) fn start(
		_security: ComSecurity,
		clsid: GUID,
		handler: ThumbnailHandler,
	) -> Result<Self, ComError> {
		let (ready_tx, ready_rx) = mpsc::sync_channel(1);
		let (stop_tx, stop_rx) = mpsc::channel();
		let (left_tx, left_rx) = mpsc::channel();
		let thread = thread::Builder::new()
			.name("filen-thumbnail-com".to_owned())
			.spawn(move || {
				let left = serve(&clsid, handler, &ready_tx, &stop_rx);
				// Fails only when the value dropped already, which then waits for nothing.
				let _ = left_tx.send(left);
			})
			.map_err(|error| ComError::Spawn(ComThread::Server, error))?;
		let error = match ready_rx.recv_timeout(START_WAIT) {
			Ok(Ok(())) => {
				SERVING.fetch_add(1, Ordering::AcqRel);
				return Ok(Self {
					running: Some(Running {
						stop: stop_tx,
						left: left_rx,
						thread,
					}),
				});
			}
			Ok(Err(error)) => error,
			// The thread ended without a word: it panicked.
			Err(RecvTimeoutError::Disconnected) => ComError::Panicked(ComThread::Server),
			Err(RecvTimeoutError::Timeout) => {
				// Dropping `stop_tx` and `ready_rx` here makes the thread revoke the class and leave
				// the apartment as soon as its COM calls return, whatever they answer.
				return Err(ComError::Timeout(ComThread::Server));
			}
		};
		// The thread has ended or is about to. It panicked only when it ended without a word,
		// which `error` says already.
		let _ = thread.join();
		Err(error)
	}

	/// Revokes the class and ends the server's thread; the requests in progress are answered
	/// first, or waited for [`STOP_WAIT`].
	pub(crate) fn stop(self) {
		drop(self);
	}
}

impl Drop for ThumbnailServer {
	fn drop(&mut self) {
		let Some(Running { stop, left, thread }) = self.running.take() else {
			return;
		};
		SERVING.fetch_sub(1, Ordering::AcqRel);
		drop(stop);
		// Waits for the requests in progress, at most `STOP_WAIT`, so off the runtime's workers
		// when dropped on one. A thread that did not leave the apartment is not joined: nothing
		// documents that its exit does not run COM's clean-up of its membership of the apartment,
		// which could wait for the request still in progress. It ends on its own, or with the
		// process.
		let joined = run_blocking(move || match left.recv() {
			Ok(false) => Ok(()),
			Ok(true) | Err(_) => thread.join(),
		});
		if joined.is_err() {
			warn!(target: LOG_TARGET, "the thumbnail server's thread panicked");
		}
	}
}

/// The server's thread: joins the apartment, registers the class and reports to `ready`, then
/// serves until `stop`'s sender drops. Then it revokes the class, waits for the requests in
/// progress and leaves the apartment, unless one outlasted the wait. Returns whether it left the
/// apartment (or never joined it). The COM security's thread stays in the apartment meanwhile, so
/// leaving it here never leaves the process without COM.
fn serve(
	clsid: &GUID,
	handler: ThumbnailHandler,
	ready: &SyncSender<Result<(), ComError>>,
	stop: &Receiver<()>,
) -> bool {
	let started = Apartment::enter().and_then(|apartment| {
		let shared = Arc::new(Shared {
			handler,
			gate: Gate::default(),
			stopping: AtomicBool::new(false),
		});
		let registration = Registration::new(clsid, Arc::clone(&shared))?;
		Ok((apartment, shared, registration))
	});
	let (apartment, shared, registration) = match started {
		Ok(started) => started,
		Err(error) => {
			// Fails only when `start` stopped waiting, which has nothing to report then.
			let _ = ready.send(Err(error));
			return true;
		}
	};
	if ready.send(Ok(())).is_ok() {
		// Nothing is sent on `stop`: this returns once its sender drops.
		let _ = stop.recv();
	}
	// No new activation reaches the class once it is revoked; the gate then refuses new
	// requests on the objects Explorer still holds and waits for those in progress, at most
	// `STOP_WAIT`: for their methods to return, not for COM to deliver the replies. Leaving the
	// apartment does not disconnect those objects: the COM security's thread keeps the apartment
	// alive for the life of the process, so they stay connected, each later request refused by the
	// closed gate (`CO_E_SERVER_STOPPING`), and keep `shared` alive until the process exits. How
	// Explorer takes that refusal, rather than a disconnect, on a stop is not measured. A request
	// still running after the wait keeps what it reads alive: its object holds `shared`, and the
	// vtables are static.
	drop(registration);
	// Each request in progress is cancelled at its next poll, so it ends well before its deadline.
	shared.stopping.store(true, Ordering::Release);
	if shared.gate.close(STOP_WAIT) {
		drop(apartment);
		return true;
	}
	warn!(
		target: LOG_TARGET,
		wait_s = STOP_WAIT.as_secs(),
		"stopping the thumbnail server with a request still in progress",
	);
	// Not left: the COM security's thread keeps the apartment alive, but nothing documents what
	// `CoUninitialize` does on one of its threads while a call is still in progress (whether it
	// waits for it), and the stop must not wait. This thread's membership, and the objects Explorer
	// holds, which the closed gate refuses, go with the process.
	mem::forget(apartment);
	false
}

/// This thread's membership of the multithreaded apartment, left when it drops. Not `Send`:
/// `CoUninitialize` must run on the thread that joined.
struct Apartment(PhantomData<*const ()>);

impl Apartment {
	#[allow(unsafe_code)] // COM FFI: joins the multithreaded apartment.
	fn enter() -> Result<Self, ComError> {
		// SAFETY: no reserved pointer; `COINIT_MULTITHREADED` is a valid mode. The matching
		// `CoUninitialize` runs when the returned value drops, on this thread, as it is not `Send`.
		let hr = unsafe { CoInitializeEx(ptr::null(), COINIT_MULTITHREADED.cast_unsigned()) };
		check("CoInitializeEx", hr)?;
		Ok(Self(PhantomData))
	}
}

impl Drop for Apartment {
	#[allow(unsafe_code)] // COM FFI: leaves the apartment.
	fn drop(&mut self) {
		// SAFETY: this thread joined the apartment in `enter`, which succeeded.
		unsafe { CoUninitialize() };
	}
}

/// The class object registered with COM, revoked when this drops.
struct Registration {
	cookie: u32,
	/// The factory's own reference, released after the revoke.
	factory: *mut ClassFactory,
}

impl Registration {
	#[allow(unsafe_code)] // COM FFI: registers the class object.
	fn new(clsid: &GUID, shared: Arc<Shared>) -> Result<Self, ComError> {
		let factory = ClassFactory::new(shared);
		let mut cookie = 0_u32;
		// SAFETY: `clsid` is a live GUID; `factory` is a live class object whose first field is its
		// `IClassFactory` vtable, which COM `AddRef`s for as long as it holds it; `cookie` is a
		// live out-param. This thread is in the apartment.
		let hr = unsafe {
			CoRegisterClassObject(
				clsid,
				factory.cast(),
				CLSCTX_LOCAL_SERVER,
				REGCLS_MULTIPLEUSE.cast_unsigned(),
				&mut cookie,
			)
		};
		if let Err(error) = check("CoRegisterClassObject", hr) {
			// SAFETY: the reference `ClassFactory::new` returned, released once.
			unsafe { ClassFactory::release(factory) };
			return Err(error);
		}
		info!(target: LOG_TARGET, cookie, "registered the thumbnail class");
		Ok(Self { cookie, factory })
	}
}

impl Drop for Registration {
	#[allow(unsafe_code)] // COM FFI: revokes the class object.
	fn drop(&mut self) {
		// SAFETY: `cookie` is the registration `CoRegisterClassObject` returned, revoked once, on
		// the thread that registered it.
		let hr = unsafe { CoRevokeClassObject(self.cookie) };
		// The factory's own reference goes either way. After a failed revoke COM keeps its
		// reference, and the factory with its `Shared`, until the process exits: the COM security's
		// thread keeps the apartment alive, so leaving it here does not release that reference.
		match check("CoRevokeClassObject", hr) {
			Ok(()) => {
				info!(target: LOG_TARGET, cookie = self.cookie, "revoked the thumbnail class")
			}
			Err(error) => warn!(target: LOG_TARGET, %error, "could not revoke the thumbnail class"),
		}
		// SAFETY: the factory's own reference, held since `new`, released once.
		unsafe { ClassFactory::release(self.factory) };
	}
}

/// `Ok` when `hr` reports success, the failure of `call` otherwise.
fn check(call: &'static str, hr: HRESULT) -> Result<(), ComError> {
	if hr >= 0 {
		return Ok(());
	}
	Err(ComError::Com {
		call,
		code: Hresult::new(hr),
	})
}

/// Lets only this user and SYSTEM call the process's COM objects, at the default authentication
/// level. The impersonation level, identify, is the one this process's own outgoing calls would
/// have without the call (the system's default), so it changes none of them.
///
/// Fails closed: when the call is refused, so is every server, since the default access the
/// process would run with instead is unverified. A process sets its COM security once: a second
/// call, and a call after COM set the defaults itself, are refused with `RPC_E_TOO_LATE`. Called
/// only by [`ComSecurity::establish`], once per process.
#[allow(unsafe_code)] // COM FFI: sets the process's COM security.
fn secure_process() -> Result<(), ComError> {
	let descriptor = access_descriptor().map_err(ComError::Security)?;
	// SAFETY: `descriptor` is a valid absolute security descriptor; no authentication service
	// list, authentication list or reserved pointer. This thread is in the apartment. The call
	// takes the descriptor as an `[in]` parameter and only reads it, so the mutable pointer
	// `as_ptr` derives from a shared borrow is never written through.
	let hr = unsafe {
		CoInitializeSecurity(
			descriptor.as_ptr(),
			// COM chooses the authentication services.
			-1,
			ptr::null(),
			ptr::null(),
			RPC_C_AUTHN_LEVEL_DEFAULT,
			RPC_C_IMP_LEVEL_IDENTIFY,
			ptr::null(),
			EOAC_NONE.cast_unsigned(),
			ptr::null(),
		)
	};
	check("CoInitializeSecurity", hr)?;
	// Never freed: nothing documents that COM copies the descriptor, so it lives as long as COM
	// may read it, the process's life. A few hundred bytes, once per process.
	mem::forget(descriptor);
	Ok(())
}

/// The access [`secure_process`] grants: owner and group this user; `COM_RIGHTS_EXECUTE` and
/// `COM_RIGHTS_EXECUTE_LOCAL` (0x3) for this user and for SYSTEM, nobody else.
fn access_descriptor() -> io::Result<AbsoluteDescriptor> {
	let sid = String::from_utf16(&current_user_sid()?)
		.map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
	AbsoluteDescriptor::from_sddl(&format!("O:{sid}G:{sid}D:(A;;0x3;;;{sid})(A;;0x3;;;SY)"))
}

/// The `IUnknown` IID: `windows_sys::core::IID_IUnknown` as a `u128`, so it can be matched on.
const IID_IUNKNOWN: u128 = 0x0000_0000_0000_0000_c000_0000_0000_0046;
/// The `IClassFactory` IID (`unknwn.h`).
const IID_ICLASSFACTORY: u128 = 0x0000_0001_0000_0000_c000_0000_0000_0046;
/// The `IInitializeWithItem` IID (`shobjidl_core.h`).
const IID_IINITIALIZEWITHITEM: u128 = 0x7f73_be3f_fb79_493c_a6c7_7ee1_4e24_5841;
/// The `IThumbnailProvider` IID (`thumbcache.h`).
const IID_ITHUMBNAILPROVIDER: u128 = 0xe357_fccd_a995_4576_b01f_2346_3015_4e96;

/// `guid` as the `u128` `GUID::from_u128` takes, since `GUID` does not implement `PartialEq`.
fn guid_u128(guid: &GUID) -> u128 {
	(u128::from(guid.data1) << 96)
		| (u128::from(guid.data2) << 80)
		| (u128::from(guid.data3) << 64)
		| u128::from(u64::from_be_bytes(guid.data4))
}

/// The `IClassFactory` vtable.
// Its slots are read by COM through the interface pointer, never by name.
#[allow(dead_code)]
#[repr(C)]
struct IClassFactoryVtbl {
	base: IUnknown_Vtbl,
	create_instance: unsafe extern "system" fn(
		this: *mut c_void,
		outer: *mut c_void,
		riid: *const GUID,
		ppv: *mut *mut c_void,
	) -> HRESULT,
	lock_server: unsafe extern "system" fn(this: *mut c_void, lock: BOOL) -> HRESULT,
}

/// The `IInitializeWithItem` vtable.
// Its slots are read by COM through the interface pointer, never by name.
#[allow(dead_code)]
#[repr(C)]
struct IInitializeWithItemVtbl {
	base: IUnknown_Vtbl,
	initialize:
		unsafe extern "system" fn(this: *mut c_void, item: *mut c_void, mode: u32) -> HRESULT,
}

/// The `IThumbnailProvider` vtable.
// Its slots are read by COM through the interface pointer, never by name.
#[allow(dead_code)]
#[repr(C)]
struct IThumbnailProviderVtbl {
	base: IUnknown_Vtbl,
	get_thumbnail: unsafe extern "system" fn(
		this: *mut c_void,
		cx: u32,
		bitmap: *mut HBITMAP,
		alpha: *mut WTS_ALPHATYPE,
	) -> HRESULT,
}

/// The start of the `IShellItem` vtable, up to `GetDisplayName`, the one slot called;
/// `GetAttributes` and `Compare` follow it.
#[repr(C)]
struct IShellItemVtbl {
	// Declared for the offset of the slots after it only.
	#[allow(dead_code)]
	base: IUnknown_Vtbl,
	/// `BindToHandler`, never called: it is how a stream of the content would be had.
	_bind_to_handler: *const c_void,
	/// `GetParent`, never called.
	_get_parent: *const c_void,
	get_display_name:
		unsafe extern "system" fn(this: *mut c_void, sigdn: SIGDN, name: *mut *mut u16) -> HRESULT,
}

/// What every object of one server shares.
struct Shared {
	handler: ThumbnailHandler,
	gate: Gate,
	/// Set once the server stops: cancels every request in progress.
	stopping: AtomicBool,
}

/// Runs the body of a method COM calls, answering `on_panic` instead of unwinding into COM, which
/// would abort the process.
fn guard<T>(method: &'static str, on_panic: T, body: impl FnOnce() -> T) -> T {
	panic::catch_unwind(AssertUnwindSafe(body)).unwrap_or_else(|_| {
		warn!(target: LOG_TARGET, method, "a COM method panicked");
		on_panic
	})
}

/// `QueryInterface`'s answer: the interface `resolve` gives for the IID written to `ppv`, which
/// `resolve` has `AddRef`ed, or `E_NOINTERFACE` with a null `*ppv` when it gives none.
///
/// # Safety
///
/// `riid` is null or points at a GUID, and `ppv` is null or writable, both for the call.
#[allow(unsafe_code)] // COM FFI: reads an IID and writes an out-param.
unsafe fn answer_query(
	object: &'static str,
	riid: *const GUID,
	ppv: *mut *mut c_void,
	resolve: impl FnOnce(u128) -> Option<*mut c_void>,
) -> HRESULT {
	if ppv.is_null() {
		return E_POINTER;
	}
	// SAFETY: `ppv` is non-null and writable (the caller's promise).
	unsafe { ppv.write(ptr::null_mut()) };
	if riid.is_null() {
		return E_POINTER;
	}
	// SAFETY: `riid` is non-null and points at a GUID (the caller's promise).
	let iid = guid_u128(unsafe { &*riid });
	match resolve(iid) {
		Some(interface) => {
			// SAFETY: as above.
			unsafe { ppv.write(interface) };
			S_OK
		}
		None => {
			debug!(target: LOG_TARGET, object, iid = %BracedGuid(iid), "asked for an interface not offered");
			E_NOINTERFACE
		}
	}
}

/// `count` one higher, returning the new count. Wraps, as the atomic does.
fn add_ref(count: &AtomicU32) -> u32 {
	count.fetch_add(1, Ordering::Relaxed).wrapping_add(1)
}

/// `count` one lower, returning the new count; at 0 the caller frees the object, after which no
/// field may be touched. Wraps, as the atomic does: a `Release` past the last reference breaks
/// COM's contract, and nothing here guards against it.
fn release_ref(count: &AtomicU32) -> u32 {
	count.fetch_sub(1, Ordering::AcqRel).wrapping_sub(1)
}

/// The class factory COM holds while the class is registered: makes one [`ThumbnailObject`] per
/// request.
#[repr(C)]
struct ClassFactory {
	/// First, as COM reads an interface pointer as a pointer to its vtable.
	// Read by COM through the interface pointer, never by name.
	#[allow(dead_code)]
	vtbl: &'static IClassFactoryVtbl,
	refs: AtomicU32,
	shared: Arc<Shared>,
}

static CLASS_FACTORY_VTBL: IClassFactoryVtbl = IClassFactoryVtbl {
	base: IUnknown_Vtbl {
		QueryInterface: factory_query_interface,
		AddRef: factory_add_ref,
		Release: factory_release,
	},
	create_instance: factory_create_instance,
	lock_server: factory_lock_server,
};

impl ClassFactory {
	/// A factory with one reference, which the caller owns. Its address is its `IUnknown` and
	/// `IClassFactory` interface pointer.
	fn new(shared: Arc<Shared>) -> *mut Self {
		Box::into_raw(Box::new(Self {
			vtbl: &CLASS_FACTORY_VTBL,
			refs: AtomicU32::new(1),
			shared,
		}))
	}

	/// The factory behind its interface pointer `this`.
	///
	/// # Safety
	///
	/// `this` is a pointer [`ClassFactory::new`] returned, whose factory holds a reference for at
	/// least `'a`.
	#[allow(unsafe_code)] // COM FFI: the factory behind an interface pointer.
	unsafe fn from_this<'a>(this: *mut c_void) -> &'a Self {
		// SAFETY: the caller's promise.
		unsafe { &*this.cast::<Self>() }
	}

	/// Releases one reference to `this`, freeing it at 0; returns the new count.
	///
	/// # Safety
	///
	/// `this` is a pointer [`ClassFactory::new`] returned, and the caller owns the reference it
	/// releases.
	#[allow(unsafe_code)] // COM FFI: releases a reference to the factory.
	unsafe fn release(this: *mut Self) -> u32 {
		// SAFETY: the caller's reference keeps the factory alive for the decrement.
		let left = release_ref(unsafe { &(*this).refs });
		if left == 0 {
			// SAFETY: that was the last reference: nothing else reads the factory, which
			// `ClassFactory::new` allocated with `Box`.
			drop(unsafe { Box::from_raw(this) });
		}
		left
	}
}

/// `IClassFactory::QueryInterface`: `IUnknown` and `IClassFactory`.
///
/// # Safety
///
/// Called by COM: `this` is a live factory's interface pointer, `riid` and `ppv` as
/// [`answer_query`] takes them.
#[allow(unsafe_code)] // COM FFI: a class factory method.
unsafe extern "system" fn factory_query_interface(
	this: *mut c_void,
	riid: *const GUID,
	ppv: *mut *mut c_void,
) -> HRESULT {
	guard("IClassFactory::QueryInterface", E_UNEXPECTED, || {
		// SAFETY: COM holds a reference to the factory for the call.
		let factory = unsafe { ClassFactory::from_this(this) };
		// SAFETY: COM passes `riid` and `ppv` as `answer_query` takes them.
		unsafe {
			answer_query("IClassFactory", riid, ppv, |iid| match iid {
				IID_IUNKNOWN | IID_ICLASSFACTORY => {
					add_ref(&factory.refs);
					Some(this)
				}
				_ => None,
			})
		}
	})
}

/// `IClassFactory::AddRef`.
///
/// # Safety
///
/// Called by COM: `this` is a live factory's interface pointer.
#[allow(unsafe_code)] // COM FFI: a class factory method.
unsafe extern "system" fn factory_add_ref(this: *mut c_void) -> u32 {
	guard("IClassFactory::AddRef", 0, || {
		// SAFETY: COM holds a reference to the factory for the call.
		add_ref(&unsafe { ClassFactory::from_this(this) }.refs)
	})
}

/// `IClassFactory::Release`.
///
/// # Safety
///
/// Called by COM: `this` is a live factory's interface pointer, and COM owns the reference it
/// releases.
#[allow(unsafe_code)] // COM FFI: a class factory method.
unsafe extern "system" fn factory_release(this: *mut c_void) -> u32 {
	guard("IClassFactory::Release", 0, || {
		// SAFETY: COM's promise, as above; the interface pointer is the factory's address.
		unsafe { ClassFactory::release(this.cast()) }
	})
}

/// `IClassFactory::CreateInstance`: one [`ThumbnailObject`], asked for `riid`.
///
/// # Safety
///
/// Called by COM: `this` is a live factory's interface pointer, `riid` and `ppv` as
/// [`answer_query`] takes them.
#[allow(unsafe_code)] // COM FFI: a class factory method.
unsafe extern "system" fn factory_create_instance(
	this: *mut c_void,
	outer: *mut c_void,
	riid: *const GUID,
	ppv: *mut *mut c_void,
) -> HRESULT {
	guard("IClassFactory::CreateInstance", E_UNEXPECTED, || {
		if ppv.is_null() {
			return E_POINTER;
		}
		// SAFETY: `ppv` is non-null, and COM passes it writable.
		unsafe { ppv.write(ptr::null_mut()) };
		if !outer.is_null() {
			return CLASS_E_NOAGGREGATION;
		}
		// SAFETY: COM holds a reference to the factory for the call.
		let factory = unsafe { ClassFactory::from_this(this) };
		if factory.shared.gate.is_closed() {
			return CO_E_SERVER_STOPPING;
		}
		let object = ThumbnailObject::new(Arc::clone(&factory.shared));
		// SAFETY: `object` holds the reference `new` gave it until the release below; `riid` and
		// `ppv` as COM passed them.
		let hr = unsafe { object_query_interface::<INIT_OFFSET>(object.cast(), riid, ppv) };
		// SAFETY: the reference `ThumbnailObject::new` returned, released once; on success the
		// query took one of its own.
		unsafe { ThumbnailObject::release(object) };
		// SAFETY: `riid` is null or points at a GUID, as COM passes it.
		let iid = (!riid.is_null()).then(|| BracedGuid(guid_u128(unsafe { &*riid })));
		debug!(
			target: LOG_TARGET,
			iid = iid.as_ref().map(field::display),
			code = %Hresult::new(hr),
			"made a thumbnail object",
		);
		hr
	})
}

/// `IClassFactory::LockServer`: nothing to do, as the server lives as long as the helper (or the
/// `--thumbnail-server` instance's idle timer), not as long as COM's lock count.
///
/// # Safety
///
/// Called by COM.
#[allow(unsafe_code)] // COM FFI: a class factory method.
unsafe extern "system" fn factory_lock_server(_this: *mut c_void, _lock: BOOL) -> HRESULT {
	S_OK
}

/// One request of Explorer's: the item's path from `IInitializeWithItem::Initialize`, then its
/// thumbnail from `IThumbnailProvider::GetThumbnail`. One allocation with two interfaces, each
/// interface pointer the address of its vtable field.
#[repr(C)]
struct ThumbnailObject {
	/// At offset 0, so the object's address is its `IUnknown` and `IInitializeWithItem`.
	// Read by COM through the interface pointer, never by name.
	#[allow(dead_code)]
	init: &'static IInitializeWithItemVtbl,
	// Read by COM through the interface pointer, never by name.
	#[allow(dead_code)]
	provider: &'static IThumbnailProviderVtbl,
	/// One count for both interfaces.
	refs: AtomicU32,
	shared: Arc<Shared>,
	/// Set once, by `Initialize`.
	path: OnceLock<PathBuf>,
}

/// Where the `IInitializeWithItem` interface pointer points in a [`ThumbnailObject`].
const INIT_OFFSET: usize = offset_of!(ThumbnailObject, init);
/// Where the `IThumbnailProvider` interface pointer points in a [`ThumbnailObject`].
const PROVIDER_OFFSET: usize = offset_of!(ThumbnailObject, provider);

static INITIALIZE_WITH_ITEM_VTBL: IInitializeWithItemVtbl = IInitializeWithItemVtbl {
	base: IUnknown_Vtbl {
		QueryInterface: object_query_interface::<INIT_OFFSET>,
		AddRef: object_add_ref::<INIT_OFFSET>,
		Release: object_release::<INIT_OFFSET>,
	},
	initialize: object_initialize,
};

static THUMBNAIL_PROVIDER_VTBL: IThumbnailProviderVtbl = IThumbnailProviderVtbl {
	base: IUnknown_Vtbl {
		QueryInterface: object_query_interface::<PROVIDER_OFFSET>,
		AddRef: object_add_ref::<PROVIDER_OFFSET>,
		Release: object_release::<PROVIDER_OFFSET>,
	},
	get_thumbnail: object_get_thumbnail,
};

impl ThumbnailObject {
	/// An object with one reference, which the caller owns.
	fn new(shared: Arc<Shared>) -> *mut Self {
		Box::into_raw(Box::new(Self {
			init: &INITIALIZE_WITH_ITEM_VTBL,
			provider: &THUMBNAIL_PROVIDER_VTBL,
			refs: AtomicU32::new(1),
			shared,
			path: OnceLock::new(),
		}))
	}

	/// The object whose interface at `OFFSET` is `this`. Pointer arithmetic only, so the result
	/// keeps the provenance of the whole allocation, which every interface pointer is made from.
	fn from_interface<const OFFSET: usize>(this: *mut c_void) -> *mut Self {
		this.wrapping_byte_sub(OFFSET).cast()
	}

	/// The interface pointer at `offset` of the object at `object`.
	fn interface(object: *mut Self, offset: usize) -> *mut c_void {
		object.wrapping_byte_add(offset).cast()
	}

	/// Releases one reference to `object`, freeing it at 0; returns the new count.
	///
	/// # Safety
	///
	/// `object` is a pointer [`ThumbnailObject::new`] returned, and the caller owns the reference
	/// it releases.
	#[allow(unsafe_code)] // COM FFI: releases a reference to a thumbnail object.
	unsafe fn release(object: *mut Self) -> u32 {
		// SAFETY: the caller's reference keeps the object alive for the decrement.
		let left = release_ref(unsafe { &(*object).refs });
		if left == 0 {
			// SAFETY: that was the last reference: nothing else reads the object, which
			// `ThumbnailObject::new` allocated with `Box`.
			drop(unsafe { Box::from_raw(object) });
		}
		left
	}
}

/// `QueryInterface` of a thumbnail object, through its interface at `OFFSET`: `IUnknown` and
/// `IInitializeWithItem` at offset 0, `IThumbnailProvider` at its own.
///
/// # Safety
///
/// `this` is the interface pointer at `OFFSET` of a live thumbnail object the caller holds a
/// reference to, `riid` and `ppv` as [`answer_query`] takes them.
#[allow(unsafe_code)] // COM FFI: a thumbnail object method.
unsafe extern "system" fn object_query_interface<const OFFSET: usize>(
	this: *mut c_void,
	riid: *const GUID,
	ppv: *mut *mut c_void,
) -> HRESULT {
	guard("ThumbnailObject::QueryInterface", E_UNEXPECTED, || {
		let object = ThumbnailObject::from_interface::<OFFSET>(this);
		// SAFETY: the caller's reference keeps the object alive for the call.
		let refs = unsafe { &(*object).refs };
		let offset = |iid: u128| match iid {
			IID_IUNKNOWN | IID_IINITIALIZEWITHITEM => Some(INIT_OFFSET),
			IID_ITHUMBNAILPROVIDER => Some(PROVIDER_OFFSET),
			_ => None,
		};
		// SAFETY: `riid` and `ppv` as the caller passed them.
		unsafe {
			answer_query("ThumbnailObject", riid, ppv, |iid| {
				offset(iid).map(|offset| {
					add_ref(refs);
					ThumbnailObject::interface(object, offset)
				})
			})
		}
	})
}

/// `AddRef` of a thumbnail object, through its interface at `OFFSET`.
///
/// # Safety
///
/// `this` is the interface pointer at `OFFSET` of a live thumbnail object the caller holds a
/// reference to.
#[allow(unsafe_code)] // COM FFI: a thumbnail object method.
unsafe extern "system" fn object_add_ref<const OFFSET: usize>(this: *mut c_void) -> u32 {
	guard("ThumbnailObject::AddRef", 0, || {
		let object = ThumbnailObject::from_interface::<OFFSET>(this);
		// SAFETY: the caller's reference keeps the object alive for the call.
		add_ref(unsafe { &(*object).refs })
	})
}

/// `Release` of a thumbnail object, through its interface at `OFFSET`.
///
/// # Safety
///
/// `this` is the interface pointer at `OFFSET` of a live thumbnail object, and the caller owns the
/// reference it releases.
#[allow(unsafe_code)] // COM FFI: a thumbnail object method.
unsafe extern "system" fn object_release<const OFFSET: usize>(this: *mut c_void) -> u32 {
	guard("ThumbnailObject::Release", 0, || {
		// SAFETY: the caller's promise, as above.
		unsafe { ThumbnailObject::release(ThumbnailObject::from_interface::<OFFSET>(this)) }
	})
}

/// `IInitializeWithItem::Initialize`: keeps the item's path. Neither keeps `item` nor opens the
/// path: reading the content would hydrate the placeholder.
///
/// # Safety
///
/// Called by COM: `this` is a live thumbnail object's `IInitializeWithItem` pointer, and `item` is
/// null or an `IShellItem` live for the call.
#[allow(unsafe_code)] // COM FFI: a thumbnail object method.
unsafe extern "system" fn object_initialize(
	this: *mut c_void,
	item: *mut c_void,
	_mode: u32,
) -> HRESULT {
	guard("IInitializeWithItem::Initialize", E_UNEXPECTED, || {
		let already = Hresult::from_win32(ERROR_ALREADY_INITIALIZED).get();
		// SAFETY: COM holds a reference to the object for the call.
		let object = unsafe { &*ThumbnailObject::from_interface::<INIT_OFFSET>(this) };
		if item.is_null() {
			return E_INVALIDARG;
		}
		if object.path.get().is_some() {
			return already;
		}
		// Counted like a thumbnail request, as it may call back into Explorer (when `item` is a
		// proxy), so the server stops only once this call is answered too, or once it waited
		// `STOP_WAIT` for it: nothing bounds such a call.
		let Some(_active) = object.shared.gate.enter() else {
			return CO_E_SERVER_STOPPING;
		};
		// SAFETY: `item` is a live `IShellItem` for the call, as COM passes it.
		let path = match unsafe { shell_item_path(item) } {
			Ok(path) => path,
			Err(hr) => return hr,
		};
		match object.path.set(path) {
			Ok(()) => S_OK,
			Err(_) => already,
		}
	})
}

/// The file system path of `item`, from `GetDisplayName(SIGDN_FILESYSPATH)`.
///
/// # Safety
///
/// `item` is an `IShellItem` live for the call.
#[allow(unsafe_code)] // COM FFI: calls IShellItem::GetDisplayName.
unsafe fn shell_item_path(item: *mut c_void) -> Result<PathBuf, HRESULT> {
	// SAFETY: an interface pointer points at its vtable pointer, and `IShellItem`'s vtable starts
	// with the slots `IShellItemVtbl` declares.
	let vtbl = unsafe { &**item.cast::<*const IShellItemVtbl>() };
	let mut name: *mut u16 = ptr::null_mut();
	// SAFETY: `item` is live for the call; `name` is a live out-param, which the call sets to a
	// string it allocated with `CoTaskMemAlloc`.
	let hr = unsafe { (vtbl.get_display_name)(item, SIGDN_FILESYSPATH, &mut name) };
	if hr < 0 {
		warn!(target: LOG_TARGET, code = %Hresult::new(hr), "IShellItem::GetDisplayName failed");
		return Err(hr);
	}
	if name.is_null() {
		warn!(target: LOG_TARGET, "IShellItem::GetDisplayName succeeded without a name");
		return Err(E_UNEXPECTED);
	}
	// SAFETY: the call succeeded, so `name` is a NUL-terminated string, read before it is freed.
	let units = unsafe { read_wide(name) };
	// SAFETY: `name` was allocated with `CoTaskMemAlloc` by the call, freed once, here.
	unsafe { CoTaskMemFree(name.cast_const().cast()) };
	match units {
		Some(units) => Ok(PathBuf::from(OsString::from_wide(&units))),
		None => {
			warn!(target: LOG_TARGET, "IShellItem::GetDisplayName returned an overlong name");
			Err(E_FAIL)
		}
	}
}

/// `IThumbnailProvider::GetThumbnail`: the handler's thumbnail of the item, at most `cx` pixels on
/// its longer side, as a top-down 32-bit DIB section the caller owns.
///
/// # Safety
///
/// Called by COM: `this` is a live thumbnail object's `IThumbnailProvider` pointer, and `bitmap`
/// and `alpha` are null or writable for the call.
#[allow(unsafe_code)] // COM FFI: a thumbnail object method.
unsafe extern "system" fn object_get_thumbnail(
	this: *mut c_void,
	cx: u32,
	bitmap: *mut HBITMAP,
	alpha: *mut WTS_ALPHATYPE,
) -> HRESULT {
	guard("IThumbnailProvider::GetThumbnail", E_UNEXPECTED, || {
		if bitmap.is_null() || alpha.is_null() {
			if !bitmap.is_null() {
				// SAFETY: non-null, and COM passes it writable.
				unsafe { bitmap.write(ptr::null_mut()) };
			}
			return E_POINTER;
		}
		// SAFETY: both are non-null, and COM passes them writable.
		unsafe {
			bitmap.write(ptr::null_mut());
			alpha.write(WTSAT_UNKNOWN);
		}
		// SAFETY: COM holds a reference to the object for the call.
		let object = unsafe { &*ThumbnailObject::from_interface::<PROVIDER_OFFSET>(this) };
		let Some(path) = object.path.get() else {
			return E_UNEXPECTED;
		};
		let started = Instant::now();
		// Counted until the out-params are written, so the stop waits for the whole method.
		let active = object.shared.gate.enter();
		let answer = match &active {
			None => Err(CO_E_SERVER_STOPPING),
			// Off the helper's runtime, as the handler needs, and watched from this thread, the one
			// servicing the call. No wait of its own: Explorer has up to two requests in flight
			// (measured), the service queues them for its slot and bounds each.
			Some(_) => answer_watched(&object.shared.handler, path, cx, || {
				object.shared.stopping.load(Ordering::Acquire) || call_cancelled()
			})
			.map_err(refusal_hresult)
			.and_then(|thumbnail| {
				let section = dib_section(thumbnail.width(), thumbnail.height(), thumbnail.bgra())
					.map_err(dib_hresult)?;
				Ok((section, alpha_type(thumbnail.alpha())))
			}),
		};
		// Until it is handed out, `DibSection` deletes the bitmap should anything below panic.
		let hr = answer.as_ref().map_or_else(|hr| *hr, |_| S_OK);
		debug!(
			target: LOG_TARGET,
			cx,
			code = %Hresult::new(hr),
			elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
			"answered a thumbnail request",
		);
		// Handed out last, so nothing that could panic runs between the handover and the answer.
		if let Ok((section, kind)) = answer {
			// Never deleted here: across processes, COM's stub marshals the bitmap and frees this
			// copy, as in process the caller owns it. Measured: no growth of the helper's GDI
			// object count over 476 thumbnails, so a `DeleteObject` here would free it twice.
			// SAFETY: both are non-null, checked above, and COM passes them writable; the caller
			// now owns the section.
			unsafe {
				bitmap.write(section.into_raw());
				alpha.write(kind);
			}
		}
		drop(active);
		hr
	})
}

/// `handler`'s answer for `path` at `cx`, made on a thread of its own while this thread, the one
/// servicing the call, checks every [`CANCEL_POLL`] whether `cancelled` says the answer is no
/// longer wanted, and then cancels the handler's request, which ends it early. Made on this
/// thread, unwatched, when no thread can be spawned. A panic of the handler is handed on. No limit
/// of its own: the handler's answer is waited for however long it takes, so bounding it, with a
/// deadline and once cancelled, is the handler's part (see [`ThumbnailHandler`]).
fn answer_watched(
	handler: &ThumbnailHandler,
	path: &Path,
	cx: u32,
	cancelled: impl Fn() -> bool,
) -> Result<Thumbnail, ThumbnailRefusal> {
	let cancel = ThumbnailCancel::default();
	// Borrows only, so it runs here too should the thread not start.
	let work = || handler(path, cx, &cancel);
	let (done, answer) = mpsc::sync_channel(1);
	thread::scope(|scope| {
		let spawned = thread::Builder::new()
			.name("filen-thumbnail-request".to_owned())
			.spawn_scoped(scope, move || {
				// Fails only once this thread stopped waiting, which happens only on its panic.
				let _ = done.send(work());
			});
		let worker = match spawned {
			Ok(worker) => worker,
			Err(error) => {
				warn!(target: LOG_TARGET, %error, "could not spawn a thumbnail request's thread; answering unwatched");
				return work();
			}
		};
		loop {
			match answer.recv_timeout(CANCEL_POLL) {
				Ok(answer) => return answer,
				Err(RecvTimeoutError::Timeout) => {
					if !cancel.is_cancelled() && cancelled() {
						debug!(target: LOG_TARGET, "a thumbnail request is no longer wanted; cancelled");
						cancel.cancel();
					}
				}
				// The thread ended without an answer: it panicked, which the join hands on.
				Err(RecvTimeoutError::Disconnected) => {
					return match worker.join() {
						Ok(()) => Err(ThumbnailRefusal::Failed),
						Err(panic) => panic::resume_unwind(panic),
					};
				}
			}
		}
	})
}

/// Whether the client cancelled the call this thread services: `CoTestCancel` answers
/// `RPC_E_CALL_CANCELED` for it then. Any other answer counts as not cancelled: the call still
/// pending, or no call serviced on this thread at all (a test calling a method directly).
#[allow(unsafe_code)] // COM FFI: asks whether the call this thread services was cancelled.
fn call_cancelled() -> bool {
	// SAFETY: takes no arguments; it reads the call context of the call serviced on this thread,
	// if any, and only answers.
	let hr = unsafe { CoTestCancel() };
	hr == RPC_E_CALL_CANCELED
}

/// What Explorer is told for `refusal`. Whether, and for how long, Explorer remembers each of
/// these is unmeasured; the service remembers the settled ones itself.
fn refusal_hresult(refusal: ThumbnailRefusal) -> HRESULT {
	match refusal {
		// Settled: this item has no thumbnail from us, so Explorer shows the generic icon.
		ThumbnailRefusal::NotInRoot
		| ThumbnailRefusal::NotAPlaceholder
		| ThumbnailRefusal::Directory
		| ThumbnailRefusal::Unsupported
		| ThumbnailRefusal::TooLarge => WTS_E_FAILEDEXTRACTION,
		// The data is not available now, and may be later.
		ThumbnailRefusal::Offline | ThumbnailRefusal::SignedOut => WTS_E_DATAFILEUNAVAILABLE,
		// Still being made, in the background, and kept once made: OneDrive answers a dehydrated
		// placeholder so, and its callers ask again (Microsoft Q&A; undocumented, and whether
		// Explorer asks again sooner for it is unmeasured).
		ThumbnailRefusal::Pending => WTS_E_EXTRACTIONPENDING,
		// No room to make it now, or the request was cancelled.
		ThumbnailRefusal::Busy => WTS_E_EXTRACTIONTIMEDOUT,
		ThumbnailRefusal::Failed => E_FAIL,
	}
}

/// The alpha type Explorer is told for a thumbnail's pixels.
fn alpha_type(alpha: ThumbnailAlpha) -> WTS_ALPHATYPE {
	match alpha {
		ThumbnailAlpha::Opaque => WTSAT_RGB,
		// Straight: Explorer composites a `WTSAT_ARGB` bitmap as straight alpha (measured), so
		// premultiplied pixels came out darkened by their alpha twice.
		ThumbnailAlpha::Straight => WTSAT_ARGB,
	}
}

/// What Explorer is told when no DIB section could be made for a thumbnail.
fn dib_hresult(error: DibError) -> HRESULT {
	match error {
		DibError::Size => E_FAIL,
		DibError::Gdi => E_OUTOFMEMORY,
	}
}

#[cfg(test)]
mod tests {
	use std::{
		ffi::c_void,
		io,
		panic::{self, AssertUnwindSafe},
		path::{Path, PathBuf},
		ptr,
		sync::{
			Arc, Mutex, PoisonError,
			atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
			mpsc,
		},
		thread,
		time::{Duration, Instant},
	};

	use windows_sys::{
		Win32::{
			Foundation::{
				CLASS_E_NOAGGREGATION, CO_E_SERVER_STOPPING, E_NOINTERFACE, E_OUTOFMEMORY,
				E_POINTER, E_UNEXPECTED, RPC_E_TOO_LATE, S_OK,
			},
			Graphics::Gdi::HBITMAP,
			System::Com::CoTaskMemAlloc,
			UI::Shell::{SIGDN, SIGDN_FILESYSPATH, WTS_ALPHATYPE, WTSAT_ARGB, WTSAT_UNKNOWN},
		},
		core::{GUID, HRESULT, IID_IUnknown, IUnknown_Vtbl},
	};

	use super::{
		ClassFactory, ComError, ComSecurity, ComThread, DibError, Gate, IClassFactoryVtbl,
		IID_ICLASSFACTORY, IID_IINITIALIZEWITHITEM, IID_ITHUMBNAILPROVIDER, IID_IUNKNOWN,
		IInitializeWithItemVtbl, IThumbnailProviderVtbl, PROVIDER_OFFSET, Shared, ThumbnailObject,
		ThumbnailServer, access_descriptor, add_ref, alpha_type, answer_watched, call_cancelled,
		dib_hresult, dib_section, guid_u128, is_serving, refusal_hresult, release_ref,
	};
	use crate::{
		os::{
			cfapi::explorer::BracedGuid,
			win::{
				ffi::{
					DescriptorEntries, current_user_sid, delete_bitmap_for_test,
					descriptor_entries_for_test, dib_bits_for_test, first_column_for_test,
				},
				tests::run_alone,
			},
		},
		thumbnails::{
			Thumbnail, ThumbnailAlpha, ThumbnailCancel, ThumbnailHandler, ThumbnailRefusal,
		},
	};

	/// `IInitializeWithStream`, which is never offered.
	const IID_IINITIALIZEWITHSTREAM: u128 = 0xb824_b49d_22ac_4161_ac8a_9916_e8fa_3f7f;
	/// `IInitializeWithFile`, which is never offered.
	const IID_IINITIALIZEWITHFILE: u128 = 0xb7d1_4566_0509_4cce_a71f_0a55_4233_bd9b;

	/// What every object of a test server shares, answering each request with `handler`.
	fn shared(
		handler: impl Fn(&Path, u32, &ThumbnailCancel) -> Result<Thumbnail, ThumbnailRefusal>
		+ Send
		+ Sync
		+ 'static,
	) -> Arc<Shared> {
		Arc::new(Shared {
			handler: Arc::new(handler),
			gate: Gate::default(),
			stopping: AtomicBool::new(false),
		})
	}

	/// A handler's answer once `cancel` is set: [`ThumbnailRefusal::Busy`], as the service's; fails
	/// after a minute without.
	fn busy_once_cancelled(cancel: &ThumbnailCancel) -> Result<Thumbnail, ThumbnailRefusal> {
		let started = Instant::now();
		while !cancel.is_cancelled() {
			assert!(
				started.elapsed() < Duration::from_secs(60),
				"the request was never cancelled"
			);
			thread::sleep(Duration::from_millis(5));
		}
		Err(ThumbnailRefusal::Busy)
	}

	/// The vtable `this` points at, read as a `T`.
	#[allow(unsafe_code)] // Tests: reads an interface pointer's vtable.
	fn vtbl<T>(this: *mut c_void) -> &'static T {
		// SAFETY: every interface pointer the tests pass points at a static vtable starting with
		// the slots of `T`.
		unsafe { &**this.cast::<*const T>() }
	}

	/// `QueryInterface` on `this` for `iid`: the answer and what it wrote.
	#[allow(unsafe_code)] // Tests: calls a COM method.
	fn query(this: *mut c_void, iid: u128) -> (HRESULT, *mut c_void) {
		let guid = GUID::from_u128(iid);
		let mut out = ptr::dangling_mut::<c_void>();
		// SAFETY: `this` is a live interface pointer the test holds a reference to; `guid` and
		// `out` are live for the call.
		let hr = unsafe { (vtbl::<IUnknown_Vtbl>(this).QueryInterface)(this, &guid, &mut out) };
		(hr, out)
	}

	/// `Release` on `this`: the count left.
	#[allow(unsafe_code)] // Tests: calls a COM method.
	fn release(this: *mut c_void) -> u32 {
		// SAFETY: `this` is a live interface pointer whose reference the test owns.
		unsafe { (vtbl::<IUnknown_Vtbl>(this).Release)(this) }
	}

	/// `IInitializeWithItem::Initialize` on the object at `object` with `item`.
	#[allow(unsafe_code)] // Tests: calls a COM method.
	fn initialize(object: *mut c_void, item: *mut c_void) -> HRESULT {
		// SAFETY: `object` is a live thumbnail object's first interface pointer; `item` is null
		// or a live fake shell item.
		unsafe { (vtbl::<IInitializeWithItemVtbl>(object).initialize)(object, item, 0) }
	}

	/// `IThumbnailProvider::GetThumbnail` on the object at `object` for `cx`: the answer, the
	/// bitmap and the alpha type it wrote.
	#[allow(unsafe_code)] // Tests: calls a COM method.
	fn get_thumbnail(object: *mut c_void, cx: u32) -> (HRESULT, HBITMAP, WTS_ALPHATYPE) {
		let provider = object.wrapping_byte_add(PROVIDER_OFFSET);
		let mut bitmap: HBITMAP = ptr::dangling_mut();
		let mut alpha: WTS_ALPHATYPE = -1;
		// SAFETY: `provider` is the live object's `IThumbnailProvider` pointer; the out-params are
		// live for the call.
		let hr = unsafe {
			(vtbl::<IThumbnailProviderVtbl>(provider).get_thumbnail)(
				provider,
				cx,
				&mut bitmap,
				&mut alpha,
			)
		};
		(hr, bitmap, alpha)
	}

	/// An `IShellItem` that only answers `GetDisplayName(SIGDN_FILESYSPATH)`, with its path.
	#[repr(C)]
	struct FakeShellItem {
		vtbl: &'static FakeShellItemVtbl,
		/// NUL-terminated.
		path: Vec<u16>,
	}

	#[repr(C)]
	struct FakeShellItemVtbl {
		base: IUnknown_Vtbl,
		bind_to_handler: usize,
		get_parent: usize,
		get_display_name: unsafe extern "system" fn(
			this: *mut c_void,
			sigdn: SIGDN,
			name: *mut *mut u16,
		) -> HRESULT,
	}

	static FAKE_SHELL_ITEM_VTBL: FakeShellItemVtbl = FakeShellItemVtbl {
		base: IUnknown_Vtbl {
			QueryInterface: fake_query_interface,
			AddRef: fake_count,
			Release: fake_count,
		},
		bind_to_handler: 0,
		get_parent: 0,
		get_display_name: fake_get_display_name,
	};

	impl FakeShellItem {
		fn new(path: &str) -> Self {
			Self {
				vtbl: &FAKE_SHELL_ITEM_VTBL,
				path: path.encode_utf16().chain([0]).collect(),
			}
		}

		fn as_interface(&mut self) -> *mut c_void {
			ptr::from_mut(self).cast()
		}
	}

	/// Never called: the code under test must not ask the item for anything.
	#[allow(unsafe_code)] // Tests: a fake COM method.
	unsafe extern "system" fn fake_query_interface(
		_this: *mut c_void,
		_riid: *const GUID,
		_ppv: *mut *mut c_void,
	) -> HRESULT {
		E_UNEXPECTED
	}

	/// Never called: the code under test must not keep the item.
	#[allow(unsafe_code)] // Tests: a fake COM method.
	unsafe extern "system" fn fake_count(_this: *mut c_void) -> u32 {
		1
	}

	#[allow(unsafe_code)] // Tests: a fake COM method.
	unsafe extern "system" fn fake_get_display_name(
		this: *mut c_void,
		sigdn: SIGDN,
		name: *mut *mut u16,
	) -> HRESULT {
		if sigdn != SIGDN_FILESYSPATH {
			return E_UNEXPECTED;
		}
		// SAFETY: `this` is a live `FakeShellItem`, as the tests pass it.
		let path = unsafe { &(*this.cast::<FakeShellItem>()).path };
		let bytes = path.len() * size_of::<u16>();
		// SAFETY: allocates `bytes` bytes, freed by the code under test with `CoTaskMemFree`.
		let copy = unsafe { CoTaskMemAlloc(bytes) }.cast::<u16>();
		if copy.is_null() {
			return E_OUTOFMEMORY;
		}
		// SAFETY: `copy` holds `path.len()` units, apart from `path`; `name` is writable.
		unsafe {
			ptr::copy_nonoverlapping(path.as_ptr(), copy, path.len());
			name.write(copy);
		}
		S_OK
	}

	/// What [`failing_get_display_name`] answers.
	const DENIED: HRESULT = 0x8007_0005_u32.cast_signed();

	/// A fake `IShellItem` whose `GetDisplayName` fails with [`DENIED`].
	static FAILING_SHELL_ITEM_VTBL: FakeShellItemVtbl = FakeShellItemVtbl {
		base: IUnknown_Vtbl {
			QueryInterface: fake_query_interface,
			AddRef: fake_count,
			Release: fake_count,
		},
		bind_to_handler: 0,
		get_parent: 0,
		get_display_name: failing_get_display_name,
	};

	/// A fake `IShellItem` whose `GetDisplayName` succeeds without a name.
	static NAMELESS_SHELL_ITEM_VTBL: FakeShellItemVtbl = FakeShellItemVtbl {
		base: IUnknown_Vtbl {
			QueryInterface: fake_query_interface,
			AddRef: fake_count,
			Release: fake_count,
		},
		bind_to_handler: 0,
		get_parent: 0,
		get_display_name: nameless_get_display_name,
	};

	#[allow(unsafe_code)] // Tests: a fake COM method.
	unsafe extern "system" fn failing_get_display_name(
		_this: *mut c_void,
		_sigdn: SIGDN,
		_name: *mut *mut u16,
	) -> HRESULT {
		DENIED
	}

	#[allow(unsafe_code)] // Tests: a fake COM method.
	unsafe extern "system" fn nameless_get_display_name(
		_this: *mut c_void,
		_sigdn: SIGDN,
		name: *mut *mut u16,
	) -> HRESULT {
		// SAFETY: `name` is writable, as the code under test passes it.
		unsafe { name.write(ptr::null_mut()) };
		S_OK
	}

	#[test]
	fn an_item_without_a_path_leaves_the_object_uninitialized() {
		let object =
			ThumbnailObject::new(shared(|_, _, _| Err(ThumbnailRefusal::Offline))).cast::<c_void>();
		for (vtbl, expected) in [
			(&FAILING_SHELL_ITEM_VTBL, DENIED),
			(&NAMELESS_SHELL_ITEM_VTBL, E_UNEXPECTED),
		] {
			let mut item = FakeShellItem {
				vtbl,
				path: Vec::new(),
			};
			assert_eq!(initialize(object, item.as_interface()), expected);
			assert_eq!(get_thumbnail(object, 96).0, E_UNEXPECTED);
		}
		let mut item = FakeShellItem::new(r"C:\Users\u\Filen\a.jpg");
		assert_eq!(initialize(object, item.as_interface()), S_OK);
		assert_eq!(get_thumbnail(object, 96).0, 0x8004_B204_u32.cast_signed());
		assert_eq!(release(object), 0);
	}

	#[test]
	fn every_iid_reads_back_as_the_u128_it_was_made_from() {
		for iid in [
			IID_IUNKNOWN,
			IID_ICLASSFACTORY,
			IID_IINITIALIZEWITHITEM,
			IID_ITHUMBNAILPROVIDER,
		] {
			assert_eq!(guid_u128(&GUID::from_u128(iid)), iid);
		}
	}

	#[test]
	fn the_iunknown_iid_is_the_one_windows_sys_declares() {
		assert_eq!(guid_u128(&IID_IUnknown), IID_IUNKNOWN);
	}

	#[test]
	fn an_iid_displays_as_the_registry_spells_it() {
		assert_eq!(
			BracedGuid(IID_ITHUMBNAILPROVIDER).to_string(),
			"{E357FCCD-A995-4576-B01F-234630154E96}"
		);
		assert_eq!(
			BracedGuid(IID_ICLASSFACTORY).to_string(),
			"{00000001-0000-0000-C000-000000000046}"
		);
	}

	#[test]
	fn each_refusal_is_answered_with_its_hresult() {
		let failed_extraction = 0x8004_B200_u32.cast_signed();
		let unavailable = 0x8004_B204_u32.cast_signed();
		for (refusal, expected) in [
			(ThumbnailRefusal::NotInRoot, failed_extraction),
			(ThumbnailRefusal::NotAPlaceholder, failed_extraction),
			(ThumbnailRefusal::Directory, failed_extraction),
			(ThumbnailRefusal::Unsupported, failed_extraction),
			(ThumbnailRefusal::TooLarge, failed_extraction),
			(ThumbnailRefusal::Offline, unavailable),
			(ThumbnailRefusal::SignedOut, unavailable),
			(ThumbnailRefusal::Pending, 0x8004_B205_u32.cast_signed()),
			(ThumbnailRefusal::Busy, 0x8004_B201_u32.cast_signed()),
			(ThumbnailRefusal::Failed, 0x8000_4005_u32.cast_signed()),
		] {
			assert_eq!(refusal_hresult(refusal), expected);
		}
	}

	#[test]
	fn the_alpha_type_follows_the_thumbnail() {
		assert_eq!(alpha_type(ThumbnailAlpha::Opaque), 1);
		assert_eq!(alpha_type(ThumbnailAlpha::Straight), 2);
	}

	#[test]
	fn a_thumbnail_object_offers_the_item_initializer_and_the_provider_only() {
		let object =
			ThumbnailObject::new(shared(|_, _, _| Err(ThumbnailRefusal::Failed))).cast::<c_void>();
		let provider = object.wrapping_byte_add(PROVIDER_OFFSET);
		for (from, iid, expected) in [
			(object, IID_IUNKNOWN, object),
			(object, IID_IINITIALIZEWITHITEM, object),
			(object, IID_ITHUMBNAILPROVIDER, provider),
			(provider, IID_IUNKNOWN, object),
			(provider, IID_ITHUMBNAILPROVIDER, provider),
		] {
			assert_eq!(query(from, iid), (S_OK, expected), "{}", BracedGuid(iid));
			release(expected);
		}
		for iid in [
			IID_IINITIALIZEWITHSTREAM,
			IID_IINITIALIZEWITHFILE,
			IID_ICLASSFACTORY,
		] {
			assert_eq!(
				query(object, iid),
				(E_NOINTERFACE, ptr::null_mut()),
				"{}",
				BracedGuid(iid)
			);
		}
		assert_eq!(release(object), 0);
	}

	#[test]
	fn the_last_release_frees_the_object() {
		let shared = shared(|_, _, _| Err(ThumbnailRefusal::Failed));
		let object = ThumbnailObject::new(Arc::clone(&shared)).cast::<c_void>();
		assert_eq!(query(object, IID_ITHUMBNAILPROVIDER).0, S_OK);
		assert_eq!(Arc::strong_count(&shared), 2);
		assert_eq!(release(object.wrapping_byte_add(PROVIDER_OFFSET)), 1);
		assert_eq!(release(object), 0);
		assert_eq!(Arc::strong_count(&shared), 1);
	}

	#[test]
	#[allow(unsafe_code)] // Tests: calls a COM method.
	fn the_factory_refuses_aggregation() {
		let shared = shared(|_, _, _| Err(ThumbnailRefusal::Failed));
		let factory = ClassFactory::new(Arc::clone(&shared)).cast::<c_void>();
		let guid = GUID::from_u128(IID_IUNKNOWN);
		let mut out = ptr::dangling_mut::<c_void>();
		// SAFETY: `factory` is live; the outer pointer is never read, only checked for null.
		let hr = unsafe {
			(vtbl::<IClassFactoryVtbl>(factory).create_instance)(
				factory,
				ptr::dangling_mut(),
				&guid,
				&mut out,
			)
		};
		assert_eq!((hr, out), (CLASS_E_NOAGGREGATION, ptr::null_mut()));
		assert_eq!(release(factory), 0);
		assert_eq!(Arc::strong_count(&shared), 1);
	}

	#[test]
	#[allow(unsafe_code)] // Tests: calls a COM method.
	fn the_factory_frees_an_object_asked_for_an_interface_it_does_not_offer() {
		let shared = shared(|_, _, _| Err(ThumbnailRefusal::Failed));
		let factory = ClassFactory::new(Arc::clone(&shared)).cast::<c_void>();
		let create = |iid: u128| {
			let guid = GUID::from_u128(iid);
			let mut out = ptr::dangling_mut::<c_void>();
			// SAFETY: `factory` is live; `guid` and `out` are live for the call.
			let hr = unsafe {
				(vtbl::<IClassFactoryVtbl>(factory).create_instance)(
					factory,
					ptr::null_mut(),
					&guid,
					&mut out,
				)
			};
			(hr, out)
		};
		assert_eq!(
			create(IID_IINITIALIZEWITHSTREAM),
			(E_NOINTERFACE, ptr::null_mut())
		);
		assert_eq!(Arc::strong_count(&shared), 2);
		let (hr, object) = create(IID_ITHUMBNAILPROVIDER);
		assert_eq!(hr, S_OK);
		assert_eq!(Arc::strong_count(&shared), 3);
		assert_eq!(release(object), 0);
		assert_eq!(release(factory), 0);
		assert_eq!(Arc::strong_count(&shared), 1);
	}

	#[test]
	fn get_thumbnail_before_initialize_is_unexpected() {
		let object =
			ThumbnailObject::new(shared(|_, _, _| Err(ThumbnailRefusal::Failed))).cast::<c_void>();
		assert_eq!(
			get_thumbnail(object, 256),
			(E_UNEXPECTED, ptr::null_mut(), WTSAT_UNKNOWN)
		);
		assert_eq!(release(object), 0);
	}

	#[test]
	fn initialize_keeps_the_items_path_for_the_handler() {
		let asked = Arc::new(Mutex::new(Vec::new()));
		let object = ThumbnailObject::new(shared({
			let asked = Arc::clone(&asked);
			move |path, cx, _| {
				asked.lock().unwrap().push((path.to_owned(), cx));
				Err(ThumbnailRefusal::Offline)
			}
		}))
		.cast::<c_void>();
		let mut item = FakeShellItem::new(r"C:\Users\u\Filen\a.jpg");
		assert_eq!(initialize(object, item.as_interface()), S_OK);
		assert_eq!(
			get_thumbnail(object, 256),
			(
				0x8004_B204_u32.cast_signed(),
				ptr::null_mut(),
				WTSAT_UNKNOWN
			)
		);
		assert_eq!(
			*asked.lock().unwrap(),
			[(PathBuf::from(r"C:\Users\u\Filen\a.jpg"), 256)]
		);
		let mut other = FakeShellItem::new(r"C:\Users\u\Filen\b.jpg");
		assert_eq!(
			initialize(object, other.as_interface()),
			0x8007_04DF_u32.cast_signed()
		);
		assert_eq!(
			initialize(object, ptr::null_mut()),
			0x8007_0057_u32.cast_signed()
		);
		assert_eq!(release(object), 0);
	}

	#[test]
	fn a_panicking_handler_is_answered_unexpected() {
		let object =
			ThumbnailObject::new(shared(|_, _, _| panic!("the handler panicked"))).cast::<c_void>();
		let mut item = FakeShellItem::new(r"C:\Users\u\Filen\a.jpg");
		assert_eq!(initialize(object, item.as_interface()), S_OK);
		assert_eq!(get_thumbnail(object, 96).0, E_UNEXPECTED);
		assert_eq!(release(object), 0);
	}

	#[test]
	fn a_closed_gate_answers_without_calling_the_handler() {
		let shared = shared(|_, _, _| panic!("the handler was called"));
		let object = ThumbnailObject::new(Arc::clone(&shared)).cast::<c_void>();
		let mut item = FakeShellItem::new(r"C:\Users\u\Filen\a.jpg");
		assert_eq!(initialize(object, item.as_interface()), S_OK);
		assert!(shared.gate.close(Duration::ZERO));
		assert_eq!(get_thumbnail(object, 96).0, CO_E_SERVER_STOPPING);
		assert_eq!(release(object), 0);
	}

	#[test]
	fn a_closed_gate_answers_initialize_without_asking_the_item() {
		let shared = shared(|_, _, _| panic!("the handler was called"));
		let object = ThumbnailObject::new(Arc::clone(&shared)).cast::<c_void>();
		assert!(shared.gate.close(Duration::ZERO));
		// Its `GetDisplayName` answers `DENIED`, so a call to it would be answered so.
		let mut item = FakeShellItem::new(r"C:\Users\u\Filen\a.jpg");
		item.vtbl = &FAILING_SHELL_ITEM_VTBL;
		assert_eq!(
			initialize(object, item.as_interface()),
			CO_E_SERVER_STOPPING
		);
		assert_eq!(release(object), 0);
	}

	/// A fake `IShellItem` whose `GetDisplayName` closes `shared`'s gate without waiting, noting
	/// whether the close found the gate idle, and then answers `path`.
	#[repr(C)]
	struct ClosingShellItem {
		vtbl: &'static FakeShellItemVtbl,
		/// NUL-terminated.
		path: Vec<u16>,
		shared: Arc<Shared>,
		closed_idle: Mutex<Option<bool>>,
	}

	static CLOSING_SHELL_ITEM_VTBL: FakeShellItemVtbl = FakeShellItemVtbl {
		base: IUnknown_Vtbl {
			QueryInterface: fake_query_interface,
			AddRef: fake_count,
			Release: fake_count,
		},
		bind_to_handler: 0,
		get_parent: 0,
		get_display_name: closing_get_display_name,
	};

	#[allow(unsafe_code)] // Tests: a fake COM method.
	unsafe extern "system" fn closing_get_display_name(
		this: *mut c_void,
		sigdn: SIGDN,
		name: *mut *mut u16,
	) -> HRESULT {
		// SAFETY: `this` is a live `ClosingShellItem`, as the test passes it; it starts with the
		// fields of a `FakeShellItem`, both `repr(C)`.
		let item = unsafe { &*this.cast::<ClosingShellItem>() };
		*item.closed_idle.lock().unwrap() = Some(item.shared.gate.close(Duration::ZERO));
		// SAFETY: as above; `name` is writable, as the code under test passes it.
		unsafe { fake_get_display_name(this, sigdn, name) }
	}

	#[test]
	fn initialize_is_counted_while_it_asks_the_item() {
		let shared = shared(|_, _, _| panic!("the handler was called"));
		let object = ThumbnailObject::new(Arc::clone(&shared)).cast::<c_void>();
		let mut item = ClosingShellItem {
			vtbl: &CLOSING_SHELL_ITEM_VTBL,
			path: r"C:\Users\u\Filen\a.jpg"
				.encode_utf16()
				.chain([0])
				.collect(),
			shared: Arc::clone(&shared),
			closed_idle: Mutex::new(None),
		};
		assert_eq!(initialize(object, ptr::from_mut(&mut item).cast()), S_OK);
		// The stop that came in while `Initialize` asked the item found it in progress.
		assert_eq!(*item.closed_idle.lock().unwrap(), Some(false));
		assert!(shared.gate.close(Duration::ZERO));
		drop(item);
		assert_eq!(release(object), 0);
	}

	#[test]
	fn get_thumbnail_is_counted_while_the_handler_answers() {
		// The server whose gate the handler closes, taken on its first call, so no cycle is left.
		let server: Arc<Mutex<Option<Arc<Shared>>>> = Arc::default();
		let closed_idle = Arc::new(Mutex::new(None));
		let shared = shared({
			let server = Arc::clone(&server);
			let closed_idle = Arc::clone(&closed_idle);
			move |_, _, _| {
				let shared = server.lock().unwrap().take().unwrap();
				*closed_idle.lock().unwrap() = Some(shared.gate.close(Duration::ZERO));
				Ok(Thumbnail::for_test(
					1,
					1,
					vec![1, 2, 3, 4],
					ThumbnailAlpha::Opaque,
				))
			}
		});
		*server.lock().unwrap() = Some(Arc::clone(&shared));
		let object = ThumbnailObject::new(Arc::clone(&shared)).cast::<c_void>();
		let mut item = FakeShellItem::new(r"C:\Users\u\Filen\a.png");
		assert_eq!(initialize(object, item.as_interface()), S_OK);
		let (hr, bitmap, _) = get_thumbnail(object, 96);
		// The stop that came in while the handler ran found the call in progress, which still
		// answered.
		assert_eq!(*closed_idle.lock().unwrap(), Some(false));
		assert_eq!(hr, S_OK);
		assert!(!bitmap.is_null());
		assert!(shared.gate.close(Duration::ZERO));
		// In process the caller owns the bitmap: the test's.
		assert!(delete_bitmap_for_test(bitmap));
		assert_eq!(release(object), 0);
	}

	#[test]
	fn each_dib_failure_is_answered_with_its_hresult() {
		assert_eq!(dib_hresult(DibError::Size), 0x8000_4005_u32.cast_signed());
		assert_eq!(dib_hresult(DibError::Gdi), E_OUTOFMEMORY);
		// Pixels that do not match the size reach Explorer as `E_FAIL`.
		assert_eq!(
			dib_section(2, 2, &[0; 8]).err().map(dib_hresult),
			Some(0x8000_4005_u32.cast_signed())
		);
	}

	#[test]
	fn reference_counts_step_by_one_and_wrap() {
		let count = AtomicU32::new(1);
		assert_eq!(add_ref(&count), 2);
		assert_eq!(release_ref(&count), 1);
		assert_eq!(release_ref(&count), 0);
		// A release past the last reference: wrapped, so not answered as a second last one.
		assert_eq!(release_ref(&count), u32::MAX);
	}

	/// [`run_alone`] for `test`, this module's test of that name. For the tests that set the
	/// process's COM security, which a process sets once, and which COM sets to its defaults itself
	/// the first time any thread marshals an interface, as another test may through the shell.
	fn alone(test: &str) -> bool {
		run_alone(module_path!(), test)
	}

	#[test]
	fn a_security_thread_that_panics_before_reporting_gives_no_proof() {
		let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), ComError>>(1);
		let thread = thread::spawn(move || {
			let _ready = ready_tx;
			panic!("the COM security's thread, as a bug there would");
		});
		assert!(matches!(
			ComSecurity::await_report(thread, &ready_rx, Duration::from_secs(60)),
			Err(ComError::Panicked(ComThread::Security))
		));
	}

	#[test]
	fn a_security_thread_that_reports_a_failure_is_joined_and_gives_no_proof() {
		let (ready_tx, ready_rx) = mpsc::sync_channel(1);
		let ended = Arc::new(AtomicBool::new(false));
		let thread = thread::spawn({
			let ended = Arc::clone(&ended);
			move || {
				ready_tx
					.send(Err(ComError::Security(io::Error::other("refused"))))
					.unwrap();
				thread::sleep(Duration::from_millis(50));
				ended.store(true, Ordering::Release);
			}
		});
		assert!(matches!(
			ComSecurity::await_report(thread, &ready_rx, Duration::from_secs(60)),
			Err(ComError::Security(_))
		));
		// Joined before the failure is answered.
		assert!(ended.load(Ordering::Acquire));
	}

	/// What this thread logged while `run` ran.
	fn logged(run: impl FnOnce()) -> String {
		/// A writer into a shared buffer.
		struct Lines(Arc<Mutex<Vec<u8>>>);

		impl io::Write for Lines {
			fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
				self.0
					.lock()
					.unwrap_or_else(PoisonError::into_inner)
					.extend_from_slice(bytes);
				Ok(bytes.len())
			}

			fn flush(&mut self) -> io::Result<()> {
				Ok(())
			}
		}

		let lines = Arc::new(Mutex::new(Vec::new()));
		let subscriber = tracing_subscriber::fmt()
			.with_max_level(tracing::Level::TRACE)
			.with_ansi(false)
			.with_writer({
				let lines = Arc::clone(&lines);
				move || Lines(Arc::clone(&lines))
			})
			.finish();
		tracing::subscriber::with_default(subscriber, run);
		let lines = lines.lock().unwrap_or_else(PoisonError::into_inner);
		String::from_utf8_lossy(&lines).into_owned()
	}

	#[test]
	fn a_failed_security_step_is_answered_with_its_cause_and_not_logged() {
		let (ready_tx, ready_rx) = mpsc::sync_channel(1);
		let thread = thread::spawn(move || {
			ready_tx
				.send(Err(ComError::Security(io::Error::other("refused"))))
				.unwrap();
		});
		let mut answered = None;
		let logs = logged(|| {
			answered = Some(ComSecurity::await_report(
				thread,
				&ready_rx,
				Duration::from_secs(60),
			));
		});
		// The caller logs it, once: nothing here.
		assert_eq!(logs, "");
		match answered {
			Some(Err(error @ ComError::Security(_))) => {
				assert!(error.to_string().ends_with(": refused"), "{error}");
			}
			other => panic!("answered {other:?}"),
		}
	}

	#[test]
	fn a_security_thread_that_reports_late_gives_no_proof_and_its_report_goes_nowhere() {
		let (ready_tx, ready_rx) = mpsc::sync_channel(1);
		let (release_tx, release_rx) = mpsc::channel::<()>();
		let (sent_tx, sent_rx) = mpsc::channel();
		let thread = thread::spawn(move || {
			// Held until the wait is over, as a COM call that hangs.
			let _ = release_rx.recv();
			sent_tx.send(ready_tx.send(Ok(())).is_ok()).unwrap();
		});
		assert!(matches!(
			ComSecurity::await_report(thread, &ready_rx, Duration::from_millis(50)),
			Err(ComError::Timeout(ComThread::Security))
		));
		drop(ready_rx);
		drop(release_tx);
		// The late success reaches nobody: no proof is made from it.
		assert!(!sent_rx.recv_timeout(Duration::from_secs(60)).unwrap());
	}

	#[test]
	fn the_process_security_is_set_once() {
		if !alone("the_process_security_is_set_once") {
			return;
		}
		ComSecurity::establish().unwrap();
		// Refused by COM, as it documents a second call: no second proof.
		match ComSecurity::establish() {
			Err(ComError::Com { call, code }) => {
				assert_eq!((call, code.get()), ("CoInitializeSecurity", RPC_E_TOO_LATE));
			}
			other => panic!("a second call answered {other:?}"),
		}
	}

	#[test]
	fn a_server_registers_the_class_and_stops() {
		if !alone("a_server_registers_the_class_and_stops") {
			return;
		}
		let security = ComSecurity::establish().unwrap();
		let clsid = GUID::from_u128(0x5f2c_1d7e_9a43_4b8e_b6d1_0c3a_7e95_2f41);
		// The one test that starts a server, so nothing else moves the process's count.
		assert!(!is_serving());
		let server = ThumbnailServer::start(
			security,
			clsid,
			Arc::new(|_, _, _| Err(ThumbnailRefusal::SignedOut)),
		)
		.unwrap();
		assert!(is_serving());
		server.stop();
		assert!(!is_serving());
		// The security outlives the server's apartment, which its stop left: another server starts
		// under it.
		let again = ThumbnailServer::start(
			security,
			clsid,
			Arc::new(|_, _, _| Err(ThumbnailRefusal::SignedOut)),
		)
		.unwrap();
		assert!(is_serving());
		again.stop();
		assert!(!is_serving());
	}

	#[test]
	#[allow(unsafe_code)] // Tests: calls COM methods with null out-params.
	fn null_out_params_are_refused() {
		let object =
			ThumbnailObject::new(shared(|_, _, _| Err(ThumbnailRefusal::Failed))).cast::<c_void>();
		let guid = GUID::from_u128(IID_IUNKNOWN);
		// SAFETY: `object` is live; a null `ppv` is checked before anything is written.
		let hr = unsafe {
			(vtbl::<IUnknown_Vtbl>(object).QueryInterface)(object, &guid, ptr::null_mut())
		};
		assert_eq!(hr, E_POINTER);
		let mut out = ptr::dangling_mut::<c_void>();
		// SAFETY: `object` is live; `out` is writable, a null `riid` is checked before it is read.
		let hr = unsafe {
			(vtbl::<IUnknown_Vtbl>(object).QueryInterface)(object, ptr::null(), &mut out)
		};
		assert_eq!((hr, out), (E_POINTER, ptr::null_mut()));
		let provider = object.wrapping_byte_add(PROVIDER_OFFSET);
		let mut alpha: WTS_ALPHATYPE = -1;
		// SAFETY: `provider` is the live object's `IThumbnailProvider` pointer; a null bitmap
		// out-param is checked before anything is written.
		let hr = unsafe {
			(vtbl::<IThumbnailProviderVtbl>(provider).get_thumbnail)(
				provider,
				96,
				ptr::null_mut(),
				&mut alpha,
			)
		};
		assert_eq!((hr, alpha), (E_POINTER, -1));
		assert_eq!(release(object), 0);
	}

	#[test]
	#[allow(unsafe_code)] // Tests: calls a COM method.
	fn a_stopping_factory_makes_no_object() {
		let shared = shared(|_, _, _| Err(ThumbnailRefusal::Failed));
		let factory = ClassFactory::new(Arc::clone(&shared)).cast::<c_void>();
		assert!(shared.gate.close(Duration::ZERO));
		let guid = GUID::from_u128(IID_ITHUMBNAILPROVIDER);
		let mut out = ptr::dangling_mut::<c_void>();
		// SAFETY: `factory` is live; `guid` and `out` are live for the call.
		let hr = unsafe {
			(vtbl::<IClassFactoryVtbl>(factory).create_instance)(
				factory,
				ptr::null_mut(),
				&guid,
				&mut out,
			)
		};
		assert_eq!((hr, out), (CO_E_SERVER_STOPPING, ptr::null_mut()));
		assert_eq!(Arc::strong_count(&shared), 2);
		assert_eq!(release(factory), 0);
		assert_eq!(Arc::strong_count(&shared), 1);
	}

	#[test]
	fn a_thumbnail_is_handed_out_as_a_bitmap_with_its_alpha_type() {
		let pixels = vec![1, 2, 3, 4, 5, 6, 7, 8];
		let object = ThumbnailObject::new(shared({
			let pixels = pixels.clone();
			move |_, _, _| {
				Ok(Thumbnail::for_test(
					1,
					2,
					pixels.clone(),
					ThumbnailAlpha::Straight,
				))
			}
		}))
		.cast::<c_void>();
		let mut item = FakeShellItem::new(r"C:\Users\u\Filen\a.png");
		assert_eq!(initialize(object, item.as_interface()), S_OK);
		let (hr, bitmap, alpha) = get_thumbnail(object, 96);
		assert_eq!((hr, alpha), (S_OK, WTSAT_ARGB));
		assert!(!bitmap.is_null());
		let bits = dib_bits_for_test(bitmap).unwrap();
		assert_eq!((bits.width, bits.height), (1, 2));
		// The thumbnail's first row is the first in memory, and the top row drawn.
		assert_eq!(bits.bytes, pixels);
		assert_eq!(first_column_for_test(bitmap, 2), [0x0001_0203, 0x0005_0607]);
		// In process the caller owns the bitmap: the test's.
		assert!(delete_bitmap_for_test(bitmap));
		assert_eq!(release(object), 0);
	}

	#[test]
	#[allow(unsafe_code)] // Tests: calls a COM method with a null out-param.
	fn a_null_alpha_out_param_nulls_the_bitmap_one() {
		let object = ThumbnailObject::new(shared(|_, _, _| panic!("the handler was called")))
			.cast::<c_void>();
		let provider = object.wrapping_byte_add(PROVIDER_OFFSET);
		let mut bitmap: HBITMAP = ptr::dangling_mut();
		// SAFETY: `provider` is the live object's `IThumbnailProvider` pointer; `bitmap` is
		// writable, a null alpha out-param is checked before it is written.
		let hr = unsafe {
			(vtbl::<IThumbnailProviderVtbl>(provider).get_thumbnail)(
				provider,
				96,
				&mut bitmap,
				ptr::null_mut(),
			)
		};
		assert_eq!((hr, bitmap), (E_POINTER, ptr::null_mut()));
		assert_eq!(release(object), 0);
	}

	#[test]
	fn the_handler_runs_on_a_thread_of_its_own_while_the_call_is_watched() {
		let caller = thread::current().id();
		let (polled, poll) = mpsc::channel();
		let poll = Mutex::new(poll);
		let handler: ThumbnailHandler = Arc::new(move |_, _, cancel| {
			assert_ne!(thread::current().id(), caller);
			// Answers only once the call was watched while this ran.
			poll.lock()
				.unwrap_or_else(PoisonError::into_inner)
				.recv_timeout(Duration::from_secs(60))
				.expect("the call was never watched");
			assert!(!cancel.is_cancelled());
			Ok(Thumbnail::for_test(
				1,
				1,
				vec![1, 2, 3, 4],
				ThumbnailAlpha::Opaque,
			))
		});
		let polls = AtomicUsize::new(0);
		let answer = answer_watched(&handler, Path::new(r"C:\Users\u\Filen\a.png"), 96, || {
			polls.fetch_add(1, Ordering::Relaxed);
			// Fails only once the handler answered, after the first poll.
			let _ = polled.send(());
			false
		});
		assert_eq!(
			answer,
			Ok(Thumbnail::for_test(
				1,
				1,
				vec![1, 2, 3, 4],
				ThumbnailAlpha::Opaque
			))
		);
		assert!(polls.load(Ordering::Relaxed) >= 1, "watched while it ran");
	}

	#[test]
	fn a_cancelled_call_cancels_the_handlers_request() {
		let handler: ThumbnailHandler = Arc::new(|_, _, cancel| busy_once_cancelled(cancel));
		let polls = AtomicUsize::new(0);
		let answer = answer_watched(&handler, Path::new(r"C:\Users\u\Filen\a.png"), 96, || {
			polls.fetch_add(1, Ordering::Relaxed) >= 2
		});
		// Busy is the handler's answer to the cancel only.
		assert_eq!(answer, Err(ThumbnailRefusal::Busy));
		assert_eq!(
			polls.load(Ordering::Relaxed),
			3,
			"polled until cancelled, then no more"
		);
	}

	#[test]
	fn a_panicking_handler_thread_hands_its_panic_on() {
		let handler: ThumbnailHandler = Arc::new(|_, _, _| panic!("the handler panicked"));
		let answered = panic::catch_unwind(AssertUnwindSafe(|| {
			answer_watched(&handler, Path::new(r"C:\Users\u\Filen\a.png"), 96, || false)
		}));
		assert!(answered.is_err());
	}

	#[test]
	fn no_call_is_cancelled_off_a_com_call() {
		assert!(!call_cancelled());
	}

	#[test]
	fn a_stopping_server_cancels_the_request_in_progress() {
		let shared = shared(|_, _, cancel| busy_once_cancelled(cancel));
		let object = ThumbnailObject::new(Arc::clone(&shared)).cast::<c_void>();
		let mut item = FakeShellItem::new(r"C:\Users\u\Filen\a.jpg");
		assert_eq!(initialize(object, item.as_interface()), S_OK);
		shared.stopping.store(true, Ordering::Release);
		// The handler answers only once cancelled, so its answer is the stop's.
		assert_eq!(get_thumbnail(object, 96).0, 0x8004_B201_u32.cast_signed());
		assert_eq!(release(object), 0);
	}

	#[test]
	fn the_server_admits_this_user_and_system_only() {
		let user = current_user_sid().unwrap();
		let system: Vec<u16> = "S-1-5-18".encode_utf16().collect();
		// A present, non-null DACL (checked in the read): whom its entries do not allow, it denies.
		assert_eq!(
			descriptor_entries_for_test(&access_descriptor().unwrap()),
			DescriptorEntries {
				owner: user.clone(),
				group: user.clone(),
				dacl: vec![(0, 0x3, user), (0, 0x3, system)],
			}
		);
	}
}

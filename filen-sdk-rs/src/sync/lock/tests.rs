#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use tokio::net::TcpListener;

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use super::test_support::{offline_client, proxied_client, test_lock, test_lock_on};
use super::*;
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use crate::auth::http::{
	ClientConfig,
	test_support::{GATEWAY_CONNECT, client_via_proxy, read_head},
};

/// The caller-chosen ceiling is what bounds how late a contended acquisition's NEXT poll is —
/// the sync engine picks a short one so a pass that misses another client's release is back
/// inside its confirmation window. Every step of the ramp has to respect it, not just the
/// ones past it.
#[test]
fn the_backoff_ramp_never_sleeps_longer_than_its_ceiling() {
	let ceiling = Duration::from_secs(5);
	assert!(
		fibonacci_iter(ceiling)
			.take(64)
			.all(|delay| delay <= ceiling)
	);
}

#[test]
fn refresh_delay_saturates_when_first_poll_is_late() {
	assert_eq!(
		refresh_delay_after(LOCK_REFRESH_INTERVAL + Duration::from_secs(1)),
		Duration::ZERO
	);
	assert_eq!(
		refresh_delay_after(Duration::from_secs(u64::MAX)),
		Duration::ZERO
	);
}

#[test]
fn refresh_delay_subtracts_elapsed_time() {
	assert_eq!(refresh_delay_after(Duration::ZERO), LOCK_REFRESH_INTERVAL);
	assert_eq!(
		refresh_delay_after(Duration::from_secs(5)),
		LOCK_REFRESH_INTERVAL - Duration::from_secs(5)
	);
}

// The runtimes below are deliberately never driven: a `current_thread` runtime only
// polls tasks inside `block_on`, so the release task spawned by `Drop` is queued (or
// cancelled) but never runs.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[test]
fn drop_on_thread_without_runtime_does_not_panic() {
	let rt = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();
	let lock = rt.block_on(async { test_lock(tokio::runtime::Handle::try_current().ok()) });
	assert!(lock.lease.handle.is_some());
	std::thread::spawn(move || std::mem::drop(lock))
		.join()
		.unwrap();
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[test]
fn drop_after_runtime_shutdown_does_not_panic() {
	let rt = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();
	let lock = rt.block_on(async { test_lock(tokio::runtime::Handle::try_current().ok()) });
	std::mem::drop(rt);
	std::thread::spawn(move || std::mem::drop(lock))
		.join()
		.unwrap();
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[test]
fn drop_without_captured_handle_does_not_panic() {
	let rt = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();
	let lock = rt.block_on(async { test_lock(None) });
	std::mem::drop(rt);
	std::thread::spawn(move || std::mem::drop(lock))
		.join()
		.unwrap();
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[test]
fn drop_prefers_ambient_runtime_over_dead_captured_handle() {
	let dead = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();
	let lock = dead.block_on(async { test_lock(tokio::runtime::Handle::try_current().ok()) });
	std::mem::drop(dead);
	let ambient = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();
	// `enter` makes `ambient` the ambient runtime without driving it.
	let _guard = ambient.enter();
	assert_eq!(ambient.metrics().num_alive_tasks(), 0);
	std::mem::drop(lock);
	// The release task must land on the live ambient runtime; spawning it on the dead
	// captured handle would cancel it silently.
	assert_eq!(ambient.metrics().num_alive_tasks(), 1);
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[test]
fn dropping_a_clone_keeps_the_lock_until_the_last_one_goes() {
	let rt = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();
	let lock = rt.block_on(async { test_lock(tokio::runtime::Handle::try_current().ok()) });
	let _guard = rt.enter();
	std::mem::drop(lock.clone());
	assert_eq!(
		rt.metrics().num_alive_tasks(),
		0,
		"the lock is still held through the original, so nothing is released"
	);
	std::mem::drop(lock);
	assert_eq!(rt.metrics().num_alive_tasks(), 1);
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[test]
fn keep_lock_alive_without_ambient_runtime_does_not_panic() {
	let rt = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();
	let lock = rt.block_on(async { test_lock(tokio::runtime::Handle::try_current().ok()) });
	// A plain thread has no ambient runtime, so the refresh task must ride the
	// captured handle.
	std::thread::spawn(move || keep_lock_alive(&lock.lease))
		.join()
		.unwrap();
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[test]
fn drop_of_lost_lock_still_attempts_release() {
	let rt = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();
	let lock = rt.block_on(async { test_lock(tokio::runtime::Handle::try_current().ok()) });
	lock.lease.valid.send_replace(false);
	let _guard = rt.enter();
	assert_eq!(rt.metrics().num_alive_tasks(), 0);
	std::mem::drop(lock);
	// A false-positive loss must not strand the resource until the TTL: the
	// release is attempted anyway (harmless no-op if the lease is truly gone).
	assert_eq!(rt.metrics().num_alive_tasks(), 1);
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[test]
fn a_lock_declared_lost_sends_a_release_once() {
	let rt = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();
	let lock = rt.block_on(async { test_lock(tokio::runtime::Handle::try_current().ok()) });
	let _guard = rt.enter();
	lock.lease.mark_lost();
	assert!(!lock.is_valid());
	assert_eq!(rt.metrics().num_alive_tasks(), 1);
	lock.lease.mark_lost();
	assert_eq!(rt.metrics().num_alive_tasks(), 1, "a loss is released once");
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[test]
fn no_release_is_waited_for_while_the_lock_is_held() {
	let rt = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();
	let lock = rt.block_on(async { test_lock(tokio::runtime::Handle::try_current().ok()) });

	assert!(
		lock.lease
			.client
			.pending_lock_releases
			.settled()
			.now_or_never()
			.is_some()
	);
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[test]
fn the_release_of_a_dropped_lock_is_waited_for_until_it_is_answered() {
	// paused, so the retries of the release, which fails offline, take no real time
	let rt = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.start_paused(true)
		.build()
		.unwrap();
	let lock = rt.block_on(async { test_lock(tokio::runtime::Handle::try_current().ok()) });
	let client = Arc::clone(&lock.lease.client);
	let _guard = rt.enter();
	std::mem::drop(lock);

	assert!(
		client
			.pending_lock_releases
			.settled()
			.now_or_never()
			.is_none(),
		"the release is not sent until the runtime runs it"
	);
	rt.block_on(client.pending_lock_releases.settled());
	assert_eq!(rt.metrics().num_alive_tasks(), 0);
}

/// A runtime for a test to watch requests arrive on, and a proxy on it that `client` sends them to.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
fn proxy_and_client() -> (
	tokio::runtime::Runtime,
	tokio::net::TcpListener,
	Arc<AuthClient>,
) {
	let observer = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();
	let proxy = observer
		.block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
		.unwrap();
	let client = proxied_client(proxy.local_addr().unwrap());
	(observer, proxy, client)
}

/// Dates `lock`'s last confirmation a server TTL back, by the wall clock: a release of it that
/// fails in transport is then not sent again, as the server has expired the lease on its own (see
/// [`release_loop`]).
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
fn confirmed_a_lease_ago(lock: &ResourceLock) {
	let ttl = chrono::TimeDelta::from_std(LOCK_SERVER_TTL).unwrap();
	lock.lease.confirmed.send_replace(Confirmed {
		at: Instant::now(),
		wall: Utc::now() - ttl,
	});
}

/// Waits for the release `client` sends through `proxy`, checks that it is waited for while it is
/// unanswered, then fails it by closing the proxy, after which it is waited for no more: the
/// lease it frees must be a TTL old (see [`confirmed_a_lease_ago`]), or the failed release would
/// be sent again.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
fn assert_release_is_sent_and_waited_for(
	observer: &tokio::runtime::Runtime,
	proxy: tokio::net::TcpListener,
	client: &AuthClient,
) {
	let (connection, _) = observer
		.block_on(async { timeout(Duration::from_secs(30), proxy.accept()).await })
		.expect("the release is sent")
		.unwrap();
	assert!(
		client
			.pending_lock_releases
			.settled()
			.now_or_never()
			.is_none(),
		"a release the server has not answered is waited for"
	);
	std::mem::drop((connection, proxy));
	observer
		.block_on(async {
			timeout(
				Duration::from_secs(30),
				client.pending_lock_releases.settled(),
			)
			.await
		})
		.expect("a failed release is waited for no more");
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[test]
fn a_release_its_runtime_cancelled_is_sent_from_a_thread_of_its_own() {
	let (observer, proxy, client) = proxy_and_client();
	let rt = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();
	let lock = rt.block_on(async {
		test_lock_on(
			Arc::clone(&client),
			tokio::runtime::Handle::try_current().ok(),
		)
	});
	confirmed_a_lease_ago(&lock);
	rt.block_on(async { std::mem::drop(lock) });

	std::mem::drop(rt);

	assert_release_is_sent_and_waited_for(&observer, proxy, &client);
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[test]
fn a_release_with_no_runtime_to_go_to_is_sent_from_a_thread_of_its_own() {
	let (observer, proxy, client) = proxy_and_client();
	let lock = test_lock_on(Arc::clone(&client), None);
	confirmed_a_lease_ago(&lock);

	std::thread::spawn(move || std::mem::drop(lock))
		.join()
		.unwrap();

	assert_release_is_sent_and_waited_for(&observer, proxy, &client);
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[test]
fn a_lock_request_its_caller_stopped_waiting_for_is_waited_for_until_it_ends() {
	// paused, so the retries of the request, which fails offline, take no real time
	let rt = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.start_paused(true)
		.build()
		.unwrap();
	let client = offline_client();
	let mut request = Box::pin(request_acquire(
		Arc::clone(&client),
		lock_url().into(),
		Bytes::new(),
		Uuid::from_u128(1),
		"test-resource".into(),
		None,
	));
	rt.block_on(async {
		assert!(futures::poll!(&mut request).is_pending());
	});
	std::mem::drop(request);

	assert!(
		client
			.pending_lock_releases
			.settled()
			.now_or_never()
			.is_none(),
		"a grant nobody waits for any more would have to be released"
	);
	rt.block_on(client.pending_lock_releases.settled());
	assert_eq!(rt.metrics().num_alive_tasks(), 0);
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test]
async fn loss_signal_flips_is_valid_and_wakes_waiters() {
	let lock = test_lock(None);
	assert!(lock.is_valid());
	let wait = lock.wait_for_loss();
	tokio::pin!(wait);
	assert!(futures::poll!(wait.as_mut()).is_pending());
	lock.lease.valid.send_replace(false);
	assert!(!lock.is_valid());
	wait.await;
	// resolves immediately when already lost
	lock.wait_for_loss().await;
}

/// A lease confirmation `ago` in the past on the wall clock only, as after a system suspend
/// the monotonic clock did not count.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
fn suspended_for(ago: Duration) -> Confirmed {
	Confirmed {
		at: Instant::now(),
		wall: Utc::now() - chrono::Duration::from_std(ago).unwrap(),
	}
}

/// A test lock last confirmed `ago`, by both clocks.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
fn unconfirmed_lock(ago: Duration) -> ResourceLock {
	let lock = test_lock(None);
	lock.lease.confirmed.send_replace(confirmed_ago(ago));
	lock
}

/// A lease confirmation `ago` in the past by both clocks.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
fn confirmed_ago(ago: Duration) -> Confirmed {
	Confirmed {
		at: Instant::now().checked_sub(ago).unwrap(),
		wall: Utc::now() - chrono::Duration::from_std(ago).unwrap(),
	}
}

/// A scripted refresh answering `answer` after `delay` every time, counting how often it is
/// asked.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
fn counted_refresh(
	asked: &std::cell::Cell<usize>,
	delay: Duration,
	answer: Result<bool, ErrorKind>,
) -> impl FnMut() -> futures::future::BoxFuture<'static, Result<bool, Error>> {
	scripted_answers(asked, std::iter::repeat((delay, answer)))
}

/// A scripted refresh giving the answers in `script`, each after its delay, counting how
/// often it is asked.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
fn scripted_answers(
	asked: &std::cell::Cell<usize>,
	script: impl IntoIterator<Item = (Duration, Result<bool, ErrorKind>)>,
) -> impl FnMut() -> futures::future::BoxFuture<'static, Result<bool, Error>> {
	let mut script = script.into_iter();
	move || {
		asked.set(asked.get() + 1);
		let (delay, answer) = script.next().expect("asked more often than scripted");
		Box::pin(async move {
			sleep(delay).await;
			answer.map_err(|kind| Error::custom(kind, "scripted refresh"))
		})
	}
}

/// A scripted refresh that is never answered, counting how often it is asked.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
fn unanswered_refresh(
	asked: &std::cell::Cell<usize>,
) -> impl FnMut() -> std::future::Pending<Result<bool, Error>> {
	move || {
		asked.set(asked.get() + 1);
		std::future::pending()
	}
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test]
async fn a_fresh_lock_is_confirmed_without_asking_the_server() {
	let lock = test_lock(None);
	let asked = std::cell::Cell::new(0);
	lock.confirm_via(counted_refresh(&asked, Duration::ZERO, Ok(true)))
		.await
		.unwrap();
	assert_eq!(asked.get(), 0);
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test]
async fn a_suspend_past_the_budget_leaves_the_lock_unconfirmed_until_the_server_confirms_it() {
	let lock = test_lock(None);
	lock.lease
		.confirmed
		.send_replace(suspended_for(LOCK_LOSS_BUDGET + Duration::from_secs(5)));
	// the monotonic clock did not see the suspend; the wall clock did
	assert!(!lock.is_valid());
	let asked = std::cell::Cell::new(0);
	lock.confirm_via(counted_refresh(&asked, Duration::ZERO, Ok(true)))
		.await
		.unwrap();
	assert_eq!(asked.get(), 1);
	assert!(lock.is_valid());
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test]
async fn a_refused_confirmation_loses_the_lock() {
	let lock = unconfirmed_lock(LOCK_LOSS_BUDGET + Duration::from_secs(5));
	let asked = std::cell::Cell::new(0);
	let error = lock
		.confirm_via(counted_refresh(&asked, Duration::ZERO, Ok(false)))
		.await
		.unwrap_err();
	assert_eq!(error.kind(), ErrorKind::LockLost);
	assert_eq!(
		error.downcast_ref::<LockLostError>().unwrap().resource,
		"test-resource"
	);
	lock.wait_for_loss().await;
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn a_confirmation_failing_in_transport_is_asked_again() {
	let lock = unconfirmed_lock(LOCK_LOSS_BUDGET + Duration::from_secs(5));
	let begin = Instant::now();
	let asked = std::cell::Cell::new(0);
	lock.confirm_via(scripted_answers(
		&asked,
		[
			(Duration::ZERO, Err(ErrorKind::Reqwest)),
			(Duration::ZERO, Ok(true)),
		],
	))
	.await
	.unwrap();
	assert_eq!(asked.get(), 2);
	// asked again after the retry interval
	assert_eq!(begin.elapsed(), LOCK_RETRY_INTERVAL);
	assert!(lock.is_valid());
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn a_confirmation_failing_in_transport_until_the_lease_lapses_loses_the_lock() {
	let lock = unconfirmed_lock(LOCK_LOSS_BUDGET + Duration::from_secs(5));
	let begin = Instant::now();
	let asked = std::cell::Cell::new(0);
	let error = lock
		.confirm_via(counted_refresh(
			&asked,
			Duration::ZERO,
			Err(ErrorKind::Reqwest),
		))
		.await
		.unwrap_err();
	assert_eq!(error.kind(), ErrorKind::LockLost);
	// asked once a second while the lease could stand its last 30s
	assert_eq!(asked.get(), 30);
	assert_eq!(begin.elapsed(), Duration::from_secs(30));
	assert!(lock.lease.is_lost());
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test]
async fn a_confirmation_failing_otherwise_is_returned_without_losing_the_lock() {
	let lock = unconfirmed_lock(LOCK_LOSS_BUDGET + Duration::from_secs(5));
	let asked = std::cell::Cell::new(0);
	let error = lock
		.confirm_via(counted_refresh(
			&asked,
			Duration::ZERO,
			Err(ErrorKind::Server),
		))
		.await
		.unwrap_err();
	assert_eq!(error.kind(), ErrorKind::Server);
	assert_eq!(asked.get(), 1);
	assert!(!lock.lease.is_lost());
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test]
async fn a_suspend_past_the_ttl_loses_the_lock_without_asking() {
	let lock = test_lock(None);
	lock.lease
		.confirmed
		.send_replace(suspended_for(LOCK_SERVER_TTL + Duration::from_secs(5)));
	let asked = std::cell::Cell::new(0);
	let error = lock
		.confirm_via(counted_refresh(&asked, Duration::ZERO, Ok(true)))
		.await
		.unwrap_err();
	assert_eq!(error.kind(), ErrorKind::LockLost);
	// an older server would confirm a lapsed lease, so it is not asked
	assert_eq!(asked.get(), 0);
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn concurrent_confirmations_share_one_refresh() {
	let lock = unconfirmed_lock(LOCK_LOSS_BUDGET + Duration::from_secs(5));
	let asked = std::cell::Cell::new(0);
	let (first, second) = tokio::join!(
		lock.confirm_via(counted_refresh(&asked, Duration::from_secs(2), Ok(true))),
		lock.confirm_via(counted_refresh(&asked, Duration::from_secs(2), Ok(true))),
	);
	first.unwrap();
	second.unwrap();
	assert_eq!(asked.get(), 1);
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn concurrent_confirmations_share_a_failed_refresh() {
	let lock = unconfirmed_lock(LOCK_LOSS_BUDGET + Duration::from_secs(5));
	let asked = std::cell::Cell::new(0);
	let failing = || counted_refresh(&asked, Duration::from_secs(2), Err(ErrorKind::Server));
	let (first, second) = tokio::join!(lock.confirm_via(failing()), lock.confirm_via(failing()));
	assert_eq!(first.unwrap_err().kind(), ErrorKind::Server);
	assert_eq!(second.unwrap_err().kind(), ErrorKind::Server);
	assert_eq!(asked.get(), 1);
	assert!(!lock.lease.is_lost());
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn a_confirmation_is_not_waited_for_past_the_lease() {
	let lock = unconfirmed_lock(LOCK_LOSS_BUDGET + Duration::from_secs(5));
	let begin = Instant::now();
	let asked = std::cell::Cell::new(0);
	let error = lock
		.confirm_via(unanswered_refresh(&asked))
		.await
		.unwrap_err();
	assert_eq!(error.kind(), ErrorKind::LockLost);
	// the lease could stand 30s more: the refresh is given that long, and no longer
	assert_eq!(begin.elapsed(), Duration::from_secs(30));
	assert_eq!(asked.get(), 1);
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn a_slow_keep_alive_confirmation_gives_the_confirmation_more_time() {
	let lock = unconfirmed_lock(LOCK_LOSS_BUDGET + Duration::from_secs(5));
	let begin = Instant::now();
	let asked = std::cell::Cell::new(0);
	// The keep-alive's refresh sent 20s before the confirmation started is answered 10s into
	// it, which leaves the lease unconfirmed (30s old) but able to stand 30s more: the
	// confirmation's own refresh, answered at 35s, is still inside the lease.
	let keep_alive_sent = confirmed_ago(Duration::from_secs(20));
	let (confirmed, ()) = tokio::join!(
		lock.confirm_via(counted_refresh(&asked, Duration::from_secs(35), Ok(true))),
		async {
			sleep(Duration::from_secs(10)).await;
			record_confirmation(&lock.lease.confirmed, keep_alive_sent);
		}
	);
	confirmed.unwrap();
	assert_eq!(begin.elapsed(), Duration::from_secs(35));
	assert_eq!(asked.get(), 1);
	assert!(!lock.lease.is_lost());
	// confirmed as of its own refresh's send
	assert_eq!(lock.lease.confirmed.borrow().at, begin);
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn a_confirmation_ends_once_the_keep_alive_confirms_the_lease() {
	let lock = unconfirmed_lock(LOCK_LOSS_BUDGET + Duration::from_secs(5));
	let begin = Instant::now();
	let asked = std::cell::Cell::new(0);
	let (confirmed, ()) = tokio::join!(lock.confirm_via(unanswered_refresh(&asked)), async {
		sleep(Duration::from_secs(2)).await;
		record_confirmation(&lock.lease.confirmed, Confirmed::now());
	});
	confirmed.unwrap();
	assert_eq!(begin.elapsed(), Duration::from_secs(2));
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn a_lock_lost_while_its_confirmation_is_asked_is_not_confirmed() {
	let lock = unconfirmed_lock(LOCK_LOSS_BUDGET + Duration::from_secs(5));
	let asked = std::cell::Cell::new(0);
	// the keep-alive learns of the loss while the confirmation's own refresh is still under
	// way; an older server would confirm that refresh all the same
	let (confirmed, ()) = tokio::join!(
		lock.confirm_via(counted_refresh(&asked, Duration::from_secs(2), Ok(true))),
		async {
			sleep(Duration::from_secs(1)).await;
			lock.lease.valid.send_replace(false);
		}
	);
	assert_eq!(confirmed.unwrap_err().kind(), ErrorKind::LockLost);
	assert!(!lock.is_valid());
}

/// A lease for [`confirm_or_retake`]: confirmed, lost, or failing to be confirmed.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[derive(Debug, PartialEq)]
enum ScriptedLease {
	Held(u8),
	Lost,
	Failing,
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
impl Lease for ScriptedLease {
	async fn confirm(&self) -> Result<(), Error> {
		match self {
			Self::Held(_) => Ok(()),
			Self::Lost => Err(LockLostError {
				resource: "test-resource".to_owned(),
			}
			.into()),
			Self::Failing => Err(Error::custom(ErrorKind::Server, "scripted refresh")),
		}
	}
}

/// A retake handing out `ScriptedLease::Held(2)`, counted in `retaken`.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
fn counted_retake(
	retaken: &std::cell::Cell<usize>,
) -> impl FnOnce() -> std::future::Ready<Result<ScriptedLease, Error>> {
	move || {
		retaken.set(retaken.get() + 1);
		std::future::ready(Ok(ScriptedLease::Held(2)))
	}
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test]
async fn confirm_or_retake_keeps_a_confirmed_lease() {
	let retaken = std::cell::Cell::new(0);
	let retake = confirm_or_retake(&ScriptedLease::Held(1), counted_retake(&retaken))
		.await
		.unwrap();
	assert_eq!(retake, None);
	assert_eq!(retaken.get(), 0);
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test]
async fn confirm_or_retake_retakes_a_lost_lease() {
	let retaken = std::cell::Cell::new(0);
	let retake = confirm_or_retake(&ScriptedLease::Lost, counted_retake(&retaken))
		.await
		.unwrap();
	assert_eq!(retake, Some(ScriptedLease::Held(2)));
	assert_eq!(retaken.get(), 1);
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test]
async fn confirm_or_retake_returns_a_failure_to_confirm() {
	let retaken = std::cell::Cell::new(0);
	let error = confirm_or_retake(&ScriptedLease::Failing, counted_retake(&retaken))
		.await
		.unwrap_err();
	assert_eq!(error.kind(), ErrorKind::Server);
	assert_eq!(retaken.get(), 0);
}

/// One scripted outcome for a [`refresh_loop`] refresh attempt.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[derive(Clone, Copy)]
enum Step {
	/// server confirms instantly (refreshed: true)
	Confirm,
	/// server confirms after this many seconds
	SlowConfirm(u64),
	/// server answers refreshed: false
	Refuse,
	/// transport-level failure, instantly
	TransportErr,
	/// the request never completes
	Hang,
	/// the lock was dropped before this attempt
	Drop,
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
type ScriptedLockFut = std::pin::Pin<Box<dyn Future<Output = Result<bool, Error>> + Send>>;

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
fn scripted_refresh(steps: &[Step]) -> impl FnMut() -> Option<ScriptedLockFut> {
	let mut steps = std::collections::VecDeque::from(steps.to_vec());
	move || {
		let step = steps
			.pop_front()
			.expect("refresh called more often than scripted");
		match step {
			Step::Drop => None,
			Step::Confirm => Some(Box::pin(async { Ok(true) })),
			Step::Refuse => Some(Box::pin(async { Ok(false) })),
			Step::TransportErr => Some(Box::pin(async {
				Err(Error::custom(
					crate::ErrorKind::Server,
					"scripted refresh failure",
				))
			})),
			Step::Hang => Some(Box::pin(std::future::pending())),
			Step::SlowConfirm(secs) => Some(Box::pin(async move {
				tokio::time::sleep(Duration::from_secs(secs)).await;
				Ok(true)
			})),
		}
	}
}

/// A lease confirmed at `at` on the monotonic clock, and now on the wall clock, which the
/// paused test clock does not move.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
fn confirmed_at(at: Instant) -> tokio::sync::watch::Sender<Confirmed> {
	tokio::sync::watch::Sender::new(Confirmed {
		at,
		wall: Utc::now(),
	})
}

/// The loss flag of a lock that is not lost.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
fn held() -> tokio::sync::watch::Sender<bool> {
	tokio::sync::watch::Sender::new(true)
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn an_unconfirmed_lock_is_invalid_until_the_server_confirms_it_again() {
	let lock = test_lock(None);
	tokio::time::advance(LOCK_LOSS_BUDGET).await;
	assert!(!lock.is_valid());
	assert!(!lock.lease.is_lost(), "the lease may still stand");
	record_confirmation(&lock.lease.confirmed, Confirmed::now());
	assert!(lock.is_valid());
}

// All timings below are exact: the paused tokio clock only advances by the
// timers the loop itself registers.

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn lock_drop_ends_refresh_loop_silently() {
	let begin = Instant::now();
	let exit = refresh_loop(
		scripted_refresh(&[Step::Confirm, Step::Drop]),
		&confirmed_at(begin),
		&held(),
	)
	.await;
	assert_eq!(exit, LoopExit::Dropped);
	// confirm at 15s, drop observed at the 30s refresh
	assert_eq!(begin.elapsed(), Duration::from_secs(30));
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn server_refusal_is_authoritative_loss() {
	let begin = Instant::now();
	let exit = refresh_loop(
		scripted_refresh(&[Step::Refuse]),
		&confirmed_at(begin),
		&held(),
	)
	.await;
	assert_eq!(exit, LoopExit::Lost);
	assert_eq!(begin.elapsed(), Duration::from_secs(15));
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn transient_refresh_error_retries_and_recovers() {
	let begin = Instant::now();
	let confirmed = confirmed_at(begin);
	let exit = refresh_loop(
		scripted_refresh(&[Step::TransportErr, Step::Confirm, Step::Confirm, Step::Drop]),
		&confirmed,
		&held(),
	)
	.await;
	assert_eq!(exit, LoopExit::Dropped);
	// err at 15s, retry confirms at 16s, confirm at 31s, drop observed at 46s
	assert_eq!(begin.elapsed(), Duration::from_secs(46));
	assert_eq!(confirmed.borrow().at - begin, Duration::from_secs(31));
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn failing_refreshes_are_retried_until_the_lease_lapses() {
	let begin = Instant::now();
	// attempts at 15s..=59s, the lease lapses at 60s — exactly 45 attempts. Past the loss
	// budget the lock reads unconfirmed, but is only lost once the server could no longer
	// hold it for us.
	let exit = refresh_loop(
		scripted_refresh(&[Step::TransportErr; 45]),
		&confirmed_at(begin),
		&held(),
	)
	.await;
	assert_eq!(exit, LoopExit::Lost);
	assert_eq!(begin.elapsed(), LOCK_SERVER_TTL);
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn hung_refresh_is_cut_off_when_the_lease_lapses() {
	let begin = Instant::now();
	let exit = refresh_loop(
		scripted_refresh(&[Step::Hang]),
		&confirmed_at(begin),
		&held(),
	)
	.await;
	assert_eq!(exit, LoopExit::Lost);
	assert_eq!(begin.elapsed(), LOCK_SERVER_TTL);
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn a_refresh_late_past_the_budget_asks_the_server_before_any_loss() {
	// The loop ran late (a slow acquire answer, a starved runtime): the server may still
	// hold the lease, so it is asked rather than the lock declared lost unheard.
	let begin = Instant::now();
	tokio::time::advance(LOCK_LOSS_BUDGET + Duration::from_secs(5)).await;
	let exit = refresh_loop(
		scripted_refresh(&[Step::Confirm, Step::Drop]),
		&confirmed_at(begin),
		&held(),
	)
	.await;
	assert_eq!(exit, LoopExit::Dropped);
	// confirms at 30s, drop observed at the 45s refresh
	assert_eq!(begin.elapsed(), Duration::from_secs(45));
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn a_suspend_past_the_ttl_is_loss_by_the_wall_clock() {
	// the monotonic clock did not see the suspend, so only the wall clock can tell
	let confirmed =
		tokio::sync::watch::Sender::new(suspended_for(LOCK_SERVER_TTL + Duration::from_secs(5)));
	let exit = refresh_loop(scripted_refresh(&[]), &confirmed, &held()).await;
	assert_eq!(exit, LoopExit::Lost);
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn a_suspend_while_waiting_is_refreshed_after_at_once() {
	let begin = Instant::now();
	let (confirmed, valid) = (confirmed_at(begin), held());
	let (exit, ()) = tokio::join!(
		refresh_loop(
			scripted_refresh(&[Step::Confirm, Step::Drop]),
			&confirmed,
			&valid
		),
		async {
			sleep(Duration::from_millis(2500)).await;
			// a 40s suspend: the wall clock moved on, the monotonic one did not
			confirmed.send_replace(Confirmed {
				at: begin,
				wall: Utc::now() - chrono::Duration::seconds(40),
			});
		}
	);
	assert_eq!(exit, LoopExit::Dropped);
	// refreshed at the next look at the clock (3s), not when the 15s timer ran out, and
	// the drop observed at the refresh after (18s)
	assert_eq!(begin.elapsed(), Duration::from_secs(18));
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn a_lock_lost_elsewhere_is_refreshed_no_more() {
	let begin = Instant::now();
	let (confirmed, valid) = (confirmed_at(begin), held());
	let (exit, ()) = tokio::join!(
		// one refresh is scripted: a second would panic
		refresh_loop(scripted_refresh(&[Step::Confirm]), &confirmed, &valid),
		async {
			sleep(Duration::from_millis(20_500)).await;
			valid.send_replace(false);
		}
	);
	assert_eq!(exit, LoopExit::Lost);
	// confirmed at 15s, the loss seen at the next look at the clock
	assert_eq!(begin.elapsed(), Duration::from_secs(21));
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn a_refresh_answered_after_the_lock_was_lost_does_not_confirm_it() {
	let begin = Instant::now();
	let confirmed = confirmed_at(begin);
	let valid = held();
	let (exit, ()) = tokio::join!(
		refresh_loop(
			scripted_refresh(&[Step::SlowConfirm(5)]),
			&confirmed,
			&valid
		),
		async {
			sleep(Duration::from_secs(17)).await;
			valid.send_replace(false);
		}
	);
	assert_eq!(exit, LoopExit::Lost);
	// sent at 15s, answered at 20s, after the loss at 17s
	assert_eq!(begin.elapsed(), Duration::from_secs(20));
	assert_eq!(confirmed.borrow().at, begin);
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn slow_confirm_keeps_cadence_anchored_to_send_time() {
	let begin = Instant::now();
	let exit = refresh_loop(
		scripted_refresh(&[Step::SlowConfirm(5), Step::Drop]),
		&confirmed_at(begin),
		&held(),
	)
	.await;
	assert_eq!(exit, LoopExit::Dropped);
	// sent at 15s, confirmed at 20s: the next refresh fires 15s after the
	// SEND (at 30s), not 15s after the response (35s) — a slow round-trip
	// must not stretch the cadence past the server TTL
	assert_eq!(begin.elapsed(), Duration::from_secs(30));
}

/// One scripted outcome for an [`acquire_loop`] poll.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[derive(Clone, Copy)]
enum AcquireStep {
	/// server answers acquired: true
	Grant,
	/// server answers acquired: true after this many seconds
	SlowGrant(u64),
	/// server answers acquired: false (someone else holds the lock)
	Refuse,
	/// no answer: the request failed in transport
	Unreachable,
	/// the server answered with an error
	Rejected,
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
fn scripted_acquire(polls: &[AcquireStep]) -> impl FnMut() -> ScriptedLockFut {
	let mut polls = std::collections::VecDeque::from(polls.to_vec());
	move || {
		let poll = polls
			.pop_front()
			.expect("acquire polled more often than scripted");
		match poll {
			AcquireStep::Grant => Box::pin(async { Ok(true) }),
			AcquireStep::SlowGrant(secs) => Box::pin(async move {
				tokio::time::sleep(Duration::from_secs(secs)).await;
				Ok(true)
			}),
			AcquireStep::Refuse => Box::pin(async { Ok(false) }),
			AcquireStep::Unreachable => Box::pin(async {
				Err(Error::custom(
					ErrorKind::Reqwest,
					"scripted transport failure",
				))
			}),
			AcquireStep::Rejected => {
				Box::pin(async { Err(Error::custom(ErrorKind::Server, "scripted server error")) })
			}
		}
	}
}

// With a 30s ceiling the fibonacci_iter delays run 0, 0.25s, 0.25s, 0.5s, 0.75s, 1.25s, 2s,
// 3.25s, ... and each is slept after the poll it belongs to.

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn acquire_refusals_use_up_the_attempts() {
	let begin = Instant::now();
	let err = acquire_loop(
		scripted_acquire(&[AcquireStep::Refuse; 3]),
		"r",
		Duration::from_secs(30),
		3,
	)
	.await
	.unwrap_err();
	assert_eq!(err.kind(), ErrorKind::RetryFailed);
	// 0 + 0.25 + 0.25s: the last attempt's delay is still slept before RetryFailed
	assert_eq!(begin.elapsed(), Duration::from_millis(500));
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn acquire_delays_stop_growing_at_max_sleep_time() {
	let begin = Instant::now();
	let err = acquire_loop(
		scripted_acquire(&[AcquireStep::Refuse; 5]),
		"r",
		Duration::from_millis(500),
		5,
	)
	.await
	.unwrap_err();
	assert_eq!(err.kind(), ErrorKind::RetryFailed);
	// 0 + 0.25 + 0.25 + 0.5 + 0.5s; uncapped the last delay would be 0.75s
	assert_eq!(begin.elapsed(), Duration::from_millis(1500));
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn acquire_grant_is_anchored_to_its_send_time() {
	let begin = Instant::now();
	let granted_at = acquire_loop(
		scripted_acquire(&[
			AcquireStep::Refuse,
			AcquireStep::Refuse,
			AcquireStep::SlowGrant(5),
		]),
		"r",
		Duration::from_secs(30),
		10,
	)
	.await
	.unwrap();
	// sent at 0.25s, answered at 5.25s: the lease runs from the send
	assert_eq!(granted_at.at - begin, Duration::from_millis(250));
	assert_eq!(begin.elapsed(), Duration::from_millis(5250));
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn acquire_transport_error_on_the_first_poll_fails_fast() {
	let begin = Instant::now();
	let err = acquire_loop(
		scripted_acquire(&[AcquireStep::Unreachable]),
		"r",
		Duration::from_secs(30),
		10,
	)
	.await
	.unwrap_err();
	// Nothing has answered yet, so an offline device gets its error at once.
	assert_eq!(err.kind(), ErrorKind::Reqwest);
	assert_eq!(begin.elapsed(), Duration::ZERO);
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn acquire_transport_error_after_an_answer_keeps_polling() {
	let begin = Instant::now();
	let granted_at = acquire_loop(
		scripted_acquire(&[
			AcquireStep::Refuse,
			AcquireStep::Unreachable,
			AcquireStep::Grant,
		]),
		"r",
		Duration::from_secs(30),
		10,
	)
	.await
	.unwrap();
	// refused at 0s, unreachable at 0s, granted at 0.25s
	assert_eq!(granted_at.at - begin, Duration::from_millis(250));
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn acquire_transport_errors_past_the_limit_fail_the_acquisition() {
	let begin = Instant::now();
	let err = acquire_loop(
		scripted_acquire(&[
			AcquireStep::Refuse,
			AcquireStep::Unreachable,
			AcquireStep::Unreachable,
			AcquireStep::Unreachable,
			AcquireStep::Unreachable,
		]),
		"r",
		Duration::from_secs(30),
		10,
	)
	.await
	.unwrap_err();
	assert_eq!(err.kind(), ErrorKind::Reqwest);
	// the fourth consecutive failure, at 0 + 0.25 + 0.25 + 0.5s, is returned
	assert_eq!(begin.elapsed(), Duration::from_secs(1));
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn acquire_answer_resets_the_transport_error_count() {
	let begin = Instant::now();
	let granted_at = acquire_loop(
		scripted_acquire(&[
			AcquireStep::Refuse,
			AcquireStep::Unreachable,
			AcquireStep::Unreachable,
			AcquireStep::Unreachable,
			AcquireStep::Refuse,
			AcquireStep::Unreachable,
			AcquireStep::Unreachable,
			AcquireStep::Unreachable,
			AcquireStep::Grant,
		]),
		"r",
		Duration::from_secs(30),
		10,
	)
	.await
	.unwrap();
	// refused again at 1s, so three more failures are waited out; granted at 8.25s
	assert_eq!(granted_at.at - begin, Duration::from_millis(8250));
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn acquire_transport_error_uses_up_an_attempt() {
	let begin = Instant::now();
	let err = acquire_loop(
		scripted_acquire(&[AcquireStep::Refuse, AcquireStep::Unreachable]),
		"r",
		Duration::from_secs(30),
		2,
	)
	.await
	.unwrap_err();
	assert_eq!(err.kind(), ErrorKind::RetryFailed);
	assert_eq!(begin.elapsed(), Duration::from_millis(250));
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn acquire_server_error_after_an_answer_is_returned() {
	let begin = Instant::now();
	let err = acquire_loop(
		scripted_acquire(&[AcquireStep::Refuse, AcquireStep::Rejected]),
		"r",
		Duration::from_secs(30),
		10,
	)
	.await
	.unwrap_err();
	assert_eq!(err.kind(), ErrorKind::Server);
	assert_eq!(begin.elapsed(), Duration::ZERO);
}

/// A release that cannot get through holds a shutdown up for the bound only: here the proxy
/// takes the release's connection and never answers it, as a network that drops every packet.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn settling_lock_releases_gives_up_after_the_bound_on_a_dead_network() {
	let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let client = Arc::new(client_via_proxy(
		ClientConfig::default(),
		proxy.local_addr().unwrap(),
	));
	drop(test_lock_on(Arc::clone(&client), None));
	let (mut release, _) = proxy.accept().await.unwrap();
	assert!(read_head(&mut release).await.starts_with(GATEWAY_CONNECT));

	let begin = Instant::now();
	assert!(
		!client
			.pending_lock_releases
			.settle(LOCK_RELEASE_SETTLE_BOUND)
			.await
	);
	assert_eq!(begin.elapsed(), LOCK_RELEASE_SETTLE_BOUND);
	drop(release);
}

/// When a lease scripted for a release test was last confirmed: now, on the paused clock.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
fn lease_confirmed_now() -> impl Fn() -> Duration {
	let confirmed = Confirmed::now();
	move || confirmed.age()
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn a_release_failing_in_transport_is_sent_again() {
	let asked = std::cell::Cell::new(0);
	let begin = Instant::now();
	release_loop(
		scripted_answers(
			&asked,
			[
				(Duration::ZERO, Err(ErrorKind::Reqwest)),
				(Duration::ZERO, Ok(true)),
			],
		),
		lease_confirmed_now(),
		"drive-write",
	)
	.await;
	assert_eq!(asked.get(), 2);
	assert_eq!(begin.elapsed(), RELEASE_RETRY_FIRST);
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn a_release_retries_with_a_growing_pause() {
	let asked = std::cell::Cell::new(0);
	let failure = (Duration::ZERO, Err(ErrorKind::Reqwest));
	let begin = Instant::now();
	release_loop(
		scripted_answers(
			&asked,
			[
				failure,
				failure,
				failure,
				failure,
				failure,
				failure,
				(Duration::ZERO, Ok(true)),
			],
		),
		lease_confirmed_now(),
		"drive-write",
	)
	.await;
	assert_eq!(asked.get(), 7);
	// 0.25 + 0.5 + 1 + 2 + 4 + 4: capped at the ceiling
	assert_eq!(
		begin.elapsed(),
		Duration::from_millis(250 + 500 + 1000 + 2000) + RELEASE_RETRY_MAX * 2
	);
}

/// Answered by the server, a release is never sent again: `released: false` means the lease
/// was not held, or not by this uuid, and asking again gets the same answer.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn a_release_the_server_answered_is_not_sent_again() {
	for answer in [Ok(true), Ok(false), Err(ErrorKind::Server)] {
		let asked = std::cell::Cell::new(0);
		release_loop(
			scripted_answers(&asked, [(Duration::ZERO, answer)]),
			lease_confirmed_now(),
			"drive-write",
		)
		.await;
		assert_eq!(asked.get(), 1, "{answer:?} is final");
	}
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn release_retries_stop_once_the_lease_has_expired() {
	let asked = std::cell::Cell::new(0);
	let begin = Instant::now();
	let sent = std::cell::RefCell::new(Vec::new());
	let mut failing = counted_refresh(&asked, Duration::ZERO, Err(ErrorKind::Reqwest));
	release_loop(
		|| {
			sent.borrow_mut().push(begin.elapsed());
			failing()
		},
		lease_confirmed_now(),
		"drive-write",
	)
	.await;
	let sent = sent.into_inner();
	assert!(sent.len() > 2, "retried while the lease could stand");
	assert!(
		sent.iter().all(|at| *at < LOCK_SERVER_TTL),
		"no retry starts past the lease: {sent:?}"
	);
	assert!(begin.elapsed() < LOCK_SERVER_TTL);
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn a_release_retry_in_flight_is_cut_off_when_the_lease_expires() {
	let asked = std::cell::Cell::new(0);
	let begin = Instant::now();
	release_loop(
		scripted_answers(
			&asked,
			[
				(Duration::ZERO, Err(ErrorKind::Reqwest)),
				(LOCK_SERVER_TTL * 10, Ok(true)),
			],
		),
		lease_confirmed_now(),
		"drive-write",
	)
	.await;
	assert_eq!(asked.get(), 2);
	assert_eq!(begin.elapsed(), LOCK_SERVER_TTL);
}

/// A lock taken for lost past its lease still sends its release once, in case the clocks
/// overstate the age, but never retries it.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn a_release_past_the_lease_is_sent_once() {
	let asked = std::cell::Cell::new(0);
	let lapsed = confirmed_ago(LOCK_SERVER_TTL);
	release_loop(
		counted_refresh(&asked, Duration::ZERO, Err(ErrorKind::Reqwest)),
		move || lapsed.age(),
		"drive-write",
	)
	.await;
	assert_eq!(asked.get(), 1);
}

/// A refresh the keep-alive records while the release retries extends the retries: the bound is
/// read from the lease's newest confirmation, not the one at the drop.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn release_retries_follow_the_newest_confirmation() {
	let confirmed = tokio::sync::watch::Sender::new(Confirmed::now());
	let begin = Instant::now();
	let mut last_sent = Duration::ZERO;
	let mut refreshed_at = None;
	release_loop(
		|| {
			last_sent = begin.elapsed();
			if refreshed_at.is_none() && last_sent >= Duration::from_secs(10) {
				// a refresh sent before the drop, answered just now
				confirmed.send_replace(Confirmed::now());
				refreshed_at = Some(last_sent);
			}
			futures::future::ready(Err(Error::custom(ErrorKind::Reqwest, "scripted release")))
		},
		|| confirmed.borrow().age(),
		"drive-write",
	)
	.await;
	let refreshed_at = refreshed_at.expect("retried for 10 s");
	assert!(last_sent > LOCK_SERVER_TTL, "retried past the drop's bound");
	assert!(last_sent < refreshed_at + LOCK_SERVER_TTL);
}

/// A shutdown waits for a release that is being retried, as the retries stay counted: until it
/// is answered, or for the shutdown's bound while it keeps failing.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn a_shutdown_waits_for_a_retrying_release_within_its_bound() {
	let failure = (Duration::ZERO, Err(ErrorKind::Reqwest));
	for (script, settled, waited) in [
		(
			vec![failure, failure, (Duration::ZERO, Ok(true))],
			true,
			Duration::from_millis(250 + 500),
		),
		(
			std::iter::repeat_n(failure, 64).collect(),
			false,
			LOCK_RELEASE_SETTLE_BOUND,
		),
	] {
		let pending = PendingReleases::default();
		let asked = std::cell::Cell::new(0);
		let guard = pending.track();
		let ((), (outcome, elapsed)) = tokio::join!(
			async {
				release_loop(
					scripted_answers(&asked, script),
					lease_confirmed_now(),
					"drive-write",
				)
				.await;
				drop(guard);
			},
			async {
				let begin = Instant::now();
				let outcome = pending.settle(LOCK_RELEASE_SETTLE_BOUND).await;
				(outcome, begin.elapsed())
			},
		);
		assert_eq!(outcome, settled);
		assert_eq!(elapsed, waited);
		assert!(asked.get() > 1, "the release was retried");
	}
}

/// A retry resends the released lease's own uuid, so it can never free a lease the client
/// acquired since: here the first release reached the server but its answer was lost, and the
/// client took the lock again under a new uuid before the retry went out.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn a_release_retry_never_frees_a_newer_lease() {
	let (old, newer) = (Uuid::new_v4(), Uuid::new_v4());
	let holder = std::cell::Cell::new(Some(old));
	let body = release_body(old, "drive-write").unwrap();
	let mut released = Vec::new();
	release_loop(
		|| {
			let request: api::v3::user::lock::Request =
				serde_json::from_slice(&body).expect("a lock request");
			assert!(matches!(request.r#type, LockType::Release));
			assert_eq!(request.resource, "drive-write");
			released.push(request.uuid);
			// the server releases only what this uuid holds
			let freed = holder.get() == Some(request.uuid);
			if freed {
				holder.set(None);
			}
			let answer = if released.len() == 1 {
				// the release landed, its answer was lost; the client acquires again
				holder.set(Some(newer));
				Err(Error::custom(ErrorKind::Reqwest, "answer lost"))
			} else {
				Ok(freed)
			};
			futures::future::ready(answer)
		},
		lease_confirmed_now(),
		"drive-write",
	)
	.await;
	assert_eq!(released, [old, old], "every attempt carries the old uuid");
	assert_eq!(holder.get(), Some(newer), "the newer lease is still held");
}

/// The events a lock listener got, in order.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
type Events = Arc<std::sync::Mutex<Vec<LockEvent>>>;

/// A lock listener that keeps every event it gets in the returned list.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
fn recording_listener() -> (Events, impl Fn(LockEvent) + Send + Sync + 'static) {
	let events = Events::default();
	let recorded = Arc::clone(&events);
	(events, move |event| recorded.lock().unwrap().push(event))
}

/// An offline client whose lock events are recorded.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
fn listened_client() -> (Arc<AuthClient>, Events) {
	let client = offline_client();
	let (events, listener) = recording_listener();
	client.lock_listener.set(Arc::new(listener));
	(client, events)
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
fn lock_event(kind: LockEventKind, uuid: Uuid, resource: &str) -> LockEvent {
	LockEvent {
		kind,
		lock: LockId {
			uuid,
			resource: resource.to_owned(),
		},
	}
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
fn recorded(events: &Events) -> Vec<LockEvent> {
	events.lock().unwrap().clone()
}

/// A runtime on a paused clock, so the retries of a release, which fails offline, take no real
/// time.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
fn paused_runtime() -> tokio::runtime::Runtime {
	tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.start_paused(true)
		.build()
		.unwrap()
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[test]
fn no_lock_is_recorded_without_a_listener() {
	assert!(
		LockListenerSlot::default()
			.record(Uuid::from_u128(1), "test-resource")
			.is_none()
	);
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[test]
fn a_dropped_lock_is_owed_until_its_release_settles() {
	let (client, events) = listened_client();
	let rt = paused_runtime();
	let lock = rt.block_on(async {
		test_lock_on(
			Arc::clone(&client),
			tokio::runtime::Handle::try_current().ok(),
		)
	});
	let uuid = lock.lease.uuid;
	let owed = lock_event(LockEventKind::Owed, uuid, "test-resource");
	assert_eq!(recorded(&events), std::slice::from_ref(&owed));

	let _guard = rt.enter();
	std::mem::drop(lock);
	assert_eq!(
		recorded(&events),
		std::slice::from_ref(&owed),
		"owed while its release is unanswered"
	);
	rt.block_on(client.pending_lock_releases.settled());
	assert_eq!(
		recorded(&events),
		[
			owed,
			lock_event(LockEventKind::Settled, uuid, "test-resource")
		]
	);
}

/// A lost lock sends a release at the loss and another when it is dropped: it is settled once,
/// after both.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[test]
fn a_lost_lock_is_settled_once_after_its_last_release() {
	let (client, events) = listened_client();
	let rt = paused_runtime();
	let lock = rt.block_on(async {
		test_lock_on(
			Arc::clone(&client),
			tokio::runtime::Handle::try_current().ok(),
		)
	});
	let uuid = lock.lease.uuid;
	let _guard = rt.enter();
	lock.lease.mark_lost();
	std::mem::drop(lock);
	assert_eq!(rt.metrics().num_alive_tasks(), 2, "two releases in flight");

	rt.block_on(client.pending_lock_releases.settled());
	assert_eq!(
		recorded(&events),
		[
			lock_event(LockEventKind::Owed, uuid, "test-resource"),
			lock_event(LockEventKind::Settled, uuid, "test-resource"),
		]
	);
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[test]
fn a_lock_request_its_caller_stopped_waiting_for_stays_owed_until_it_ends() {
	let (client, events) = listened_client();
	let rt = paused_runtime();
	let uuid = Uuid::from_u128(1);
	let mut request = Box::pin(request_acquire(
		Arc::clone(&client),
		lock_url().into(),
		Bytes::new(),
		uuid,
		"test-resource".into(),
		client.lock_listener.record(uuid, "test-resource"),
	));
	rt.block_on(async {
		assert!(futures::poll!(&mut request).is_pending());
	});
	std::mem::drop(request);

	let owed = lock_event(LockEventKind::Owed, uuid, "test-resource");
	assert_eq!(
		recorded(&events),
		std::slice::from_ref(&owed),
		"a grant nobody waits for any more would have to be released"
	);
	rt.block_on(client.pending_lock_releases.settled());
	assert_eq!(
		recorded(&events),
		[
			owed,
			lock_event(LockEventKind::Settled, uuid, "test-resource")
		]
	);
}

/// Offline, the first poll fails at once: the lock is owed from its send until its request task
/// has handed the answer over.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn a_failed_acquisition_reports_its_lock_owed_then_settled() {
	let client = crate::auth::unauth::test_support::offline_client();
	let (events, listener) = recording_listener();
	client.set_lock_listener(listener);

	client
		.acquire_lock("test-resource", MAX_SLEEP_TIME_DEFAULT, 1)
		.await
		.unwrap_err();
	client.wait_for_lock_releases().await;

	let events = recorded(&events);
	let [owed, settled] = events.as_slice() else {
		panic!("one lock owed, then settled: {events:?}");
	};
	assert_eq!(
		owed,
		&lock_event(LockEventKind::Owed, owed.lock.uuid, "test-resource")
	);
	assert_eq!(
		settled,
		&lock_event(LockEventKind::Settled, owed.lock.uuid, "test-resource")
	);
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[test]
fn a_listener_set_later_does_not_see_an_earlier_lock_settle() {
	let (client, first) = listened_client();
	let rt = paused_runtime();
	let lock = rt.block_on(async {
		test_lock_on(
			Arc::clone(&client),
			tokio::runtime::Handle::try_current().ok(),
		)
	});
	let uuid = lock.lease.uuid;
	let (second, listener) = recording_listener();
	client.lock_listener.set(Arc::new(listener));

	let _guard = rt.enter();
	std::mem::drop(lock);
	rt.block_on(client.pending_lock_releases.settled());
	assert_eq!(
		recorded(&first),
		[
			lock_event(LockEventKind::Owed, uuid, "test-resource"),
			lock_event(LockEventKind::Settled, uuid, "test-resource"),
		]
	);
	assert_eq!(recorded(&second), []);
}

/// Locks of an earlier run are released like a dropped lock, owed until each release is given up
/// on: offline, once the lease could have lapsed, judged from the call (the bound itself is
/// `release_loop`'s, tested above).
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn released_locks_are_owed_until_their_releases_are_given_up_on() {
	let client = crate::auth::unauth::test_support::offline_client();
	let (events, listener) = recording_listener();
	client.set_lock_listener(listener);
	let (drive, notes) = (Uuid::from_u128(1), Uuid::from_u128(2));

	let (begin, begin_wall) = (Instant::now(), Utc::now());
	client
		.release_locks([
			LockId {
				uuid: drive,
				resource: "drive-write".to_owned(),
			},
			LockId {
				uuid: notes,
				resource: "notes-write".to_owned(),
			},
		])
		.await;
	// The lease's age is the further of the paused clock and the wall clock, which the real
	// connection attempts of the offline client advance on their own: a release given up on
	// at that age has run for at least as long on the two clocks together.
	let elapsed = begin.elapsed();
	let wall = (Utc::now() - begin_wall).to_std().unwrap_or(Duration::ZERO);
	assert!(
		elapsed + wall >= LOCK_SERVER_TTL - RELEASE_RETRY_MAX,
		"retried while the lease could stand: {elapsed:?} paused, {wall:?} on the wall clock"
	);
	assert!(client.wait_for_lock_releases().now_or_never().is_some());

	let events = recorded(&events);
	assert_eq!(
		events.get(..2),
		Some(
			[
				lock_event(LockEventKind::Owed, drive, "drive-write"),
				lock_event(LockEventKind::Owed, notes, "notes-write"),
			]
			.as_slice()
		)
	);
	let mut settled = events.get(2..).unwrap_or_default().to_vec();
	settled.sort_by_key(|event| event.lock.uuid);
	assert_eq!(
		settled,
		[
			lock_event(LockEventKind::Settled, drive, "drive-write"),
			lock_event(LockEventKind::Settled, notes, "notes-write"),
		]
	);
}

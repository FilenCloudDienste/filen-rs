use std::borrow::Cow;

use chrono::TimeDelta;
use filen_types::api::v3::socket::LockAnnouncement;

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use super::super::test_support::{offline_client, test_lock_on};
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use super::super::{Confirmed, LeaseConfirmed, OwedRelease, lock_url};
use super::*;

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use tokio::time::sleep;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasmtimer::tokio::sleep;

const DRIVE: &str = "drive-write";

fn announcement(resource: &str, holder: u128, lease_id: &str) -> LockAnnouncement<'static> {
	announcement_of_uuid(resource, Uuid::from_u128(holder), lease_id)
}

fn announcement_of_uuid(resource: &str, holder: Uuid, lease_id: &str) -> LockAnnouncement<'static> {
	LockAnnouncement {
		resource: Cow::Owned(resource.to_owned()),
		uuid: holder,
		lease_id: Cow::Owned(lease_id.to_owned()),
		timestamp: DateTime::from_timestamp_millis(1_760_270_400_123).unwrap(),
	}
}

fn acquired(lease_id: &str, holder: u128) -> LockEventType<'static> {
	LockEventType::LockAcquired(announcement(DRIVE, holder, lease_id))
}

fn released(lease_id: &str, holder: u128) -> LockEventType<'static> {
	LockEventType::LockReleased(announcement(DRIVE, holder, lease_id))
}

fn timed_out(lease_id: &str, holder: u128) -> LockEventType<'static> {
	LockEventType::LockTimeout(announcement(DRIVE, holder, lease_id))
}

fn lease(holder: u128, lock_message_id: u64) -> Holder {
	Holder::Lease(OpenLease {
		holder: Uuid::from_u128(holder),
		lock_message_id,
	})
}

/// Every order of `0..n`.
fn permutations(n: usize) -> Vec<Vec<usize>> {
	if n == 0 {
		return vec![Vec::new()];
	}
	let mut all = Vec::new();
	for shorter in permutations(n - 1) {
		for at in 0..=shorter.len() {
			let mut order = shorter.clone();
			order.insert(at, n - 1);
			all.push(order);
		}
	}
	all
}

/// The holder of [`DRIVE`] after `events` arrive, on a fresh board, in every order they can
/// arrive in.
fn holders_in_every_order(events: &[(LockEventType<'static>, u64)]) -> Vec<Holder> {
	permutations(events.len())
		.into_iter()
		.map(|order| {
			let board = LockBoard::default();
			for i in order {
				let (event, lock_message_id) = &events[i];
				board.announced(event, *lock_message_id);
			}
			board.holder(DRIVE)
		})
		.collect()
}

#[tokio::test(start_paused = true)]
async fn every_order_of_two_leases_announcements_gives_the_same_holder() {
	let both_ended = [
		(acquired("lease-a", 1), 1),
		(released("lease-a", 1), 2),
		(acquired("lease-b", 2), 3),
		(timed_out("lease-b", 2), 4),
	];
	assert_eq!(holders_in_every_order(&both_ended), [Holder::Free; 24]);

	let second_open = [
		(acquired("lease-a", 1), 1),
		(timed_out("lease-a", 1), 2),
		(acquired("lease-b", 2), 3),
	];
	assert_eq!(holders_in_every_order(&second_open), [lease(2, 3); 6]);

	// The second lease's end arrived, the first's acquisition did not.
	let first_open = [
		(released("lease-a", 1), 2),
		(acquired("lease-b", 2), 3),
		(released("lease-b", 2), 4),
	];
	assert_eq!(holders_in_every_order(&first_open), [Holder::Free; 6]);
}

#[tokio::test(start_paused = true)]
async fn an_announcement_applied_twice_changes_nothing() {
	let events = [
		(acquired("lease-a", 1), 1),
		(acquired("lease-a", 1), 1),
		(released("lease-a", 1), 2),
		(released("lease-a", 1), 2),
		(acquired("lease-b", 2), 3),
		(acquired("lease-b", 2), 3),
	];
	assert_eq!(holders_in_every_order(&events), [lease(2, 3); 720]);
}

/// While an end is on its way or was lost, two leases are open: the one with the higher counter
/// holds, whichever arrived first.
#[tokio::test(start_paused = true)]
async fn the_higher_lock_message_id_holds_while_two_leases_are_open() {
	let events = [(acquired("lease-b", 2), 9), (acquired("lease-a", 1), 5)];
	assert_eq!(holders_in_every_order(&events), [lease(2, 9); 2]);
}

#[tokio::test(start_paused = true)]
async fn a_resource_nothing_was_announced_for_has_an_unknown_holder() {
	let board = LockBoard::default();
	assert_eq!(board.holder(DRIVE), Holder::Unknown);
	board.announced(
		&LockEventType::LockAcquired(announcement("notes-write", 1, "lease-a")),
		1,
	);
	assert_eq!(board.holder(DRIVE), Holder::Unknown);
	assert_eq!(
		board.holder("notes-write"),
		Holder::Lease(OpenLease {
			holder: Uuid::from_u128(1),
			lock_message_id: 1
		})
	);
}

/// Announcements sent while there was no connection are lost, so a connection's end and the
/// next one's start both forget what was announced.
#[tokio::test(start_paused = true)]
async fn the_announced_holders_are_forgotten_between_connections() {
	let board = LockBoard::default();
	board.announced(&acquired("lease-a", 1), 1);
	board.disconnected();
	assert_eq!(board.holder(DRIVE), Holder::Unknown);

	board.announced(&acquired("lease-a", 1), 1);
	board.connected();
	assert_eq!(board.holder(DRIVE), Holder::Unknown);
}

#[tokio::test(start_paused = true)]
async fn an_ended_lease_wakes_the_waits_for_its_resource_only() {
	let board = LockBoard::default();
	let mut drive = board.wakes(DRIVE);
	let notes = board.wakes("notes-write");

	board.announced(&acquired("lease-a", 1), 1);
	assert!(
		!drive.has_changed().unwrap(),
		"an acquisition wakes nothing"
	);

	board.announced(&released("lease-a", 1), 2);
	assert!(drive.has_changed().unwrap());
	assert!(!notes.has_changed().unwrap());

	drive.mark_unchanged();
	board.announced(&timed_out("lease-b", 2), 3);
	assert!(drive.has_changed().unwrap());
	assert!(!notes.has_changed().unwrap());
}

/// Announcements may have been lost before a connection, so it wakes every wait.
#[tokio::test(start_paused = true)]
async fn a_connection_wakes_every_wait() {
	let board = LockBoard::default();
	let drive = board.wakes(DRIVE);
	let notes = board.wakes("notes-write");
	board.connected();
	assert!(drive.has_changed().unwrap());
	assert!(notes.has_changed().unwrap());
}

/// A wait's wake channel is kept while it waits only.
#[tokio::test(start_paused = true)]
async fn the_wake_channel_of_a_wait_that_ended_is_dropped() {
	let board = LockBoard::default();
	drop(board.wakes(DRIVE));
	board.announced(&released("lease-a", 1), 1);
	assert_eq!(board.board().wakes.len(), 0);

	drop(board.wakes(DRIVE));
	let notes = board.wakes("notes-write");
	assert_eq!(
		board.board().wakes.len(),
		1,
		"the ended wait's channel goes"
	);
	board.connected();
	assert_eq!(board.board().wakes.len(), 1);
	drop(notes);
	board.connected();
	assert_eq!(board.board().wakes.len(), 0);
}

#[test]
fn only_names_a_hash_tag_can_place_are_announced() {
	let cases = [
		("drive-write", true),
		("test:notes", true),
		("{a}b", true),
		("x{a}b}", true),
		("{ab}", true),
		("{", true),
		("a{b", true),
		("a}b", false),
		("}", false),
		("{}x", false),
		("{}a{b}", false),
		("}{a}", true),
		("a}{b", false),
	];
	for (resource, announced) in cases {
		assert_eq!(announces(resource), announced, "{resource}");
	}
}

/// An ended lease is remembered for ten minutes: a `lockAcquired` arriving later than that
/// opens it again.
#[tokio::test(start_paused = true)]
async fn an_ended_lease_is_remembered_for_ten_minutes() {
	let (kept, forgotten) = (LockBoard::default(), LockBoard::default());
	kept.announced(&released("lease-a", 1), 2);
	forgotten.announced(&released("lease-a", 1), 2);

	sleep(Duration::from_secs(600) - Duration::from_millis(1)).await;
	kept.announced(&acquired("lease-a", 1), 1);
	assert_eq!(kept.holder(DRIVE), Holder::Free);

	sleep(Duration::from_millis(1)).await;
	forgotten.announced(&acquired("lease-a", 1), 1);
	assert_eq!(forgotten.holder(DRIVE), lease(1, 1));
}

/// A uuid is remembered as released on its resource only.
#[test]
fn a_released_uuid_is_retired_on_its_resource() {
	let board = LockBoard::default();
	board.retire(Uuid::from_u128(1), &Arc::from(DRIVE));
	assert!(board.is_retired(DRIVE, Uuid::from_u128(1)));
	assert!(!board.is_retired("notes-write", Uuid::from_u128(1)));
	assert!(!board.is_retired(DRIVE, Uuid::from_u128(2)));
}

/// A retirement is kept for 70 s from the release's send on the wall clock, and while that
/// clock reads earlier than the send, as when it was set back.
#[test]
fn a_retirement_is_kept_for_seventy_seconds_on_the_wall_clock() {
	let at = DateTime::from_timestamp_millis(1_760_270_400_123).unwrap();
	let retired = Retired {
		resource: Arc::from(DRIVE),
		at,
	};
	assert!(retired.is_kept_at(at));
	assert!(retired.is_kept_at(at + TimeDelta::seconds(70) - TimeDelta::milliseconds(1)));
	assert!(!retired.is_kept_at(at + TimeDelta::seconds(70)));
	assert!(retired.is_kept_at(at - TimeDelta::hours(1)));
}

/// An expired retirement is no longer reported, and the next retirement forgets it.
#[test]
fn an_expired_retirement_is_forgotten() {
	let board = LockBoard::default();
	board.retire(Uuid::from_u128(1), &Arc::from(DRIVE));
	board
		.board()
		.retired
		.get_mut(&Uuid::from_u128(1))
		.unwrap()
		.at = Utc::now() - TimeDelta::seconds(70);
	assert!(!board.is_retired(DRIVE, Uuid::from_u128(1)));

	board.retire(Uuid::from_u128(2), &Arc::from(DRIVE));
	assert_eq!(
		board.board().retired.len(),
		1,
		"a retirement forgets the expired ones"
	);
}

/// The monotonic clock does not age a retirement: on wasm it counts from each worker's own
/// time origin, and a uuid retired on one worker is looked up on the websocket task's.
#[tokio::test(start_paused = true)]
async fn a_retirement_outlasts_a_monotonic_clock_running_ahead() {
	let board = LockBoard::default();
	board.retire(Uuid::from_u128(1), &Arc::from(DRIVE));
	sleep(RETIRED_KEEP * 2).await;
	assert!(board.is_retired(DRIVE, Uuid::from_u128(1)));
}

/// A release dropped before it was sent retires its uuid at the drop: on wasm it is never sent.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[test]
fn a_release_dropped_unsent_retires_its_uuid_at_once() {
	let client = offline_client();
	let uuid = Uuid::from_u128(1);
	let owed = OwedRelease::new(
		Arc::clone(&client),
		lock_url().into(),
		uuid,
		Arc::from(DRIVE),
		// a lease ago, so the detached release, which fails offline, is not sent again
		LeaseConfirmed::Sent(Confirmed {
			at: Instant::now(),
			wall: Utc::now() - TimeDelta::from_std(LOCK_SERVER_TTL).unwrap(),
		}),
		None,
	);
	assert!(!client.lock_board.is_retired(DRIVE, uuid));
	drop(owed);
	assert!(client.lock_board.is_retired(DRIVE, uuid));
}

/// A lease is registered as held until its last handle is dropped, and its uuid is retired once
/// its release goes out.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn a_lease_is_held_until_its_last_handle_drops_then_retired() {
	let client = offline_client();
	let lock = test_lock_on(Arc::clone(&client), None);
	let uuid = lock.lease.uuid;
	let held = client.lock_board.held(uuid).expect("the lease is held");
	assert!(Arc::ptr_eq(&held, &lock.lease));
	drop(held);
	assert!(!client.lock_board.is_retired("test-resource", uuid));

	let clone = lock.clone();
	drop(lock);
	assert!(
		client.lock_board.held(uuid).is_some(),
		"a clone still holds it"
	);
	drop(clone);
	assert!(client.lock_board.held(uuid).is_none());
	assert_eq!(client.lock_board.board().held.len(), 0, "and forgotten");

	client.pending_lock_releases.settled().await;
	assert!(client.lock_board.is_retired("test-resource", uuid));
}

/// The resource of the locks [`test_lock_on`] makes.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
const HELD: &str = "test-resource";

/// A lock `client` holds on [`HELD`], with no keep-alive, so only the announcements change it.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
fn held_lock(client: &Arc<AuthClient>) -> super::super::ResourceLock {
	test_lock_on(Arc::clone(client), None)
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn a_release_of_a_held_lease_announced_makes_it_unconfirmed() {
	let client = offline_client();
	let lock = held_lock(&client);
	assert!(lock.is_valid());
	lock_announced(
		&client,
		&LockEventType::LockReleased(announcement_of_uuid(HELD, lock.lease.uuid, "lease-a")),
		2,
	);
	assert!(!lock.is_valid());
	assert!(!lock.lease.is_lost(), "only the server's answer loses it");
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn a_timeout_of_a_held_lease_announced_makes_it_unconfirmed() {
	let client = offline_client();
	let lock = held_lock(&client);
	lock_announced(
		&client,
		&LockEventType::LockTimeout(announcement_of_uuid(HELD, lock.lease.uuid, "lease-a")),
		2,
	);
	assert!(!lock.is_valid());
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn another_lease_of_a_held_resource_announced_makes_it_unconfirmed() {
	let client = offline_client();
	let lock = held_lock(&client);
	lock_announced(
		&client,
		&LockEventType::LockAcquired(announcement(HELD, 1, "lease-b")),
		3,
	);
	assert!(!lock.is_valid());
	assert_eq!(*client.pending_lock_releases.0.borrow(), 0);
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn the_lease_of_a_held_lock_announced_acquired_says_nothing() {
	let client = offline_client();
	let lock = held_lock(&client);
	lock_announced(
		&client,
		&LockEventType::LockAcquired(announcement_of_uuid(HELD, lock.lease.uuid, "lease-a")),
		1,
	);
	assert!(lock.is_valid());
	assert_eq!(*client.pending_lock_releases.0.borrow(), 0);
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn announcements_for_another_resource_say_nothing_of_a_held_lock() {
	let client = offline_client();
	let lock = held_lock(&client);
	lock_announced(
		&client,
		&LockEventType::LockAcquired(announcement(DRIVE, 1, "lease-b")),
		3,
	);
	lock_announced(
		&client,
		&LockEventType::LockReleased(announcement_of_uuid(DRIVE, lock.lease.uuid, "lease-a")),
		4,
	);
	assert!(lock.is_valid());
}

/// An announcement that arrives twice raises its suspicion once: one refresh confirmed after the
/// first answers both.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn an_announcement_applied_twice_raises_one_suspicion() {
	let client = offline_client();
	let lock = held_lock(&client);
	let foreign = LockEventType::LockAcquired(announcement(HELD, 1, "lease-b"));
	lock_announced(&client, &foreign, 3);
	let mark = lock.lease.suspicion.mark();
	lock_announced(&client, &foreign, 3);
	lock.lease.suspicion.answered(mark);
	assert!(lock.is_valid());
}

/// A lease announced acquired for a uuid this client let go was granted late: it is released
/// once, however often the announcement arrives, and counted in the pending releases until that
/// release is given up on.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn a_late_grant_of_a_retired_uuid_is_released_once() {
	let client = offline_client();
	let uuid = Uuid::from_u128(1);
	client.lock_board.retire(uuid, &Arc::from(DRIVE));
	let late = acquired("lease-a", 1);

	lock_announced(&client, &late, 5);
	assert_eq!(*client.pending_lock_releases.0.borrow(), 1);
	lock_announced(&client, &late, 5);
	assert_eq!(*client.pending_lock_releases.0.borrow(), 1);

	client.pending_lock_releases.settled().await;
	assert!(client.lock_board.is_retired(DRIVE, uuid));
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn a_lease_of_a_uuid_retired_on_another_resource_is_not_released() {
	let client = offline_client();
	client
		.lock_board
		.retire(Uuid::from_u128(1), &Arc::from("notes-write"));
	lock_announced(&client, &acquired("lease-a", 1), 5);
	assert_eq!(*client.pending_lock_releases.0.borrow(), 0);
}

/// A lease that was announced ended before its `lockAcquired` arrived is over: its late grant
/// needs no release.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[tokio::test(start_paused = true)]
async fn an_ended_lease_of_a_retired_uuid_is_not_released() {
	let client = offline_client();
	client
		.lock_board
		.retire(Uuid::from_u128(1), &Arc::from(DRIVE));
	lock_announced(&client, &released("lease-a", 1), 6);
	lock_announced(&client, &acquired("lease-a", 1), 5);
	assert_eq!(*client.pending_lock_releases.0.borrow(), 0);
}

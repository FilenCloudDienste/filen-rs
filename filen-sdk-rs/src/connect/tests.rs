use std::borrow::Cow;

use filen_types::{auth::MetaEncryptionVersion, crypto::LinkHashedPassword};

use super::*;

fn test_meta_key() -> MetaKey {
	MetaKey::from_str_and_version(
		"0123456789abcdefghijklmnopqrstuv",
		MetaEncryptionVersion::V2,
	)
	.unwrap()
}

// An owner reading a password-protected dir link back from the server holds the accepted
// password only as a `Hashed` credential. Converting the read-write link into the listing
// link must keep that hash so `get_password_hash` sends it instead of "empty" (which the
// server rejects with WrongPassword).
#[test]
fn dir_public_link_rw_to_dir_public_link_preserves_hashed_password() {
	let hashed = LinkHashedPassword(Cow::Owned("deadbeefhash".to_string()));
	let rw = DirPublicLinkRW {
		link_uuid: Uuid::new_v4(),
		link_key: Some(test_meta_key()),
		password: PasswordState::Hashed(hashed),
		expiration: PublicLinkExpiration::Never,
		enable_download: true,
		salt: LinkPasswordSalt::None,
	};

	let link = DirPublicLink::try_from(rw).expect("conversion should succeed");

	assert_eq!(
		link.get_password_hash().unwrap().0,
		Cow::Borrowed("deadbeefhash"),
		"hashed credential must survive the conversion instead of deriving to \"empty\""
	);
}

fn test_dir(name: &str) -> NonRootItemType<'static, Normal> {
	let now = chrono::Utc::now();
	let (uuid, meta) = RemoteDirectory::make_parts(name, now).unwrap();
	NonRootItemType::Dir(Cow::Owned(RemoteDirectory::new_from_parts(
		uuid,
		meta,
		Uuid::new_v4().into(),
		now,
	)))
}

fn test_link() -> ConnectedLink {
	ConnectedLink {
		link: ListedPublicLink {
			link_uuid: Uuid::new_v4(),
			link_key: filen_types::crypto::EncryptedMetaKey(filen_types::crypto::EncryptedString(
				Cow::Borrowed("encrypted"),
			)),
		},
		crypter: test_meta_key(),
	}
}

#[test]
fn targets_are_empty_only_without_links_and_users() {
	assert!(ConnectedTargets::default().is_empty());
	let with_link = ConnectedTargets {
		links: vec![test_link()],
		users: Vec::new(),
	};
	assert!(!with_link.is_empty());
	assert!(!ConnectedTargets::with_test_users(1).is_empty());
}

#[test]
fn without_keeps_only_new_links_and_users() {
	let link = test_link();
	let old = ConnectedTargets {
		links: vec![link.clone()],
		users: ConnectedTargets::with_test_users(1).users,
	};
	let new = ConnectedTargets {
		links: vec![link, test_link()],
		users: ConnectedTargets::with_test_users(2).users,
	};
	let added = new.without(&old);
	assert_eq!(added.links.len(), 1);
	assert_eq!(added.links[0].link.link_uuid, new.links[1].link.link_uuid);
	assert_eq!(added.users.iter().map(|u| u.id).collect::<Vec<_>>(), [2]);
	assert!(old.without(&new).is_empty());
}

#[test]
fn no_targets_means_no_propagation_operations() {
	let items = [test_dir("a"), test_dir("b")];
	assert_eq!(ConnectedTargets::default().operations(&items).count(), 0);
}

#[test]
fn operations_cover_every_link_and_user_for_every_item() {
	let items = [test_dir("a"), test_dir("b"), test_dir("c")];
	let [a, b, c] = items.each_ref().map(HasUUID::uuid);
	let targets = ConnectedTargets {
		links: vec![test_link(), test_link()],
		users: ConnectedTargets::with_test_users(1).users,
	};
	let [l0, l1] = [0, 1].map(|i| Some(targets.links[i].link.link_uuid));

	let pairs = targets
		.operations(&items)
		.map(|op| match op {
			PropagationOp::Link { link, item } => (Some(link.link.link_uuid), None, item.uuid()),
			PropagationOp::Share { user, item } => (None, Some(user.id), item.uuid()),
		})
		.collect::<Vec<_>>();
	assert_eq!(
		pairs,
		[
			(l0, None, a),
			(l0, None, b),
			(l0, None, c),
			(l1, None, a),
			(l1, None, b),
			(l1, None, c),
			(None, Some(1), a),
			(None, Some(1), b),
			(None, Some(1), c),
		],
		"links first, then users; item order kept"
	);
}

// A single failing propagation (e.g. one undecryptable link key) must not abort the
// updates to the remaining links/receivers: every future is awaited and all errors are
// collected rather than returning on the first one.
#[tokio::test]
async fn drain_collecting_errors_awaits_all_and_aggregates_failures() {
	let futures: FuturesUnordered<MaybeSendBoxFuture<'_, Result<(), Error>>> =
		FuturesUnordered::new();
	futures.push(Box::pin(async { Ok(()) }) as MaybeSendBoxFuture<'_, Result<(), Error>>);
	futures.push(
		Box::pin(async { Err(Error::custom(ErrorKind::Conversion, "first")) })
			as MaybeSendBoxFuture<'_, Result<(), Error>>,
	);
	futures.push(
		Box::pin(async { Err(Error::custom(ErrorKind::Conversion, "second")) })
			as MaybeSendBoxFuture<'_, Result<(), Error>>,
	);

	let errors = drain_collecting_errors(futures).await;

	assert_eq!(
		errors.len(),
		2,
		"both failures must be collected — draining must not abort on the first error"
	);
}

/// Locks built inside a runtime that is never driven, so dropping one queues its release
/// and nothing leaves the process.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
mod held_targets {
	use std::sync::Arc;

	use filen_types::fs::Uuid;

	use super::super::{ConnectedTargets, HeldTargets};
	use crate::{
		auth::test_support::offline_client,
		sync::lock::{
			ResourceLock,
			test_support::{held_lock, lose, undriven_runtime},
		},
	};

	const DIR: Uuid = Uuid::from_u128(1);

	/// The generation a lookup that missed must keep its answer under.
	fn miss(held: &mut HeldTargets, hold: &Arc<ResourceLock>) -> u64 {
		held.cached(hold, DIR)
			.err()
			.expect("nothing is kept for DIR yet")
	}

	fn users_kept(held: &mut HeldTargets, hold: &Arc<ResourceLock>) -> Option<usize> {
		held.cached(hold, DIR)
			.ok()
			.map(|lookup| lookup.targets.users.len())
	}

	#[test]
	fn a_hold_answers_the_targets_it_kept() {
		let rt = undriven_runtime();
		let lock = held_lock(&rt);
		let mut held = HeldTargets::default();
		let generation = miss(&mut held, &lock);
		held.keep(
			&lock,
			DIR,
			Arc::new(ConnectedTargets::with_test_users(2)),
			generation,
		);
		assert_eq!(users_kept(&mut held, &lock), Some(2));
	}

	#[test]
	fn a_new_hold_answers_nothing_an_earlier_one_kept() {
		let rt = undriven_runtime();
		let first = held_lock(&rt);
		let second = held_lock(&rt);
		let mut held = HeldTargets::default();
		let generation = miss(&mut held, &first);
		held.keep(
			&first,
			DIR,
			Arc::new(ConnectedTargets::with_test_users(2)),
			generation,
		);
		assert_eq!(users_kept(&mut held, &second), None);
		// Switching to the second hold cleared what the first one kept.
		assert_eq!(users_kept(&mut held, &first), None);
	}

	#[test]
	fn a_lost_hold_neither_answers_nor_keeps() {
		let rt = undriven_runtime();
		let lock = held_lock(&rt);
		let mut held = HeldTargets::default();
		let generation = miss(&mut held, &lock);
		held.keep(
			&lock,
			DIR,
			Arc::new(ConnectedTargets::with_test_users(2)),
			generation,
		);
		lose(&lock);
		// Another device may hold the drive now: what was kept is not answered...
		assert_eq!(users_kept(&mut held, &lock), None);
		// ...and a lookup that began before the loss keeps nothing.
		held.dirs.clear();
		held.keep(
			&lock,
			DIR,
			Arc::new(ConnectedTargets::with_test_users(2)),
			generation,
		);
		assert!(held.dirs.is_empty());
	}

	#[test]
	fn a_lookup_begun_before_a_change_keeps_nothing() {
		let rt = undriven_runtime();
		let lock = held_lock(&rt);
		let mut held = HeldTargets::default();
		let generation = miss(&mut held, &lock);
		held.forget();
		held.keep(
			&lock,
			DIR,
			Arc::new(ConnectedTargets::with_test_users(2)),
			generation,
		);
		assert_eq!(users_kept(&mut held, &lock), None);
	}

	#[test]
	fn a_change_forgets_when_it_ends() {
		let rt = undriven_runtime();
		let lock = held_lock(&rt);
		let client = offline_client();
		{
			let mut held = client.lock_held_targets();
			let generation = miss(&mut held, &lock);
			held.keep(
				&lock,
				DIR,
				Arc::new(ConnectedTargets::with_test_users(2)),
				generation,
			);
			assert_eq!(users_kept(&mut held, &lock), Some(2));
		}
		std::mem::drop(client.connected_targets_change());
		assert_eq!(users_kept(&mut client.lock_held_targets(), &lock), None);
	}

	#[test]
	fn a_running_change_neither_answers_nor_keeps() {
		let rt = undriven_runtime();
		let lock = held_lock(&rt);
		let client = offline_client();
		// A lookup that missed before the change and one that misses during it.
		let before = miss(&mut client.lock_held_targets(), &lock);
		let change = client.connected_targets_change();
		let during = miss(&mut client.lock_held_targets(), &lock);
		{
			let mut held = client.lock_held_targets();
			held.keep(
				&lock,
				DIR,
				Arc::new(ConnectedTargets::with_test_users(2)),
				before,
			);
			held.keep(
				&lock,
				DIR,
				Arc::new(ConnectedTargets::with_test_users(2)),
				during,
			);
			assert!(
				held.dirs.is_empty(),
				"nothing is kept while the change runs"
			);
			// An entry already there is not answered either.
			held.dirs
				.insert(DIR, Arc::new(ConnectedTargets::with_test_users(2)));
			assert_eq!(users_kept(&mut held, &lock), None);
		}
		std::mem::drop(change);
		assert_eq!(users_kept(&mut client.lock_held_targets(), &lock), None);
	}

	#[cfg(feature = "sync-engine")]
	#[test]
	fn an_ended_hold_forgets_what_it_kept_and_a_shared_one_does_not() {
		let rt = undriven_runtime();
		let lock = held_lock(&rt);
		let client = offline_client();
		client.keep_held_targets(&lock, DIR, ConnectedTargets::with_test_users(2));
		// `lock` still holds it.
		client.forget_ended_hold_targets();
		assert!(client.kept_held_targets(&lock, DIR).is_some());
		std::mem::drop(lock);
		client.forget_ended_hold_targets();
		assert!(client.lock_held_targets().dirs.is_empty());
	}

	#[cfg(feature = "sync-engine")]
	#[test]
	fn a_directory_inherits_what_a_kept_lookup_of_its_parent_answered() {
		const CHILD: Uuid = Uuid::from_u128(2);
		let rt = undriven_runtime();
		let lock = held_lock(&rt);
		let client = offline_client();
		client.keep_held_targets(&lock, DIR, ConnectedTargets::with_test_users(2));
		let lookup = client
			.lock_held_targets()
			.cached(&lock, DIR)
			.expect("DIR's targets are kept");
		client.inherit_connected_targets(&lock, CHILD, &lookup);
		let child = client.kept_held_targets(&lock, CHILD);
		assert_eq!(child.map(|targets| targets.users.len()), Some(2));
	}

	#[cfg(feature = "sync-engine")]
	#[test]
	fn a_directory_inherits_nothing_from_a_lookup_begun_before_a_change() {
		const CHILD: Uuid = Uuid::from_u128(2);
		let rt = undriven_runtime();
		let lock = held_lock(&rt);
		let client = offline_client();
		client.keep_held_targets(&lock, DIR, ConnectedTargets::with_test_users(2));
		let lookup = client
			.lock_held_targets()
			.cached(&lock, DIR)
			.expect("DIR's targets are kept");
		std::mem::drop(client.connected_targets_change());
		client.inherit_connected_targets(&lock, CHILD, &lookup);
		assert!(client.kept_held_targets(&lock, CHILD).is_none());
	}
}

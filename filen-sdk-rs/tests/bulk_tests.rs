use std::{borrow::Cow, fmt::Debug, slice, sync::Arc};

use chrono::Utc;
use filen_macros::shared_test_runtime;
use filen_sdk_rs::{
	Error, ErrorKind,
	auth::Client,
	chats::Chat,
	connect::PublicLinkSharedClientExt,
	fs::{
		HasMetaExt, HasName, HasParent, HasUUID, NewDir, NewDirParent, dir::RemoteDirectory,
		file::RemoteFile,
	},
	notes::{Note, NoteTag},
	sync::lock::ResourceLock,
};
use filen_types::{
	api::v3::{
		bulk::{
			FileOrDir, chat as bulk_chat,
			dir::{color, link::add},
			exists, favorite,
			file::stable,
			linked, notes as bulk_notes, present, rename, restore, share, shared,
		},
		chat,
		contacts::Contact,
		dir::{self, color::DirColor, link::PublicLinkExpiration},
		file, notes,
	},
	crypto::rsa::RSAEncryptedString,
	fs::{ObjectType, ParentUuid, StableUuid, Uuid},
	traits::CowHelpers,
};
use serde::{Serialize, de::DeserializeOwned};

mod drive_helpers;

fn new_dir(name: &str, parent: NewDirParent) -> NewDir<'_> {
	NewDir {
		name,
		created: Utc::now(),
		parent,
	}
}

#[shared_test_runtime]
async fn create_dirs_puts_children_under_the_uuids_their_parents_were_answered() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let results = client
		.create_dirs_for_test(&[
			new_dir("a", NewDirParent::Existing(test_dir.uuid())),
			new_dir("b", NewDirParent::Earlier(0)),
			new_dir("c", NewDirParent::Earlier(1)),
		])
		.await
		.unwrap();
	let [a, b, c] = results
		.into_iter()
		.map(Result::unwrap)
		.collect::<Vec<_>>()
		.try_into()
		.unwrap();
	assert!(!a.merged && !b.merged && !c.merged, "{a:?} {b:?} {c:?}");

	for (created, name, parent) in [
		(a, "a", test_dir.uuid()),
		(b, "b", a.uuid),
		(c, "c", b.uuid),
	] {
		let found = client.get_dir(created.uuid).await.unwrap();
		assert_eq!(found.name(), Some(name));
		assert_eq!(
			*found.parent(),
			ParentUuid::Uuid(parent),
			"parent of {name}"
		);
	}
}

#[shared_test_runtime]
async fn create_dirs_merges_an_existing_dir_and_creates_its_children_in_it() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let existing = client.create_dir(&test_dir.into(), "x").await.unwrap();

	let results = client
		.create_dirs_for_test(&[
			new_dir("x", NewDirParent::Existing(test_dir.uuid())),
			new_dir("y", NewDirParent::Earlier(0)),
		])
		.await
		.unwrap();
	let [x, y] = results
		.into_iter()
		.map(Result::unwrap)
		.collect::<Vec<_>>()
		.try_into()
		.unwrap();
	assert_eq!((x.uuid, x.merged), (existing.uuid(), true));
	assert!(!y.merged, "{y:?}");

	let found = client.get_dir(y.uuid).await.unwrap();
	assert_eq!(found.name(), Some("y"));
	assert_eq!(*found.parent(), ParentUuid::Uuid(existing.uuid()));
	let (dirs, _) = client
		.list_dir(&test_dir.into(), None::<&fn(u64, Option<u64>)>)
		.await
		.unwrap();
	assert_eq!(dir_uuids(&dirs), [existing.uuid()]);
}

#[shared_test_runtime]
async fn trash_items_trashes_the_topmost_dirs_it_is_given() {
	// Without the drive lock held across the test: the trash lock below is taken before the drive
	// lock everywhere else (`empty_trash` locks the drive), so holding both in the other order
	// could deadlock with another leg. Each bulk call locks the drive itself.
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let created = client
		.create_dirs_for_test(&[
			new_dir("kept", NewDirParent::Existing(test_dir.uuid())),
			new_dir("p", NewDirParent::Existing(test_dir.uuid())),
			new_dir("p_child", NewDirParent::Earlier(1)),
			new_dir("q", NewDirParent::Existing(test_dir.uuid())),
		])
		.await
		.unwrap()
		.into_iter()
		.map(|result| result.unwrap().uuid)
		.collect::<Vec<_>>();
	let [kept, p, _, q] = created.try_into().unwrap();

	// Hold the trash lock across trash -> get_dir: another leg's account-global empty-trash
	// (serialized on this lock) would permanently delete the dirs before they are looked up.
	let _trash_lock = client
		.acquire_lock_with_default(test_utils::locks::TRASH)
		.await
		.unwrap();
	let results = client
		.trash_items_for_test(&[
			FileOrDir::Dir(dir::trash::Request { uuid: p }),
			FileOrDir::Dir(dir::trash::Request { uuid: q }),
		])
		.await
		.unwrap();
	assert_eq!(results.len(), 2);
	for result in results {
		result.unwrap();
	}

	for uuid in [p, q] {
		let found = client.get_dir(uuid).await.unwrap();
		assert_eq!(*found.parent(), ParentUuid::Trash(test_dir.uuid()));
	}
	let (dirs, _) = client
		.list_dir(&test_dir.into(), None::<&fn(u64, Option<u64>)>)
		.await
		.unwrap();
	assert_eq!(dir_uuids(&dirs), [kept]);
}

/// Dirs `a`, `b` and `s` and the file `f.txt` in `test_dir`.
async fn move_fixtures(client: &Client, test_dir: &RemoteDirectory) -> ([Uuid; 3], RemoteFile) {
	let dirs = client
		.create_dirs_for_test(&[
			new_dir("a", NewDirParent::Existing(test_dir.uuid())),
			new_dir("b", NewDirParent::Existing(test_dir.uuid())),
			new_dir("s", NewDirParent::Existing(test_dir.uuid())),
		])
		.await
		.unwrap()
		.into_iter()
		.map(|result| result.unwrap().uuid)
		.collect::<Vec<_>>();
	let file = drive_helpers::upload(client, test_dir, "f.txt", b"f").await;
	(dirs.try_into().unwrap(), file)
}

#[shared_test_runtime]
async fn move_items_moves_files_and_dirs_into_their_destinations() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let ([a, b, s], f) = move_fixtures(client, test_dir).await;

	let results = client
		.move_items_for_test(&[
			FileOrDir::File(file::r#move::Request {
				uuid: f.uuid(),
				new_parent: a,
			}),
			FileOrDir::Dir(dir::r#move::Request { uuid: s, to: b }),
		])
		.await
		.unwrap();
	assert_eq!(results.len(), 2);
	for result in results {
		result.unwrap();
	}

	let found = client.get_file(f.uuid()).await.unwrap();
	assert_eq!(*found.parent(), ParentUuid::Uuid(a));
	let found = client.get_dir(s).await.unwrap();
	assert_eq!(*found.parent(), ParentUuid::Uuid(b));
	let (dirs, files) = client
		.list_dir(&test_dir.into(), None::<&fn(u64, Option<u64>)>)
		.await
		.unwrap();
	assert_eq!(names(&files), Vec::<&str>::new());
	assert_eq!(names(&dirs), ["a", "b"]);
}

#[shared_test_runtime]
async fn move_items_fails_only_the_item_the_server_refuses() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let ([a, b, s], f) = move_fixtures(client, test_dir).await;

	let results = client
		.move_items_for_test(&[
			FileOrDir::File(file::r#move::Request {
				uuid: f.uuid(),
				new_parent: a,
			}),
			FileOrDir::Dir(dir::r#move::Request { uuid: a, to: a }),
			FileOrDir::Dir(dir::r#move::Request { uuid: s, to: b }),
		])
		.await
		.unwrap();
	let [moved_file, into_itself, moved_dir] = results.try_into().unwrap();
	moved_file.unwrap();
	assert_eq!(
		into_itself.unwrap_err().server_code().as_deref(),
		Some("cannot_move_folder_into_itself")
	);
	moved_dir.unwrap();

	let found = client.get_file(f.uuid()).await.unwrap();
	assert_eq!(*found.parent(), ParentUuid::Uuid(a));
	let found = client.get_dir(s).await.unwrap();
	assert_eq!(*found.parent(), ParentUuid::Uuid(b));
	let found = client.get_dir(a).await.unwrap();
	assert_eq!(*found.parent(), ParentUuid::Uuid(test_dir.uuid()));
}

#[shared_test_runtime]
async fn delete_items_permanently_deletes_files_and_dirs() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let mut d = client.create_dir(&test_dir.into(), "d").await.unwrap();
	let f = drive_helpers::upload(client, test_dir, "f.txt", b"f").await;

	let results = client
		.delete_items_permanently_for_test(&[
			FileOrDir::File(file::delete::permanent::Request { uuid: f.uuid() }),
			FileOrDir::Dir(dir::delete::permanent::Request { uuid: d.uuid() }),
		])
		.await
		.unwrap();
	assert_eq!(results.len(), 2);
	for result in results {
		result.unwrap();
	}

	assert_eq!(
		client.get_file(f.uuid()).await.unwrap_err().kind(),
		ErrorKind::FileNotFound
	);
	// Until the server purges a deleted folder, `v3/dir` still answers it as trashed, as after a
	// single `dir/delete/permanent`, so the purge is not something to wait for. Restoring tells it
	// apart from a trashed folder (restored) and from one left alone (`folder_not_in_trash`).
	let restored = client.restore_dir(&mut d).await.unwrap_err();
	assert_eq!(
		(restored.kind(), restored.server_code().as_deref()),
		(ErrorKind::FolderNotFound, Some("folder_not_found"))
	);
	let (dirs, files) = client
		.list_dir(&test_dir.into(), None::<&fn(u64, Option<u64>)>)
		.await
		.unwrap();
	assert_eq!((dirs.len(), files.len()), (0, 0));
}

#[shared_test_runtime]
async fn update_metadata_renames_files_and_dirs() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let a = drive_helpers::upload(client, test_dir, "a.txt", b"a").await;
	let b = client.create_dir(&test_dir.into(), "b").await.unwrap();

	let items = [
		client
			.metadata_item_for_test(FileOrDir::File(&a), "a2.txt")
			.await
			.unwrap(),
		client
			.metadata_item_for_test(FileOrDir::Dir(&b), "b2")
			.await
			.unwrap(),
	];
	let results = client.update_metadata_for_test(&items).await.unwrap();
	assert_eq!(results.len(), 2);
	for result in results {
		result.unwrap();
	}

	// Each name decrypts from what the server stored, the only check that the item's
	// encryption is right.
	let found = client.get_file(a.uuid()).await.unwrap();
	assert_eq!(
		(found.uuid(), found.name(), found.parent()),
		(a.uuid(), Some("a2.txt"), &ParentUuid::Uuid(test_dir.uuid()))
	);
	let found = client.get_dir(b.uuid()).await.unwrap();
	assert_eq!(
		(found.uuid(), found.name(), found.parent()),
		(b.uuid(), Some("b2"), &ParentUuid::Uuid(test_dir.uuid()))
	);
}

#[shared_test_runtime]
async fn trash_items_trashes_files_and_dirs_in_one_request() {
	// Without the drive lock held, for the reason trash_items_trashes_the_topmost_dirs_it_is_given
	// gives.
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let d = client
		.create_dirs_for_test(&[new_dir("d", NewDirParent::Existing(test_dir.uuid()))])
		.await
		.unwrap()
		.remove(0)
		.unwrap()
		.uuid;
	let f = drive_helpers::upload(client, test_dir, "f.txt", b"f").await;

	// Held across trash -> get_*: another leg's empty-trash would delete the items first.
	let _trash_lock = client
		.acquire_lock_with_default(test_utils::locks::TRASH)
		.await
		.unwrap();
	let results = client
		.trash_items_for_test(&[
			FileOrDir::File(file::trash::Request { uuid: f.uuid() }),
			FileOrDir::Dir(dir::trash::Request { uuid: d }),
		])
		.await
		.unwrap();
	assert_eq!(results.len(), 2);
	for result in results {
		result.unwrap();
	}

	let found = client.get_file(f.uuid()).await.unwrap();
	assert_eq!(*found.parent(), ParentUuid::Trash(test_dir.uuid()));
	let found = client.get_dir(d).await.unwrap();
	assert_eq!(*found.parent(), ParentUuid::Trash(test_dir.uuid()));
	let (dirs, files) = client
		.list_dir(&test_dir.into(), None::<&fn(u64, Option<u64>)>)
		.await
		.unwrap();
	assert_eq!((dirs.len(), files.len()), (0, 0));
}

#[shared_test_runtime]
async fn create_dirs_fails_only_a_dir_named_like_a_file_in_its_parent() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	drive_helpers::upload(client, test_dir, "x", b"x").await;

	let results = client
		.create_dirs_for_test(&[
			new_dir("x", NewDirParent::Existing(test_dir.uuid())),
			new_dir("y", NewDirParent::Existing(test_dir.uuid())),
		])
		.await
		.unwrap();
	let [x, y] = results.try_into().unwrap();
	assert_eq!(
		x.unwrap_err().server_code().as_deref(),
		Some("file_with_same_name_exists_in_parent")
	);
	let y = y.unwrap();
	assert!(!y.merged, "{y:?}");

	let (dirs, files) = client
		.list_dir(&test_dir.into(), None::<&fn(u64, Option<u64>)>)
		.await
		.unwrap();
	assert_eq!(dir_uuids(&dirs), [y.uuid]);
	assert_eq!(names(&files), ["x"]);
}

/// The share account as a contact of the test account, with both accounts' contact lock, which
/// the caller holds for the whole test and takes before the drive lock, as the share tests in
/// connect_tests do.
async fn contact(
	client: &Client,
	share_client: &Client,
) -> (Contact<'static>, [Arc<ResourceLock>; 2]) {
	let locks = [
		client
			.acquire_lock_with_default(test_utils::locks::CONTACT)
			.await
			.unwrap(),
		share_client
			.acquire_lock_with_default(test_utils::locks::CONTACT)
			.await
			.unwrap(),
	];
	if let Some(contact) = find_contact(client, share_client.email()).await {
		return (contact, locks);
	}
	// Only when the accounts are not contacts yet: between contacts the server answers a request
	// with the existing contact's uuid, which is no request to accept.
	test_utils::add_contact(client, share_client).await;
	let contact = find_contact(client, share_client.email()).await.unwrap();
	(contact, locks)
}

async fn find_contact(client: &Client, email: &str) -> Option<Contact<'static>> {
	client
		.get_contacts()
		.await
		.unwrap()
		.into_iter()
		.find(|contact| contact.email == email)
}

/// The dir "shared" in `test_dir` and the file "f.txt" in it.
async fn share_fixtures(
	client: &Client,
	test_dir: &RemoteDirectory,
) -> (RemoteDirectory, RemoteFile) {
	let d = client.create_dir(&test_dir.into(), "shared").await.unwrap();
	let f = drive_helpers::upload(client, &d, "f.txt", b"f").await;
	(d, f)
}

/// How many of `client`'s outgoing shares are `uuid`. Other legs share from the share account
/// outside the contact lock, so only rows for this test's dir can be counted.
async fn out_shared_count(client: &Client, uuid: Uuid) -> usize {
	let (dirs, _) = client
		.list_out_shared::<fn(u64, Option<u64>)>(None, None)
		.await
		.unwrap();
	dirs.iter()
		.filter(|dir| dir.get_dir().uuid() == uuid)
		.count()
}

#[shared_test_runtime]
async fn share_items_shares_a_dir_and_its_file_with_the_receiver() {
	let client = test_utils::RESOURCES.client().await;
	let share_client = test_utils::SHARE_RESOURCES.client().await;
	let (contact, _contact_locks) = contact(&client, &share_client).await;
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let (d, f) = share_fixtures(&client, &resources.dir).await;

	let items = [
		share::Item {
			uuid: d.uuid(),
			parent: None,
			r#type: ObjectType::Dir,
			metadata: d.get_rsa_encrypted_meta(&contact.public_key).await.unwrap(),
		},
		share::Item {
			uuid: f.uuid(),
			parent: Some(d.uuid()),
			r#type: ObjectType::File,
			metadata: f.get_rsa_encrypted_meta(&contact.public_key).await.unwrap(),
		},
	];
	let results = client
		.share_items_for_test(&share::Request {
			email: contact.email.as_borrowed_cow(),
			items: &items,
		})
		.await
		.unwrap();
	assert_eq!(results.len(), 2);
	for result in results {
		result.unwrap();
	}

	assert_eq!(out_shared_count(&client, d.uuid()).await, 1);
	// The names decrypt on the receiver only if each item's metadata was encrypted with its key.
	let (dirs_in, _) = share_client
		.list_in_shared_root::<fn(u64, Option<u64>)>(None)
		.await
		.unwrap();
	let shared_in = dirs_in
		.iter()
		.filter(|dir| dir.get_dir().uuid() == d.uuid())
		.collect::<Vec<_>>();
	let [shared_in] = shared_in.as_slice() else {
		panic!("the receiver lists {} rows for the dir", shared_in.len());
	};
	assert_eq!(shared_in.get_dir().name(), Some("shared"));
	let (_, files_in) = share_client
		.list_shared_dir::<fn(u64, Option<u64>)>(
			&(*shared_in).into(),
			shared_in.sharing_role(),
			None,
		)
		.await
		.unwrap();
	assert_eq!(names(&files_in), ["f.txt"]);
}

#[shared_test_runtime]
async fn share_items_fails_only_an_item_that_does_not_exist() {
	let client = test_utils::RESOURCES.client().await;
	let share_client = test_utils::SHARE_RESOURCES.client().await;
	let (contact, _contact_locks) = contact(&client, &share_client).await;
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let (d, f) = share_fixtures(&client, &resources.dir).await;

	let d_meta = d.get_rsa_encrypted_meta(&contact.public_key).await.unwrap();
	let items = [
		share::Item {
			uuid: d.uuid(),
			parent: None,
			r#type: ObjectType::Dir,
			// cloned: the missing dir below reuses it
			metadata: d_meta.clone(),
		},
		share::Item {
			uuid: Uuid::new_v4(),
			parent: None,
			r#type: ObjectType::Dir,
			metadata: d_meta,
		},
		share::Item {
			uuid: f.uuid(),
			parent: Some(d.uuid()),
			r#type: ObjectType::File,
			metadata: f.get_rsa_encrypted_meta(&contact.public_key).await.unwrap(),
		},
	];
	let results = client
		.share_items_for_test(&share::Request {
			email: contact.email.as_borrowed_cow(),
			items: &items,
		})
		.await
		.unwrap();
	let [shared_dir, missing, shared_file] = results.try_into().unwrap();
	shared_dir.unwrap();
	let missing = missing.unwrap_err();
	assert_eq!(
		(missing.kind(), missing.server_code().as_deref()),
		(ErrorKind::FolderNotFound, Some("folder_not_found"))
	);
	shared_file.unwrap();

	assert_eq!(out_shared_count(&client, d.uuid()).await, 1);
}

#[shared_test_runtime]
async fn shared_reports_who_each_item_is_shared_with() {
	let client = test_utils::RESOURCES.client().await;
	let share_client = test_utils::SHARE_RESOURCES.client().await;
	let (contact, _contact_locks) = contact(&client, &share_client).await;
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let (d, f) = share_fixtures(&client, &resources.dir).await;
	let u = client
		.create_dir(&(&resources.dir).into(), "u")
		.await
		.unwrap();
	client
		.share_dir::<fn(u64, Option<u64>)>(&d, &contact, None)
		.await
		.unwrap();

	let results = client
		.shared_for_test(&[
			shared::Item { uuid: d.uuid() },
			shared::Item { uuid: f.uuid() },
			shared::Item { uuid: u.uuid() },
			shared::Item {
				uuid: Uuid::new_v4(),
			},
		])
		.await
		.unwrap();
	let users = results
		.into_iter()
		.map(|result| {
			let response = result.unwrap();
			let users = response
				.users
				.into_iter()
				.map(|user| (user.id, user.email.into_owned()))
				.collect::<Vec<_>>();
			(response.sharing, users)
		})
		.collect::<Vec<_>>();
	let receiver = vec![(contact.user_id, share_client.email().to_owned())];
	assert_eq!(
		users,
		[
			(true, receiver.clone()),
			(true, receiver),
			(false, Vec::new()),
			(false, Vec::new()),
		]
	);
}

#[shared_test_runtime]
async fn linked_reports_the_links_each_item_is_in() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let d = client.create_dir(&test_dir.into(), "linked").await.unwrap();
	let f = drive_helpers::upload(client, &d, "f.txt", b"f").await;
	let u = client.create_dir(&test_dir.into(), "u").await.unwrap();
	let link = client
		.public_link_dir::<fn(u64, Option<u64>)>(&d, None)
		.await
		.unwrap();

	let results = client
		.linked_for_test(&[
			linked::Item { uuid: d.uuid() },
			linked::Item { uuid: f.uuid() },
			linked::Item { uuid: u.uuid() },
			linked::Item {
				uuid: Uuid::new_v4(),
			},
		])
		.await
		.unwrap();
	let links = results
		.into_iter()
		.map(|result| {
			let response = result.unwrap();
			let links = response
				.links
				.iter()
				.map(|link| link.link_uuid)
				.collect::<Vec<_>>();
			(response.link, links)
		})
		.collect::<Vec<_>>();
	assert_eq!(
		links,
		[
			(true, vec![link.uuid()]),
			(true, vec![link.uuid()]),
			(false, Vec::new()),
			(false, Vec::new()),
		]
	);
}

#[shared_test_runtime]
async fn add_to_dir_link_adds_items_to_an_existing_link() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	// Linked while empty, then filled by calls that do not add to links: the bulk dir create and
	// the bulk move.
	let d = client.create_dir(&test_dir.into(), "d").await.unwrap();
	let info = drive_helpers::dir_link_info(client, &d).await;
	let s = client
		.create_dirs_for_test(&[new_dir("s", NewDirParent::Existing(d.uuid()))])
		.await
		.unwrap()
		.remove(0)
		.unwrap()
		.uuid;
	let s = client.get_dir(s).await.unwrap();
	let f = drive_helpers::upload(client, test_dir, "f.txt", b"f").await;
	let moved = client
		.move_items_for_test(&[FileOrDir::File(file::r#move::Request {
			uuid: f.uuid(),
			new_parent: d.uuid(),
		})])
		.await
		.unwrap();
	for result in moved {
		result.unwrap();
	}
	let (dirs, files) = client
		.list_linked_dir::<fn(u64, Option<u64>)>(&(&info.root).into(), &info.link, None)
		.await
		.unwrap();
	assert_eq!((dirs.len(), files.len()), (0, 0));

	let listed = client
		.linked_for_test(&[linked::Item { uuid: d.uuid() }])
		.await
		.unwrap()
		.remove(0)
		.unwrap()
		.links
		.remove(0);
	assert_eq!(listed.link_uuid, *info.link.uuid());
	let crypter = info.link.crypter_for_test();
	let items = [
		add::Item {
			uuid: s.uuid(),
			parent: Some(d.uuid()),
			r#type: ObjectType::Dir,
			metadata: s.get_encrypted_meta(crypter).await.unwrap(),
		},
		add::Item {
			uuid: f.uuid(),
			parent: Some(d.uuid()),
			r#type: ObjectType::File,
			metadata: f.get_encrypted_meta(crypter).await.unwrap(),
		},
	];
	let results = client
		.add_to_dir_link_for_test(&add::Request {
			link_uuid: listed.link_uuid,
			key: listed.link_key,
			expiration: PublicLinkExpiration::Never,
			items: &items,
		})
		.await
		.unwrap();
	assert_eq!(results.len(), 2);
	for result in results {
		result.unwrap();
	}

	// The names decrypt only if each item's metadata was encrypted with the link's key.
	let (dirs, files) = client
		.list_linked_dir::<fn(u64, Option<u64>)>(&(&info.root).into(), &info.link, None)
		.await
		.unwrap();
	assert_eq!(names(&dirs), ["s"]);
	assert_eq!(names(&files), ["f.txt"]);
}

#[shared_test_runtime]
async fn get_files_by_stable_uuid_resolves_each_lineage() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let a = drive_helpers::upload(client, test_dir, "a.txt", b"a").await;
	let b = drive_helpers::upload(client, test_dir, "b.txt", b"b").await;

	let results = client
		.get_files_by_stable_uuid_for_test(&[
			stable::Item {
				stable_uuid: a.stable_uuid(),
			},
			stable::Item {
				stable_uuid: b.stable_uuid(),
			},
		])
		.await
		.unwrap();
	let [found_a, found_b] = results.try_into().unwrap();
	let (found_a, found_b) = (found_a.unwrap(), found_b.unwrap());
	assert_eq!((found_a.uuid(), found_a.name()), (a.uuid(), Some("a.txt")));
	assert_eq!((found_b.uuid(), found_b.name()), (b.uuid(), Some("b.txt")));
	assert_eq!(
		found_a,
		client
			.get_file_by_stable_uuid(a.stable_uuid())
			.await
			.unwrap()
	);
	assert_eq!(
		found_b,
		client
			.get_file_by_stable_uuid(b.stable_uuid())
			.await
			.unwrap()
	);
}

#[shared_test_runtime]
async fn get_files_by_stable_uuid_fails_only_an_unknown_lineage() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let a = drive_helpers::upload(client, test_dir, "a.txt", b"a").await;
	let b = drive_helpers::upload(client, test_dir, "b.txt", b"b").await;

	let results = client
		.get_files_by_stable_uuid_for_test(&[
			stable::Item {
				stable_uuid: a.stable_uuid(),
			},
			stable::Item {
				stable_uuid: StableUuid::new_for_test(Uuid::new_v4()),
			},
			stable::Item {
				stable_uuid: b.stable_uuid(),
			},
		])
		.await
		.unwrap();
	let [found_a, unknown, found_b] = results.try_into().unwrap();
	assert_eq!(found_a.unwrap().uuid(), a.uuid());
	let unknown = unknown.unwrap_err();
	assert_eq!(
		(unknown.kind(), unknown.server_code().as_deref()),
		(ErrorKind::FileNotFound, Some("file_not_found"))
	);
	assert_eq!(found_b.unwrap().uuid(), b.uuid());
}

#[shared_test_runtime]
async fn exists_finds_files_and_dirs_by_name() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let f = drive_helpers::upload(client, test_dir, "f.txt", b"f").await;
	let d = client.create_dir(&test_dir.into(), "d").await.unwrap();

	let item = |name: &str| exists::Item {
		name_hashed: Cow::Owned(client.hash_name(name)),
	};
	let items = [
		FileOrDir::File(item("f.txt")),
		FileOrDir::File(item("missing.txt")),
		FileOrDir::Dir(item("d")),
		FileOrDir::Dir(item("missing")),
	];
	let results = client
		.post_bulk_for_test::<_, file::exists::Response>(
			exists::ENDPOINT,
			&exists::Request {
				parent: test_dir.uuid(),
				items: &items,
			},
			&items,
		)
		.await
		.unwrap();
	let found = results
		.into_iter()
		.map(|result| result.unwrap().0)
		.collect::<Vec<_>>();
	assert_eq!(found, [Some(f.uuid()), None, Some(d.uuid()), None]);
}

#[shared_test_runtime]
async fn present_reports_what_each_uuid_is() {
	// Without get_resources_with_lock, for the reason trash_items_trashes_the_topmost_dirs_it_is_given
	// gives: the trash lock comes first, then the drive lock.
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	// Held to the end: another leg's empty-trash would delete the trashed file first.
	let _trash_lock = client
		.acquire_lock_with_default(test_utils::locks::TRASH)
		.await
		.unwrap();
	let _drive_lock = client.lock_drive().await.unwrap();

	let f = drive_helpers::upload(client, test_dir, "f.txt", b"f").await;
	let g = drive_helpers::upload(client, test_dir, "g.txt", b"g").await;
	let d = client.create_dir(&test_dir.into(), "d").await.unwrap();
	for result in client
		.trash_items_for_test(&[FileOrDir::File(file::trash::Request { uuid: g.uuid() })])
		.await
		.unwrap()
	{
		result.unwrap();
	}

	let items = [
		FileOrDir::File(present::Item { uuid: f.uuid() }),
		FileOrDir::File(present::Item { uuid: g.uuid() }),
		FileOrDir::Dir(present::Item { uuid: d.uuid() }),
		FileOrDir::File(present::Item {
			uuid: Uuid::new_v4(),
		}),
		FileOrDir::Dir(present::Item { uuid: f.uuid() }),
	];
	let results = client
		.post_bulk_for_test::<_, present::Data>(
			present::ENDPOINT,
			&present::Request { items: &items },
			&items,
		)
		.await
		.unwrap();
	let [found_f, found_g, found_d, unknown, file_as_dir] = results
		.into_iter()
		.map(Result::unwrap)
		.collect::<Vec<_>>()
		.try_into()
		.unwrap();
	assert!(
		matches!(
			found_f,
			present::Data::File { versioned: false, trash: false, uuid, stable_uuid }
				if uuid == f.uuid() && stable_uuid == f.stable_uuid()
		),
		"{found_f:?}"
	);
	assert!(
		matches!(
			found_g,
			present::Data::File { versioned: false, trash: true, uuid, .. } if uuid == g.uuid()
		),
		"{found_g:?}"
	);
	assert!(
		matches!(found_d, present::Data::Dir { trash: false }),
		"{found_d:?}"
	);
	assert!(matches!(unknown, present::Data::Missing), "{unknown:?}");
	assert!(
		matches!(file_as_dir, present::Data::Missing),
		"{file_as_dir:?}"
	);
}

#[shared_test_runtime]
async fn rename_renames_files_and_dirs() {
	// The generic seam takes no lock: the test holds the drive lock the single routes take.
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let a = drive_helpers::upload(client, test_dir, "a.txt", b"a").await;
	let b = client.create_dir(&test_dir.into(), "b").await.unwrap();

	let items = [
		client
			.metadata_item_for_test(FileOrDir::File(&a), "a2.txt")
			.await
			.unwrap(),
		client
			.metadata_item_for_test(FileOrDir::Dir(&b), "b2")
			.await
			.unwrap(),
	];
	let results = client
		.post_bulk_for_test::<_, ()>(rename::ENDPOINT, &rename::Request { items: &items }, &items)
		.await
		.unwrap();
	assert_eq!(results.len(), 2);
	for result in results {
		result.unwrap();
	}

	// Each name decrypts from what the server stored, the only check that the item's encryption
	// is right; the old name's hash no longer finds the file.
	assert_eq!(
		client.get_file(a.uuid()).await.unwrap().name(),
		Some("a2.txt")
	);
	assert_eq!(client.get_dir(b.uuid()).await.unwrap().name(), Some("b2"));
	assert_eq!(
		client.file_exists("a.txt", &test_dir.into()).await.unwrap(),
		None
	);
}

#[shared_test_runtime]
async fn restore_returns_trashed_items_to_their_parent() {
	// Without get_resources_with_lock, for the reason trash_items_trashes_the_topmost_dirs_it_is_given
	// gives: the trash lock comes first, then the drive lock.
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	// Held across trash -> restore: another leg's empty-trash would delete the items first.
	let _trash_lock = client
		.acquire_lock_with_default(test_utils::locks::TRASH)
		.await
		.unwrap();
	let _drive_lock = client.lock_drive().await.unwrap();

	let f = drive_helpers::upload(client, test_dir, "f.txt", b"f").await;
	let d = client.create_dir(&test_dir.into(), "d").await.unwrap();
	for result in client
		.trash_items_for_test(&[
			FileOrDir::File(file::trash::Request { uuid: f.uuid() }),
			FileOrDir::Dir(dir::trash::Request { uuid: d.uuid() }),
		])
		.await
		.unwrap()
	{
		result.unwrap();
	}

	let items = [
		FileOrDir::File(file::restore::Request { uuid: f.uuid() }),
		FileOrDir::Dir(dir::restore::Request { uuid: d.uuid() }),
	];
	let results = client
		.post_bulk_for_test::<_, ()>(
			restore::ENDPOINT,
			&restore::Request { items: &items },
			&items,
		)
		.await
		.unwrap();
	assert_eq!(results.len(), 2);
	for result in results {
		result.unwrap();
	}

	assert_eq!(
		*client.get_file(f.uuid()).await.unwrap().parent(),
		ParentUuid::Uuid(test_dir.uuid())
	);
	assert_eq!(
		*client.get_dir(d.uuid()).await.unwrap().parent(),
		ParentUuid::Uuid(test_dir.uuid())
	);
	let (dirs, files) = client
		.list_dir(&test_dir.into(), None::<&fn(u64, Option<u64>)>)
		.await
		.unwrap();
	assert_eq!(dir_uuids(&dirs), [d.uuid()]);
	assert_eq!(names(&files), ["f.txt"]);
}

/// The `bulk/favorite` item that sets `uuid` favorited.
fn favorite(uuid: Uuid, r#type: ObjectType) -> favorite::Item {
	favorite::Item {
		uuid,
		r#type,
		value: true,
	}
}

/// Each item's answer to one `bulk/favorite` request: the item echoed back, as
/// `uuid, type, value`.
async fn post_favorite(
	client: &Client,
	items: &[favorite::Item],
) -> Vec<Result<(Uuid, ObjectType, bool), Error>> {
	client
		// the single route answers the request as sent, so the echo has the item's type
		.post_bulk_for_test::<_, favorite::Item>(
			favorite::ENDPOINT,
			&favorite::Request { items },
			items,
		)
		.await
		.unwrap()
		.into_iter()
		.map(|result| result.map(|echo| (echo.uuid, echo.r#type, echo.value)))
		.collect()
}

async fn assert_favorited(client: &Client, f: Uuid, d: Uuid) {
	assert!(
		client.get_file(f).await.unwrap().favorited,
		"file favorited"
	);
	assert!(client.get_dir(d).await.unwrap().favorited, "dir favorited");
}

#[shared_test_runtime]
async fn favorite_sets_files_and_dirs_favorited() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let f = drive_helpers::upload(client, test_dir, "f.txt", b"f").await;
	let d = client.create_dir(&test_dir.into(), "d").await.unwrap();

	let results = post_favorite(
		client,
		&[
			favorite(f.uuid(), ObjectType::File),
			favorite(d.uuid(), ObjectType::Dir),
		],
	)
	.await;
	let echoes = results.into_iter().map(Result::unwrap).collect::<Vec<_>>();
	assert_eq!(
		echoes,
		[
			(f.uuid(), ObjectType::File, true),
			(d.uuid(), ObjectType::Dir, true)
		]
	);
	assert_favorited(client, f.uuid(), d.uuid()).await;
}

#[shared_test_runtime]
async fn favorite_fails_only_an_item_that_does_not_exist() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let f = drive_helpers::upload(client, test_dir, "f.txt", b"f").await;
	let d = client.create_dir(&test_dir.into(), "d").await.unwrap();

	let results = post_favorite(
		client,
		&[
			favorite(f.uuid(), ObjectType::File),
			favorite(Uuid::new_v4(), ObjectType::File),
			favorite(d.uuid(), ObjectType::Dir),
		],
	)
	.await;
	let [file, missing, dir] = results.try_into().unwrap();
	assert_eq!(file.unwrap(), (f.uuid(), ObjectType::File, true));
	assert_eq!(
		missing.unwrap_err().server_code().as_deref(),
		Some("file_not_found")
	);
	assert_eq!(dir.unwrap(), (d.uuid(), ObjectType::Dir, true));
	assert_favorited(client, f.uuid(), d.uuid()).await;
}

#[shared_test_runtime]
async fn dir_color_sets_each_dirs_color() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let mut uuids = Vec::new();
	for name in ["a", "b", "c"] {
		uuids.push(
			client
				.create_dir(&test_dir.into(), name)
				.await
				.unwrap()
				.uuid(),
		);
	}
	let colors = [
		DirColor::Blue,
		DirColor::Custom(Cow::Borrowed("#ff0000")),
		DirColor::Gray,
	];
	let items = uuids
		.iter()
		.zip(&colors)
		.map(|(&uuid, color)| color::Item {
			uuid,
			color: color.as_borrowed_cow(),
		})
		.collect::<Vec<_>>();
	let results = client
		.post_bulk_for_test::<_, ()>(color::ENDPOINT, &color::Request { items: &items }, &items)
		.await
		.unwrap();
	assert_eq!(results.len(), 3);
	for result in results {
		result.unwrap();
	}

	let mut found = Vec::new();
	for uuid in uuids {
		found.push(client.get_dir(uuid).await.unwrap().color);
	}
	assert_eq!(found, colors);
}

#[shared_test_runtime]
async fn linked_rename_updates_an_items_name_in_a_link() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let d = client.create_dir(&test_dir.into(), "d").await.unwrap();
	let s = client.create_dir(&(&d).into(), "s").await.unwrap();
	let info = drive_helpers::dir_link_info(client, &d).await;
	// The bulk metadata route renames on the drive alone, leaving the link's copy of the name.
	for result in client
		.update_metadata_for_test(&[client
			.metadata_item_for_test(FileOrDir::Dir(&s), "s2")
			.await
			.unwrap()])
		.await
		.unwrap()
	{
		result.unwrap();
	}
	let (dirs, _) = client
		.list_linked_dir::<fn(u64, Option<u64>)>(&(&info.root).into(), &info.link, None)
		.await
		.unwrap();
	assert_eq!(names(&dirs), ["s"]);

	let renamed = client.get_dir(s.uuid()).await.unwrap();
	let items = [linked::rename::Item {
		uuid: s.uuid(),
		link_uuid: *info.link.uuid(),
		metadata: renamed
			.get_encrypted_meta(info.link.crypter_for_test())
			.await
			.unwrap(),
	}];
	let results = client
		.post_bulk_for_test::<_, ()>(
			linked::rename::ENDPOINT,
			&linked::rename::Request { items: &items },
			&items,
		)
		.await
		.unwrap();
	let [renamed_in_link] = results.try_into().unwrap();
	renamed_in_link.unwrap();

	// The name decrypts only if the metadata was encrypted with the link's key.
	let (dirs, _) = client
		.list_linked_dir::<fn(u64, Option<u64>)>(&(&info.root).into(), &info.link, None)
		.await
		.unwrap();
	assert_eq!(names(&dirs), ["s2"]);
}

/// The name the receiver `share_client` reads for the dir `uuid` shared with it.
async fn shared_in_name(share_client: &Client, uuid: Uuid) -> Option<String> {
	let (dirs_in, _) = share_client
		.list_in_shared_root::<fn(u64, Option<u64>)>(None)
		.await
		.unwrap();
	let shared_in = dirs_in
		.iter()
		.filter(|dir| dir.get_dir().uuid() == uuid)
		.collect::<Vec<_>>();
	let [shared_in] = shared_in.as_slice() else {
		panic!("the receiver lists {} rows for the dir", shared_in.len());
	};
	shared_in.get_dir().name().map(str::to_owned)
}

#[shared_test_runtime]
async fn shared_rename_updates_an_items_name_for_its_receiver() {
	let client = test_utils::RESOURCES.client().await;
	let share_client = test_utils::SHARE_RESOURCES.client().await;
	let (contact, _contact_locks) = contact(&client, &share_client).await;
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;

	let d = client
		.create_dir(&(&resources.dir).into(), "old")
		.await
		.unwrap();
	client
		.share_dir::<fn(u64, Option<u64>)>(&d, &contact, None)
		.await
		.unwrap();
	// The bulk metadata route renames on the drive alone, leaving the receiver's copy of the name.
	for result in client
		.update_metadata_for_test(&[client
			.metadata_item_for_test(FileOrDir::Dir(&d), "new")
			.await
			.unwrap()])
		.await
		.unwrap()
	{
		result.unwrap();
	}
	assert_eq!(
		shared_in_name(&share_client, d.uuid()).await.as_deref(),
		Some("old")
	);

	let renamed = client.get_dir(d.uuid()).await.unwrap();
	let items = [shared::rename::Item {
		uuid: d.uuid(),
		receiver_id: contact.user_id,
		metadata: renamed
			.get_rsa_encrypted_meta(&contact.public_key)
			.await
			.unwrap(),
	}];
	let results = client
		.post_bulk_for_test::<_, ()>(
			shared::rename::ENDPOINT,
			&shared::rename::Request { items: &items },
			&items,
		)
		.await
		.unwrap();
	let [renamed_for_receiver] = results.try_into().unwrap();
	renamed_for_receiver.unwrap();

	// The name decrypts on the receiver only if the metadata was encrypted with its key.
	assert_eq!(
		shared_in_name(&share_client, d.uuid()).await.as_deref(),
		Some("new")
	);
}

fn dir_uuids(dirs: &[RemoteDirectory]) -> Vec<Uuid> {
	dirs.iter().map(HasUUID::uuid).collect()
}

/// The names of `items`, sorted: a listing comes back in no set order.
fn names(items: &[impl HasName]) -> Vec<&str> {
	let mut names = items
		.iter()
		.map(|item| item.name().unwrap())
		.collect::<Vec<_>>();
	names.sort_unstable();
	names
}

/// Two new notes. The caller holds the notes test lock for the whole test, and deletes both at its
/// end: notes are account-wide and nothing cleans them up.
async fn two_notes(client: &Client) -> [Note; 2] {
	[
		client.create_note(None).await.unwrap(),
		client.create_note(None).await.unwrap(),
	]
}

/// Posts one bulk notes `request`, whose items are `items`, to `endpoint` and answers each item's
/// result. The generic seam takes no lock, so this holds the notes write lock the single routes
/// take.
async fn post_notes<I, T: DeserializeOwned>(
	client: &Client,
	endpoint: &'static str,
	request: &(impl Serialize + Debug),
	items: &[I],
) -> Vec<Result<T, Error>> {
	let _lock = client.lock_notes().await.unwrap();
	client
		.post_bulk_for_test(endpoint, request, items)
		.await
		.unwrap()
}

/// Each of exactly `N` item results, unwrapped.
fn all_ok<T: Debug, const N: usize>(results: Vec<Result<T, Error>>) -> [T; N] {
	results
		.into_iter()
		.map(Result::unwrap)
		.collect::<Vec<_>>()
		.try_into()
		.unwrap()
}

/// `note` as the server lists it now.
async fn fetch_note(client: &Client, note: &Note) -> Note {
	client.get_note(*note.uuid()).await.unwrap().unwrap()
}

async fn delete_notes(client: &Client, notes: impl IntoIterator<Item = Note>) {
	for note in notes {
		client.delete_note(note).await.unwrap();
	}
}

fn tag_names(note: &Note) -> Vec<Option<&str>> {
	note.tags().iter().map(NoteTag::name).collect()
}

#[shared_test_runtime]
async fn notes_pinned_pins_and_unpins_notes() {
	let client = test_utils::RESOURCES.client().await;
	let _lock = client
		.acquire_lock_with_default(test_utils::locks::NOTES)
		.await
		.unwrap();
	let [n1, mut n2] = two_notes(&client).await;
	client.set_note_pinned(&mut n2, true).await.unwrap();

	let items = [
		notes::pinned::Request {
			uuid: *n1.uuid(),
			pinned: true,
		},
		notes::pinned::Request {
			uuid: *n2.uuid(),
			pinned: false,
		},
	];
	let [(), ()] = all_ok(
		post_notes(
			&client,
			bulk_notes::pinned::ENDPOINT,
			&bulk_notes::pinned::Request { items: &items },
			&items,
		)
		.await,
	);

	assert!(fetch_note(&client, &n1).await.pinned(), "n1 pinned");
	assert!(!fetch_note(&client, &n2).await.pinned(), "n2 unpinned");
	delete_notes(&client, [n1, n2]).await;
}

#[shared_test_runtime]
async fn notes_favorite_favorites_and_unfavorites_notes() {
	let client = test_utils::RESOURCES.client().await;
	let _lock = client
		.acquire_lock_with_default(test_utils::locks::NOTES)
		.await
		.unwrap();
	let [n1, mut n2] = two_notes(&client).await;
	client.set_note_favorited(&mut n2, true).await.unwrap();

	let items = [
		notes::favorite::Request {
			uuid: *n1.uuid(),
			favorite: true,
		},
		notes::favorite::Request {
			uuid: *n2.uuid(),
			favorite: false,
		},
	];
	let [(), ()] = all_ok(
		post_notes(
			&client,
			bulk_notes::favorite::ENDPOINT,
			&bulk_notes::favorite::Request { items: &items },
			&items,
		)
		.await,
	);

	assert!(fetch_note(&client, &n1).await.favorited(), "n1 favorited");
	assert!(
		!fetch_note(&client, &n2).await.favorited(),
		"n2 unfavorited"
	);
	delete_notes(&client, [n1, n2]).await;
}

#[shared_test_runtime]
async fn notes_trash_trashes_notes_and_unarchives_them() {
	let client = test_utils::RESOURCES.client().await;
	let _lock = client
		.acquire_lock_with_default(test_utils::locks::NOTES)
		.await
		.unwrap();
	let [n1, mut n2] = two_notes(&client).await;
	client.archive_note(&mut n2).await.unwrap();

	let items = [
		notes::trash::Request { uuid: *n1.uuid() },
		notes::trash::Request { uuid: *n2.uuid() },
	];
	let [(), ()] = all_ok(
		post_notes(
			&client,
			bulk_notes::trash::ENDPOINT,
			&bulk_notes::trash::Request { items: &items },
			&items,
		)
		.await,
	);

	for (name, note) in [("n1", &n1), ("n2", &n2)] {
		let fetched = fetch_note(&client, note).await;
		assert_eq!(
			(fetched.trashed(), fetched.archived()),
			(true, false),
			"{name} (trashed, archived)"
		);
	}
	delete_notes(&client, [n1, n2]).await;
}

#[shared_test_runtime]
async fn notes_archive_archives_notes_and_untrashes_them() {
	let client = test_utils::RESOURCES.client().await;
	let _lock = client
		.acquire_lock_with_default(test_utils::locks::NOTES)
		.await
		.unwrap();
	let [n1, mut n2] = two_notes(&client).await;
	client.trash_note(&mut n2).await.unwrap();

	let items = [
		notes::archive::Request { uuid: *n1.uuid() },
		notes::archive::Request { uuid: *n2.uuid() },
	];
	let [(), ()] = all_ok(
		post_notes(
			&client,
			bulk_notes::archive::ENDPOINT,
			&bulk_notes::archive::Request { items: &items },
			&items,
		)
		.await,
	);

	for (name, note) in [("n1", &n1), ("n2", &n2)] {
		let fetched = fetch_note(&client, note).await;
		assert_eq!(
			(fetched.trashed(), fetched.archived()),
			(false, true),
			"{name} (trashed, archived)"
		);
	}
	delete_notes(&client, [n1, n2]).await;
}

#[shared_test_runtime]
async fn notes_restore_clears_trash_and_archive() {
	let client = test_utils::RESOURCES.client().await;
	let _lock = client
		.acquire_lock_with_default(test_utils::locks::NOTES)
		.await
		.unwrap();
	let [mut n1, mut n2] = two_notes(&client).await;
	client.trash_note(&mut n1).await.unwrap();
	client.archive_note(&mut n2).await.unwrap();

	let items = [
		notes::restore::Request { uuid: *n1.uuid() },
		notes::restore::Request { uuid: *n2.uuid() },
	];
	let [(), ()] = all_ok(
		post_notes(
			&client,
			bulk_notes::restore::ENDPOINT,
			&bulk_notes::restore::Request { items: &items },
			&items,
		)
		.await,
	);

	for (name, note) in [("n1", &n1), ("n2", &n2)] {
		let fetched = fetch_note(&client, note).await;
		assert_eq!(
			(fetched.trashed(), fetched.archived()),
			(false, false),
			"{name} (trashed, archived)"
		);
	}
	delete_notes(&client, [n1, n2]).await;
}

#[shared_test_runtime]
async fn notes_delete_deletes_notes() {
	let client = test_utils::RESOURCES.client().await;
	let _lock = client
		.acquire_lock_with_default(test_utils::locks::NOTES)
		.await
		.unwrap();
	let [n1, n2] = two_notes(&client).await;

	let items = [
		notes::delete::Request { uuid: *n1.uuid() },
		notes::delete::Request { uuid: *n2.uuid() },
	];
	let [(), ()] = all_ok(
		post_notes(
			&client,
			bulk_notes::delete::ENDPOINT,
			&bulk_notes::delete::Request { items: &items },
			&items,
		)
		.await,
	);

	assert!(client.get_note(*n1.uuid()).await.unwrap().is_none(), "n1");
	assert!(client.get_note(*n2.uuid()).await.unwrap().is_none(), "n2");
}

#[shared_test_runtime]
async fn notes_tag_tags_notes() {
	let client = test_utils::RESOURCES.client().await;
	// Also keeps note_tag_manipulation, which deletes every tag, off this test's tag.
	let _lock = client
		.acquire_lock_with_default(test_utils::locks::NOTES)
		.await
		.unwrap();
	let tag = client.create_note_tag("bulk tag".to_owned()).await.unwrap();
	let [n1, n2] = two_notes(&client).await;

	let items = [
		notes::tag::Request {
			uuid: *n1.uuid(),
			tag: tag.uuid_for_test(),
		},
		notes::tag::Request {
			uuid: *n2.uuid(),
			tag: tag.uuid_for_test(),
		},
	];
	// A newly tagged note answers the tag's new edit time.
	let [_, _]: [notes::tag::Response; 2] = all_ok(
		post_notes(
			&client,
			bulk_notes::tag::ENDPOINT,
			&bulk_notes::tag::Request { items: &items },
			&items,
		)
		.await,
	);

	for (name, note) in [("n1", &n1), ("n2", &n2)] {
		assert_eq!(
			tag_names(&fetch_note(&client, note).await),
			[Some("bulk tag")],
			"{name}"
		);
	}
	delete_notes(&client, [n1, n2]).await;
	client.delete_note_tag(tag).await.unwrap();
}

#[shared_test_runtime]
async fn notes_untag_untags_notes() {
	let client = test_utils::RESOURCES.client().await;
	let _lock = client
		.acquire_lock_with_default(test_utils::locks::NOTES)
		.await
		.unwrap();
	let mut tag = client
		.create_note_tag("bulk untag".to_owned())
		.await
		.unwrap();
	let [mut n1, mut n2] = two_notes(&client).await;
	client.add_tag_to_note(&mut n1, &mut tag).await.unwrap();
	client.add_tag_to_note(&mut n2, &mut tag).await.unwrap();

	let items = [
		notes::untag::Request {
			uuid: *n1.uuid(),
			tag: tag.uuid_for_test(),
		},
		notes::untag::Request {
			uuid: *n2.uuid(),
			tag: tag.uuid_for_test(),
		},
	];
	let [(), ()] = all_ok(
		post_notes(
			&client,
			bulk_notes::untag::ENDPOINT,
			&bulk_notes::untag::Request { items: &items },
			&items,
		)
		.await,
	);

	for (name, note) in [("n1", &n1), ("n2", &n2)] {
		assert_eq!(
			tag_names(&fetch_note(&client, note).await),
			[] as [Option<&str>; 0],
			"{name}"
		);
	}
	delete_notes(&client, [n1, n2]).await;
	client.delete_note_tag(tag).await.unwrap();
}

#[shared_test_runtime]
async fn notes_participants_add_adds_a_contact_who_can_read_the_note() {
	let client = test_utils::RESOURCES.client().await;
	let share_client = test_utils::SHARE_RESOURCES.client().await;
	// The contact locks come before the notes lock, as in note_tests' note_sharing.
	let (contact, _contact_locks) = contact(&client, &share_client).await;
	let _lock = client
		.acquire_lock_with_default(test_utils::locks::NOTES)
		.await
		.unwrap();
	let mut note = client.create_note(None).await.unwrap();
	client
		.set_note_content(&mut note, "bulk shared content", "bulk".to_owned())
		.await
		.unwrap();

	let items = [note.participant_item_for_test(&contact, false).unwrap()];
	let [_]: [notes::participants::add::Response<'static>; 1] = all_ok(
		post_notes(
			&client,
			bulk_notes::participants::add::ENDPOINT,
			&bulk_notes::participants::add::Request {
				uuid: *note.uuid(),
				items: &items,
			},
			&items,
		)
		.await,
	);

	assert!(
		fetch_note(&client, &note)
			.await
			.participants()
			.iter()
			.any(|participant| participant.user_id() == contact.user_id),
		"the contact is a participant"
	);
	// The receiver decrypts the content with the note key it unwraps from the item's metadata.
	let mut shared = share_client.get_note(*note.uuid()).await.unwrap().unwrap();
	assert_eq!(
		share_client
			.get_note_content(&mut shared)
			.await
			.unwrap()
			.as_deref(),
		Some("bulk shared content")
	);

	client
		.remove_note_participant(&mut note, contact.user_id)
		.await
		.unwrap();
	client.delete_note(note).await.unwrap();
}

/// The share account as a contact, then the share account's and this account's `test:chats`
/// lock, in the order chat_tests' `lock_chats` takes them: both contact locks before either chats
/// lock, and the share account's chats lock first. Taking them the other way round deadlocked
/// against another leg.
async fn chat_contact_and_locks(
	client: &Client,
	share_client: &Client,
) -> (Contact<'static>, [Arc<ResourceLock>; 4]) {
	let (contact, [contact_lock, share_contact_lock]) = contact(client, share_client).await;
	let share_chat_lock = test_utils::lock_chat(share_client).await;
	let chat_lock = test_utils::lock_chat(client).await;
	(
		contact,
		[contact_lock, share_contact_lock, share_chat_lock, chat_lock],
	)
}

/// Posts `items` to `bulk/chat/conversations/participants/add` for `chat` under the chats write
/// lock the single route takes, and answers each item's result.
async fn add_chat_participants(
	client: &Client,
	chat: &Chat,
	items: &[bulk_chat::conversations::participants::add::Item<'_>],
) -> Vec<Result<chat::conversations::participants::add::Response, Error>> {
	let _lock = client.lock_chats().await.unwrap();
	client
		.post_bulk_for_test(
			bulk_chat::conversations::participants::add::ENDPOINT,
			&bulk_chat::conversations::participants::add::Request {
				uuid: chat.uuid(),
				items,
			},
			items,
		)
		.await
		.unwrap()
}

async fn fetch_chat(client: &Client, uuid: Uuid) -> Chat {
	client.get_chat(uuid).await.unwrap().unwrap()
}

fn sorted_emails<'a>(emails: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
	let mut emails = emails.into_iter().collect::<Vec<_>>();
	emails.sort_unstable();
	emails
}

/// How many reads [`unread_once_recorded`] makes at most: each is one round trip of a few tens of
/// milliseconds, so this bounds the wait to a few seconds.
const UNREAD_READS: usize = 100;

/// `client`'s unread count in `chat` once it is no longer 0, or 0 after [`UNREAD_READS`] reads.
/// `chat/send` answers before the server records the recipients' unread notifications (20 to
/// 80 ms later when measured), so a read straight after the send can still see 0. The reads follow
/// each other with no sleep between them.
async fn unread_once_recorded(client: &Client, chat: &Chat) -> u64 {
	for _ in 0..UNREAD_READS {
		let unread = client.get_chat_unread_count(chat).await.unwrap();
		if unread != 0 {
			return unread;
		}
	}
	0
}

#[shared_test_runtime]
async fn chat_participants_add_adds_a_contact_who_can_use_the_chat() {
	let client = test_utils::RESOURCES.client().await;
	let share_client = test_utils::SHARE_RESOURCES.client().await;
	let (contact, _locks) = chat_contact_and_locks(&client, &share_client).await;
	let chat = client.create_chat(&[]).await.unwrap();

	let items = [chat.participant_item_for_test(&contact).unwrap()];
	let [_] = all_ok(add_chat_participants(&client, &chat, &items).await);

	let fetched = fetch_chat(&client, chat.uuid()).await;
	assert_eq!(
		sorted_emails(fetched.participants().iter().map(|p| p.email())),
		sorted_emails([client.email(), share_client.email()])
	);
	// The new participant sends with the chat key it unwraps from the item's metadata; a wrong
	// wrap leaves its chat without a key and the send fails with MetadataWasNotDecrypted.
	let mut shared = fetch_chat(&share_client, chat.uuid()).await;
	share_client
		.send_chat_message(&mut shared, "hello".to_owned(), None)
		.await
		.unwrap();

	client.delete_chat(chat).await.unwrap();
}

#[shared_test_runtime]
async fn chat_participants_add_fails_a_contact_that_does_not_exist() {
	let client = test_utils::RESOURCES.client().await;
	let _lock = test_utils::lock_chat(&client).await;
	let chat = client.create_chat(&[]).await.unwrap();

	// Hand-built: the server refuses an unknown contact before it looks at the metadata.
	let items = [bulk_chat::conversations::participants::add::Item {
		contact_uuid: Uuid::new_v4(),
		metadata: RSAEncryptedString(Cow::Borrowed("x")),
	}];
	let [result] = add_chat_participants(&client, &chat, &items)
		.await
		.try_into()
		.unwrap();

	assert_eq!(
		result.unwrap_err().server_code().as_deref(),
		Some("contact_not_found")
	);
	assert_eq!(
		fetch_chat(&client, chat.uuid()).await.participants().len(),
		1
	);
	client.delete_chat(chat).await.unwrap();
}

#[shared_test_runtime]
async fn chat_conversations_read_clears_the_unread_count() {
	let client = test_utils::RESOURCES.client().await;
	let share_client = test_utils::SHARE_RESOURCES.client().await;
	let (contact, _locks) = chat_contact_and_locks(&client, &share_client).await;
	let chat = client.create_chat(slice::from_ref(&contact)).await.unwrap();
	let mut shared = fetch_chat(&share_client, chat.uuid()).await;
	share_client
		.send_chat_message(&mut shared, "unread".to_owned(), None)
		.await
		.unwrap();
	assert_eq!(unread_once_recorded(&client, &chat).await, 1);

	// An unknown conversation is a status-true no-op. The single route takes no lock, so neither
	// does this.
	let items = [
		bulk_chat::conversations::read::Item { uuid: chat.uuid() },
		bulk_chat::conversations::read::Item {
			uuid: Uuid::new_v4(),
		},
	];
	let [(), ()] = all_ok(
		client
			.post_bulk_for_test(
				bulk_chat::conversations::read::ENDPOINT,
				&bulk_chat::conversations::read::Request { items: &items },
				&items,
			)
			.await
			.unwrap(),
	);

	assert_eq!(client.get_chat_unread_count(&chat).await.unwrap(), 0);
	client.delete_chat(chat).await.unwrap();
}

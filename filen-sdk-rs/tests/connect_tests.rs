use std::{borrow::Cow, sync::Arc};

use filen_macros::shared_test_runtime;
use filen_sdk_rs::{
	ErrorKind,
	auth::Client,
	connect::{DirPublicLink, PublicLinkSharedClientExt},
	consts::CHUNK_SIZE,
	fs::{
		HasName, HasUUID,
		categories::{DirType, Shared},
		copy::{CopyOptions, CopySource, CopySourceDir, JobControl},
		dir::{RemoteDirectory, meta::DirectoryMetaChanges},
		file::meta::FileMetaChanges,
	},
	io::{HasFileInfo, client_impl::IoSharedClientExt},
};
use filen_types::api::v3::{contacts::Contact, dir::link::PublicLinkExpiration};
use tokio::sync::watch;

mod copy_helpers;
use copy_helpers::{PauseOnCreate, Recorder, contents, copy, data, upload, wait_until_paused};

#[shared_test_runtime]
async fn dir_public_link() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let unauth_client = client.get_unauthed();

	let dir = client.create_dir(&test_dir.into(), "dir").await.unwrap();
	let mut sub_dir = client.create_dir(&(&dir).into(), "sub_dir").await.unwrap();

	let dir_file = client
		.make_file_builder("empty_dir.txt", dir.uuid())
		.unwrap();
	let dir_file = client.upload_file(dir_file, b"").await.unwrap();

	let file = client.make_file_builder("a.txt", sub_dir.uuid()).unwrap();
	let file = client.upload_file(file, b"Hello, world!").await.unwrap();

	let empty_file = client
		.make_file_builder("empty.txt", sub_dir.uuid())
		.unwrap();
	let empty_file = client.upload_file(empty_file, b"").await.unwrap();

	let no_link = client.get_dir_link_rw(&dir).await.unwrap();
	assert_eq!(no_link, None);

	let mut link_rw = client
		.public_link_dir::<fn(u64, Option<u64>)>(&dir, None)
		.await
		.unwrap();

	let found_link = client.get_dir_link_rw(&dir).await.unwrap().unwrap();
	assert_eq!(
		&link_rw, &found_link,
		"get_dir_link_status didn't match created link"
	);

	let link: DirPublicLink = found_link.try_into().unwrap();
	let info = unauth_client
		.get_dir_public_link_info(*link.uuid(), &link.key_string())
		.await
		.unwrap();

	let (dirs, files) = client
		.list_linked_dir::<fn(u64, Option<u64>)>(&(&info.root).into(), &link, None)
		.await
		.unwrap();
	let linked_sub_dir = dirs.iter().find(|d| d.inner() == &sub_dir).unwrap();

	assert_eq!(&files, &vec![dir_file.clone().into_anonymous()]);

	let (sub_dirs, sub_files) = unauth_client
		.list_linked_dir::<fn(u64, Option<u64>)>(&linked_sub_dir.into(), &info.link, None)
		.await
		.unwrap();
	assert_eq!(sub_dirs.len(), 0);
	assert_eq!(sub_files.len(), 2);
	assert!(sub_files.contains(&file.clone().into_anonymous()));
	assert!(sub_files.contains(&empty_file.clone().into_anonymous()));

	let (dirs, files) = unauth_client
		.list_linked_dir::<fn(u64, Option<u64>)>(&(&info.root).into(), &info.link, None)
		.await
		.unwrap();
	let linked_sub_dir = dirs.iter().find(|d| d.inner() == &sub_dir).unwrap();
	assert_eq!(&files, &vec![dir_file.clone().into_anonymous()]);

	let password = "some_password";
	link_rw.set_password(password.to_string());
	link_rw.set_expiration(PublicLinkExpiration::OneHour);
	client.update_dir_link(&dir, &link_rw).await.unwrap();

	let found_link = client.get_dir_link_rw(&dir).await.unwrap().unwrap();
	assert_eq!(
		link.uuid(),
		&found_link.uuid(),
		"get_dir_link_status didn't match created link"
	);

	let mut sub_sub_dir = client
		.create_dir(&(&sub_dir).into(), "sub_sub_dir")
		.await
		.unwrap();
	let sub_sub_file = client
		.make_file_builder("sub_sub_file.txt", sub_dir.uuid())
		.unwrap();
	let mut sub_sub_file = client
		.upload_file(sub_sub_file, b"Hello, world!")
		.await
		.unwrap();

	let err = unauth_client
		.list_linked_dir::<fn(u64, Option<u64>)>(&linked_sub_dir.into(), &link, None)
		.await
		.unwrap_err();
	assert_eq!(err.kind(), ErrorKind::WrongPassword);

	let info = unauth_client
		.get_dir_public_link_info(*link.uuid(), &link.key_string())
		.await
		.unwrap();

	let mut link = info.link;
	link.set_password(password.to_string());

	let (sub_dirs, sub_files) = unauth_client
		.list_linked_dir::<fn(u64, Option<u64>)>(&linked_sub_dir.into(), &link, None)
		.await
		.unwrap();
	assert_eq!(sub_dirs.len(), 1);
	assert_eq!(sub_dirs[0].inner(), &sub_sub_dir);
	assert!(sub_files.contains(&sub_sub_file.clone().into_anonymous()));
	assert_eq!(sub_files.len(), 3);

	client
		.update_file_metadata(
			&mut sub_sub_file,
			FileMetaChanges::default()
				.name("new_file_name.txt")
				.unwrap(),
		)
		.await
		.unwrap();

	let (_, sub_files) = client
		.list_linked_dir::<fn(u64, Option<u64>)>(&linked_sub_dir.into(), &link, None)
		.await
		.unwrap();
	let found_file = sub_files
		.iter()
		.find(|f| f.name().is_some_and(|n| n == "new_file_name.txt"));
	assert!(found_file.is_some());

	client
		.update_dir_metadata(
			&mut sub_dir,
			DirectoryMetaChanges::default()
				.name("new_dir_name")
				.unwrap(),
		)
		.await
		.unwrap();
	let (dirs, _) = unauth_client
		.list_linked_dir::<fn(u64, Option<u64>)>(&(&info.root).into(), &link, None)
		.await
		.unwrap();
	assert_eq!(dirs.len(), 1);
	assert_eq!(dirs[0].name(), Some("new_dir_name"));

	client.trash_dir(&mut sub_sub_dir).await.unwrap();
	client.trash_file(&mut sub_sub_file).await.unwrap();

	let (sub_dirs, sub_files) = unauth_client
		.list_linked_dir::<fn(u64, Option<u64>)>(&linked_sub_dir.into(), &link, None)
		.await
		.unwrap();
	assert_eq!(sub_dirs.len(), 0);
	assert_eq!(sub_files.len(), 2);
	assert!(!sub_files.contains(&sub_sub_file.clone().into_anonymous()));
}

// Moving a subtree into a publicly-linked parent must mirror the moved items'
// metadata into that link (as create/upload do); otherwise link recipients
// cannot see or decrypt the moved-in directory or its files. Exercises the
// move_dir propagation site (F055); move_file/restore_dir/restore_file share the
// identical `update_item_with_maybe_connected_parent` call.
#[shared_test_runtime]
async fn move_into_linked_dir_propagates() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let unauth_client = client.get_unauthed();

	// Destination parent with a public link.
	let parent = client
		.create_dir(&test_dir.into(), "linked_parent")
		.await
		.unwrap();
	client
		.public_link_dir::<fn(u64, Option<u64>)>(&parent, None)
		.await
		.unwrap();

	// Source subtree (with a nested file) created OUTSIDE the link.
	let mut moved = client.create_dir(&test_dir.into(), "moved").await.unwrap();
	let child = client.make_file_builder("child.txt", moved.uuid()).unwrap();
	let child = client.upload_file(child, b"hi").await.unwrap();

	// Move the subtree into the linked parent.
	client
		.move_dir(&mut moved, &(&parent).into())
		.await
		.unwrap();

	// The link must now expose the moved directory...
	let link: DirPublicLink = client
		.get_dir_link_rw(&parent)
		.await
		.unwrap()
		.unwrap()
		.try_into()
		.unwrap();
	let info = unauth_client
		.get_dir_public_link_info(*link.uuid(), &link.key_string())
		.await
		.unwrap();
	let (dirs, _files) = unauth_client
		.list_linked_dir::<fn(u64, Option<u64>)>(&(&info.root).into(), &info.link, None)
		.await
		.unwrap();
	let linked_moved = dirs
		.iter()
		.find(|d| d.inner().uuid() == moved.uuid())
		.expect("moved dir must be visible in the destination link");

	// ...and its nested file.
	let (_sub_dirs, sub_files) = unauth_client
		.list_linked_dir::<fn(u64, Option<u64>)>(&linked_moved.into(), &info.link, None)
		.await
		.unwrap();
	assert!(
		sub_files.iter().any(|f| f.uuid() == child.uuid()),
		"moved dir's nested file must be visible in the destination link"
	);
}

#[shared_test_runtime]
async fn dir_public_link_remove() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let unauth_client = client.get_unauthed();

	// A small tree: a file at the link root and a sub_dir holding another file, so removal has to
	// tear down a recursively-linked structure rather than a single entry.
	let dir = client
		.create_dir(&test_dir.into(), "link_remove_dir")
		.await
		.unwrap();
	let sub_dir = client.create_dir(&(&dir).into(), "sub_dir").await.unwrap();

	let dir_file = client.make_file_builder("a.txt", dir.uuid()).unwrap();
	let dir_file = client
		.upload_file(dir_file, b"Hello, world!")
		.await
		.unwrap();

	let sub_file = client.make_file_builder("b.txt", sub_dir.uuid()).unwrap();
	let sub_file = client.upload_file(sub_file, b"").await.unwrap();

	// No link exists before we create one.
	assert_eq!(client.get_dir_link_rw(&dir).await.unwrap(), None);

	// Create the public link over the whole tree.
	let link_rw = client
		.public_link_dir::<fn(u64, Option<u64>)>(&dir, None)
		.await
		.unwrap();

	// The owner can read the link back and it matches what we just created.
	let found_link = client.get_dir_link_rw(&dir).await.unwrap().unwrap();
	assert_eq!(
		link_rw, found_link,
		"get_dir_link_rw didn't match the created link"
	);

	// Capture the public-resolution coordinates now; after removal there is no link left to read
	// them from, but we still want to confirm the old coordinates stop resolving.
	let link_uuid = link_rw.uuid();
	let key_string = link_rw
		.key_string()
		.expect("created link has a decrypted key");

	// The link resolves without authentication and exposes the linked tree.
	let info = unauth_client
		.get_dir_public_link_info(link_uuid, &key_string)
		.await
		.unwrap();
	let (dirs, files) = unauth_client
		.list_linked_dir::<fn(u64, Option<u64>)>(&(&info.root).into(), &info.link, None)
		.await
		.unwrap();
	assert_eq!(files, vec![dir_file.clone().into_anonymous()]);
	let linked_sub_dir = dirs
		.iter()
		.find(|d| d.inner() == &sub_dir)
		.expect("sub_dir should be present in the linked tree");
	let (sub_dirs, sub_files) = unauth_client
		.list_linked_dir::<fn(u64, Option<u64>)>(&linked_sub_dir.into(), &info.link, None)
		.await
		.unwrap();
	assert_eq!(sub_dirs.len(), 0);
	assert!(sub_files.contains(&sub_file.clone().into_anonymous()));

	// Remove the link. The link is identified for removal by the directory it links, so the link
	// object we created earlier (`link_rw`) is no longer needed here.
	client.remove_dir_link(&dir).await.unwrap();

	// The owner no longer sees a link...
	assert_eq!(
		client.get_dir_link_rw(&dir).await.unwrap(),
		None,
		"link still present after removal"
	);

	// ...and it no longer resolves publicly.
	let resolved = unauth_client
		.get_dir_public_link_info(link_uuid, &key_string)
		.await;
	assert!(
		resolved.is_err(),
		"removed link should not resolve publicly anymore"
	);

	// Removal leaves the directory in a clean state: it can be re-linked and re-removed.
	client
		.public_link_dir::<fn(u64, Option<u64>)>(&dir, None)
		.await
		.unwrap();
	assert!(client.get_dir_link_rw(&dir).await.unwrap().is_some());
	client.remove_dir_link(&dir).await.unwrap();
	assert_eq!(client.get_dir_link_rw(&dir).await.unwrap(), None);
}

#[shared_test_runtime]
async fn file_public_link() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let share_resources = test_utils::SHARE_RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let share_client = &share_resources.client;
	let unauth_client = share_client.get_unauthed();

	let file = client.make_file_builder("a.txt", test_dir.uuid()).unwrap();
	let mut file = client.upload_file(file, b"Hello, world!").await.unwrap();

	let no_link = client.get_file_link_status(&file).await.unwrap();
	assert_eq!(no_link, None);

	let mut link = client.public_link_file(&file).await.unwrap();
	let found_link = client.get_file_link_status(&file).await.unwrap().unwrap();
	assert_eq!(
		&link.uuid(),
		&found_link.uuid(),
		"get_file_link_status didn't match created link"
	);
	let file_key = file.key().unwrap().to_str();

	let linked_file = unauth_client
		.get_linked_file(link.uuid(), file_key.as_ref(), None)
		.await
		.unwrap();
	assert_eq!(linked_file, file);

	let password = "some_password";

	link.set_password(password.to_string());
	link.set_expiration(PublicLinkExpiration::OneHour);
	client.update_file_link(&file, &link).await.unwrap();
	let found_link = client.get_file_link_status(&file).await.unwrap().unwrap();
	let mut cloned_found_link = found_link.clone();
	cloned_found_link.set_password(password.to_string());
	assert_eq!(&link, &cloned_found_link);

	let linked_file = unauth_client
		.get_linked_file(link.uuid(), file_key.as_ref(), Some(password))
		.await
		.unwrap();
	assert_eq!(linked_file, file);
	drop(file_key);

	client
		.update_file_metadata(
			&mut file,
			FileMetaChanges::default()
				.name("new_file_name.txt")
				.unwrap(),
		)
		.await
		.unwrap();
	let file_key = file.key().unwrap().to_str();
	let linked_file = share_client
		.get_linked_file(link.uuid(), file_key.as_ref(), Some(password))
		.await
		.unwrap();
	assert_eq!(linked_file, file);
}

// #[shared_test_runtime]
// async fn contact_interactions() {
// 	let resources = test_utils::RESOURCES.get_resources().await;
// 	let client = &resources.client;

// 	let share_resources = test_utils::SHARE_RESOURCES.get_resources().await;
// 	let share_client = &share_resources.client;

// 	let _lock = client
// 		.acquire_lock_with_default("test:contact")
// 		.await
// 		.unwrap();
// 	let _lock = share_client
// 		.acquire_lock_with_default("test:contact")
// 		.await
// 		.unwrap();

// 	// clean up all existing contacts
// 	for contact in client.get_contacts().await.unwrap() {
// 		client.delete_contact(contact.uuid).await.unwrap();
// 	}
// 	for contact in share_client.get_contacts().await.unwrap() {
// 		share_client.delete_contact(contact.uuid).await.unwrap();
// 	}
// 	for contact in client.list_incoming_contact_requests().await.unwrap() {
// 		client.deny_contact_request(contact.uuid).await.unwrap();
// 	}
// 	for contact in share_client.list_incoming_contact_requests().await.unwrap() {
// 		share_client
// 			.deny_contact_request(contact.uuid)
// 			.await
// 			.unwrap();
// 	}
// 	for contact in client.list_outgoing_contact_requests().await.unwrap() {
// 		client.cancel_contact_request(contact.uuid).await.unwrap();
// 	}
// 	for contact in share_client.list_outgoing_contact_requests().await.unwrap() {
// 		share_client
// 			.cancel_contact_request(contact.uuid)
// 			.await
// 			.unwrap();
// 	}

// 	assert_eq!(
// 		client.list_outgoing_contact_requests().await.unwrap().len(),
// 		0
// 	);
// 	client
// 		.send_contact_request(share_client.email())
// 		.await
// 		.unwrap();
// 	let out_requests = client.list_outgoing_contact_requests().await.unwrap();

// 	assert_eq!(out_requests.len(), 1);
// 	assert_eq!(out_requests[0].email, share_client.email());

// 	let in_requests = share_client.list_incoming_contact_requests().await.unwrap();
// 	assert_eq!(in_requests.len(), 1);
// 	assert_eq!(in_requests[0].email, client.email());

// 	share_client
// 		.accept_contact_request(in_requests[0].uuid)
// 		.await
// 		.unwrap();

// 	let in_requests = client.list_incoming_contact_requests().await.unwrap();
// 	assert_eq!(in_requests.len(), 0);
// 	let out_requests = client.list_outgoing_contact_requests().await.unwrap();
// 	assert_eq!(out_requests.len(), 0);

// 	let contacts = client.get_contacts().await.unwrap();
// 	assert_eq!(contacts.len(), 1);
// 	assert_eq!(contacts[0].email, share_client.email());

// 	let share_contacts = share_client.get_contacts().await.unwrap();
// 	assert_eq!(share_contacts.len(), 1);
// 	assert_eq!(share_contacts[0].email, client.email());

// 	client.delete_contact(contacts[0].uuid).await.unwrap();

// 	let share_contacts = share_client.get_contacts().await.unwrap();
// 	assert_eq!(share_contacts.len(), 0);
// 	let contacts = client.get_contacts().await.unwrap();
// 	assert_eq!(contacts.len(), 0);
// }

// async fn set_up_contact_no_add<'a>(
// 	client: &'a Client,
// 	share_client: &'a Client,
// ) -> (Arc<ResourceLock>, Arc<ResourceLock>, usize, usize) {
// 	let lock1 = client
// 		.acquire_lock_with_default("test:contact")
// 		.await
// 		.unwrap();
// 	let lock2 = share_client
// 		.acquire_lock_with_default("test:contact")
// 		.await
// 		.unwrap();

// 	let _ = futures::join!(
// 		async {
// 			for contact in client.get_contacts().await.unwrap() {
// 				let _ = client.delete_contact(contact.uuid).await;
// 			}
// 		},
// 		async {
// 			for contact in share_client.get_contacts().await.unwrap() {
// 				let _ = share_client.delete_contact(contact.uuid).await;
// 			}
// 		},
// 		async {
// 			for contact in client.list_outgoing_contact_requests().await.unwrap() {
// 				let _ = client.cancel_contact_request(contact.uuid).await;
// 			}
// 		},
// 		async {
// 			for contact in share_client.list_incoming_contact_requests().await.unwrap() {
// 				let _ = share_client.deny_contact_request(contact.uuid).await;
// 			}
// 		},
// 		async {
// 			let (out_dirs, out_files) = client.list_out_shared(None).await.unwrap();
// 			let mut out_futures = out_dirs
// 				.into_iter()
// 				.filter_map(|d| {
// 					if d.get_dir().name().unwrap().starts_with("compat-") {
// 						None
// 					} else {
// 						Some((*d.get_dir().uuid(), d.get_source_id()))
// 					}
// 				})
// 				.chain(
// 					out_files
// 						.into_iter()
// 						.map(|f| (*f.get_file().uuid(), f.get_source_id())),
// 				)
// 				.map(|(uuid, source_id)| async move {
// 					let _ = client.remove_shared_link_out(uuid, source_id).await;
// 				})
// 				.collect::<FuturesUnordered<_>>();
// 			while (out_futures.next().await).is_some() {}
// 		},
// 		async {
// 			let (in_dirs, in_files) = share_client.list_in_shared().await.unwrap();

// 			let mut in_futures = in_dirs
// 				.into_iter()
// 				.filter_map(|d| {
// 					if d.get_dir().name().unwrap().starts_with("compat-") {
// 						None
// 					} else {
// 						Some(*d.get_dir().uuid())
// 					}
// 				})
// 				.chain(in_files.into_iter().map(|f| *f.get_file().uuid()))
// 				.map(|uuid| async move {
// 					let _ = share_client.remove_shared_link_in(uuid).await;
// 				})
// 				.collect::<FuturesUnordered<_>>();
// 			while (in_futures.next().await).is_some() {}
// 		},
// 		async {
// 			let blocked_contacts = client.get_blocked_contacts().await.unwrap();
// 			let mut futures = blocked_contacts
// 				.into_iter()
// 				.map(|c| async move {
// 					let _ = client.unblock_contact(c.uuid).await;
// 				})
// 				.collect::<FuturesUnordered<_>>();
// 			while (futures.next().await).is_some() {}
// 		},
// 		async {
// 			let blocked_contacts = share_client.get_blocked_contacts().await.unwrap();
// 			let mut futures = blocked_contacts
// 				.into_iter()
// 				.map(|c| async move {
// 					let _ = share_client.unblock_contact(c.uuid).await;
// 				})
// 				.collect::<FuturesUnordered<_>>();
// 			while (futures.next().await).is_some() {}
// 		}
// 	);
// 	tokio::time::sleep(std::time::Duration::from_secs(300)).await;
// 	let (out_dirs, _) = client.list_out_shared(None).await.unwrap();
// 	let (in_dirs, _) = share_client.list_in_shared().await.unwrap();
// 	(lock1, lock2, out_dirs.len(), in_dirs.len())
// }

// async fn set_up_contact<'a>(
// 	client: &'a Client,
// 	share_client: &'a Client,
// ) -> (Arc<ResourceLock>, Arc<ResourceLock>, usize, usize) {
// 	let (lock1, lock2, num_shared_out, num_shared_in) =
// 		set_up_contact_no_add(client, share_client).await;

// 	let request_uuid = client
// 		.send_contact_request(share_client.email())
// 		.await
// 		.unwrap();

// 	share_client
// 		.accept_contact_request(request_uuid)
// 		.await
// 		.unwrap();

// 	(lock1, lock2, num_shared_out, num_shared_in)
// }

#[shared_test_runtime]
async fn share_dir() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let share_resources = test_utils::SHARE_RESOURCES.get_resources().await;
	let share_client = &share_resources.client;

	let mut dir = client.create_dir(&test_dir.into(), "dir").await.unwrap();
	let sub_dir = client.create_dir(&(&dir).into(), "sub_dir").await.unwrap();
	let dir_file = client.make_file_builder("a.txt", dir.uuid()).unwrap();
	let mut dir_file = client
		.upload_file(dir_file, b"Hello, world!")
		.await
		.unwrap();
	let file = client.make_file_builder("a.txt", sub_dir.uuid()).unwrap();
	let sub_file = client.upload_file(file, b"").await.unwrap();

	let (_lock1, _lock2) = test_utils::set_up_contact(client, share_client).await;

	let contacts = client.get_contacts().await.unwrap();
	assert_eq!(contacts.len(), 1);
	let contact = &contacts[0];
	assert_eq!(contact.email, share_client.email());
	client
		.share_dir::<fn(u64, Option<u64>)>(&dir, contact, None)
		.await
		.unwrap();
	// sleep because sometimes this takes a second to become available on the backend
	tokio::time::sleep(std::time::Duration::from_secs(5)).await;

	let (shared_dirs_out, _) = client
		.list_out_shared::<fn(u64, Option<u64>)>(None, None)
		.await
		.unwrap();
	// uuid-scoped asserts only: the share account's global share count is mutated by other CI
	// legs (compat fixture rebuilds) outside the "test:contact" lock, so len()-based asserts
	// race (nightly 2026-08-14). Counting exactly one row for OUR dir stays race-free and
	// still catches a duplicated share.
	assert_eq!(
		shared_dirs_out
			.iter()
			.filter(|d| d.get_dir().uuid() == dir.uuid())
			.count(),
		1
	);

	let (shared_dirs_in, _) = share_client
		.list_in_shared_root::<fn(u64, Option<u64>)>(None)
		.await
		.unwrap();
	assert_eq!(
		shared_dirs_in
			.iter()
			.filter(|d| d.get_dir().uuid() == dir.uuid())
			.count(),
		1
	);

	let shared_dir_in = shared_dirs_in
		.iter()
		.find(|d| d.get_dir().uuid() == dir.uuid())
		.unwrap();

	let shared_dir_out = shared_dirs_out
		.iter()
		.find(|d| d.get_dir().uuid() == dir.uuid())
		.unwrap();

	assert_eq!(shared_dir_in.get_dir(), shared_dir_out.get_dir());

	let (shared_dirs_out, shared_files_out) = client
		.list_shared_dir::<fn(u64, Option<u64>)>(
			&shared_dir_out.into(),
			shared_dir_out.sharing_role(),
			None,
		)
		.await
		.unwrap();
	let (shared_dirs_in, shared_files_in) = share_client
		.list_shared_dir::<fn(u64, Option<u64>)>(
			&shared_dir_in.into(),
			shared_dir_in.sharing_role(),
			None,
		)
		.await
		.unwrap();

	assert_eq!(shared_dirs_out.len(), 1);
	assert_eq!(shared_dirs_in.len(), 1);
	assert_eq!(shared_files_out.len(), 1);
	assert_eq!(shared_files_in.len(), 1);

	assert_eq!(shared_dirs_in[0].get_dir(), shared_dirs_out[0].get_dir());
	assert_eq!(shared_files_out[0], shared_files_in[0]);

	assert_eq!(
		&share_client
			.download_file(&shared_files_in[0])
			.await
			.unwrap(),
		b"Hello, world!"
	);
	assert_eq!(&shared_files_in[0], &dir_file.clone().into_anonymous());
	assert_eq!(
		client.download_file(&shared_files_out[0]).await.unwrap(),
		b"Hello, world!"
	);
	assert_eq!(&shared_files_out[0], &dir_file.clone().into_anonymous());

	let (dirs, files) = client
		.list_dir_recursive::<Shared, fn(u64, Option<u64>)>(
			&shared_dir_out.into(),
			None,
			shared_dir_out.sharing_role(),
		)
		.await
		.unwrap();
	assert_eq!(dirs.len(), 1);
	assert_eq!(files.len(), 2);
	assert_eq!(dirs[0].get_dir(), &sub_dir);
	assert!(files.contains(&dir_file.clone().into_anonymous()));
	assert!(files.contains(&sub_file.clone().into_anonymous()));

	let (dirs, files) = share_client
		.list_dir_recursive::<Shared, fn(u64, Option<u64>)>(
			&shared_dir_in.into(),
			None,
			shared_dir_in.sharing_role(),
		)
		.await
		.unwrap();
	assert_eq!(dirs.len(), 1);
	assert_eq!(files.len(), 2);
	assert_eq!(dirs[0].get_dir().uuid(), sub_dir.uuid());
	assert_eq!(dirs[0].get_dir().name(), sub_dir.name());
	assert!(files.contains(&dir_file.clone().into_anonymous()));

	// change metadata
	client
		.update_dir_metadata(
			&mut dir,
			DirectoryMetaChanges::default().name("new_name").unwrap(),
		)
		.await
		.unwrap();

	client
		.update_file_metadata(
			&mut dir_file,
			FileMetaChanges::default()
				.name("new_file_name.txt")
				.unwrap(),
		)
		.await
		.unwrap();
	let (shared_dirs_in, _) = share_client
		.list_in_shared_root::<fn(u64, Option<u64>)>(None)
		.await
		.unwrap();

	let shared_dir_in = shared_dirs_in
		.iter()
		.find(|d| d.get_dir().uuid() == dir.uuid())
		.unwrap();

	assert_eq!(shared_dir_in.get_dir().name().unwrap(), "new_name");

	let (_, shared_files_in) = share_client
		.list_shared_dir::<fn(u64, Option<u64>)>(
			&shared_dir_in.into(),
			shared_dir_in.sharing_role(),
			None,
		)
		.await
		.unwrap();
	assert_eq!(shared_files_in.len(), 1);
	assert_eq!(shared_files_in[0].name().unwrap(), "new_file_name.txt");

	assert!(files.contains(&sub_file.clone().into_anonymous()));

	copy_with_shares(
		client,
		share_client,
		test_dir,
		&share_resources.dir,
		contact,
	)
	.await;
}

// #[shared_test_runtime]
// async fn share_file() {
// 	let resources = test_utils::RESOURCES.get_resources().await;
// 	let client = &resources.client;
// 	let test_dir = &resources.dir;

// 	let share_resources = test_utils::SHARE_RESOURCES.get_resources().await;
// 	let share_client = &share_resources.client;

// 	let _locks = set_up_contact(client, share_client).await;

// 	let file = client.make_file_builder("a.txt", *test_dir.uuid()).unwrap().build();
// 	let mut file = client
// 		.upload_file(file, b"Hello, world!")
// 		.await
// 		.unwrap();

// 	let contacts = client.get_contacts().await.unwrap();
// 	assert_eq!(contacts.len(), 1);
// 	let contact = &contacts[0];

// 	client.share_file(&file, contact).await.unwrap();

// 	let (_, shared_files_out) = client.list_out_shared(None).await.unwrap();
// 	assert_eq!(shared_files_out.len(), 1);
// 	let shared_file = shared_files_out[0].get_file();
// 	assert_eq!(shared_file, &file);
// 	let (_, shared_files_in) = share_client.list_in_shared().await.unwrap();
// 	assert_eq!(shared_files_in.len(), 1);
// 	let shared_file = shared_files_in[0].get_file();
// 	assert_eq!(shared_file, &file);
// 	let buf = share_client.download_file(shared_file).await.unwrap();
// 	assert_eq!(buf, b"Hello, world!");
// 	let buf = client.download_file(shared_file).await.unwrap();
// 	assert_eq!(buf, b"Hello, world!");

// 	let new_created = Utc::now();
// 	let changes = FileMetaChanges::default()
// 		.name("new_file_name.txt")
// 		.unwrap()
// 		.created(Some(new_created));
// 	client
// 		.update_file_metadata(&mut file, changes)
// 		.await
// 		.unwrap();

// 	let (_, shared_files_in) = share_client.list_in_shared().await.unwrap();
// 	assert_eq!(shared_files_in.len(), 1);
// 	assert_eq!(
// 		shared_files_in[0].get_file().name().unwrap(),
// 		"new_file_name.txt"
// 	);
// 	assert_eq!(
// 		shared_files_in[0].get_file().created().unwrap(),
// 		new_created.round_subsecs(3),
// 		"created date not updated"
// 	);
// }

// #[shared_test_runtime]
// async fn remove_link() {
// 	let resources = test_utils::RESOURCES.get_resources().await;
// 	let client = &resources.client;
// 	let test_dir = &resources.dir;

// 	let share_resources = test_utils::SHARE_RESOURCES.get_resources().await;
// 	let share_client = &share_resources.client;

// 	let (_lock1, _lock2, num_shared_out, num_shared_in) =
// 		set_up_contact(client, share_client).await;

// 	let out_dir = client
// 		.create_dir(test_dir, "out".to_string())
// 		.await
// 		.unwrap();
// 	let in_dir = client.create_dir(test_dir, "in".to_string()).await.unwrap();

// 	let contacts = client.get_contacts().await.unwrap();
// 	assert_eq!(contacts.len(), 1);
// 	let contact = &contacts[0];

// 	client.share_dir(&out_dir, contact).await.unwrap();
// 	client.share_dir(&in_dir, contact).await.unwrap();

// 	let (shared_dirs_out, _) = client.list_out_shared(None).await.unwrap();
// 	assert_eq!(shared_dirs_out.len(), num_shared_out + 2);

// 	let (shared_dirs_in, _) = share_client.list_in_shared().await.unwrap();
// 	assert_eq!(shared_dirs_in.len(), num_shared_in + 2);
// 	client
// 		.remove_shared_link_out(*out_dir.uuid(), contact.user_id)
// 		.await
// 		.unwrap();
// 	share_client
// 		.remove_shared_link_in(*in_dir.uuid())
// 		.await
// 		.unwrap();

// 	tokio::time::sleep(std::time::Duration::from_secs(300)).await;

// 	let shared_dirs_out = client.list_out_shared(None).await.unwrap().0;
// 	assert_eq!(shared_dirs_out.len(), num_shared_out);
// 	let shared_dirs_in = share_client.list_in_shared().await.unwrap().0;
// 	assert_eq!(shared_dirs_in.len(), num_shared_in);
// }

// #[shared_test_runtime]
// async fn block() {
// 	let resources = test_utils::RESOURCES.get_resources().await;
// 	let client = &resources.client;

// 	let share_resources = test_utils::SHARE_RESOURCES.get_resources().await;
// 	let share_client = &share_resources.client;

// 	let _lock = set_up_contact_no_add(client, share_client).await;

// 	client
// 		.send_contact_request(share_client.email())
// 		.await
// 		.unwrap();
// 	let requests = share_client.list_incoming_contact_requests().await.unwrap();
// 	assert_eq!(requests.len(), 1);
// 	assert_eq!(requests[0].email, client.email());

// 	share_client
// 		.block_contact(&requests[0].email)
// 		.await
// 		.unwrap();
// 	let requests = share_client.list_incoming_contact_requests().await.unwrap();
// 	assert_eq!(requests.len(), 0);

// 	let blocked = share_client.get_blocked_contacts().await.unwrap();
// 	assert_eq!(blocked.len(), 1);
// 	assert_eq!(blocked[0].email, client.email());

// 	let err = client
// 		.send_contact_request(share_client.email())
// 		.await
// 		.unwrap_err();
// 	assert!(err.to_string().contains("Contact blocked"));

// 	let requests = share_client.list_incoming_contact_requests().await.unwrap();
// 	assert_eq!(requests.len(), 0);

// 	share_client.unblock_contact(blocked[0].uuid).await.unwrap();
// 	let requests = share_client.list_incoming_contact_requests().await.unwrap();
// 	assert_eq!(requests.len(), 1);
// 	assert_eq!(requests[0].email, client.email());

// 	let blocked = share_client.get_blocked_contacts().await.unwrap();
// 	assert_eq!(blocked.len(), 0);

// 	share_client
// 		.accept_contact_request(requests[0].uuid)
// 		.await
// 		.unwrap();
// 	let contacts = share_client.get_contacts().await.unwrap();
// 	assert_eq!(contacts.len(), 1);
// 	assert_eq!(contacts[0].email, client.email());
// }

/// Copying with shares: shared-in files and directories as sources, a shared and linked
/// destination, a destination below a shared one, and a destination shared while the copy
/// runs. It runs inside `share_dir` to reuse that test's contact setup (a long wait, and a
/// lock every CI leg contends for). Every fixture lives in the two accounts' per-test
/// directories, which are deleted, with their shares and links, when the test ends.
async fn copy_with_shares(
	client: &Arc<Client>,
	share_client: &Arc<Client>,
	test_dir: &RemoteDirectory,
	share_test_dir: &RemoteDirectory,
	contact: &Contact<'_>,
) {
	let client = client.clone();
	let share_client = share_client.clone();
	// the main account's fixtures
	let shared = client.create_dir(&test_dir.into(), "shared").await.unwrap();
	let nested = client
		.create_dir(&(&shared).into(), "nested")
		.await
		.unwrap();
	let top = upload(&client, &shared, "top.txt", b"top of the share").await;
	let deep = upload(&client, &nested, "deep.bin", &data(CHUNK_SIZE + 5, 3)).await;
	let solo = upload(&client, test_dir, "solo.txt", b"a shared file").await;
	let own = client.create_dir(&test_dir.into(), "own").await.unwrap();
	let own_file = upload(&client, &own, "own.txt", b"copied into shares").await;
	let dest = client.create_dir(&test_dir.into(), "dest").await.unwrap();
	let inner = client.create_dir(&(&dest).into(), "inner").await.unwrap();
	let later = client.create_dir(&test_dir.into(), "later").await.unwrap();

	client
		.share_dir::<fn(u64, Option<u64>)>(&shared, contact, None)
		.await
		.unwrap();
	client.share_file(&solo, contact).await.unwrap();
	client
		.share_dir::<fn(u64, Option<u64>)>(&dest, contact, None)
		.await
		.unwrap();
	let dest_link: DirPublicLink = client
		.public_link_dir::<fn(u64, Option<u64>)>(&dest, None)
		.await
		.unwrap()
		.try_into()
		.unwrap();
	// shares take a moment to show up on the other side
	tokio::time::sleep(std::time::Duration::from_secs(5)).await;

	// Shared-in sources: the share root, a directory and a file inside it, and a shared file.
	let (in_dirs, in_files) = share_client
		.list_in_shared_root::<fn(u64, Option<u64>)>(None)
		.await
		.unwrap();
	let shared_in = in_dirs
		.iter()
		.find(|d| d.get_dir().uuid() == shared.uuid())
		.unwrap()
		.clone();
	let role = shared_in.sharing_role().clone();
	let (sub_dirs, sub_files) = share_client
		.list_shared_dir::<fn(u64, Option<u64>)>(
			&DirType::Root(Cow::Borrowed(&shared_in)),
			&role,
			None,
		)
		.await
		.unwrap();
	let nested_in = sub_dirs
		.iter()
		.find(|d| d.get_dir().uuid() == nested.uuid())
		.unwrap()
		.clone();
	let top_in = sub_files
		.iter()
		.find(|f| f.uuid() == top.uuid())
		.unwrap()
		.clone();
	let solo_in = in_files
		.iter()
		.find(|f| f.uuid() == solo.uuid())
		.unwrap()
		.clone();
	let outcome = copy(
		&share_client,
		vec![
			CopySource::Dir(CopySourceDir::Shared(
				DirType::Root(Cow::Owned(shared_in)),
				role.clone(),
			)),
			CopySource::Dir(CopySourceDir::Shared(
				DirType::Dir(Cow::Owned(nested_in)),
				role,
			)),
			CopySource::File(top_in.into()),
			CopySource::File(solo_in.into()),
		],
		share_test_dir,
	)
	.await;
	outcome.result.unwrap();
	assert!(outcome.report.failures.is_empty());
	assert_eq!(outcome.report.top_level.len(), 4);
	let (_, copied) = contents(&share_client, share_test_dir).await;
	for path in [
		"shared/top.txt",
		"shared/nested/deep.bin",
		"nested/deep.bin",
		"top.txt",
		"solo.txt",
	] {
		assert!(copied.iter().any(|(p, _)| p == path), "{path} was copied");
	}
	for (path, original) in [
		("shared/nested/deep.bin", &deep),
		("top.txt", &top),
		("solo.txt", &solo),
	] {
		let (_, copy) = copied.iter().find(|(p, _)| p == path).unwrap();
		assert_eq!(
			share_client.download_file(copy).await.unwrap(),
			client.download_file(original).await.unwrap(),
			"{path} has the same contents"
		);
	}

	// One call mixing every source kind, into one destination: the copier's own file and
	// directory, a directory shared with it, and a directory read through a public link.
	let mine = share_client
		.create_dir(&share_test_dir.into(), "mine")
		.await
		.unwrap();
	let mine_inside = upload(&share_client, &mine, "mine.txt", b"the copier's own").await;
	let mine_file = upload(&share_client, share_test_dir, "mine-file.txt", b"own file").await;
	let own_link: DirPublicLink = client
		.public_link_dir::<fn(u64, Option<u64>)>(&own, None)
		.await
		.unwrap()
		.try_into()
		.unwrap();
	let own_info = share_client
		.get_unauthed()
		.get_dir_public_link_info(*own_link.uuid(), &own_link.key_string())
		.await
		.unwrap();
	let (in_dirs, _) = share_client
		.list_in_shared_root::<fn(u64, Option<u64>)>(None)
		.await
		.unwrap();
	let shared_in = in_dirs
		.iter()
		.find(|d| d.get_dir().uuid() == shared.uuid())
		.unwrap()
		.clone();
	let role = shared_in.sharing_role().clone();
	let mixed = share_client
		.create_dir(&share_test_dir.into(), "mixed")
		.await
		.unwrap();
	let outcome = copy(
		&share_client,
		vec![
			CopySource::File(mine_file.clone().into()),
			CopySource::Dir(CopySourceDir::Normal(mine.clone())),
			CopySource::Dir(CopySourceDir::Shared(
				DirType::Root(Cow::Owned(shared_in)),
				role,
			)),
			CopySource::Dir(CopySourceDir::Linked(
				DirType::Root(Cow::Owned(own_info.root.clone())),
				own_info.link.clone(),
			)),
		],
		&mixed,
	)
	.await;
	outcome.result.unwrap();
	let report = &outcome.report;
	assert!(report.failures.is_empty());
	assert!(report.skipped.is_empty());
	let mut top_names: Vec<&str> = report
		.top_level
		.iter()
		.map(|t| t.item.name().unwrap())
		.collect();
	top_names.sort_unstable();
	assert_eq!(top_names, ["mine", "mine-file.txt", "own", "shared"]);
	let (counts, totals) = (report.counts, report.totals);
	assert_eq!((counts.dirs_created, counts.dirs_failed), (totals.dirs, 0));
	assert_eq!((counts.files_done, counts.files_failed), (totals.files, 0));
	assert_eq!((counts.bytes_done, counts.bytes_failed), (totals.bytes, 0));
	assert_eq!(
		(
			counts.dirs_not_attempted,
			counts.files_not_attempted,
			counts.bytes_not_attempted
		),
		(0, 0, 0)
	);
	// mine, shared, shared/nested, own
	assert_eq!(totals.dirs, 4);
	// mine-file.txt, mine/mine.txt, shared/top.txt, shared/nested/deep.bin, own/own.txt
	assert_eq!(totals.files, 5);
	let (_, copied) = contents(&share_client, &mixed).await;
	let mut paths: Vec<&str> = copied.iter().map(|(p, _)| p.as_str()).collect();
	paths.sort_unstable();
	assert_eq!(
		paths,
		[
			"mine-file.txt",
			"mine/mine.txt",
			"own/own.txt",
			"shared/nested/deep.bin",
			"shared/top.txt",
		]
	);
	for (path, original, owner) in [
		("mine-file.txt", &mine_file, &share_client),
		("mine/mine.txt", &mine_inside, &share_client),
		("own/own.txt", &own_file, &client),
		("shared/nested/deep.bin", &deep, &client),
		("shared/top.txt", &top, &client),
	] {
		let (_, copy) = copied.iter().find(|(p, _)| p == path).unwrap();
		assert_eq!(
			share_client.download_file(copy).await.unwrap(),
			owner.download_file(original).await.unwrap(),
			"{path} has the same contents"
		);
	}

	// Shared (and linked) destinations: a copy into the shared directory and one into a
	// directory below it both reach the other account and the public link.
	for destination in [&dest, &inner] {
		copy(
			&client,
			vec![CopySource::Dir(CopySourceDir::Normal(own.clone()))],
			destination,
		)
		.await
		.result
		.unwrap();
	}
	let (in_dirs, _) = share_client
		.list_in_shared_root::<fn(u64, Option<u64>)>(None)
		.await
		.unwrap();
	let dest_in = in_dirs
		.iter()
		.find(|d| d.get_dir().uuid() == dest.uuid())
		.unwrap();
	let (_, seen_files) = share_client
		.list_shared_dir_recursive::<fn(u64, Option<u64>)>(
			&dest_in.into(),
			dest_in.sharing_role(),
			None,
		)
		.await
		.unwrap();
	let copies_seen = seen_files
		.iter()
		.filter(|f| f.name() == own_file.name())
		.count();
	assert_eq!(copies_seen, 2, "both copies are shared");
	let link_info = client
		.get_unauthed()
		.get_dir_public_link_info(*dest_link.uuid(), &dest_link.key_string())
		.await
		.unwrap();
	let (_, linked_files) = client
		.get_unauthed()
		.list_linked_dir_recursive::<fn(u64, Option<u64>)>(
			&(&link_info.root).into(),
			&link_info.link,
			None,
		)
		.await
		.unwrap();
	assert_eq!(
		linked_files
			.iter()
			.filter(|f| f.name() == own_file.name())
			.count(),
		2,
		"both copies are in the public link"
	);

	// A destination shared while the copy runs: paused once the copy's directory exists,
	// shared, then resumed; the finished copy reaches the other account.
	let recorder = Arc::new(Recorder::default());
	let (pause, pause_rx) = watch::channel(false);
	let running = tokio::spawn({
		let client = client.clone();
		let own = own.clone();
		let later = later.clone();
		let callback = PauseOnCreate {
			recorder: recorder.clone(),
			pause: pause.clone(),
		};
		async move {
			client
				.copy_items(
					vec![CopySource::Dir(CopySourceDir::Normal(own))],
					later.into(),
					CopyOptions::default(),
					callback,
					JobControl::new(Some(pause_rx), None),
				)
				.await
		}
	});
	wait_until_paused(&recorder).await;
	client
		.share_dir::<fn(u64, Option<u64>)>(&later, contact, None)
		.await
		.unwrap();
	pause.send_replace(false);
	running.await.unwrap().result.unwrap();
	tokio::time::sleep(std::time::Duration::from_secs(5)).await;
	let (in_dirs, _) = share_client
		.list_in_shared_root::<fn(u64, Option<u64>)>(None)
		.await
		.unwrap();
	let later_in = in_dirs
		.iter()
		.find(|d| d.get_dir().uuid() == later.uuid())
		.unwrap();
	let (_, seen_files) = share_client
		.list_shared_dir_recursive::<fn(u64, Option<u64>)>(
			&later_in.into(),
			later_in.sharing_role(),
			None,
		)
		.await
		.unwrap();
	assert!(
		seen_files.iter().any(|f| f.name() == own_file.name()),
		"items copied before the share was added are shared too"
	);
}

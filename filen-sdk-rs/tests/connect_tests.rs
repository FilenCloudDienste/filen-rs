// use std::sync::Arc;

// use chrono::{SubsecRound, Utc};
// use filen_macros::shared_test_runtime;
// use filen_sdk_rs::{
// 	auth::Client,
// 	connect::PasswordState,
// 	fs::{
// 		HasName, HasUUID,
// 		dir::meta::DirectoryMetaChanges,
// 		file::{
// 			meta::FileMetaChanges,
// 			traits::{HasFileInfo, HasRemoteFileInfo},
// 		},
// 	},
// 	sync::lock::ResourceLock,
// };
// use filen_types::api::v3::dir::link::PublicLinkExpiration;
// use futures::{StreamExt, stream::FuturesUnordered};

#[shared_test_runtime]
async fn dir_public_link() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let unauth_client = client.get_unauthed();

	let dir = client.create_dir(&test_dir.into(), "dir").await.unwrap();
	let mut sub_dir = client.create_dir(&(&dir).into(), "sub_dir").await.unwrap();

	let dir_file = client
		.make_file_builder("empty_dir.txt", *dir.uuid())
		.unwrap()
		.build();
	let dir_file = client.upload_file(dir_file.into(), b"").await.unwrap();

	let file = client
		.make_file_builder("a.txt", *sub_dir.uuid())
		.unwrap()
		.build();
	let file = client
		.upload_file(file.into(), b"Hello, world!")
		.await
		.unwrap();

	let empty_file = client
		.make_file_builder("empty.txt", *sub_dir.uuid())
		.unwrap()
		.build();
	let empty_file = client.upload_file(empty_file.into(), b"").await.unwrap();

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

	assert_eq!(&files, &vec![dir_file.clone()]);

	let (sub_dirs, sub_files) = unauth_client
		.list_linked_dir::<fn(u64, Option<u64>)>(&linked_sub_dir.into(), &info.link, None)
		.await
		.unwrap();
	assert_eq!(sub_dirs.len(), 0);
	assert_eq!(sub_files.len(), 2);
	assert!(sub_files.contains(&file));
	assert!(sub_files.contains(&empty_file));

	let (dirs, files) = unauth_client
		.list_linked_dir::<fn(u64, Option<u64>)>(&(&info.root).into(), &info.link, None)
		.await
		.unwrap();
	let linked_sub_dir = dirs.iter().find(|d| d.inner() == &sub_dir).unwrap();
	assert_eq!(&files, &vec![dir_file.clone()]);

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
		.make_file_builder("sub_sub_file.txt", *sub_dir.uuid())
		.unwrap()
		.build();
	let mut sub_sub_file = client
		.upload_file(sub_sub_file.into(), b"Hello, world!")
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
	assert!(sub_files.contains(&sub_sub_file));
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
	assert!(!sub_files.contains(&sub_sub_file));
}

use std::borrow::Cow;

use filen_macros::shared_test_runtime;
use filen_sdk_rs::{
	ErrorKind,
	connect::{DirPublicLink, PublicLinkSharedClientExt},
	fs::{
		HasName, HasUUID, categories::Shared, dir::meta::DirectoryMetaChanges,
		file::meta::FileMetaChanges,
	},
	io::{HasFileInfo, client_impl::IoSharedClientExt},
};
use filen_types::api::v3::dir::link::PublicLinkExpiration;

#[shared_test_runtime]
async fn file_public_link() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let share_resources = test_utils::SHARE_RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let share_client = &share_resources.client;
	let unauth_client = share_client.get_unauthed();

	let file = client
		.make_file_builder("a.txt", *test_dir.uuid())
		.unwrap()
		.build();
	let mut file = client
		.upload_file(file.into(), b"Hello, world!")
		.await
		.unwrap();

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
		.get_linked_file(link.uuid(), Cow::Borrowed(file_key.as_ref()), None)
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
		.get_linked_file(
			link.uuid(),
			Cow::Borrowed(file_key.as_ref()),
			Some(password),
		)
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
		.get_linked_file(
			link.uuid(),
			Cow::Borrowed(file_key.as_ref()),
			Some(password),
		)
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
	let dir_file = client
		.make_file_builder("a.txt", *dir.uuid())
		.unwrap()
		.build();
	let mut dir_file = client
		.upload_file(dir_file.into(), b"Hello, world!")
		.await
		.unwrap();
	let file = client
		.make_file_builder("a.txt", *sub_dir.uuid())
		.unwrap()
		.build();
	let sub_file = client.upload_file(file.into(), b"").await.unwrap();

	let (_lock1, _lock2, num_shared_out, num_shared_in) =
		test_utils::set_up_contact(client, share_client).await;

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
	assert_eq!(shared_dirs_out.len(), num_shared_out + 1);
	assert!(
		shared_dirs_out
			.iter()
			.any(|d| d.get_dir().uuid() == dir.uuid())
	);

	let (shared_dirs_in, _) = share_client
		.list_in_shared_root::<fn(u64, Option<u64>)>(None)
		.await
		.unwrap();
	assert_eq!(shared_dirs_in.len(), num_shared_in + 1);

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
	assert_eq!(&shared_files_in[0], &dir_file);
	assert_eq!(
		client.download_file(&shared_files_out[0]).await.unwrap(),
		b"Hello, world!"
	);
	assert_eq!(&shared_files_out[0], &dir_file);

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
	assert!(files.contains(&dir_file));
	assert!(files.contains(&sub_file));

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
	assert!(files.contains(&dir_file));

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
	assert_eq!(shared_dirs_in.len(), num_shared_in + 1);

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

	assert!(files.contains(&sub_file));
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
// 		.upload_file(file.into(), b"Hello, world!")
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

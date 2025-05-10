use filen_sdk_rs::{
	auth::Client,
	connect::PasswordState,
	fs::{
		HasName, HasUUID,
		file::traits::{HasFileInfo, HasRemoteFileInfo},
	},
	sync::lock::ResourceLock,
};
use filen_types::api::v3::dir::link::PublicLinkExpiration;
use futures::{AsyncReadExt, AsyncWriteExt};

mod test_utils;

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn dir_public_link() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let dir = client.create_dir(test_dir, "dir").await.unwrap();
	let sub_dir = client.create_dir(&dir, "sub_dir").await.unwrap();

	let dir_file = client.make_file_builder("empty_dir.txt", &dir).build();
	let mut writer = client.get_file_writer(dir_file);
	writer.close().await.unwrap();
	let dir_file = writer.into_remote_file().unwrap();

	let file = client.make_file_builder("a.txt", &sub_dir).build();
	let mut writer = client.get_file_writer(file);
	writer.write_all(b"Hello, world!").await.unwrap();
	writer.close().await.unwrap();
	let file = writer.into_remote_file().unwrap();

	let empty_file = client.make_file_builder("empty.txt", &sub_dir).build();
	let mut writer = client.get_file_writer(empty_file);
	writer.close().await.unwrap();
	let empty_file = writer.into_remote_file().unwrap();

	let mut link = client.public_link_dir(&dir).await.unwrap();

	let found_link = client.get_dir_link_status(&dir).await.unwrap().unwrap();
	assert_eq!(
		&link, &found_link,
		"get_dir_link_status didn't match created link"
	);

	let (dirs, files) = client.list_linked_dir(&dir, &link).await.unwrap();
	assert_eq!(&dirs, &vec![sub_dir.clone()]);
	assert_eq!(&files, &vec![dir_file.clone()]);

	let (sub_dirs, sub_files) = client.list_linked_dir(&sub_dir, &found_link).await.unwrap();
	assert_eq!(sub_dirs.len(), 0);
	assert_eq!(sub_files.len(), 2);
	assert!(sub_files.contains(&file));
	assert!(sub_files.contains(&empty_file));

	let (dirs, files) = client.list_linked_dir(&dir, &found_link).await.unwrap();
	assert_eq!(&dirs, &vec![sub_dir.clone()]);
	assert_eq!(&files, &vec![dir_file.clone()]);

	let password = "some_password";
	link.set_password(password.to_string());
	link.set_expiration(PublicLinkExpiration::OneHour);
	client.update_dir_link(&dir, &link).await.unwrap();

	let found_link = client.get_dir_link_status(&dir).await.unwrap().unwrap();
	assert_eq!(
		&link.uuid(),
		&found_link.uuid(),
		"get_dir_link_status didn't match created link"
	);

	let sub_sub_dir = client.create_dir(&sub_dir, "sub_sub_dir").await.unwrap();
	let sub_sub_file = client
		.make_file_builder("sub_sub_file.txt", &sub_dir)
		.build();
	let mut writer = client.get_file_writer(sub_sub_file);
	writer.write_all(b"Hello, world!").await.unwrap();
	writer.close().await.unwrap();
	let sub_sub_file = writer.into_remote_file().unwrap();
	let (sub_dirs, sub_files) = client.list_linked_dir(&sub_dir, &link).await.unwrap();
	assert_eq!(sub_dirs.len(), 1);
	assert!(sub_dirs.contains(&sub_sub_dir));
	assert!(sub_files.contains(&sub_sub_file));
	assert_eq!(sub_files.len(), 3);

	client.trash_dir(&sub_sub_dir).await.unwrap();
	client.trash_file(&sub_sub_file).await.unwrap();

	let (sub_dirs, sub_files) = client.list_linked_dir(&sub_dir, &link).await.unwrap();
	assert_eq!(sub_dirs.len(), 0);
	assert_eq!(sub_files.len(), 2);
	assert!(!sub_files.contains(&sub_sub_file));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn file_public_link() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let file = client.make_file_builder("a.txt", test_dir).build();
	let mut writer = client.get_file_writer(file);
	writer.write_all(b"Hello, world!").await.unwrap();
	writer.close().await.unwrap();
	let file = writer.into_remote_file().unwrap();

	let mut link = client.public_link_file(&file).await.unwrap();
	let found_link = client.get_file_link_status(&file).await.unwrap().unwrap();
	assert_eq!(
		&link.uuid(),
		&found_link.uuid(),
		"get_file_link_status didn't match created link"
	);

	let password = "some_password";

	link.set_password(password.to_string());
	link.set_expiration(PublicLinkExpiration::OneHour);
	client.update_file_link(&file, &link).await.unwrap();
	let found_link = client.get_file_link_status(&file).await.unwrap().unwrap();
	let mut cloned_found_link = found_link.clone();
	cloned_found_link.set_password(password.to_string());
	assert_eq!(&link, &cloned_found_link);

	let linked_info = client.get_linked_file(&link).await.unwrap();
	assert_eq!(linked_info.uuid, file.uuid());
	assert_eq!(linked_info.name, file.name());
	assert_eq!(linked_info.mime, file.mime());
	assert_eq!(
		&PasswordState::Hashed(linked_info.hashed_password.clone().unwrap()),
		found_link.password()
	);
	assert_eq!(linked_info.chunks, file.chunks());
	assert_eq!(linked_info.size, file.size());
	assert_eq!(linked_info.region, file.region());
	assert_eq!(linked_info.bucket, file.bucket());

	let found_linked_info = client.get_linked_file(&found_link).await.unwrap();
	assert_eq!(found_linked_info, linked_info);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn contact_interactions() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;

	let share_resources = test_utils::SHARE_RESOURCES.get_resources().await;
	let share_client = &share_resources.client;

	let _lock = client
		.acquire_lock_with_default("test:contact")
		.await
		.unwrap();
	let _lock = share_client
		.acquire_lock_with_default("test:contact")
		.await
		.unwrap();

	// clean up all existing contacts
	for contact in client.get_contacts().await.unwrap() {
		client.delete_contact(contact.uuid).await.unwrap();
	}
	for contact in share_client.get_contacts().await.unwrap() {
		share_client.delete_contact(contact.uuid).await.unwrap();
	}
	for contact in client.list_incoming_contact_requests().await.unwrap() {
		client.deny_contact_request(contact.uuid).await.unwrap();
	}
	for contact in share_client.list_incoming_contact_requests().await.unwrap() {
		share_client
			.deny_contact_request(contact.uuid)
			.await
			.unwrap();
	}
	for contact in client.list_outgoing_contact_requests().await.unwrap() {
		client.cancel_contact_request(contact.uuid).await.unwrap();
	}
	for contact in share_client.list_outgoing_contact_requests().await.unwrap() {
		share_client
			.cancel_contact_request(contact.uuid)
			.await
			.unwrap();
	}

	assert_eq!(
		client.list_outgoing_contact_requests().await.unwrap().len(),
		0
	);
	client
		.send_contact_request(share_client.email())
		.await
		.unwrap();
	let out_requests = client.list_outgoing_contact_requests().await.unwrap();

	assert_eq!(out_requests.len(), 1);
	assert_eq!(out_requests[0].email, share_client.email());

	let in_requests = share_client.list_incoming_contact_requests().await.unwrap();
	assert_eq!(in_requests.len(), 1);
	assert_eq!(in_requests[0].email, client.email());

	share_client
		.accept_contact_request(in_requests[0].uuid)
		.await
		.unwrap();

	let in_requests = client.list_incoming_contact_requests().await.unwrap();
	assert_eq!(in_requests.len(), 0);
	let out_requests = client.list_outgoing_contact_requests().await.unwrap();
	assert_eq!(out_requests.len(), 0);

	let contacts = client.get_contacts().await.unwrap();
	assert_eq!(contacts.len(), 1);
	assert_eq!(contacts[0].email, share_client.email());

	let share_contacts = share_client.get_contacts().await.unwrap();
	assert_eq!(share_contacts.len(), 1);
	assert_eq!(share_contacts[0].email, client.email());

	client.delete_contact(contacts[0].uuid).await.unwrap();

	let share_contacts = share_client.get_contacts().await.unwrap();
	assert_eq!(share_contacts.len(), 0);
	let contacts = client.get_contacts().await.unwrap();
	assert_eq!(contacts.len(), 0);
}

async fn set_up_contact<'a>(
	client: &'a Client,
	share_client: &'a Client,
) -> (ResourceLock<'a>, ResourceLock<'a>) {
	let lock1 = client
		.acquire_lock_with_default("test:contact")
		.await
		.unwrap();
	let lock2 = share_client
		.acquire_lock_with_default("test:contact")
		.await
		.unwrap();
	let _ = futures::join!(
		async {
			for contact in client.get_contacts().await.unwrap() {
				client.delete_contact(contact.uuid).await.unwrap();
			}
		},
		async {
			for contact in client.list_outgoing_contact_requests().await.unwrap() {
				client.delete_contact(contact.uuid).await.unwrap();
			}
		},
		async {
			for contact in client.list_incoming_contact_requests().await.unwrap() {
				client.delete_contact(contact.uuid).await.unwrap();
			}
		}
	);

	let request_uuid = client
		.send_contact_request(share_client.email())
		.await
		.unwrap();

	share_client
		.accept_contact_request(request_uuid)
		.await
		.unwrap();
	(lock1, lock2)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn share_dir() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let share_resources = test_utils::SHARE_RESOURCES.get_resources().await;
	let share_client = &share_resources.client;

	let dir = client.create_dir(test_dir, "dir").await.unwrap();
	let sub_dir = client.create_dir(&dir, "sub_dir").await.unwrap();
	let dir_file = client.make_file_builder("a.txt", &dir).build();
	let mut writer = client.get_file_writer(dir_file);
	writer.write_all(b"Hello, world!").await.unwrap();
	writer.close().await.unwrap();
	let dir_file = writer.into_remote_file().unwrap();
	let file = client.make_file_builder("a.txt", &sub_dir).build();
	let mut writer = client.get_file_writer(file);
	writer.close().await.unwrap();

	let _locks = set_up_contact(client, share_client).await;

	let contacts = client.get_contacts().await.unwrap();
	assert_eq!(contacts.len(), 1);
	assert_eq!(contacts[0].email, share_client.email());
	let share_user = client.make_user_from_contact(&contacts[0]).await.unwrap();
	client.share_dir(&dir, &share_user).await.unwrap();

	let (shared_dirs_out, _) = client.list_out_shared(None).await.unwrap();
	assert_eq!(shared_dirs_out.len(), 1);
	assert!(
		shared_dirs_out
			.iter()
			.any(|d| d.get_dir().uuid() == dir.uuid())
	);

	let (shared_dirs_in, _) = share_client.list_in_shared().await.unwrap();
	assert_eq!(shared_dirs_in.len(), 1);

	assert_eq!(shared_dirs_in[0].get_dir(), shared_dirs_out[0].get_dir());

	let (shared_dirs_out, shared_files_out) =
		client.list_out_shared_dir(&dir, &share_user).await.unwrap();
	let (shared_dirs_in, shared_files_in) = share_client.list_in_shared_dir(&dir).await.unwrap();

	assert_eq!(shared_dirs_out.len(), 1);
	assert_eq!(shared_dirs_in.len(), 1);
	assert_eq!(shared_files_out.len(), 1);
	assert_eq!(shared_files_in.len(), 1);

	assert_eq!(shared_dirs_out[0].get_dir(), shared_dirs_in[0].get_dir());
	assert_eq!(
		shared_files_out[0].get_file(),
		shared_files_in[0].get_file()
	);

	let mut in_reader = share_client.get_file_reader(shared_files_in[0].get_file());
	let mut buf = Vec::new();
	in_reader.read_to_end(&mut buf).await.unwrap();
	assert_eq!(buf, b"Hello, world!");
	assert_eq!(shared_files_in[0].get_file(), &dir_file);
	let mut out_reader = client.get_file_reader(shared_files_out[0].get_file());
	let mut buf = Vec::new();
	out_reader.read_to_end(&mut buf).await.unwrap();
	assert_eq!(buf, b"Hello, world!");
	assert_eq!(shared_files_out[0].get_file(), &dir_file);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn share_file() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let share_resources = test_utils::SHARE_RESOURCES.get_resources().await;
	let share_client = &share_resources.client;

	let _locks = set_up_contact(client, share_client).await;

	let file = client.make_file_builder("a.txt", test_dir).build();
	let mut writer = client.get_file_writer(file);
	writer.write_all(b"Hello, world!").await.unwrap();
	writer.close().await.unwrap();
	let file = writer.into_remote_file().unwrap();

	let contacts = client.get_contacts().await.unwrap();
	assert_eq!(contacts.len(), 1);
	let contact = &contacts[0];
	let share_user = client.make_user_from_contact(contact).await.unwrap();

	client.share_file(&file, &share_user).await.unwrap();

	let (_, shared_files_out) = client.list_out_shared(None).await.unwrap();
	assert_eq!(shared_files_out.len(), 1);
	let shared_file = shared_files_out[0].get_file();
	assert_eq!(shared_file, &file);
	let (_, shared_files_in) = share_client.list_in_shared().await.unwrap();
	assert_eq!(shared_files_in.len(), 1);
	let shared_file = shared_files_in[0].get_file();
	assert_eq!(shared_file, &file);
	let mut in_reader = share_client.get_file_reader(shared_file);
	let mut buf = Vec::new();
	in_reader.read_to_end(&mut buf).await.unwrap();
	assert_eq!(buf, b"Hello, world!");
	let mut out_reader = client.get_file_reader(shared_file);
	let mut buf = Vec::new();
	out_reader.read_to_end(&mut buf).await.unwrap();
	assert_eq!(buf, b"Hello, world!");
}

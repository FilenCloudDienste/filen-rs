use filen_macros::shared_test_runtime;
use filen_sdk_rs::{auth::Client, chats::Chat};

#[shared_test_runtime]
async fn conversation_creation() {
	let client = test_utils::RESOURCES.client().await;
	let _lock = client
		.acquire_lock_with_default("test:chats")
		.await
		.unwrap();

	let chat = client.create_chat(&[]).await.unwrap();

	let chats = client.list_chats().await.unwrap();

	let found = chats.into_iter().find(|c| c.uuid() == chat.uuid()).unwrap();
	assert_eq!(found, chat);
}

#[shared_test_runtime]
async fn conversation_deletion() {
	let client = test_utils::RESOURCES.client().await;
	let _lock = client
		.acquire_lock_with_default("test:chats")
		.await
		.unwrap();

	let chat = client.create_chat(&[]).await.unwrap();
	let chats = client.list_chats().await.unwrap();
	let found = chats.into_iter().find(|c| c.uuid() == chat.uuid()).unwrap();
	client.delete_chat(found).await.unwrap();
	let chats = client.list_chats().await.unwrap();
	let found = chats.into_iter().find(|c| c.uuid() == chat.uuid());
	assert!(found.is_none());
}

#[shared_test_runtime]
async fn conversation_renaming() {
	let client = test_utils::RESOURCES.client().await;
	let _lock = client
		.acquire_lock_with_default("test:chats")
		.await
		.unwrap();

	let mut chat = client.create_chat(&[]).await.unwrap();
	let chats = client.list_chats().await.unwrap();
	let found = chats.into_iter().find(|c| c.uuid() == chat.uuid()).unwrap();
	assert_eq!(chat, found);
	let new_name = "My new chat name";
	client
		.rename_chat(&mut chat, new_name.to_owned())
		.await
		.unwrap();
	assert_eq!(chat.name(), Some(new_name));
	let chats = client.list_chats().await.unwrap();
	let found = chats.into_iter().find(|c| c.uuid() == chat.uuid()).unwrap();
	assert_eq!(chat, found);
}

#[shared_test_runtime]
async fn conversation_muting() {
	let client = test_utils::RESOURCES.client().await;
	let _lock = client
		.acquire_lock_with_default("test:chats")
		.await
		.unwrap();

	let mut chat = client.create_chat(&[]).await.unwrap();
	let fetched = client.get_chat(chat.uuid()).await.unwrap().unwrap();
	assert_eq!(chat, fetched);
	assert!(!chat.muted());
	client.mute_chat(&mut chat, true).await.unwrap();
	assert!(chat.muted());
	let fetched = client.get_chat(chat.uuid()).await.unwrap().unwrap();
	assert_eq!(chat, fetched);
	client.mute_chat(&mut chat, false).await.unwrap();
	assert!(!chat.muted());
	let fetched = client.get_chat(chat.uuid()).await.unwrap().unwrap();
	assert_eq!(chat, fetched);
}
async fn make_chat(client: &Client, share_client: &Client) -> Chat {
	if let Ok(uuid) = client.send_contact_request(share_client.email()).await {
		let _ = share_client.accept_contact_request(uuid).await;
	}

	let chats = client.list_chats().await.unwrap();
	for chat in chats {
		let _ = client.leave_chat(&chat).await;
		let _ = client.delete_chat(chat).await;
	}
	let chats = share_client.list_chats().await.unwrap();
	for chat in chats {
		let _ = share_client.leave_chat(&chat).await;
		let _ = share_client.delete_chat(chat).await;
	}

	let contact = client
		.get_contacts()
		.await
		.unwrap()
		.into_iter()
		.find(|c| c.email == share_client.email())
		.unwrap();

	client.create_chat(&[contact]).await.unwrap()
}

#[shared_test_runtime]
async fn conversation_participant_management() {
	let client = test_utils::RESOURCES.client().await;
	let share_client = test_utils::SHARE_RESOURCES.client().await;
	let _lock = client
		.acquire_lock_with_default("test:chats")
		.await
		.unwrap();
	let _lock1 = share_client
		.acquire_lock_with_default("test:chats")
		.await
		.unwrap();

	let chats = client.list_chats().await.unwrap();
	for chat in chats {
		let _ = client.leave_chat(&chat).await;
		let _ = client.delete_chat(chat).await;
	}
	let chats = share_client.list_chats().await.unwrap();
	for chat in chats {
		let _ = share_client.leave_chat(&chat).await;
		let _ = share_client.delete_chat(chat).await;
	}
	let chats = client.list_chats().await.unwrap();
	assert!(chats.is_empty());

	let mut chat = client.create_chat(&[]).await.unwrap();

	if let Ok(uuid) = client.send_contact_request(share_client.email()).await {
		let _ = share_client.accept_contact_request(uuid).await;
	}

	let contact = client
		.get_contacts()
		.await
		.unwrap()
		.into_iter()
		.find(|c| c.email == share_client.email())
		.unwrap();

	client
		.add_chat_participant(&mut chat, &contact)
		.await
		.unwrap();

	let fetched = client.get_chat(chat.uuid()).await.unwrap().unwrap();

	assert_eq!(chat, fetched);
	assert_eq!(chat.participants().len(), 2);

	client
		.remove_chat_participant(&mut chat, &contact)
		.await
		.unwrap();
	let fetched = client.get_chat(chat.uuid()).await.unwrap().unwrap();
	assert_eq!(chat, fetched);
	assert_eq!(chat.participants().len(), 1);
}

#[shared_test_runtime]
async fn chat_msgs() {
	let client = test_utils::RESOURCES.client().await;
	let share_client = test_utils::SHARE_RESOURCES.client().await;
	let _lock = client
		.acquire_lock_with_default("test:chats")
		.await
		.unwrap();
	let _lock1 = share_client
		.acquire_lock_with_default("test:chats")
		.await
		.unwrap();

	let mut chat = make_chat(&client, &share_client).await;

	client
		.send_chat_message(&mut chat, "Hello!".to_string(), None)
		.await
		.unwrap();

	let msgs = client.list_messages(&chat).await.unwrap();

	assert_eq!(msgs.len(), 1);
	assert_eq!(*chat.last_message().unwrap(), msgs[0]);

	let mut shared_chat = share_client.get_chat(chat.uuid()).await.unwrap().unwrap();

	let shared_msgs = share_client.list_messages(&shared_chat).await.unwrap();

	assert_eq!(shared_msgs.len(), 1);
	assert_eq!(shared_msgs[0], *chat.last_message().unwrap());

	share_client
		.send_chat_message(
			&mut shared_chat,
			"Hi!".to_string(),
			Some(shared_msgs[0].clone().into_inner()),
		)
		.await
		.unwrap();

	let shared_msgs = share_client.list_messages(&shared_chat).await.unwrap();

	assert_eq!(shared_msgs.len(), 2);
	assert_eq!(shared_msgs[1], *shared_chat.last_message().unwrap());

	let mut msgs = client.list_messages(&chat).await.unwrap();
	assert_eq!(msgs.len(), 2);
	assert_eq!(shared_msgs, msgs);

	let edited_msg = "Edited!";

	client
		.edit_message(&chat, &mut msgs[0], edited_msg.to_string())
		.await
		.unwrap();

	assert_eq!(msgs[0].message(), Some(edited_msg));

	let fetched_msgs = client.list_messages(&chat).await.unwrap();
	// will fail because the reply_to cannot easily be updated after an edit
	// assert_eq!(fetched_msgs, msgs);
	assert_eq!(fetched_msgs[0], msgs[0]);

	client.disable_message_embed(&mut msgs[0]).await.unwrap();
	assert!(msgs[0].embed_disabled());

	client
		.delete_message(&mut chat, &msgs.remove(0))
		.await
		.unwrap();
	let fetched_msgs = client.list_messages(&chat).await.unwrap();
	assert_eq!(fetched_msgs.len(), 1);

	let unread = client.get_chat_unread_count(&chat).await.unwrap();
	assert_eq!(unread, 1);
	let all_unread = client.get_all_chats_unread_count().await.unwrap();
	assert_eq!(all_unread, 1);

	client.update_chat_online_status(&mut chat).await.unwrap();
	let fetched = client.get_chat(chat.uuid()).await.unwrap().unwrap();
	assert_eq!(chat, fetched);

	assert_eq!(chat.last_focus(), None);

	let mut chats = [chat];
	let before = chrono::Utc::now() - chrono::Duration::seconds(1);
	client
		.update_last_chat_focus_times_now(&mut chats)
		.await
		.unwrap();

	let chat = chats.into_iter().next().unwrap();
	let last_focus = chat.last_focus().unwrap();
	assert!(last_focus >= before);
	assert!(last_focus <= chrono::Utc::now());
	let fetched = client.get_chat(chat.uuid()).await.unwrap().unwrap();
	assert_eq!(chat, fetched);
}

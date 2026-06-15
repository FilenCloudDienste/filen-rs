use std::sync::Arc;

use filen_macros::shared_test_runtime;
use filen_sdk_rs::{auth::Client, chats::Chat, sync::lock::ResourceLock};
use filen_types::traits::CowHelpers;
use test_utils::set_up_contact;

/// Janitor (mirrors `client_tests::cleanup_test_dirs`): sweep stale conversations that prior runs
/// left behind — the per-test [`ChatGuard`] only clears when a chat test actually runs, so a run
/// that filters the chat tests out never cleans up, and leaked conversations feed the
/// `conversations/create` rate limit. Age-filtered (older than a day) so a concurrent run's fresh
/// conversations are never deleted out from under it; takes the same `test:chats` lock so it
/// never races a live chat test.
#[shared_test_runtime]
async fn cleanup_test_chats() {
	let client = test_utils::RESOURCES.client().await;
	let _lock = lock_chat(&client).await;

	let now = chrono::Utc::now();
	let chats = client.list_chats().await.unwrap();
	for chat in chats {
		if now - chat.created() > chrono::Duration::days(1) {
			let _ = client.leave_chat(&chat).await;
			let _ = client.delete_chat(chat).await;
		}
	}
}

#[shared_test_runtime]
async fn conversation_creation() {
	let client = test_utils::RESOURCES.client().await;
	let _guard = ChatGuard::acquire(&client).await;

	let chat = retry_rate_limited(|| client.create_chat(&[])).await;

	let chats = client.list_chats().await.unwrap();

	let found = chats.into_iter().find(|c| c.uuid() == chat.uuid()).unwrap();
	assert_eq!(found, chat);
}

#[shared_test_runtime]
async fn conversation_deletion() {
	let client = test_utils::RESOURCES.client().await;
	let _guard = ChatGuard::acquire(&client).await;

	let chat = retry_rate_limited(|| client.create_chat(&[])).await;
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
	let _guard = ChatGuard::acquire(&client).await;

	let mut chat = retry_rate_limited(|| client.create_chat(&[])).await;
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
	let _guard = ChatGuard::acquire(&client).await;

	let mut chat = retry_rate_limited(|| client.create_chat(&[])).await;
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

async fn lock_chat(client: &Client) -> Arc<ResourceLock> {
	client
		.acquire_lock_with_default("test:chats")
		.await
		.unwrap()
}

/// Delete every conversation `client` owns or participates in (best-effort). Chat tests
/// historically leaked the conversations they created — only the deletion test cleaned up — and
/// the server rate-limits `conversations/create` once enough accumulate on the shared account.
async fn clear_chats(client: &Client) {
	if let Ok(chats) = client.list_chats().await {
		for chat in chats {
			let _ = client.leave_chat(&chat).await;
			let _ = client.delete_chat(chat).await;
		}
	}
}

/// RAII clean-slate guard for one client's chats: it holds the account-wide `test:chats` lock (so
/// chat tests never overlap) and deletes every existing conversation on acquire, so the shared
/// account never accumulates beyond one in-flight test's worth — the root cause of the
/// `rate_limited` failures on `conversations/create`.
struct ChatGuard {
	_lock: Arc<ResourceLock>,
}

impl ChatGuard {
	async fn acquire(client: &Client) -> Self {
		let lock = lock_chat(client).await;
		clear_chats(client).await;
		Self { _lock: lock }
	}
}

/// `true` if `error` is the server's `rate_limited` response (it maps to the generic
/// `ErrorKind::Server`, so the code string is the only discriminator).
fn is_rate_limited(error: &filen_sdk_rs::Error) -> bool {
	matches!(
		error.downcast_ref::<filen_types::error::ResponseError>(),
		Some(filen_types::error::ResponseError::ApiError { code, .. })
			if code.as_deref() == Some("rate_limited")
	)
}

/// Run `op`, retrying on `rate_limited` with backoff (up to ~60s). The shared account's
/// conversation backlog can be deep enough on the first post-cleanup run that `create_chat` is
/// briefly throttled while [`clear_chats`] drains it.
async fn retry_rate_limited<T, F, Fut>(mut op: F) -> T
where
	F: FnMut() -> Fut,
	Fut: std::future::Future<Output = Result<T, filen_sdk_rs::Error>>,
{
	let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
	loop {
		match op().await {
			Ok(value) => return value,
			Err(e) if is_rate_limited(&e) && std::time::Instant::now() < deadline => {
				eprintln!("rate limited; retrying: {e}");
				tokio::time::sleep(std::time::Duration::from_secs(2)).await;
			}
			Err(e) => panic!("chat operation failed: {e:?}"),
		}
	}
}

async fn lock_chats(
	client: &Client,
	share_client: &Client,
) -> (
	Arc<ResourceLock>,
	Arc<ResourceLock>,
	Arc<ResourceLock>,
	Arc<ResourceLock>,
) {
	let lock1 = lock_chat(client).await;
	let lock2 = lock_chat(share_client).await;

	let (lock3, lock4, _, _) = set_up_contact(client, share_client).await;
	(lock1, lock2, lock3, lock4)
}

async fn make_chat(
	client: &Client,
	share_client: &Client,
) -> (
	(
		Arc<ResourceLock>,
		Arc<ResourceLock>,
		Arc<ResourceLock>,
		Arc<ResourceLock>,
	),
	Chat,
) {
	let locks = lock_chats(client, share_client).await;

	clear_chats(client).await;
	clear_chats(share_client).await;

	let contact = client
		.get_contacts()
		.await
		.unwrap()
		.into_iter()
		.find(|c| c.email == share_client.email())
		.unwrap();

	let chat = retry_rate_limited(|| client.create_chat(std::slice::from_ref(&contact))).await;
	(locks, chat)
}

#[shared_test_runtime]
async fn conversation_participant_management() {
	let client = test_utils::RESOURCES.client().await;
	let share_client = test_utils::SHARE_RESOURCES.client().await;
	let _locks = lock_chats(&client, &share_client).await;

	clear_chats(&client).await;
	clear_chats(&share_client).await;
	let chats = client.list_chats().await.unwrap();
	assert!(chats.is_empty());

	let mut chat = retry_rate_limited(|| client.create_chat(&[])).await;

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
		.remove_chat_participant(&mut chat, contact.user_id)
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

	let (_locks, mut chat) = make_chat(&client, &share_client).await;

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
	// all_unread must include at least the unread from this chat; other chats from prior
	// test runs may contribute additional unread counts on the server
	assert!(all_unread >= unread);

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

#[shared_test_runtime]
async fn user_info() {
	let client = test_utils::RESOURCES.client().await;

	let info = client.get_user_info().await.unwrap();
	assert_eq!(info.email, client.email());
}

#[shared_test_runtime]
#[ignore = "Currently the server doesn't block messages from blocked contacts, should be re-enabled once that is implemented"]
async fn test_blocking() {
	let share_client = test_utils::SHARE_RESOURCES.client().await;
	let client = test_utils::RESOURCES.client().await;
	let _locks = set_up_contact(&client, &share_client).await;

	let contacts = share_client.get_contacts().await.unwrap();
	let contact = contacts.iter().find(|c| c.email == client.email()).unwrap();

	let mut chat = share_client
		.create_chat(&[contact.as_borrowed_cow()])
		.await
		.unwrap();

	let unblocked_msg = "hello";

	share_client
		.send_chat_message(&mut chat, unblocked_msg.to_string(), None)
		.await
		.unwrap();

	let client_chat = client.get_chat(chat.uuid()).await.unwrap().unwrap();
	assert!(
		client_chat
			.last_message()
			.is_some_and(|msg| msg.message().unwrap() == unblocked_msg)
	);

	client.block_contact(share_client.email()).await.unwrap();

	tokio::time::sleep(std::time::Duration::from_secs(30)).await;

	let blocked = client.get_blocked_contacts().await.unwrap();
	assert!(blocked.iter().any(|c| c.email == share_client.email()));

	let blocked_msg = "goodbye";

	share_client
		.send_chat_message(&mut chat, blocked_msg.to_string(), None)
		.await
		.unwrap();

	let msgs = client.list_messages(&client_chat).await.unwrap();
	println!("msgs:D {msgs:#?}");
	assert!(msgs.iter().all(|msg| msg.message().unwrap() != blocked_msg));
}

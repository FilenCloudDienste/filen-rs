use std::{borrow::Cow, time::Duration};

use filen_macros::shared_test_runtime;
use filen_sdk_rs::{
	ErrorKind,
	fs::{
		HasUUID, NonRootFSObject,
		dir::meta::DirectoryMetaChanges,
		file::meta::{FileMeta, FileMetaChanges},
	},
	socket::DecryptedSocketEvent,
};
use filen_types::{
	api::v3::{chat::typing::ChatTypingType, dir::color::DirColor},
	crypto::MaybeEncrypted,
	traits::CowHelpersExt,
};
use test_utils::{await_event, await_map_event, await_not_event};

#[shared_test_runtime]
async fn test_websocket_auth() {
	let client = test_utils::RESOURCES.client().await;

	let (events_sender, mut events_receiver) = tokio::sync::mpsc::unbounded_channel();

	let _handle = client
		.add_event_listener(
			Box::new(move |event| {
				let _ = events_sender.send(event.to_owned_cow());
			}),
			None,
		)
		.await
		.unwrap();
	await_event(
		&mut events_receiver,
		|event| *event == DecryptedSocketEvent::AuthSuccess,
		Duration::from_secs(20),
		"authSuccess",
	)
	.await;
}

#[shared_test_runtime]
async fn test_websocket_event_filtering() {
	let client = test_utils::RESOURCES.client().await;

	let (events_sender, mut events_receiver) = tokio::sync::mpsc::unbounded_channel();

	let handle1_fut = client.add_event_listener(
		Box::new(move |event| {
			let _ = events_sender.send(event.to_owned_cow());
		}),
		None,
	);

	let (filtered_events_sender, mut filtered_events_receiver) =
		tokio::sync::mpsc::unbounded_channel();

	let handle2_fut = client.add_event_listener(
		Box::new(move |event| {
			let _ = filtered_events_sender.send(event.to_owned_cow());
		}),
		Some(vec![Cow::Borrowed("authSuccess")]),
	);

	let (_handle1, _handle2) = tokio::try_join!(handle1_fut, handle2_fut).unwrap();

	await_event(
		&mut events_receiver,
		|event| *event == DecryptedSocketEvent::AuthSuccess,
		Duration::from_secs(20),
		"authSuccess",
	)
	.await;

	await_not_event(
		&mut filtered_events_receiver,
		|event| *event != DecryptedSocketEvent::AuthSuccess,
		Duration::from_secs(1),
	)
	.await;
}

#[shared_test_runtime]
async fn test_websocket_bad_auth() {
	let client = test_utils::RESOURCES.client().await;

	let (events_sender, mut events_receiver) = tokio::sync::mpsc::unbounded_channel();

	let mut stringified = client.to_stringified();
	stringified.api_key = "invalid_api_key".to_string();

	let unauthed = client.get_unauthed();

	let client = unauthed.from_stringified(stringified).unwrap();
	let result = client
		.add_event_listener(
			Box::new(move |event| {
				let _ = events_sender.send(event.to_owned_cow());
			}),
			None,
		)
		.await;

	match result {
		Ok(_) => panic!("Expected error when adding listener with invalid API key"),
		Err(e) if e.kind() == ErrorKind::Unauthenticated => (),
		Err(e) => panic!("Unexpected error kind: {:?}", e),
	}

	await_event(
		&mut events_receiver,
		|event| *event == DecryptedSocketEvent::AuthFailed,
		Duration::from_secs(5),
		"authFailed",
	)
	.await;
}

#[shared_test_runtime]
async fn test_websocket_file_events() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let dir = &resources.dir;
	let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
	let _handle = client
		.add_event_listener(
			Box::new(move |event| {
				let _ = sender.send(event.to_owned_cow());
			}),
			None,
		)
		.await
		.unwrap();

	let file_a = client.make_file_builder("file_a.txt", dir).build();
	let mut file_a = client
		.upload_file(file_a.into(), b"file a contents")
		.await
		.unwrap();

	let event = await_map_event(
		&mut receiver,
		|event| match event {
			DecryptedSocketEvent::FileNew(data) => {
				if data.0.uuid == *file_a.uuid() {
					Some(data)
				} else {
					None
				}
			}
			_ => None,
		},
		Duration::from_secs(20),
		"fileNew",
	)
	.await;

	assert_eq!(event.0, file_a);

	client.trash_file(&mut file_a).await.unwrap();
	await_event(
		&mut receiver,
		|event| match event {
			DecryptedSocketEvent::FileTrash(data) => data.uuid == *file_a.uuid(),
			_ => false,
		},
		Duration::from_secs(20),
		"fileTrash",
	)
	.await;

	client.restore_file(&mut file_a).await.unwrap();

	let event = await_map_event(
		&mut receiver,
		|event| match event {
			DecryptedSocketEvent::FileRestore(data) => {
				if data.0.uuid == *file_a.uuid() {
					Some(data)
				} else {
					None
				}
			}
			_ => None,
		},
		Duration::from_secs(20),
		"fileRestore",
	)
	.await;

	assert_eq!(event.0, file_a);

	let old_file_a = file_a;

	let file_a = client.make_file_builder("file_a.txt", dir).build();
	let mut file_a = client
		.upload_file(file_a.into(), b"file b contents")
		.await
		.unwrap();

	await_event(
		&mut receiver,
		|event| match event {
			DecryptedSocketEvent::FileArchived(file) => file.uuid == *old_file_a.uuid(),
			_ => false,
		},
		Duration::from_secs(20),
		"fileArchived",
	)
	.await;

	client.set_favorite(&mut file_a, true).await.unwrap();
	let event = await_map_event(
		&mut receiver,
		|event| match event {
			DecryptedSocketEvent::ItemFavorite(inner) => {
				if inner.0.uuid() == file_a.uuid() {
					Some(inner)
				} else {
					None
				}
			}
			_ => None,
		},
		Duration::from_secs(20),
		"itemFavorite",
	)
	.await;

	assert_eq!(event.0, NonRootFSObject::File(Cow::Borrowed(&file_a)));

	let old_version = client
		.list_file_versions(&file_a)
		.await
		.unwrap()
		.pop()
		.unwrap();

	client
		.restore_file_version(&mut file_a, old_version)
		.await
		.unwrap();

	let mut event = await_map_event(
		&mut receiver,
		|event| match event {
			DecryptedSocketEvent::FileArchiveRestored(file)
				if file.file.uuid() == file_a.uuid() =>
			{
				Some(file)
			}
			_ => None,
		},
		Duration::from_secs(20),
		"fileArchiveRestored",
	)
	.await;
	if let (FileMeta::Decoded(event_meta), FileMeta::Decoded(meta)) =
		(&mut event.file.meta, &file_a.meta)
	{
		// restore file version updates the last modified time to fix a bug in the old sync engine
		// so we need to adjust that here before we assert_eq
		event_meta.last_modified = meta.last_modified;
	}
	// og favorited status is kept in the event and listed history
	// but is not set in the updated file during restore
	// so we need to adjust that here before we assert_eq
	event.file.favorited = file_a.favorited;

	assert_eq!(event.file, file_a);

	await_event(
		&mut receiver,
		|event| match event {
			DecryptedSocketEvent::FileMetadataChanged(data) => data.uuid == *file_a.uuid(),
			_ => false,
		},
		Duration::from_secs(20),
		"fileMetadataChanged",
	)
	.await;

	let old_file_a = file_a.clone();
	let new_name = "file_a_renamed.txt";

	client
		.update_file_metadata(
			&mut file_a,
			FileMetaChanges::default()
				.name(new_name.to_string())
				.unwrap(),
		)
		.await
		.unwrap();

	let event = await_map_event(
		&mut receiver,
		|event| match event {
			DecryptedSocketEvent::FileMetadataChanged(data) => {
				if data.uuid == *file_a.uuid() {
					Some(data)
				} else {
					None
				}
			}
			_ => None,
		},
		Duration::from_secs(20),
		"fileMetadataChanged",
	)
	.await;

	assert_eq!(file_a.meta, event.metadata);
	assert_eq!(old_file_a.meta, event.old_metadata);
	assert_eq!(
		MaybeEncrypted::Decrypted(Cow::Borrowed(new_name)),
		event.name
	);

	let new_parent = client
		.create_dir(dir, "move_target".to_string())
		.await
		.unwrap();

	client
		.move_file(&mut file_a, &(&new_parent).into())
		.await
		.unwrap();

	let event = await_map_event(
		&mut receiver,
		|event| match event {
			DecryptedSocketEvent::FileMove(data) => {
				if data.0.uuid == *file_a.uuid() {
					Some(data)
				} else {
					None
				}
			}
			_ => None,
		},
		Duration::from_secs(20),
		"fileMove",
	)
	.await;

	assert_eq!(event.0, file_a);

	let uuid = *file_a.uuid();

	client.delete_file_permanently(file_a).await.unwrap();
	await_event(
		&mut receiver,
		|event| match event {
			DecryptedSocketEvent::FileDeletedPermanent(data) => data.uuid == uuid,
			_ => false,
		},
		Duration::from_secs(20),
		"fileDeletedPermanent",
	)
	.await;
}

#[shared_test_runtime]
async fn test_websocket_folder_events() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let dir = &resources.dir;
	let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
	let _handle = client
		.add_event_listener(
			Box::new(move |event| {
				let _ = sender.send(event.to_owned_cow());
			}),
			None,
		)
		.await
		.unwrap();

	let mut dir_a = client.create_dir(dir, "a".to_string()).await.unwrap();
	let event = await_map_event(
		&mut receiver,
		|event| match event {
			DecryptedSocketEvent::FolderSubCreated(data) => {
				if data.0.uuid == *dir_a.uuid() {
					Some(data)
				} else {
					None
				}
			}
			_ => None,
		},
		Duration::from_secs(20),
		"folderSubCreated",
	)
	.await;
	assert_eq!(event.0, dir_a);

	client.trash_dir(&mut dir_a).await.unwrap();
	await_event(
		&mut receiver,
		|event| match event {
			DecryptedSocketEvent::FolderTrash(data) => data.uuid == *dir_a.uuid(),
			_ => false,
		},
		Duration::from_secs(20),
		"folderTrash",
	)
	.await;

	client.restore_dir(&mut dir_a).await.unwrap();
	let event = await_map_event(
		&mut receiver,
		|event| match event {
			DecryptedSocketEvent::FolderRestore(data) => {
				if data.0.uuid == *dir_a.uuid() {
					Some(data)
				} else {
					None
				}
			}
			_ => None,
		},
		Duration::from_secs(20),
		"folderRestore",
	)
	.await;
	assert_eq!(event.0, dir_a);

	client.set_favorite(&mut dir_a, true).await.unwrap();
	let event = await_map_event(
		&mut receiver,
		|event| match event {
			DecryptedSocketEvent::ItemFavorite(inner) => {
				if inner.0.uuid() == dir_a.uuid() {
					Some(inner)
				} else {
					None
				}
			}
			_ => None,
		},
		Duration::from_secs(20),
		"itemFavorite",
	)
	.await;
	assert_eq!(event.0, NonRootFSObject::Dir(Cow::Borrowed(&dir_a)));

	client
		.update_dir_metadata(
			&mut dir_a,
			DirectoryMetaChanges::default()
				.name("a_changed".to_string())
				.unwrap(),
		)
		.await
		.unwrap();
	let event = await_map_event(
		&mut receiver,
		|event| match event {
			DecryptedSocketEvent::FolderMetadataChanged(data) => {
				if data.uuid == *dir_a.uuid() {
					Some(data)
				} else {
					None
				}
			}
			_ => None,
		},
		Duration::from_secs(20),
		"folderMetadataChanged",
	)
	.await;
	assert_eq!(event.meta, dir_a.meta);

	let new_parent_dir = client
		.create_dir(dir, "new_parent".to_string())
		.await
		.unwrap();
	client
		.move_dir(&mut dir_a, &(&new_parent_dir).into())
		.await
		.unwrap();

	let event = await_map_event(
		&mut receiver,
		|event| match event {
			DecryptedSocketEvent::FolderMove(data) => {
				if data.0.uuid == *dir_a.uuid() {
					Some(data)
				} else {
					None
				}
			}
			_ => None,
		},
		Duration::from_secs(20),
		"folderMove",
	)
	.await;
	assert_eq!(event.0, dir_a);
	// todo should be moved to the top later when all the events return DirColor
	// so we can test them properly
	client
		.set_dir_color(&mut dir_a, DirColor::Blue)
		.await
		.unwrap();

	let event = await_map_event(
		&mut receiver,
		|event| match event {
			DecryptedSocketEvent::FolderColorChanged(data) => {
				if data.uuid == *dir_a.uuid() {
					Some(data)
				} else {
					None
				}
			}
			_ => None,
		},
		Duration::from_secs(20),
		"folderColorChanged",
	)
	.await;

	assert_eq!(event.color, DirColor::Blue);

	let uuid = *dir_a.uuid();
	client.delete_dir_permanently(dir_a).await.unwrap();

	await_event(
		&mut receiver,
		|event| match event {
			DecryptedSocketEvent::FolderDeletedPermanent(data) => data.uuid == uuid,
			_ => false,
		},
		Duration::from_secs(20),
		"folderDeletedPermanent",
	)
	.await;
}

#[shared_test_runtime]
async fn chat() {
	let client = test_utils::RESOURCES.client().await;
	let share_client = test_utils::SHARE_RESOURCES.client().await;
	let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
	let (share_sender, mut share_receiver) = tokio::sync::mpsc::unbounded_channel();

	let _handle = client
		.add_event_listener(
			Box::new(move |event| {
				let _ = sender.send(event.to_owned_cow());
			}),
			None,
		)
		.await
		.unwrap();

	let _handle = share_client
		.add_event_listener(
			Box::new(move |event| {
				let _ = share_sender.send(event.to_owned_cow());
			}),
			None,
		)
		.await
		.unwrap();

	let _locks = test_utils::set_up_contact(&client, &share_client).await;

	let event = await_map_event(
		&mut share_receiver,
		|event| match event {
			DecryptedSocketEvent::ContactRequestReceived(event) => {
				if event.sender_email == client.email() {
					Some(event)
				} else {
					None
				}
			}
			_ => None,
		},
		Duration::from_secs(10),
		"contactRequestReceived",
	)
	.await;

	assert_eq!(event.sender_email, client.email());
	let info = client.get_user_info().await.unwrap();
	assert_eq!(event.sender_id, info.id);
	assert_eq!(event.sender_avatar.unwrap_or_default(), info.avatar_url);

	let share_contact = client
		.get_contacts()
		.await
		.unwrap()
		.into_iter()
		.find(|c| c.email == share_client.email())
		.unwrap();

	let mut chat = client.create_chat(&[]).await.unwrap();

	let event = await_map_event(
		&mut receiver,
		|event| match event {
			DecryptedSocketEvent::ChatConversationsNew(data) => {
				if data.0.uuid() == chat.uuid() {
					Some(data)
				} else {
					None
				}
			}
			_ => None,
		},
		Duration::from_secs(10),
		"chatConversationNew",
	)
	.await;

	assert_eq!(event.0, chat);

	client
		.add_chat_participant(&mut chat, &share_contact)
		.await
		.unwrap();

	let share_event = await_map_event(
		&mut share_receiver,
		|event| match event {
			DecryptedSocketEvent::ChatConversationsNew(data) => {
				if data.0.uuid() == chat.uuid() {
					Some(data)
				} else {
					None
				}
			}
			_ => None,
		},
		Duration::from_secs(10),
		"chatConversationNew",
	)
	.await;

	// we compare fields one by one to avoid issues with created time being different
	// since it depends on when the user was added

	assert_eq!(share_event.0.participants(), chat.participants());
	assert_eq!(share_event.0.name(), chat.name());
	assert_eq!(share_event.0.uuid(), chat.uuid());
	assert_eq!(share_event.0.last_message(), chat.last_message());
	assert_eq!(share_event.0.muted(), chat.muted());

	let event = await_map_event(
		&mut receiver,
		|event| match event {
			DecryptedSocketEvent::ChatConversationParticipantNew(data) => {
				if data.chat == chat.uuid() {
					Some(data)
				} else {
					None
				}
			}
			_ => None,
		},
		Duration::from_secs(10),
		"chatConversationParticipantNew",
	)
	.await;

	assert_eq!(event.participant.email(), share_client.email());

	let msg = client
		.send_chat_message(&mut chat, "hello".to_string(), None)
		.await
		.unwrap();

	let event = await_map_event(
		&mut share_receiver,
		|event| match event {
			DecryptedSocketEvent::ChatMessageNew(data) if data.0.uuid() == msg.uuid() => Some(data),
			_ => None,
		},
		Duration::from_secs(10),
		"chatMessageNew",
	)
	.await;

	assert_eq!(&event.0, msg);

	let mut msg = msg.clone();

	client
		.edit_message(&chat, &mut msg, "hello edited".to_string())
		.await
		.unwrap();

	let event = await_map_event(
		&mut share_receiver,
		|event| match event {
			DecryptedSocketEvent::ChatMessageEdited(data) if data.uuid == *msg.uuid() => Some(data),
			_ => None,
		},
		Duration::from_secs(10),
		"chatMessageEdited",
	)
	.await;

	assert_eq!(
		MaybeEncrypted::Decrypted(Cow::Borrowed(msg.message().unwrap())),
		event.new_content
	);

	client
		.rename_chat(&mut chat, "new name".to_string())
		.await
		.unwrap();

	let event = await_map_event(
		&mut receiver,
		|event| match event {
			DecryptedSocketEvent::ChatConversationNameEdited(data) if data.chat == chat.uuid() => {
				Some(data)
			}
			_ => None,
		},
		Duration::from_secs(10),
		"chatConversationNameEdited",
	)
	.await;
	assert_eq!(
		MaybeEncrypted::Decrypted(Cow::Borrowed(chat.name().unwrap())),
		event.new_name
	);

	client
		.send_typing_signal(&chat, ChatTypingType::Down)
		.await
		.unwrap();
	let event = await_map_event(
		&mut share_receiver,
		|event| match event {
			DecryptedSocketEvent::ChatTyping(data) if data.chat == chat.uuid() => Some(data),
			_ => None,
		},
		Duration::from_secs(10),
		"chatTyping",
	)
	.await;

	assert_eq!(event.typing_type, ChatTypingType::Down);
}

#[shared_test_runtime]
async fn note() {
	let client = test_utils::RESOURCES.client().await;
	let shared_client = test_utils::SHARE_RESOURCES.client().await;
	let _locks = test_utils::set_up_contact(&client, &shared_client).await;

	let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
	let _handle = client
		.add_event_listener(
			Box::new(move |event| {
				let _ = sender.send(event.to_owned_cow());
			}),
			None,
		)
		.await
		.unwrap();

	let mut note = client
		.create_note(Some("Test Note".to_string()))
		.await
		.unwrap();

	await_event(
		&mut receiver,
		|event| matches!(event, DecryptedSocketEvent::NoteNew(data) if data.note == *note.uuid()),
		Duration::from_secs(10),
		"noteCreated",
	)
	.await;

	let event = await_map_event(
		&mut receiver,
		|event| match event {
			DecryptedSocketEvent::NoteParticipantNew(data) if data.note == *note.uuid() => {
				Some(data)
			}
			_ => None,
		},
		Duration::from_secs(10),
		"noteTitleEdited",
	)
	.await;

	assert_eq!(note.participants().first().unwrap(), &event.participant);

	client
		.set_note_content(&mut note, "new note content", "preview".to_string())
		.await
		.unwrap();

	let event = await_map_event(
		&mut receiver,
		|event| match event {
			DecryptedSocketEvent::NoteContentEdited(data) if data.note == *note.uuid() => {
				Some(data)
			}
			_ => None,
		},
		Duration::from_secs(10),
		"noteCreated",
	)
	.await;

	assert_eq!(
		MaybeEncrypted::Decrypted(Cow::Borrowed("new note content")),
		event.content
	);

	client.archive_note(&mut note).await.unwrap();
	await_event(
		&mut receiver,
		|event| matches!(event, DecryptedSocketEvent::NoteArchived(data) if data.note == *note.uuid()),
		Duration::from_secs(10),
		"noteArchived",
	)
	.await;

	client.restore_note(&mut note).await.unwrap();
	await_event(
		&mut receiver,
		|event| matches!(event, DecryptedSocketEvent::NoteRestored(data) if data.note == *note.uuid()),
		Duration::from_secs(10),
		"noteRestored",
	)
	.await;

	client
		.set_note_title(&mut note, "new title".to_string())
		.await
		.unwrap();

	let event = await_map_event(
		&mut receiver,
		|event| match event {
			DecryptedSocketEvent::NoteTitleEdited(data) if data.note == *note.uuid() => Some(data),
			_ => None,
		},
		Duration::from_secs(10),
		"noteTitleEdited",
	)
	.await;

	assert_eq!(
		MaybeEncrypted::Decrypted(Cow::Borrowed(note.title().unwrap())),
		event.new_title
	);
}

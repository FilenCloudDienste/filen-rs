use filen_macros::shared_test_runtime;
use filen_types::api::v3::notes::NoteType;

#[shared_test_runtime]
async fn note_creation() {
	let client = test_utils::RESOURCES.client().await;
	let _lock = client
		.acquire_lock_with_default("test:notes")
		.await
		.unwrap();

	let note = client.create_note(None).await.unwrap();
	let fetched = client.get_note(*note.uuid()).await.unwrap().unwrap();
	assert_eq!(note, fetched);
	client.delete_note(note).await.unwrap();
}

#[shared_test_runtime]
async fn note_favoriting() {
	let client = test_utils::RESOURCES.client().await;
	let _lock = client
		.acquire_lock_with_default("test:notes")
		.await
		.unwrap();

	let mut note = client.create_note(None).await.unwrap();
	let fetched = client.get_note(*note.uuid()).await.unwrap().unwrap();
	assert_eq!(note, fetched);

	client.set_note_favorited(&mut note, true).await.unwrap();
	assert!(note.favorited());
	let fetched = client.get_note(*note.uuid()).await.unwrap().unwrap();
	assert_eq!(note, fetched);

	client.set_note_favorited(&mut note, false).await.unwrap();
	assert!(!note.favorited());
	let fetched = client.get_note(*note.uuid()).await.unwrap().unwrap();
	assert_eq!(note, fetched);

	client.delete_note(note).await.unwrap();
}

#[shared_test_runtime]
async fn note_pinning() {
	let client = test_utils::RESOURCES.client().await;
	let _lock = client
		.acquire_lock_with_default("test:notes")
		.await
		.unwrap();

	let mut note = client.create_note(None).await.unwrap();

	client.set_note_pinned(&mut note, true).await.unwrap();
	assert!(note.pinned());
	let fetched = client.get_note(*note.uuid()).await.unwrap().unwrap();
	assert_eq!(note, fetched);

	client.set_note_pinned(&mut note, false).await.unwrap();
	assert!(!note.pinned());
	let fetched = client.get_note(*note.uuid()).await.unwrap().unwrap();
	assert_eq!(note, fetched);

	client.delete_note(note).await.unwrap();
}

#[shared_test_runtime]
async fn note_typing() {
	let client = test_utils::RESOURCES.client().await;
	let _lock = client
		.acquire_lock_with_default("test:notes")
		.await
		.unwrap();

	let mut note = client.create_note(None).await.unwrap();

	let content = client.get_note_content(&mut note).await.unwrap();
	assert_eq!(content, Some(String::new()));

	client
		.set_note_type(&mut note, NoteType::Md, None)
		.await
		.unwrap();
	assert_eq!(note.note_type(), NoteType::Md);
	let fetched = client.get_note(*note.uuid()).await.unwrap().unwrap();
	assert_eq!(note, fetched);

	client
		.set_note_type(&mut note, NoteType::Text, None)
		.await
		.unwrap();
	assert_eq!(note.note_type(), NoteType::Text);
	let fetched = client.get_note(*note.uuid()).await.unwrap().unwrap();
	assert_eq!(note, fetched);
	client.delete_note(note).await.unwrap();
}

#[shared_test_runtime]
async fn note_removing() {
	let client = test_utils::RESOURCES.client().await;
	let _lock = client
		.acquire_lock_with_default("test:notes")
		.await
		.unwrap();

	let mut note = client.create_note(None).await.unwrap();
	let fetched = client.get_note(*note.uuid()).await.unwrap().unwrap();
	assert_eq!(note, fetched);

	client.trash_note(&mut note).await.unwrap();
	assert!(note.trashed());
	let fetched = client.get_note(*note.uuid()).await.unwrap().unwrap();
	assert_eq!(note, fetched);

	client.restore_note(&mut note).await.unwrap();
	assert!(!note.trashed());
	let fetched = client.get_note(*note.uuid()).await.unwrap().unwrap();
	assert_eq!(note, fetched);

	let uuid = *note.uuid();
	client.delete_note(note).await.unwrap();
	assert!(client.get_note(uuid).await.unwrap().is_none());
}

#[shared_test_runtime]
async fn note_archiving() {
	let client = test_utils::RESOURCES.client().await;
	let _lock = client
		.acquire_lock_with_default("test:notes")
		.await
		.unwrap();

	let mut note = client.create_note(None).await.unwrap();
	let fetched = client.get_note(*note.uuid()).await.unwrap().unwrap();
	assert_eq!(note, fetched);

	client.archive_note(&mut note).await.unwrap();
	assert!(note.archived());
	let fetched = client.get_note(*note.uuid()).await.unwrap().unwrap();
	assert_eq!(note, fetched);

	client.restore_note(&mut note).await.unwrap();
	assert!(!note.archived());
	let fetched = client.get_note(*note.uuid()).await.unwrap().unwrap();
	assert_eq!(note, fetched);

	client.archive_note(&mut note).await.unwrap();
	client.trash_note(&mut note).await.unwrap();
	assert!(!note.archived());
	assert!(note.trashed());
	let fetched = client.get_note(*note.uuid()).await.unwrap().unwrap();
	assert_eq!(note, fetched);
	client.archive_note(&mut note).await.unwrap();
	assert!(note.archived());
	assert!(!note.trashed());
	let fetched = client.get_note(*note.uuid()).await.unwrap().unwrap();
	assert_eq!(note, fetched);
}

#[shared_test_runtime]
async fn note_titling() {
	let client = test_utils::RESOURCES.client().await;
	let _lock = client
		.acquire_lock_with_default("test:notes")
		.await
		.unwrap();

	let mut note = client.create_note(None).await.unwrap();
	let fetched = client.get_note(*note.uuid()).await.unwrap().unwrap();
	assert_eq!(note, fetched);

	let title = "My Note Title";
	client
		.set_note_title(&mut note, title.to_string())
		.await
		.unwrap();
	assert_eq!(note.title(), Some(title));
	let fetched = client.get_note(*note.uuid()).await.unwrap().unwrap();
	assert_eq!(note, fetched);

	let title = "New Title";
	client
		.set_note_title(&mut note, title.to_string())
		.await
		.unwrap();
	assert_eq!(note.title(), Some(title));
	let fetched = client.get_note(*note.uuid()).await.unwrap().unwrap();
	assert_eq!(note, fetched);

	let note = client
		.create_note(Some("Initial Title".to_string()))
		.await
		.unwrap();
	assert_eq!(note.title(), Some("Initial Title"));
	let fetched = client.get_note(*note.uuid()).await.unwrap().unwrap();
	assert_eq!(note, fetched);

	let _ = client.delete_note(note).await;
}

#[shared_test_runtime]
async fn note_listing() {
	let client = test_utils::RESOURCES.client().await;
	let _lock = client
		.acquire_lock_with_default("test:notes")
		.await
		.unwrap();
	let _lock = client.lock_notes().await.unwrap();

	let notes = client.list_notes().await.unwrap();

	for note in notes {
		client.delete_note(note).await.unwrap();
	}

	let notes = client.list_notes().await.unwrap();
	assert!(notes.is_empty());

	let note1 = client.create_note(None).await.unwrap();
	let note2 = client.create_note(None).await.unwrap();
	let note3 = client.create_note(None).await.unwrap();

	let notes = client.list_notes().await.unwrap();
	assert_eq!(notes.len(), 3);
	assert!(notes.contains(&note1));
	assert!(notes.contains(&note2));
	assert!(notes.contains(&note3));

	client.delete_note(note1).await.unwrap();
	client.delete_note(note2).await.unwrap();
	client.delete_note(note3).await.unwrap();
}

#[shared_test_runtime]
async fn note_content_editing() {
	let client = test_utils::RESOURCES.client().await;
	let _lock = client
		.acquire_lock_with_default("test:notes")
		.await
		.unwrap();

	let mut note = client.create_note(None).await.unwrap();
	let fetched = client.get_note(*note.uuid()).await.unwrap().unwrap();
	assert_eq!(note, fetched);

	let content = client.get_note_content(&mut note).await.unwrap();
	assert_eq!(content, Some(String::new()));

	let new_content = "This is the new content of the note.";
	let new_preview = "This is the new preview";
	client
		.set_note_content(&mut note, new_content, new_preview.to_string())
		.await
		.unwrap();
	assert_eq!(note.preview(), Some(new_preview));
	let content = client.get_note_content(&mut note).await.unwrap();
	assert_eq!(content, Some(new_content.to_string()));
	let fetched = client.get_note(*note.uuid()).await.unwrap().unwrap();
	assert_eq!(note, fetched);

	let new_content = "Short";
	client
		.set_note_content(&mut note, new_content, new_preview.to_string())
		.await
		.unwrap();
	let content = client.get_note_content(&mut note).await.unwrap();
	assert_eq!(content, Some(new_content.to_string()));
	let fetched = client.get_note(*note.uuid()).await.unwrap().unwrap();
	assert_eq!(note, fetched);

	client.delete_note(note).await.unwrap();
}

#[shared_test_runtime]
async fn note_duplication() {
	let client = test_utils::RESOURCES.client().await;
	let _lock = client
		.acquire_lock_with_default("test:notes")
		.await
		.unwrap();

	let mut note = client.create_note(None).await.unwrap();
	let content = "Content to be duplicated.".to_string();
	let preview = "Content to be dup".to_string();
	client
		.set_note_content(&mut note, &content, preview.clone())
		.await
		.unwrap();

	let mut duplicated = client.duplicate_note(&mut note).await.unwrap();
	let duplicated_content = client.get_note_content(&mut duplicated).await.unwrap();
	assert_eq!(duplicated.title(), note.title());
	assert_eq!(Some(content), duplicated_content);
	assert_eq!(duplicated.preview(), note.preview());

	client.delete_note(note).await.unwrap();
	client.delete_note(duplicated).await.unwrap();
}

#[shared_test_runtime]
async fn note_history() {
	let client = test_utils::RESOURCES.client().await;
	let _lock = client
		.acquire_lock_with_default("test:notes")
		.await
		.unwrap();

	let mut note = client.create_note(None).await.unwrap();
	let content = "Initial content".to_string();
	let preview = "Initial preview".to_string();
	client
		.set_note_content(&mut note, &content, preview.clone())
		.await
		.unwrap();

	let content_v2 = "Second version of content".to_string();
	let preview_v2 = "Second preview".to_string();
	client
		.set_note_content(&mut note, &content_v2, preview_v2.clone())
		.await
		.unwrap();

	let content_v3 = "Third version of content".to_string();
	let preview_v3 = "Third preview".to_string();
	client
		.set_note_content(&mut note, &content_v3, preview_v3.clone())
		.await
		.unwrap();

	let history = client.get_note_history(&note).await.unwrap();
	assert_eq!(history.len(), 4);
	assert_eq!(history[0].preview(), Some(""));
	assert_eq!(history[1].preview(), Some(preview.as_str()));
	assert_eq!(history[2].preview(), Some(preview_v2.as_str()));
	assert_eq!(history[3].preview(), Some(preview_v3.as_str()));

	assert_eq!(history[0].content(), Some(""));
	assert_eq!(history[1].content(), Some(content.as_str()));
	assert_eq!(history[2].content(), Some(content_v2.as_str()));
	assert_eq!(history[3].content(), Some(content_v3.as_str()));

	client
		.restore_note_from_history(&mut note, history[1].clone())
		.await
		.unwrap();

	let restored_content = client.get_note_content(&mut note).await.unwrap();
	assert_eq!(restored_content, Some(content));
	assert_eq!(note.preview(), Some(preview.as_str()));

	let fetched = client.get_note(*note.uuid()).await.unwrap().unwrap();
	assert_eq!(note, fetched);

	client.delete_note(note).await.unwrap();
}

#[shared_test_runtime]
async fn note_tag_manipulation() {
	let client = test_utils::RESOURCES.client().await;
	let _lock = client
		.acquire_lock_with_default("test:notes")
		.await
		.unwrap();

	let tags = client.list_note_tags().await.unwrap();
	for tag in tags {
		client.delete_note_tag(tag).await.unwrap();
	}

	let mut tag1 = client.create_note_tag("Tag1".to_string()).await.unwrap();
	let tag2 = client.create_note_tag("Tag2".to_string()).await.unwrap();
	let tag3 = client.create_note_tag("Tag3".to_string()).await.unwrap();

	let tags = client.list_note_tags().await.unwrap();
	assert_eq!(tags.len(), 3);
	assert!(tags.contains(&tag1));
	assert!(tags.contains(&tag2));
	assert!(tags.contains(&tag3));

	client
		.rename_note_tag(&mut tag1, "New Tag1".to_string())
		.await
		.unwrap();

	assert_eq!(tag1.name(), Some("New Tag1"));

	let tags = client.list_note_tags().await.unwrap();
	assert!(tags.contains(&tag1));

	client
		.set_note_tag_favorited(&mut tag1, true)
		.await
		.unwrap();
	assert!(tag1.favorited());
	let tags = client.list_note_tags().await.unwrap();
	assert!(tags.contains(&tag1));

	client
		.set_note_tag_favorited(&mut tag1, false)
		.await
		.unwrap();
	assert!(!tag1.favorited());
	let tags = client.list_note_tags().await.unwrap();
	assert!(tags.contains(&tag1));

	client.delete_note_tag(tag2).await.unwrap();
	let tags = client.list_note_tags().await.unwrap();
	assert_eq!(tags.len(), 2);
	assert!(tags.contains(&tag1));
	assert!(tags.contains(&tag3));
}

#[shared_test_runtime]
async fn note_tagging() {
	let client = test_utils::RESOURCES.client().await;
	let _lock = client
		.acquire_lock_with_default("test:notes")
		.await
		.unwrap();

	let mut tag = client.create_note_tag("Tag13".to_string()).await.unwrap();
	let mut note = client.create_note(None).await.unwrap();
	assert!(note.tags().is_empty());
	client.add_tag_to_note(&mut note, &mut tag).await.unwrap();
	assert!(note.tags().contains(&tag));
	let fetched = client.get_note(*note.uuid()).await.unwrap().unwrap();
	assert!(fetched.tags().iter().any(|t| t.name() == tag.name()));

	// waiting for v3/notes/tag to return timestamp for note
	assert!(note.tags().contains(&tag));

	let fetched = client.get_note(*note.uuid()).await.unwrap().unwrap();
	assert_eq!(note, fetched);
}

#[shared_test_runtime]
async fn note_sharing() {
	let client = test_utils::RESOURCES.client().await;
	let share_client = test_utils::SHARE_RESOURCES.client().await;

	let _lock1 = client
		.acquire_lock_with_default("test:contact")
		.await
		.unwrap();
	let _lock2 = share_client
		.acquire_lock_with_default("test:contact")
		.await
		.unwrap();
	let _lock = client
		.acquire_lock_with_default("test:notes")
		.await
		.unwrap();

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

	let mut note = client.create_note(None).await.unwrap();
	let content = "Content to be shared.";
	let preview = "new preview.";
	client
		.set_note_content(&mut note, content, preview.to_string())
		.await
		.unwrap();

	client
		.add_note_participant(&mut note, &contact, false)
		.await
		.unwrap();

	let fetched = client.get_note(*note.uuid()).await.unwrap().unwrap();
	assert_eq!(note, fetched);
	let fetched_content = client.get_note_content(&mut note).await.unwrap();
	let shared_fetched_content = share_client.get_note_content(&mut note).await.unwrap();
	assert_eq!(fetched_content, Some(content.to_string()));
	assert_eq!(shared_fetched_content, Some(content.to_string()));

	let content2 = "This is the updated content of the shared note.";
	let preview2 = "updated preview";

	share_client
		.set_note_content(&mut note, content2, preview2.to_string())
		.await
		.unwrap_err();

	client
		.remove_note_participant(&mut note, &contact)
		.await
		.unwrap();
	let fetched = client.get_note(*note.uuid()).await.unwrap().unwrap();
	assert_eq!(note, fetched);

	client
		.add_note_participant(&mut note, &contact, true)
		.await
		.unwrap();
	let fetched = client.get_note(*note.uuid()).await.unwrap().unwrap();
	assert_eq!(note, fetched);

	share_client
		.set_note_content(&mut note, content2, preview2.to_string())
		.await
		.unwrap();
	let fetched_content = client.get_note_content(&mut note).await.unwrap();
	let shared_fetched_content = share_client.get_note_content(&mut note).await.unwrap();
	assert_eq!(fetched_content, Some(content2.to_string()));
	assert_eq!(shared_fetched_content, Some(content2.to_string()));
	let fetched = client.get_note(*note.uuid()).await.unwrap().unwrap();
	assert_eq!(note, fetched);

	client
		.set_note_participant_permission(&mut note, &contact, false)
		.await
		.unwrap();
	let fetched = client.get_note(*note.uuid()).await.unwrap().unwrap();
	assert_eq!(note, fetched);

	share_client
		.set_note_content(&mut note, content, preview.to_string())
		.await
		.unwrap_err();
	let fetched_content = client.get_note_content(&mut note).await.unwrap();
	let shared_fetched_content = share_client.get_note_content(&mut note).await.unwrap();
	assert_eq!(fetched_content, Some(content2.to_string()));
	assert_eq!(shared_fetched_content, Some(content2.to_string()));
}

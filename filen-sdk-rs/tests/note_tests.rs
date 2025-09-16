use filen_macros::shared_test_runtime;
use filen_types::api::v3::notes::NoteType;

#[shared_test_runtime]
async fn note_creation() {
	let client = test_utils::RESOURCES.client().await;

	let note = client.create_note(None).await.unwrap();
	let fetched = client.get_note(*note.uuid()).await.unwrap().unwrap();
	assert_eq!(note, fetched);
	client.delete_note(note).await.unwrap();
}

#[shared_test_runtime]
async fn note_favoriting() {
	let client = test_utils::RESOURCES.client().await;

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

// #[shared_test_runtime]
// async fn note_typing() {
// 	let client = test_utils::RESOURCES.client().await;

// 	let mut note = client.create_note(None).await.unwrap();

// 	client
// 		.set_note_content_and_type(&mut note, "", String::new(), NoteType::Md)
// 		.await
// 		.unwrap();
// 	assert_eq!(note.note_type(), NoteType::Md);
// 	let fetched = client.get_note(*note.uuid()).await.unwrap().unwrap();
// 	assert_eq!(note, fetched);

// 	client
// 		.set_note_content_and_type(&mut note, "", String::new(), NoteType::Text)
// 		.await
// 		.unwrap();
// 	assert_eq!(note.note_type(), NoteType::Text);
// 	let fetched = client.get_note(*note.uuid()).await.unwrap().unwrap();
// 	assert_eq!(note, fetched);
// 	client.delete_note(note).await.unwrap();
// }

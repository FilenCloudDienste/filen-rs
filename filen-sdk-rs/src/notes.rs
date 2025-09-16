use std::borrow::Cow;

use chrono::{DateTime, Utc};
use filen_types::{
	api::v3::{
		contacts::Contact,
		notes::{NoteType, participants::add::ContactUuid},
	},
	fs::UuidStr,
};
use rsa::RsaPublicKey;
use serde::{Deserialize, Serialize};

use crate::{
	Error, ErrorKind, api,
	auth::Client,
	crypto::{
		notes::NoteKey,
		shared::{CreateRandom, MetaCrypter},
	},
	error::MetadataWasNotDecryptedError,
};

const EMPTY_CHECKLIST_NOTE: &str = r#"<ul data-checked="false"><li><br></li></ul>"#;

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct NoteTag {
	pub uuid: UuidStr,
	// none if decryption fails
	pub name: Option<String>,
	pub favorite: bool,
	pub edited_timestamp: DateTime<Utc>,
	pub created_timestamp: DateTime<Utc>,
}

impl NoteTag {
	fn decrypt_with_key(
		tag: &filen_types::api::v3::notes::NoteTag<'_>,
		note_key: Option<&impl MetaCrypter>,
		outer_tmp_vec: &mut Vec<u8>,
	) -> Self {
		let name = if let Some(key) = note_key {
			let tmp_vec = std::mem::take(outer_tmp_vec);
			let (name, tmp_vec) = match key.decrypt_meta_into(&tag.name, tmp_vec) {
				Ok(s) => match serde_json::from_str::<NoteTagName>(&s) {
					Ok(ntm) => (Some(ntm.name.into_owned()), s.into_bytes()),
					Err(e) => {
						log::error!("Failed to parse note tag name JSON: {e}");
						(None, s.into_bytes())
					}
				},
				Err((e, s_vec)) => {
					log::error!("Failed to decrypt note tag name: {e}");
					(None, s_vec)
				}
			};
			*outer_tmp_vec = tmp_vec;
			name
		} else {
			None
		};

		Self {
			uuid: tag.uuid,
			name,
			favorite: tag.favorite,
			edited_timestamp: tag.edited_timestamp,
			created_timestamp: tag.created_timestamp,
		}
	}
}

struct NoteParticipantParts {
	pub user_id: u64,
	pub email: String,
	pub avatar: Option<String>,
	pub nick_name: String,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct NoteParticipant {
	pub user_id: u64,
	pub is_owner: bool,
	pub email: String,
	pub avatar: Option<String>,
	pub nick_name: String,
	pub permissions_write: bool,
	pub added_timestamp: DateTime<Utc>,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Note {
	uuid: UuidStr,
	owner_id: u64,
	// last_edited_by: u64,
	favorite: bool,
	pinned: bool,
	tags: Vec<NoteTag>,
	note_type: NoteType,
	// none if decryption fails
	encryption_key: Option<NoteKey>,
	// none if decryption fails
	title: Option<String>,
	// none if decryption fails
	preview: Option<String>,
	trash: bool,
	archive: bool,
	created_timestamp: DateTime<Utc>,
	edited_timestamp: DateTime<Utc>,
	participants: Vec<NoteParticipant>,
}

impl Note {
	pub fn uuid(&self) -> &UuidStr {
		&self.uuid
	}

	pub fn favorited(&self) -> bool {
		self.favorite
	}

	pub fn pinned(&self) -> bool {
		self.pinned
	}

	pub fn note_type(&self) -> NoteType {
		self.note_type
	}
}

#[derive(Clone)]
pub struct NoteHistory {
	id: u64,
	preview: Option<String>,
	content: Option<String>,
	edited_timestamp: DateTime<Utc>,
	editor_id: u64,
	note_type: NoteType,
}

impl NoteHistory {
	fn decrypt_with_key(
		note_history: &filen_types::api::v3::notes::history::NoteHistory<'_>,
		note_key: &NoteKey,
		outer_tmp_vec: &mut Vec<u8>,
	) -> Self {
		let tmp_vec = std::mem::take(outer_tmp_vec);
		let (decrypted_preview, tmp_vec) =
			match note_key.decrypt_meta_into(&note_history.preview, tmp_vec) {
				Ok(s) => match serde_json::from_str::<NotePreview>(&s) {
					Ok(np) => (Some(np.preview.into_owned()), s.into_bytes()),
					Err(e) => {
						log::error!("Failed to parse note preview JSON: {e}");
						(None, s.into_bytes())
					}
				},
				Err((e, s_vec)) => {
					log::error!("Failed to decrypt note history preview: {e}");
					(None, s_vec)
				}
			};

		let (decrypted_content, tmp_vec) =
			match note_key.decrypt_meta_into(&note_history.content, tmp_vec) {
				Ok(s) => match serde_json::from_str::<NoteContent>(&s) {
					Ok(nc) => (Some(nc.content.into_owned()), s.into_bytes()),
					Err(e) => {
						log::error!("Failed to parse note content JSON: {e}");
						(None, s.into_bytes())
					}
				},
				Err((e, s_vec)) => {
					log::error!("Failed to decrypt note content: {e}");
					(None, s_vec)
				}
			};

		*outer_tmp_vec = tmp_vec;

		Self {
			id: note_history.id,
			preview: decrypted_preview,
			content: decrypted_content,
			edited_timestamp: note_history.edited_timestamp,
			editor_id: note_history.editor_id,
			note_type: note_history.note_type,
		}
	}
}

#[derive(Deserialize, Serialize)]
struct NoteKeyStruct<'a> {
	key: Cow<'a, NoteKey>,
}

#[derive(Deserialize, Serialize)]
struct NoteTitle<'a> {
	title: Cow<'a, str>,
}

#[derive(Deserialize, Serialize)]
struct NotePreview<'a> {
	preview: Cow<'a, str>,
}

#[derive(Deserialize, Serialize)]
struct NoteTagName<'a> {
	name: Cow<'a, str>,
}

#[derive(Deserialize, Serialize)]
struct NoteContent<'a> {
	content: Cow<'a, str>,
}

impl Client {
	pub async fn is_shared(
		&self,
		uuid: UuidStr,
	) -> Result<api::v3::item::shared::Response<'static>, Error> {
		api::v3::item::shared::post(self.client(), &api::v3::item::shared::Request { uuid }).await
	}

	fn decrypt_note_key(
		&self,
		note: &filen_types::api::v3::notes::Note<'_>,
	) -> Result<NoteKey, Error> {
		let participant = note
			.participants
			.iter()
			.find(|p| p.user_id == self.user_id)
			.ok_or_else(|| {
				Error::custom(ErrorKind::Response, "User is not a participant in the note")
			})?;

		let key =
			crate::crypto::rsa::decrypt_with_private_key(self.private_key(), &participant.metadata)
				.map_err(|e| {
					log::error!("Failed to decrypt note key for note {}: {e}", note.uuid);
					Error::custom_with_source(ErrorKind::Response, e, Some("decrypt note key"))
				})?;
		let key_str = str::from_utf8(&key)
			.map_err(|_| Error::custom(ErrorKind::Response, "Failed to parse note key as UTF-8"))?;
		let key_struct: NoteKeyStruct = serde_json::from_str(key_str)?;
		Ok(key_struct.key.into_owned())
	}

	fn decrypt_note(
		&self,
		note: filen_types::api::v3::notes::Note<'_>,
		outer_tmp_vec: &mut Vec<u8>,
	) -> Note {
		let tmp_vec = std::mem::take(outer_tmp_vec);
		let key = self.decrypt_note_key(&note).ok();

		let (title, preview, mut tmp_vec) = if let Some(key) = &key {
			let (title, tmp_vec) = match key.decrypt_meta_into(&note.title, tmp_vec) {
				Ok(s) => match serde_json::from_str::<NoteTitle>(&s) {
					Ok(nt) => (Some(nt.title.into_owned()), s.into_bytes()),
					Err(e) => {
						log::error!("Failed to parse note title JSON: {e}");
						(None, s.into_bytes())
					}
				},
				Err((e, s_vec)) => {
					log::error!("Failed to decrypt note title: {e}");
					(None, s_vec)
				}
			};

			let (preview, tmp_vec) = if note.preview.0.is_empty() {
				(Some(String::new()), tmp_vec)
			} else {
				match key.decrypt_meta_into(&note.preview, tmp_vec) {
					Ok(s) => match serde_json::from_str::<NotePreview>(&s) {
						Ok(np) => (Some(np.preview.into_owned()), s.into_bytes()),
						Err(e) => {
							log::error!("Failed to parse note preview JSON: {e}");
							(None, s.into_bytes())
						}
					},
					Err((e, s_vec)) => {
						log::error!("Failed to decrypt note preview: {e}");
						(None, s_vec)
					}
				}
			};

			(title, preview, tmp_vec)
		} else {
			(None, None, tmp_vec)
		};
		let tags = note
			.tags
			.into_iter()
			.map(|tag| NoteTag::decrypt_with_key(&tag, key.as_ref(), &mut tmp_vec))
			.collect::<Vec<_>>();

		let participants = note
			.participants
			.into_iter()
			.map(|p| NoteParticipant {
				user_id: p.user_id,
				is_owner: p.is_owner,
				email: p.email.into_owned(),
				avatar: p.avatar.map(|a| a.into_owned()),
				nick_name: p.nick_name.into_owned(),
				permissions_write: p.permissions_write,
				added_timestamp: p.added_timestamp,
			})
			.collect::<Vec<_>>();

		*outer_tmp_vec = tmp_vec;

		Note {
			uuid: note.uuid,
			owner_id: note.owner_id,
			favorite: note.favorite,
			pinned: note.pinned,
			tags,
			note_type: note.note_type,
			encryption_key: key,
			title,
			preview,
			trash: note.trash,
			archive: note.archive,
			created_timestamp: note.created_timestamp,
			edited_timestamp: note.edited_timestamp,
			participants,
		}
	}

	pub async fn list_all_notes(&self) -> Result<Vec<Note>, Error> {
		let notes = crate::api::v3::notes::get(self.client()).await?;
		// opportunity for par_iter here if we ever start using rayon
		let mut outer_tmp_vec = Vec::new();
		let notes = notes
			.0
			.into_iter()
			.map(|note| {
				// TS sdk filters participants to make sure the user is included
				self.decrypt_note(note, &mut outer_tmp_vec)
			})
			.collect::<Vec<_>>();
		Ok(notes)
	}

	async fn inner_add_note_participant(
		&self,
		note: &mut Note,
		contact_uuid: ContactUuid,
		write: bool,
		public_key: &RsaPublicKey,
		note_participant_parts: NoteParticipantParts,
	) -> Result<(), Error> {
		let data = crate::crypto::rsa::encrypt_with_public_key(
			public_key,
			serde_json::to_string(&NoteKeyStruct {
				key: Cow::Borrowed(
					note.encryption_key
						.as_ref()
						.ok_or(MetadataWasNotDecryptedError)?,
				),
			})?
			.as_bytes(),
		)
		.map_err(|e| {
			Error::custom_with_source(ErrorKind::Conversion, e, Some("add participant"))
		})?;

		let response = crate::api::v3::notes::participants::add::post(
			self.client(),
			&crate::api::v3::notes::participants::add::Request {
				uuid: note.uuid,
				contact_uuid,
				metadata: data,
				permissions_write: write,
			},
		)
		.await?;

		note.participants.push(NoteParticipant {
			user_id: note_participant_parts.user_id,
			is_owner: note_participant_parts.user_id == note.owner_id,
			email: note_participant_parts.email,
			avatar: note_participant_parts.avatar,
			nick_name: note_participant_parts.nick_name,
			permissions_write: write,
			added_timestamp: response.timestamp,
		});

		Ok(())
	}

	pub async fn add_note_participant(
		&self,
		note: &mut Note,
		contact: &Contact<'_>,
		write: bool,
	) -> Result<(), Error> {
		self.inner_add_note_participant(
			note,
			ContactUuid::Uuid(contact.uuid),
			write,
			&contact.public_key,
			NoteParticipantParts {
				user_id: contact.user_id,
				email: contact.email.clone().into_owned(),
				avatar: contact.avatar.clone().map(|a| a.into_owned()),
				nick_name: contact.nick_name.clone().into_owned(),
			},
		)
		.await
	}

	pub async fn remove_note_participant(
		&self,
		note: &Note,
		contact: &Contact<'_>,
	) -> Result<(), Error> {
		crate::api::v3::notes::participants::remove::post(
			self.client(),
			&crate::api::v3::notes::participants::remove::Request {
				uuid: note.uuid,
				user_id: contact.user_id,
			},
		)
		.await
	}

	pub async fn set_note_participant_permission(
		&self,
		note: &Note,
		contact: &Contact<'_>,
		write: bool,
	) -> Result<(), Error> {
		crate::api::v3::notes::participants::permissions::post(
			self.client(),
			&crate::api::v3::notes::participants::permissions::Request {
				uuid: note.uuid,
				user_id: contact.user_id,
				permissions_write: write,
			},
		)
		.await
	}

	pub async fn get_note(&self, uuid: UuidStr) -> Result<Option<Note>, Error> {
		// I hate this
		self.list_all_notes()
			.await
			.map(|notes| notes.into_iter().find(|n| n.uuid == uuid))
	}

	pub async fn create_note(&self, title: Option<String>) -> Result<Note, Error> {
		let uuid = UuidStr::new_v4();
		let title = title.unwrap_or_else(|| Utc::now().format("%a %b %d %Y %X").to_string());
		let key = NoteKey::generate();
		let key_struct = NoteKeyStruct {
			key: Cow::Borrowed(&key),
		};
		let title_struct = NoteTitle {
			title: Cow::Borrowed(&title),
		};

		let key_string = self
			.crypter()
			.encrypt_meta(&serde_json::to_string(&key_struct)?);
		let title_string = key.encrypt_meta(&serde_json::to_string(&title_struct)?);

		let _lock = self.lock_notes().await?;

		let response = api::v3::notes::create::post(
			self.client(),
			&api::v3::notes::create::Request {
				uuid,
				title: title_string,
				metadata: key_string,
			},
		)
		.await?;

		let mut new = Note {
			uuid,
			owner_id: self.user_id,
			favorite: false,
			pinned: false,
			tags: vec![],
			note_type: NoteType::Text,
			encryption_key: Some(key),
			title: Some(title),
			preview: Some(String::new()),
			trash: false,
			archive: false,
			created_timestamp: response.created_timestamp,
			edited_timestamp: response.edited_timestamp,
			participants: vec![],
		};

		self.inner_add_note_participant(
			&mut new,
			ContactUuid::Owner,
			true,
			self.public_key(),
			NoteParticipantParts {
				user_id: self.user_id,
				email: self.email().to_string(),
				// todo, remove these when participants/add returns them
				avatar: None,
				nick_name: String::new(),
			},
		)
		.await?;

		Ok(new)
	}

	pub async fn get_note_content(&self, note: &mut Note) -> Result<Option<String>, Error> {
		let response = api::v3::notes::content::post(
			self.client(),
			&api::v3::notes::content::Request { uuid: note.uuid },
		)
		.await?;

		note.note_type = response.note_type;
		note.edited_timestamp = response.edited_timestamp;
		note.preview = Some(response.preview.into_owned());

		if response.content.0.is_empty() {
			match note.note_type {
				NoteType::Checklist => Ok(Some(EMPTY_CHECKLIST_NOTE.to_string())),
				_ => Ok(Some(String::new())),
			}
		} else {
			let Some(key) = note.encryption_key.as_ref() else {
				return Ok(None);
			};

			let Ok(decrypted_content) = key.decrypt_meta(&response.content) else {
				log::error!("Failed to decrypt note content: No encryption key available");
				return Ok(None);
			};

			let content = match NoteContent::deserialize(&mut serde_json::Deserializer::from_str(
				&decrypted_content,
			)) {
				Ok(content) => content.content.into_owned(),
				Err(e) => {
					log::error!("Failed to parse note content JSON: {e}");
					return Ok(None);
				}
			};

			if content.is_empty() && note.note_type == NoteType::Checklist {
				Ok(Some(EMPTY_CHECKLIST_NOTE.to_string()))
			} else {
				Ok(Some(content))
			}
		}
	}

	pub async fn set_note_content(
		&self,
		note: &mut Note,
		new_content: &str,
		new_preview: String,
	) -> Result<(), Error> {
		self.set_note_content_and_type(note, new_content, new_preview, note.note_type)
			.await
	}

	pub async fn set_note_content_and_type(
		&self,
		note: &mut Note,
		new_content: &str,
		new_preview: String,
		new_note_type: NoteType,
	) -> Result<(), Error> {
		let note_key = note
			.encryption_key
			.as_ref()
			.ok_or(MetadataWasNotDecryptedError)?;

		let _lock = self.lock_notes().await?;
		api::v3::notes::content::edit::post(
			self.client(),
			&api::v3::notes::content::edit::Request {
				uuid: note.uuid,
				content: note_key.encrypt_meta(
					&serde_json::to_string(&NoteContent {
						content: Cow::Borrowed(new_content),
					})
					.expect("Failed to serialize note content (should never happen)"),
				),
				preview: note_key.encrypt_meta(
					&serde_json::to_string(&NotePreview {
						preview: Cow::Borrowed(&new_preview),
					})
					.expect("Failed to serialize note preview (should never happen)"),
				),
				note_type: new_note_type,
			},
		)
		.await?;
		note.note_type = new_note_type;
		note.preview = Some(new_preview);
		Ok(())
	}

	pub async fn set_note_title(&self, note: &mut Note, new_title: &str) -> Result<(), Error> {
		let title = note
			.encryption_key
			.as_ref()
			.ok_or(MetadataWasNotDecryptedError)?
			.encrypt_meta(
				&serde_json::to_string(&NoteTitle {
					title: Cow::Borrowed(new_title),
				})
				.expect("Failed to serialize note title (should never happen)"),
			);

		let _lock = self.lock_notes().await?;
		api::v3::notes::title::edit::post(
			self.client(),
			&api::v3::notes::title::edit::Request {
				uuid: note.uuid,
				title,
			},
		)
		.await?;
		note.title = Some(new_title.to_string());
		Ok(())
	}

	pub async fn delete_note(&self, note: Note) -> Result<(), Error> {
		let _lock = self.lock_notes().await?;
		api::v3::notes::delete::post(
			self.client(),
			&api::v3::notes::delete::Request { uuid: note.uuid },
		)
		.await
	}

	pub async fn archive_note(&self, note: &mut Note) -> Result<(), Error> {
		let _lock = self.lock_notes().await?;
		api::v3::notes::archive::post(
			self.client(),
			&api::v3::notes::archive::Request { uuid: note.uuid },
		)
		.await?;
		note.archive = true;
		Ok(())
	}

	pub async fn trash_note(&self, note: &mut Note) -> Result<(), Error> {
		let _lock = self.lock_notes().await?;
		api::v3::notes::trash::post(
			self.client(),
			&api::v3::notes::trash::Request { uuid: note.uuid },
		)
		.await?;
		note.trash = true;
		Ok(())
	}

	pub async fn set_note_favorited(&self, note: &mut Note, favorite: bool) -> Result<(), Error> {
		let _lock = self.lock_notes().await?;
		api::v3::notes::favorite::post(
			self.client(),
			&api::v3::notes::favorite::Request {
				uuid: note.uuid,
				favorite,
			},
		)
		.await?;
		note.favorite = favorite;
		Ok(())
	}

	pub async fn set_note_pinned(&self, note: &mut Note, pinned: bool) -> Result<(), Error> {
		let _lock = self.lock_notes().await?;
		api::v3::notes::pinned::post(
			self.client(),
			&api::v3::notes::pinned::Request {
				uuid: note.uuid,
				pinned,
			},
		)
		.await?;
		note.pinned = pinned;
		Ok(())
	}

	/// Restore a note from the archive or trash.
	pub async fn restore_note(&self, note: &mut Note) -> Result<(), Error> {
		let _lock = self.lock_notes().await?;
		api::v3::notes::restore::post(
			self.client(),
			&api::v3::notes::restore::Request { uuid: note.uuid },
		)
		.await?;
		note.archive = false;
		note.trash = false;
		Ok(())
	}

	pub async fn duplicate_note(&self, note: &mut Note) -> Result<Note, Error> {
		let _lock = self.lock_notes().await?;
		let content = self.get_note_content(note).await?;

		let mut new = self.create_note(note.title.clone()).await?;
		self.set_note_content_and_type(
			&mut new,
			content.as_deref().unwrap_or(""),
			note.preview.clone().unwrap_or_default(),
			note.note_type,
		)
		.await?;
		Ok(new)
	}

	pub async fn get_note_history(&self, note: &Note) -> Result<Vec<NoteHistory>, Error> {
		let note_key = note
			.encryption_key
			.as_ref()
			.ok_or(MetadataWasNotDecryptedError)?;

		let histories = api::v3::notes::history::post(
			self.client(),
			&api::v3::notes::history::Request { uuid: note.uuid },
		)
		.await?;

		let mut outer_tmp_vec = Vec::new();

		let histories = histories
			.0
			.into_iter()
			.map(|h| NoteHistory::decrypt_with_key(&h, note_key, &mut outer_tmp_vec))
			.collect::<Vec<_>>();

		Ok(histories)
	}

	pub async fn restore_note_from_history(
		&self,
		note: &mut Note,
		history: NoteHistory,
	) -> Result<(), Error> {
		let _lock = self.lock_notes().await?;
		api::v3::notes::history::restore::post(
			self.client(),
			&api::v3::notes::history::restore::Request {
				uuid: note.uuid,
				id: history.id,
			},
		)
		.await?;

		note.edited_timestamp = history.edited_timestamp;
		note.note_type = history.note_type;
		note.preview = history.preview;
		// todo when editor_id is added
		// note.editor_id = history.editor_id;

		Ok(())
	}

	pub async fn add_tag_to_note(&self, note: &mut Note, tag: NoteTag) -> Result<(), Error> {
		let _lock = self.lock_notes().await?;
		api::v3::notes::tag::post(
			self.client(),
			&api::v3::notes::tag::Request {
				uuid: note.uuid,
				tag: tag.uuid,
			},
		)
		.await?;

		// avoid duplicates
		if !note.tags.iter().any(|t| t.uuid == tag.uuid) {
			note.tags.push(tag);
		}

		Ok(())
	}

	pub async fn remove_tag_from_note(&self, note: &mut Note, tag: &NoteTag) -> Result<(), Error> {
		let _lock = self.lock_notes().await?;
		api::v3::notes::untag::post(
			self.client(),
			&api::v3::notes::untag::Request {
				uuid: note.uuid,
				tag: tag.uuid,
			},
		)
		.await?;
		note.tags.retain(|t| t.uuid != tag.uuid);

		Ok(())
	}

	pub async fn list_all_note_tags(&self) -> Result<Vec<NoteTag>, Error> {
		let response = api::v3::notes::tags::post(self.client()).await?;
		let mut outer_tmp_vec = Vec::new();
		Ok(response
			.0
			.into_iter()
			.map(|tag| NoteTag::decrypt_with_key(&tag, Some(self.crypter()), &mut outer_tmp_vec))
			.collect::<Vec<_>>())
	}

	pub async fn create_note_tag(&self, name: String) -> Result<NoteTag, Error> {
		// is this necessary?
		if let Some(existing) = self.list_all_note_tags().await?.into_iter().find(|t| {
			if let Some(t_name) = &t.name {
				*t_name == name
			} else {
				false
			}
		}) {
			return Ok(existing);
		}

		let name_struct = NoteTagName {
			name: Cow::Borrowed(&name),
		};
		let name_string = self
			.crypter()
			.encrypt_meta(&serde_json::to_string(&name_struct)?);

		let _lock = self.lock_notes().await?;
		let response = api::v3::notes::tags::create::post(
			self.client(),
			&api::v3::notes::tags::create::Request { name: name_string },
		)
		.await?;

		Ok(NoteTag {
			uuid: response.uuid,
			name: Some(name),
			favorite: false,
			edited_timestamp: response.edited_timestamp,
			created_timestamp: response.created_timestamp,
		})
	}

	pub async fn rename_note_tag(&self, tag: &mut NoteTag, new_name: String) -> Result<(), Error> {
		let name_string = self
			.crypter()
			.encrypt_meta(&serde_json::to_string(&NoteTagName {
				name: Cow::Borrowed(&new_name),
			})?);

		let _lock = self.lock_notes().await?;
		api::v3::notes::tags::rename::post(
			self.client(),
			&api::v3::notes::tags::rename::Request {
				uuid: tag.uuid,
				name: name_string,
			},
		)
		.await?;
		tag.name = Some(new_name);
		Ok(())
	}

	pub async fn set_note_tag_favorited(
		&self,
		tag: &mut NoteTag,
		favorite: bool,
	) -> Result<(), Error> {
		let _lock = self.lock_notes().await?;
		api::v3::notes::tags::favorite::post(
			self.client(),
			&api::v3::notes::tags::favorite::Request {
				uuid: tag.uuid,
				favorite,
			},
		)
		.await?;
		tag.favorite = favorite;
		Ok(())
	}

	pub async fn delete_note_tag(&self, tag: NoteTag) -> Result<(), Error> {
		let _lock = self.lock_notes().await?;
		api::v3::notes::tags::delete::post(
			self.client(),
			&api::v3::notes::tags::delete::Request { uuid: tag.uuid },
		)
		.await
	}
}

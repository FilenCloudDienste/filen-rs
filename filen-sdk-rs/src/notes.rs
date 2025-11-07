use std::borrow::Cow;

use chrono::{DateTime, Utc};
use filen_types::{
	api::v3::{
		contacts::Contact,
		notes::{NoteType, participants::add::ContactUuid},
	},
	crypto::EncryptedString,
	fs::UuidStr,
};
#[cfg(feature = "multi-threaded-crypto")]
use rayon::iter::ParallelIterator;
use rsa::RsaPublicKey;

use crate::{
	Error, ErrorKind, api,
	auth::Client,
	crypto::{
		notes_and_chats::{NoteOrChatCarrierCryptoExt, NoteOrChatKey, NoteOrChatKeyStruct},
		shared::{CreateRandom, MetaCrypter},
	},
	error::{MetadataWasNotDecryptedError, ResultExt},
	runtime::{blocking_join, do_cpu_intensive},
	util::IntoMaybeParallelIterator,
};

use crypto::*;

#[derive(Debug, PartialEq, Eq, Clone)]
#[cfg_attr(
	feature = "wasm-full",
	derive(serde::Serialize, serde::Deserialize, tsify::Tsify),
	tsify(into_wasm_abi, from_wasm_abi, large_number_types_as_bigints),
	serde(rename_all = "camelCase")
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct NoteTag {
	uuid: UuidStr,
	// none if decryption fails
	#[cfg_attr(
		feature = "wasm-full",
		serde(default, skip_serializing_if = "Option::is_none"),
		tsify(type = "string")
	)]
	name: Option<String>,
	favorite: bool,
	#[cfg_attr(
		feature = "wasm-full",
		serde(with = "chrono::serde::ts_milliseconds"),
		tsify(type = "bigint")
	)]
	edited_timestamp: DateTime<Utc>,
	#[cfg_attr(
		feature = "wasm-full",
		serde(with = "chrono::serde::ts_milliseconds"),
		tsify(type = "bigint")
	)]
	created_timestamp: DateTime<Utc>,
}

impl NoteTag {
	fn blocking_decrypt_with_key(
		tag: &filen_types::api::v3::notes::NoteTag<'_>,
		crypter: &impl MetaCrypter,
	) -> Self {
		let name = NoteTagName::blocking_try_decrypt(crypter, &tag.name).ok();

		Self {
			uuid: tag.uuid,
			name,
			favorite: tag.favorite,
			edited_timestamp: tag.edited_timestamp,
			created_timestamp: tag.created_timestamp,
		}
	}

	pub fn name(&self) -> Option<&str> {
		self.name.as_deref()
	}

	pub fn favorited(&self) -> bool {
		self.favorite
	}

	pub fn created(&self) -> DateTime<Utc> {
		self.created_timestamp
	}

	pub fn edited(&self) -> DateTime<Utc> {
		self.edited_timestamp
	}
}

struct NoteParticipantParts {
	user_id: u64,
	email: String,
}

#[derive(Debug, PartialEq, Eq, Clone)]
#[cfg_attr(
	feature = "wasm-full",
	derive(serde::Serialize, serde::Deserialize, tsify::Tsify),
	tsify(into_wasm_abi, from_wasm_abi, large_number_types_as_bigints),
	serde(rename_all = "camelCase")
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct NoteParticipant {
	user_id: u64,
	is_owner: bool,
	email: String,
	#[cfg_attr(
		feature = "wasm-full",
		serde(default, skip_serializing_if = "Option::is_none"),
		tsify(type = "string")
	)]
	avatar: Option<String>,
	nick_name: String,
	permissions_write: bool,
	#[cfg_attr(
		feature = "wasm-full",
		serde(with = "chrono::serde::ts_milliseconds"),
		tsify(type = "bigint")
	)]
	added_timestamp: DateTime<Utc>,
}

impl NoteParticipant {
	pub fn user_id(&self) -> u64 {
		self.user_id
	}
}

#[derive(Debug, PartialEq, Eq, Clone)]
#[cfg_attr(
	feature = "wasm-full",
	derive(serde::Serialize, serde::Deserialize, tsify::Tsify),
	tsify(into_wasm_abi, from_wasm_abi, large_number_types_as_bigints),
	serde(rename_all = "camelCase")
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct Note {
	uuid: UuidStr,
	owner_id: u64,
	last_editor_id: u64,
	favorite: bool,
	pinned: bool,
	tags: Vec<NoteTag>,
	note_type: NoteType,
	// none if decryption fails
	#[cfg_attr(
		feature = "wasm-full",
		serde(default, skip_serializing_if = "Option::is_none"),
		tsify(type = "string")
	)]
	encryption_key: Option<NoteOrChatKey>,
	// none if decryption fails
	#[cfg_attr(
		feature = "wasm-full",
		serde(default, skip_serializing_if = "Option::is_none"),
		tsify(type = "string")
	)]
	title: Option<String>,
	// none if decryption fails
	#[cfg_attr(
		feature = "wasm-full",
		serde(default, skip_serializing_if = "Option::is_none"),
		tsify(type = "string")
	)]
	preview: Option<String>,
	trash: bool,
	archive: bool,
	#[cfg_attr(
		feature = "wasm-full",
		serde(with = "chrono::serde::ts_milliseconds"),
		tsify(type = "bigint")
	)]
	created_timestamp: DateTime<Utc>,
	#[cfg_attr(
		feature = "wasm-full",
		serde(with = "chrono::serde::ts_milliseconds"),
		tsify(type = "bigint")
	)]
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

	pub fn trashed(&self) -> bool {
		self.trash
	}

	pub fn archived(&self) -> bool {
		self.archive
	}

	pub fn title(&self) -> Option<&str> {
		self.title.as_deref()
	}

	pub fn preview(&self) -> Option<&str> {
		self.preview.as_deref()
	}

	pub fn tags(&self) -> &[NoteTag] {
		&self.tags
	}

	pub fn participants(&self) -> &[NoteParticipant] {
		&self.participants
	}

	pub fn participants_mut(&mut self) -> &mut [NoteParticipant] {
		&mut self.participants
	}

	pub async fn decrypt_string(&self, meta: &EncryptedString<'_>) -> Result<String, Error> {
		let key = self
			.encryption_key
			.as_ref()
			.ok_or(MetadataWasNotDecryptedError)?;
		key.decrypt_meta(meta)
			.await
			.context("decrypt_meta_with_note_key")
	}
}

#[derive(Clone, Debug)]
#[cfg_attr(
	feature = "wasm-full",
	derive(serde::Serialize, serde::Deserialize, tsify::Tsify),
	tsify(into_wasm_abi, from_wasm_abi, large_number_types_as_bigints),
	serde(rename_all = "camelCase")
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct NoteHistory {
	id: u64,
	#[cfg_attr(
		feature = "wasm-full",
		serde(default, skip_serializing_if = "Option::is_none"),
		tsify(type = "string")
	)]
	preview: Option<String>,
	#[cfg_attr(
		feature = "wasm-full",
		serde(default, skip_serializing_if = "Option::is_none"),
		tsify(type = "string")
	)]
	content: Option<String>,
	#[cfg_attr(
		feature = "wasm-full",
		serde(with = "chrono::serde::ts_milliseconds"),
		tsify(type = "bigint")
	)]
	edited_timestamp: DateTime<Utc>,
	editor_id: u64,
	note_type: NoteType,
}

impl NoteHistory {
	pub fn preview(&self) -> Option<&str> {
		self.preview.as_deref()
	}

	pub fn content(&self) -> Option<&str> {
		self.content.as_deref()
	}

	pub fn note_type(&self) -> NoteType {
		self.note_type
	}
}

impl NoteHistory {
	fn blocking_decrypt_with_key(
		note_history: &filen_types::api::v3::notes::history::NoteHistory<'_>,
		note_key: &NoteOrChatKey,
	) -> Self {
		let (preview, content) = blocking_join!(
			|| NotePreview::blocking_try_decrypt(note_key, &note_history.preview),
			|| NoteContent::blocking_try_decrypt(note_key, &note_history.content)
		);
		Self {
			id: note_history.id,
			preview: preview.ok(),
			content: content.ok(),
			edited_timestamp: note_history.edited_timestamp,
			editor_id: note_history.editor_id,
			note_type: note_history.note_type,
		}
	}
}

mod crypto {
	use std::borrow::Cow;

	use serde::{Deserialize, Serialize};

	use crate::crypto::notes_and_chats::impl_note_or_chat_carrier_crypto;

	#[derive(Deserialize, Serialize)]
	pub(super) struct NoteTitle<'a> {
		#[serde(borrow)]
		title: Cow<'a, str>,
	}
	impl_note_or_chat_carrier_crypto!(NoteTitle, title, "note title", str);

	#[derive(Deserialize, Serialize)]
	pub(super) struct NotePreview<'a> {
		#[serde(borrow)]
		preview: Cow<'a, str>,
	}
	impl_note_or_chat_carrier_crypto!(NotePreview, preview, "note preview", str);

	#[derive(Deserialize, Serialize)]
	pub(super) struct NoteTagName<'a> {
		#[serde(borrow)]
		name: Cow<'a, str>,
	}
	impl_note_or_chat_carrier_crypto!(NoteTagName, name, "note tag name", str);

	#[derive(Deserialize, Serialize)]
	pub(super) struct NoteContent<'a> {
		#[serde(borrow)]
		content: Cow<'a, str>,
	}
	impl_note_or_chat_carrier_crypto!(NoteContent, content, "note content", str);
}

impl Client {
	pub async fn is_shared(
		&self,
		uuid: UuidStr,
	) -> Result<api::v3::item::shared::Response<'static>, Error> {
		api::v3::item::shared::post(self.client(), &api::v3::item::shared::Request { uuid }).await
	}

	fn blocking_decrypt_note_key(
		&self,
		note: &filen_types::api::v3::notes::Note<'_>,
	) -> Result<NoteOrChatKey, Error> {
		let participant = note
			.participants
			.iter()
			.find(|p| p.user_id == self.user_id)
			.ok_or_else(|| {
				Error::custom(ErrorKind::Response, "User is not a participant in the note")
			})?;

		NoteOrChatKeyStruct::blocking_try_decrypt_rsa(self.private_key(), &participant.metadata)
	}

	fn blocking_decrypt_note(
		&self,
		crypter: &impl MetaCrypter,
		note: filen_types::api::v3::notes::Note<'_>,
	) -> Note {
		let key = self.blocking_decrypt_note_key(&note).ok();

		let tags_closure = || {
			note.tags
				.into_maybe_par_iter()
				.map(|tag| NoteTag::blocking_decrypt_with_key(&tag, crypter))
				.collect::<Vec<_>>()
		};

		let participants_closure = || {
			let mut participants = note
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

			participants.sort_by_key(|p| p.added_timestamp);
			participants
		};

		let (title, preview, tags, participants) = if let Some(key) = &key {
			blocking_join!(
				|| { NoteTitle::blocking_try_decrypt(key, &note.title).ok() },
				|| { NotePreview::blocking_try_decrypt(key, &note.preview).ok() },
				tags_closure,
				participants_closure
			)
		} else {
			let (tags, participants) = blocking_join!(tags_closure, participants_closure);
			(None, None, tags, participants)
		};

		Note {
			uuid: note.uuid,
			owner_id: note.owner_id,
			last_editor_id: note.editor_id,
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

	pub async fn list_notes(&self) -> Result<Vec<Note>, Error> {
		let notes = crate::api::v3::notes::get(self.client()).await?;

		let crypter = self.crypter();

		let notes = do_cpu_intensive(|| {
			notes
				.0
				.into_maybe_par_iter()
				.map(|note| self.blocking_decrypt_note(&*crypter, note))
				.collect::<Vec<_>>()
		})
		.await;

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
		let data = do_cpu_intensive(|| {
			NoteOrChatKeyStruct::blocking_try_encrypt_rsa(
				public_key,
				note.encryption_key
					.as_ref()
					.ok_or(MetadataWasNotDecryptedError)?,
			)
		})
		.await
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
			avatar: response.avatar.map(Cow::into_owned),
			nick_name: response.nick_name.into_owned(),
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
			},
		)
		.await
	}

	pub async fn remove_note_participant(
		&self,
		note: &mut Note,
		participant_id: u64,
	) -> Result<(), Error> {
		crate::api::v3::notes::participants::remove::post(
			self.client(),
			&crate::api::v3::notes::participants::remove::Request {
				uuid: note.uuid,
				user_id: participant_id,
			},
		)
		.await?;

		note.participants.retain(|p| p.user_id != participant_id);
		Ok(())
	}

	pub async fn set_note_participant_permission(
		&self,
		note_uuid: UuidStr,
		participant: &mut NoteParticipant,
		write: bool,
	) -> Result<(), Error> {
		crate::api::v3::notes::participants::permissions::post(
			self.client(),
			&crate::api::v3::notes::participants::permissions::Request {
				uuid: note_uuid,
				user_id: participant.user_id,
				permissions_write: write,
			},
		)
		.await?;

		participant.permissions_write = write;

		Ok(())
	}

	pub async fn get_note(&self, uuid: UuidStr) -> Result<Option<Note>, Error> {
		// I hate this
		self.list_notes()
			.await
			.map(|notes| notes.into_iter().find(|n| n.uuid == uuid))
	}

	pub async fn create_note(&self, title: Option<String>) -> Result<Note, Error> {
		let uuid = UuidStr::new_v4();
		let title = title.unwrap_or_else(|| Utc::now().format("%a %b %d %Y %X").to_string());
		let key = NoteOrChatKey::generate();

		let crypter = self.crypter();

		let ((key_string, title_string), _lock) = futures::join!(
			do_cpu_intensive(|| {
				blocking_join!(
					|| NoteOrChatKeyStruct::blocking_encrypt_symmetric(&*crypter, &key),
					|| NoteTitle::blocking_encrypt(&key, &title)
				)
			}),
			self.lock_notes()
		);

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
			last_editor_id: self.user_id,
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

		let key = note
			.encryption_key
			.as_ref()
			.ok_or(MetadataWasNotDecryptedError)?;

		let (preview, content) = do_cpu_intensive(|| {
			blocking_join!(
				|| NotePreview::blocking_try_decrypt(key, &response.preview),
				|| {
					if response.content.0.is_empty() {
						Some(String::new())
					} else {
						NoteContent::blocking_try_decrypt(key, &response.content).ok()
					}
				}
			)
		})
		.await;

		note.preview = preview.ok();
		Ok(content)
	}

	pub async fn set_note_type(
		&self,
		note: &mut Note,
		new_type: NoteType,
		known_content: Option<&str>,
	) -> Result<(), Error> {
		let _lock = self.lock_notes().await?;

		let content = if let Some(content) = known_content {
			Cow::Borrowed(content)
		} else {
			self.get_note_content(note)
				.await?
				.ok_or(MetadataWasNotDecryptedError)
				.map(Cow::Owned)?
		};

		let note_key = note
			.encryption_key
			.as_ref()
			.ok_or(MetadataWasNotDecryptedError)?;
		let preview = note.preview.as_ref().ok_or(MetadataWasNotDecryptedError)?;

		let (preview, content) = do_cpu_intensive(|| {
			blocking_join!(|| NotePreview::blocking_encrypt(note_key, preview,), || {
				NoteContent::blocking_encrypt(note_key, &content)
			})
		})
		.await;

		let resp = api::v3::notes::r#type::change::post(
			self.client(),
			&api::v3::notes::r#type::change::Request {
				uuid: note.uuid,
				preview,
				content,
				note_type: new_type,
			},
		)
		.await?;

		note.note_type = new_type;
		note.edited_timestamp = resp.edited_timestamp;
		note.last_editor_id = resp.editor_id;
		Ok(())
	}

	pub async fn set_note_content(
		&self,
		note: &mut Note,
		new_content: &str,
		new_preview: String,
	) -> Result<(), Error> {
		let note_key = note
			.encryption_key
			.as_ref()
			.ok_or(MetadataWasNotDecryptedError)?;

		let ((content, preview), _lock) = futures::join!(
			do_cpu_intensive(|| {
				blocking_join!(
					|| NoteContent::blocking_encrypt(note_key, new_content),
					|| NotePreview::blocking_encrypt(note_key, &new_preview)
				)
			}),
			self.lock_notes()
		);

		let response = api::v3::notes::content::edit::post(
			self.client(),
			&api::v3::notes::content::edit::Request {
				uuid: note.uuid,
				content,
				preview,
				note_type: note.note_type,
			},
		)
		.await?;
		note.preview = Some(new_preview);
		note.edited_timestamp = response.timestamp;
		note.last_editor_id = self.user_id;
		Ok(())
	}

	pub async fn set_note_title(&self, note: &mut Note, new_title: String) -> Result<(), Error> {
		let key = note
			.encryption_key
			.as_ref()
			.ok_or(MetadataWasNotDecryptedError)?;

		let (title, _lock) = futures::join!(
			do_cpu_intensive(|| NoteTitle::blocking_encrypt(key, &new_title,)),
			self.lock_notes()
		);

		api::v3::notes::title::edit::post(
			self.client(),
			&api::v3::notes::title::edit::Request {
				uuid: note.uuid,
				title,
			},
		)
		.await?;
		note.title = Some(new_title);
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
		note.trash = false;
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
		note.archive = false;
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
		self.set_note_content(
			&mut new,
			content.as_deref().unwrap_or(""),
			note.preview.clone().unwrap_or_default(),
		)
		.await?;

		self.set_note_type(&mut new, note.note_type, content.as_deref())
			.await?;
		Ok(new)
	}

	/// Get the edit history of a note, sorted by edited timestamp (oldest first).
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

		let histories = do_cpu_intensive(|| {
			let mut histories = histories
				.0
				.into_maybe_par_iter()
				.map(|h| NoteHistory::blocking_decrypt_with_key(&h, note_key))
				.collect::<Vec<_>>();
			histories.sort_by_key(|h| h.edited_timestamp);
			histories
		})
		.await;

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
		note.last_editor_id = history.editor_id;

		Ok(())
	}

	pub async fn add_tag_to_note(&self, note: &mut Note, tag: &mut NoteTag) -> Result<(), Error> {
		let _lock = self.lock_notes().await?;
		let resp = api::v3::notes::tag::post(
			self.client(),
			&api::v3::notes::tag::Request {
				uuid: note.uuid,
				tag: tag.uuid,
			},
		)
		.await?;

		tag.edited_timestamp = resp.edited_timestamp;

		// avoid duplicates
		if !note.tags.iter().any(|t| t.uuid == tag.uuid) {
			note.tags.push(tag.clone());
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

	pub async fn list_note_tags(&self) -> Result<Vec<NoteTag>, Error> {
		let response = api::v3::notes::tags::post(self.client()).await?;
		let crypter = self.crypter();

		let tags = do_cpu_intensive(|| {
			response
				.0
				.into_maybe_par_iter()
				.map(|tag| NoteTag::blocking_decrypt_with_key(&tag, &*crypter))
				.collect::<Vec<_>>()
		})
		.await;

		Ok(tags)
	}

	pub async fn create_note_tag(&self, name: String) -> Result<NoteTag, Error> {
		// is this necessary?
		if let Some(existing) = self.list_note_tags().await?.into_iter().find(|t| {
			if let Some(t_name) = &t.name {
				*t_name == name
			} else {
				false
			}
		}) {
			return Ok(existing);
		}

		let crypter = self.crypter();

		let (encrypted_name, _lock) = futures::join!(
			do_cpu_intensive(|| NoteTagName::blocking_encrypt(&*crypter, &name)),
			self.lock_notes()
		);

		let response = api::v3::notes::tags::create::post(
			self.client(),
			&api::v3::notes::tags::create::Request {
				name: encrypted_name,
			},
		)
		.await?;

		Ok(NoteTag {
			uuid: response.uuid,
			name: Some(name),
			favorite: false,
			edited_timestamp: response.timestamp,
			created_timestamp: response.timestamp,
		})
	}

	pub async fn rename_note_tag(&self, tag: &mut NoteTag, new_name: String) -> Result<(), Error> {
		let crypter = self.crypter();
		let (encrypted_name, _lock) = futures::join!(
			do_cpu_intensive(|| NoteTagName::blocking_encrypt(&*crypter, &new_name)),
			self.lock_notes()
		);

		let resp = api::v3::notes::tags::rename::post(
			self.client(),
			&api::v3::notes::tags::rename::Request {
				uuid: tag.uuid,
				name: encrypted_name,
			},
		)
		.await?;
		tag.name = Some(new_name);
		tag.edited_timestamp = resp.edited_timestamp;
		Ok(())
	}

	pub async fn set_note_tag_favorited(
		&self,
		tag: &mut NoteTag,
		favorite: bool,
	) -> Result<(), Error> {
		let _lock = self.lock_notes().await?;
		let resp = api::v3::notes::tags::favorite::post(
			self.client(),
			&api::v3::notes::tags::favorite::Request {
				uuid: tag.uuid,
				favorite,
			},
		)
		.await?;
		tag.favorite = favorite;
		tag.edited_timestamp = resp.edited_timestamp;
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

#[cfg(any(feature = "wasm-full", feature = "uniffi"))]
pub mod js_impls {
	use filen_types::{api::v3::notes::NoteType, crypto::EncryptedString, fs::UuidStr};

	use crate::{
		Error, auth::JsClient, connect::js_impls::Contact, notes::NoteParticipant,
		runtime::do_on_commander,
	};

	use super::{Note, NoteHistory, NoteTag};

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		derive(serde::Serialize, tsify::Tsify),
		tsify(into_wasm_abi)
	)]
	#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
	pub struct DuplicateNoteResponse {
		pub original: Note,
		pub duplicated: Note,
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		derive(serde::Serialize, tsify::Tsify),
		tsify(into_wasm_abi)
	)]
	#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
	pub struct AddTagToNoteResponse {
		pub note: Note,
		pub tag: NoteTag,
	}

	/// Decrypts note data using the provided chat key.
	/// Meant to be used in socket event handlers where this cannot currently be done automatically.
	///
	/// Should not be used outside of that context.
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	#[wasm_bindgen::prelude::wasm_bindgen(js_name = "decryptMetaWithNoteKey")]
	pub async fn decrypt_meta_with_note_key(
		note: Note,
		#[wasm_bindgen(unchecked_param_type = "EncryptedString")] encrypted: String,
	) -> Result<String, Error> {
		do_on_commander(move || async move {
			note.decrypt_string(&EncryptedString(std::borrow::Cow::Owned(encrypted)))
				.await
		})
		.await
	}

	#[cfg(feature = "uniffi")]
	#[uniffi::export]
	pub async fn decrypt_meta_with_note_key(
		note: Note,
		encrypted: EncryptedString<'static>,
	) -> Result<String, Error> {
		do_on_commander(move || async move { note.decrypt_string(&encrypted).await }).await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_class = "Client")
	)]
	#[cfg_attr(feature = "uniffi", uniffi::export)]
	impl JsClient {
		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "listNotes")
		)]
		pub async fn list_notes(&self) -> Result<Vec<Note>, Error> {
			let this = self.inner();
			do_on_commander(move || async move { this.list_notes().await }).await
		}

		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "addNoteParticipant")
		)]
		pub async fn add_note_participant(
			&self,
			mut note: Note,
			contact: Contact,
			write: bool,
		) -> Result<Note, Error> {
			let this = self.inner();
			do_on_commander(move || async move {
				this.add_note_participant(&mut note, &contact.into(), write)
					.await?;
				Ok(note)
			})
			.await
		}

		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "removeNoteParticipant")
		)]
		pub async fn remove_note_participant(
			&self,
			mut note: Note,
			participant_id: u64,
		) -> Result<Note, Error> {
			let this = self.inner();
			do_on_commander(move || async move {
				this.remove_note_participant(&mut note, participant_id)
					.await?;
				Ok(note)
			})
			.await
		}

		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "setNoteParticipantPermission")
		)]
		pub async fn set_note_participant_permission(
			&self,
			note_uuid: UuidStr,
			mut participant: NoteParticipant,
			write: bool,
		) -> Result<NoteParticipant, Error> {
			let this = self.inner();
			do_on_commander(move || async move {
				this.set_note_participant_permission(note_uuid, &mut participant, write)
					.await?;
				Ok(participant)
			})
			.await
		}

		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "getNote")
		)]
		pub async fn get_note(&self, note_uuid: UuidStr) -> Result<Option<Note>, Error> {
			let this = self.inner();
			do_on_commander(move || async move { this.get_note(note_uuid).await }).await
		}

		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "createNote")
		)]
		pub async fn create_note(&self, title: Option<String>) -> Result<Note, Error> {
			let this = self.inner();
			do_on_commander(move || async move { this.create_note(title).await }).await
		}

		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "getNoteContent")
		)]
		pub async fn get_note_content(&self, mut note: Note) -> Result<Option<String>, Error> {
			let this = self.inner();
			do_on_commander(move || async move { this.get_note_content(&mut note).await }).await
		}

		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "setNoteType")
		)]
		pub async fn set_note_type(
			&self,
			mut note: Note,
			note_type: NoteType,
			known_content: Option<String>,
		) -> Result<Note, Error> {
			let this = self.inner();
			do_on_commander(move || async move {
				this.set_note_type(&mut note, note_type, known_content.as_deref())
					.await?;
				Ok(note)
			})
			.await
		}

		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "setNoteContent")
		)]
		pub async fn set_note_content(
			&self,
			mut note: Note,
			new_content: String,
			new_preview: String,
		) -> Result<Note, Error> {
			let this = self.inner();
			do_on_commander(move || async move {
				this.set_note_content(&mut note, &new_content, new_preview)
					.await?;
				Ok(note)
			})
			.await
		}

		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "setNoteTitle")
		)]
		pub async fn set_note_title(
			&self,
			mut note: Note,
			new_title: String,
		) -> Result<Note, Error> {
			let this = self.inner();
			do_on_commander(move || async move {
				this.set_note_title(&mut note, new_title).await?;
				Ok(note)
			})
			.await
		}

		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "deleteNote")
		)]
		pub async fn delete_note(&self, note: Note) -> Result<(), Error> {
			let this = self.inner();
			do_on_commander(move || async move { this.delete_note(note).await }).await
		}

		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "archiveNote")
		)]
		pub async fn archive_note(&self, mut note: Note) -> Result<Note, Error> {
			let this = self.inner();
			do_on_commander(move || async move {
				this.archive_note(&mut note).await?;
				Ok(note)
			})
			.await
		}

		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "trashNote")
		)]
		pub async fn trash_note(&self, mut note: Note) -> Result<Note, Error> {
			let this = self.inner();
			do_on_commander(move || async move {
				this.trash_note(&mut note).await?;
				Ok(note)
			})
			.await
		}

		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "setNoteFavorited")
		)]
		pub async fn set_note_favorited(
			&self,
			mut note: Note,
			favorite: bool,
		) -> Result<Note, Error> {
			let this = self.inner();
			do_on_commander(move || async move {
				this.set_note_favorited(&mut note, favorite).await?;
				Ok(note)
			})
			.await
		}

		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "setNotePinned")
		)]
		pub async fn set_note_pinned(&self, mut note: Note, pinned: bool) -> Result<Note, Error> {
			let this = self.inner();
			do_on_commander(move || async move {
				this.set_note_pinned(&mut note, pinned).await?;
				Ok(note)
			})
			.await
		}

		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "restoreNote")
		)]
		pub async fn restore_note(&self, mut note: Note) -> Result<Note, Error> {
			let this = self.inner();
			do_on_commander(move || async move {
				this.restore_note(&mut note).await?;
				Ok(note)
			})
			.await
		}

		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "duplicateNote")
		)]
		pub async fn duplicate_note(&self, mut note: Note) -> Result<DuplicateNoteResponse, Error> {
			let this = self.inner();
			do_on_commander(move || async move {
				let new = this.duplicate_note(&mut note).await?;

				Ok(DuplicateNoteResponse {
					original: note,
					duplicated: new,
				})
			})
			.await
		}

		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "getNoteHistory")
		)]
		pub async fn get_note_history(&self, note: Note) -> Result<Vec<NoteHistory>, Error> {
			let this = self.inner();
			do_on_commander(move || async move { this.get_note_history(&note).await }).await
		}

		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "restoreNoteFromHistory")
		)]
		pub async fn restore_note_from_history(
			&self,
			mut note: Note,
			history: NoteHistory,
		) -> Result<Note, Error> {
			let this = self.inner();
			do_on_commander(move || async move {
				this.restore_note_from_history(&mut note, history).await?;
				Ok(note)
			})
			.await
		}

		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "listNoteTags")
		)]
		pub async fn list_note_tags(&self) -> Result<Vec<NoteTag>, Error> {
			let this = self.inner();
			do_on_commander(move || async move { this.list_note_tags().await }).await
		}

		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "addTagToNote")
		)]
		pub async fn add_tag_to_note(
			&self,
			mut note: Note,
			mut tag: NoteTag,
		) -> Result<AddTagToNoteResponse, Error> {
			let this = self.inner();
			let (note, tag) = do_on_commander(move || async move {
				this.add_tag_to_note(&mut note, &mut tag).await?;
				Ok::<_, Error>((note, tag))
			})
			.await?;

			Ok(AddTagToNoteResponse { note, tag })
		}

		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "removeTagFromNote")
		)]
		pub async fn remove_tag_from_note(
			&self,
			mut note: Note,
			tag: NoteTag,
		) -> Result<Note, Error> {
			let this = self.inner();
			do_on_commander(move || async move {
				this.remove_tag_from_note(&mut note, &tag).await?;
				Ok(note)
			})
			.await
		}

		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "createNoteTag")
		)]
		pub async fn create_note_tag(&self, name: String) -> Result<NoteTag, Error> {
			let this = self.inner();
			do_on_commander(move || async move { this.create_note_tag(name).await }).await
		}

		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "renameNoteTag")
		)]
		pub async fn rename_note_tag(
			&self,
			mut tag: NoteTag,
			new_name: String,
		) -> Result<NoteTag, Error> {
			let this = self.inner();
			do_on_commander(move || async move {
				this.rename_note_tag(&mut tag, new_name).await?;
				Ok(tag)
			})
			.await
		}

		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "setNoteTagFavorited")
		)]
		pub async fn set_note_tag_favorited(
			&self,
			mut tag: NoteTag,
			favorite: bool,
		) -> Result<NoteTag, Error> {
			let this = self.inner();
			do_on_commander(move || async move {
				this.set_note_tag_favorited(&mut tag, favorite).await?;
				Ok(tag)
			})
			.await
		}

		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "deleteNoteTag")
		)]
		pub async fn delete_note_tag(&self, tag: NoteTag) -> Result<(), Error> {
			let this = self.inner();
			do_on_commander(move || async move { this.delete_note_tag(tag).await }).await
		}
	}
}

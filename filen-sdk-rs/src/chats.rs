use std::{
	borrow::Cow,
	sync::{Arc, Mutex},
};

use chrono::{DateTime, Utc};
use filen_types::{
	api::v3::{
		chat::{last_focus_update::ChatLastFocusValues, typing::ChatTypingType},
		contacts::Contact,
	},
	fs::UuidStr,
	traits::CowHelpers,
};
use futures::{StreamExt, stream::FuturesUnordered};

use crate::{
	Error, ErrorKind, api,
	auth::Client,
	crypto::{
		notes_and_chats::{NoteOrChatCarrierCryptoExt, NoteOrChatKey, NoteOrChatKeyStruct},
		shared::CreateRandom,
	},
	error::{MetadataWasNotDecryptedError, ResultExt},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatParticipant {
	user_id: u64,
	email: String,
	avatar: Option<String>,
	nick_name: String,
	permissions_add: bool,
	added: DateTime<Utc>,
	appear_offline: bool,
	last_active: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatConversation {
	uuid: UuidStr,
	last_message: Option<ChatMessage>,
	owner_id: u64,
	key: Option<NoteOrChatKey>,
	name: Option<String>,
	participants: Vec<ChatParticipant>,
	muted: bool,
	created: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatMessagePartial {
	uuid: UuidStr,
	sender_id: u64,
	sender_email: String,
	sender_avatar: Option<String>,
	sender_nick_name: String,
	message: Option<String>,
}

impl ChatMessagePartial {
	fn decrypt(
		encrypted: filen_types::api::v3::chat::messages::ChatMessagePartial<'_>,
		chat_key: Option<&NoteOrChatKey>,
		tmp_vec: &mut Vec<u8>,
	) -> Self {
		Self {
			uuid: encrypted.uuid,
			sender_id: encrypted.sender_id,
			sender_email: encrypted.sender_email.into_owned(),
			sender_avatar: encrypted.sender_avatar.map(Cow::into_owned),
			sender_nick_name: encrypted.sender_nick_name.into_owned(),
			message: chat_key.and_then(|chat_key| {
				crypto::ChatMessage::try_decrypt(chat_key, &encrypted.message, tmp_vec).ok()
			}),
		}
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatMessage {
	conversation: UuidStr,
	inner: ChatMessagePartial,
	reply_to: Option<ChatMessagePartial>,
	embed_disabled: bool,
	edited: bool,
	edited_timestamp: DateTime<Utc>,
	sent_timestamp: DateTime<Utc>,
}

impl ChatMessage {
	fn decrypt(
		encrypted: filen_types::api::v3::chat::messages::ChatMessage<'_>,
		private_key: Option<&NoteOrChatKey>,
		tmp_vec: &mut Vec<u8>,
	) -> Self {
		Self {
			conversation: encrypted.conversation,
			inner: ChatMessagePartial::decrypt(encrypted.inner, private_key, tmp_vec),
			reply_to: encrypted
				.reply_to
				.map(|r| ChatMessagePartial::decrypt(r, private_key, tmp_vec)),
			embed_disabled: encrypted.embed_disabled,
			edited: encrypted.edited,
			edited_timestamp: encrypted.edited_timestamp,
			sent_timestamp: encrypted.sent_timestamp,
		}
	}
}

mod crypto {
	use std::borrow::Cow;

	use serde::{Deserialize, Serialize};

	use crate::crypto::notes_and_chats::impl_note_or_chat_carrier_crypto;

	#[derive(Deserialize, Serialize)]
	pub(super) struct ChatName<'a> {
		#[serde(borrow)]
		name: Cow<'a, str>,
	}
	impl_note_or_chat_carrier_crypto!(ChatName, name, "chat name", str);

	#[derive(Deserialize, Serialize)]
	pub(super) struct ChatMessage<'a> {
		#[serde(borrow)]
		message: Cow<'a, str>,
	}
	impl_note_or_chat_carrier_crypto!(ChatMessage, message, "chat message", str);
}

impl Client {
	fn decrypt_chat_key(
		&self,
		chat: &filen_types::api::v3::chat::conversations::ChatConversation<'_>,
	) -> Result<NoteOrChatKey, Error> {
		let participant = chat
			.participants
			.iter()
			.find(|p| p.user_id == self.user_id)
			.ok_or_else(|| {
				Error::custom(ErrorKind::Response, "User is not a participant in the chat")
			})?;

		NoteOrChatKeyStruct::try_decrypt_rsa(self.private_key(), &participant.metadata)
	}

	async fn list_messages(
		&self,
		conversation: &ChatConversation,
		before: DateTime<Utc>,
	) -> Result<Vec<ChatMessage>, Error> {
		let resp = api::v3::chat::messages::post(
			self.client(),
			&api::v3::chat::messages::Request {
				conversation: conversation.uuid,
				timestamp: before,
			},
		)
		.await?;

		let tmp_vec = &mut Vec::new();

		let messages = resp
			.0
			.into_iter()
			.map(|message| ChatMessage::decrypt(message, conversation.key.as_ref(), tmp_vec))
			.collect::<Vec<_>>();

		Ok(messages)
	}

	fn decrypt_chat_conversation(
		&self,
		encrypted: filen_types::api::v3::chat::conversations::ChatConversation<'_>,
		outer_tmp_vec: &mut Vec<u8>,
	) -> ChatConversation {
		let mut tmp_vec = std::mem::take(outer_tmp_vec);

		let key = self.decrypt_chat_key(&encrypted).ok();

		let (name, last_message) = key.as_ref().map_or((None, None), |k| {
			let chat_name = encrypted
				.name
				.as_ref()
				.and_then(|name| crypto::ChatName::try_decrypt(k, name, &mut tmp_vec).ok());
			let last_message = encrypted
				.last_message_full
				.as_ref()
				.map(|msg| ChatMessage::decrypt(msg.clone(), Some(k), &mut tmp_vec));

			(chat_name, last_message)
		});

		let participants = encrypted
			.participants
			.into_iter()
			.map(|p| ChatParticipant {
				user_id: p.user_id,
				email: p.email.into_owned(),
				avatar: p.avatar.map(Cow::into_owned),
				nick_name: p.nick_name.into_owned(),
				permissions_add: p.permissions_add,
				added: p.added_timestamp,
				// adjust if the backend returns online status
				appear_offline: true,
				last_active: DateTime::<Utc>::default(),
			})
			.collect::<Vec<_>>();

		ChatConversation {
			uuid: encrypted.uuid,
			last_message,
			owner_id: encrypted.owner_id,
			key,
			name,
			participants,
			muted: encrypted.muted,
			created: encrypted.created_timestamp,
		}
	}

	pub async fn list_conversations(&self) -> Result<Vec<ChatConversation>, Error> {
		let resp = api::v3::chat::conversations::get(self.client()).await?;

		let tmp_vec = &mut Vec::new();
		Ok(resp
			.0
			.into_iter()
			.map(|chat| self.decrypt_chat_conversation(chat, tmp_vec))
			.collect::<Vec<_>>())
	}

	async fn inner_add_chat_participant(
		&self,
		key: &NoteOrChatKey,
		chat_uuid: UuidStr,
		contact: &Contact<'_>,
	) -> Result<ChatParticipant, Error> {
		let metadata = NoteOrChatKeyStruct::try_encrypt_rsa(&contact.public_key, key)?;
		let _lock = self.lock_chats().await?;
		api::v3::chat::conversations::participants::add::post(
			self.client(),
			&api::v3::chat::conversations::participants::add::Request {
				uuid: chat_uuid,
				contact_uuid: contact.uuid,
				metadata,
			},
		)
		.await?;

		Ok(ChatParticipant {
			user_id: contact.user_id,
			email: contact.email.to_string(),
			avatar: contact.avatar.as_ref().map(|a| a.clone().into_owned()),
			nick_name: contact.nick_name.clone().into_owned(),
			permissions_add: false,
			// temporary
			added: chrono::Utc::now(),
			appear_offline: true,
			last_active: contact.last_active,
		})
	}

	pub async fn create_conversation(
		&self,
		contacts: &[Contact<'_>],
	) -> Result<ChatConversation, Error> {
		let uuid = UuidStr::new_v4();
		let key = NoteOrChatKey::generate();

		let key_string = NoteOrChatKeyStruct::encrypt_symmetric(self.crypter(), &key);
		let key_asymm_string = NoteOrChatKeyStruct::try_encrypt_rsa(self.public_key(), &key)?;
		let _lock = self.lock_chats().await?;

		api::v3::chat::conversations::create::post(
			self.client(),
			&api::v3::chat::conversations::create::Request {
				uuid,
				metadata: key_asymm_string.as_borrowed_cow(),
				owner_metadata: key_string.as_borrowed_cow(),
			},
		)
		.await?;

		let mut new = ChatConversation {
			uuid,
			last_message: None,
			owner_id: self.user_id,
			key: Some(key),
			name: None,
			// will be replaced later
			participants: Vec::new(),
			muted: false,
			created: chrono::Utc::now(),
		};
		let key = new
			.key
			.as_ref()
			.expect("we just created the chat, so the key must be Some");

		let mut participants = Vec::with_capacity(contacts.len() + 1);
		participants.push(ChatParticipant {
			user_id: self.user_id,
			email: self.email().to_string(),
			// temporary, remove when /v3/chat/conversations/create returns this
			avatar: Some(self.get_user_info().await?.avatar_url.into_owned()),
			nick_name: String::new(), // todo: get real nick name
			permissions_add: true,
			// todo: get real added time
			added: chrono::Utc::now(),
			appear_offline: false,
			last_active: chrono::Utc::now(),
		});

		let participants = Arc::new(Mutex::new(Vec::with_capacity(contacts.len())));

		let mut participant_futures = contacts
			.iter()
			.map(|contact| {
				let participants = participants.clone();
				async move {
					let participant = self
						.inner_add_chat_participant(key, new.uuid, contact)
						.await?;
					participants
						.lock()
						.unwrap_or_else(|e| e.into_inner())
						.push(participant);
					Ok::<(), Error>(())
				}
			})
			.collect::<FuturesUnordered<_>>();

		while let Some(res) = participant_futures.next().await {
			match res {
				Ok(()) => {}
				Err(e) => return Err(e.with_context("add chat participant")),
			}
		}
		std::mem::drop(participant_futures);

		let participants = match Arc::try_unwrap(participants) {
			Ok(mutex) => Mutex::into_inner(mutex).unwrap_or_else(|e| e.into_inner()),
			// should be unreachable
			Err(arc) => arc.lock().unwrap_or_else(|e| e.into_inner()).clone(),
		};

		new.participants = participants;

		Ok(new)
	}

	pub async fn rename_conversation(
		&self,
		conversation: &mut ChatConversation,
		new_name: String,
	) -> Result<(), Error> {
		let key = conversation
			.key
			.as_ref()
			.ok_or(MetadataWasNotDecryptedError)
			.context("rename_conversation")?;
		let encrypted_name = crypto::ChatName::encrypt(key, &new_name);
		let _lock = self.lock_chats().await?;

		api::v3::chat::conversations::name::edit::post(
			self.client(),
			&api::v3::chat::conversations::name::edit::Request {
				uuid: conversation.uuid,
				name: encrypted_name,
			},
		)
		.await?;

		conversation.name = Some(new_name);

		Ok(())
	}

	pub async fn delete_conversation(&self, chat: ChatConversation) -> Result<(), Error> {
		let _lock = self.lock_chats().await?;

		api::v3::chat::conversations::delete::post(
			self.client(),
			&api::v3::chat::conversations::delete::Request { uuid: chat.uuid },
		)
		.await
	}

	pub async fn send_chat_message<'a>(
		&self,
		conversation: &'a mut ChatConversation,
		message: String,
		reply_to: Option<ChatMessagePartial>,
	) -> Result<&'a ChatMessage, Error> {
		let key = conversation
			.key
			.as_ref()
			.ok_or(MetadataWasNotDecryptedError)
			.context("send_chat_message")?;
		let uuid = UuidStr::new_v4();

		let encrypted_message = crypto::ChatMessage::encrypt(key, &message);

		api::v3::chat::send::post(
			self.client(),
			&api::v3::chat::send::Request {
				conversation: conversation.uuid,
				uuid,
				message: encrypted_message,
				reply_to: reply_to.as_ref().map(|r| r.uuid),
			},
		)
		.await?;

		conversation.last_message = Some(ChatMessage {
			conversation: conversation.uuid,
			inner: ChatMessagePartial {
				uuid,
				sender_id: self.user_id,
				sender_email: self.email().to_string(),
				// todo get real avatar
				sender_avatar: None,
				sender_nick_name: String::new(),
				message: Some(message),
			},
			reply_to,
			embed_disabled: false,
			edited: false,
			// todo get real timestamps
			edited_timestamp: DateTime::<Utc>::default(),
			sent_timestamp: DateTime::<Utc>::default(),
		});

		Ok(conversation
			.last_message
			.as_ref()
			.expect("we just set it above"))
	}

	// this API is a bit annoying because ideally we'd want to allow the consumer to pass in a mutable reference to
	// the last message in the conversation if it exists, so we can update it in place
	// but we can't do this because we also need a reference to the inner key
	// and we don't want to expose that part of the struct publicly
	pub async fn edit_message(
		&self,
		conversation: &ChatConversation,
		message: &mut ChatMessage,
		new_message: String,
	) -> Result<(), Error> {
		let key = conversation
			.key
			.as_ref()
			.ok_or(MetadataWasNotDecryptedError)
			.context("edit_message")?;

		let encrypted_message = crypto::ChatMessage::encrypt(key, &new_message);

		api::v3::chat::edit::post(
			self.client(),
			&api::v3::chat::edit::Request {
				conversation: message.conversation,
				uuid: message.inner.uuid,
				message: encrypted_message,
			},
		)
		.await?;

		message.inner.message = Some(new_message);
		message.edited = true;
		// todo get real edited timestamp
		message.edited_timestamp = chrono::Utc::now();

		Ok(())
	}

	pub async fn delete_message(
		&self,
		conversation: &mut ChatConversation,
		message: &ChatMessage,
	) -> Result<(), Error> {
		let _lock = self.lock_chats().await?;

		api::v3::chat::delete::post(
			self.client(),
			&api::v3::chat::delete::Request {
				uuid: message.inner.uuid,
			},
		)
		.await?;

		// is there a last message
		if let Some(last_message) = &conversation.last_message
			// is it the same message
			&& last_message.inner.uuid == message.inner.uuid
		{
			let messages = self.list_messages(conversation, chrono::Utc::now()).await?;
			conversation.last_message = messages.into_iter().next();
		}

		Ok(())
	}

	pub async fn disable_message_embed(&self, message: &mut ChatMessage) -> Result<(), Error> {
		api::v3::chat::message::embed::disable::post(
			self.client(),
			&api::v3::chat::message::embed::disable::Request {
				uuid: message.inner.uuid,
			},
		)
		.await?;

		message.embed_disabled = true;
		Ok(())
	}

	pub async fn send_typing_signal(
		&self,
		conversation: &ChatConversation,
		signal_type: ChatTypingType,
	) -> Result<(), Error> {
		api::v3::chat::typing::post(
			self.client(),
			&api::v3::chat::typing::Request {
				conversation: conversation.uuid,
				signal_type,
			},
		)
		.await
	}

	pub async fn add_chat_participant(
		&self,
		chat: &mut ChatConversation,
		contact: &Contact<'_>,
	) -> Result<(), Error> {
		let key = chat
			.key
			.as_ref()
			.ok_or(MetadataWasNotDecryptedError)
			.context("add_chat_participant")?;

		let participant = self
			.inner_add_chat_participant(key, chat.uuid, contact)
			.await?;

		chat.participants.push(participant);

		Ok(())
	}

	pub async fn remove_chat_participant(
		&self,
		chat: &ChatConversation,
		contact: &Contact<'_>,
	) -> Result<(), Error> {
		let _lock = self.lock_chats().await?;

		api::v3::chat::conversations::participants::remove::post(
			self.client(),
			&api::v3::chat::conversations::participants::remove::Request {
				uuid: chat.uuid,
				user_id: contact.user_id,
			},
		)
		.await
	}

	pub async fn mark_conversation_read(&self, chat: &ChatConversation) -> Result<(), Error> {
		api::v3::chat::conversations::read::post(
			self.client(),
			&api::v3::chat::conversations::read::Request { uuid: chat.uuid },
		)
		.await
	}

	pub async fn get_conversation_unread_count(
		&self,
		chat: &ChatConversation,
	) -> Result<u64, Error> {
		Ok(api::v3::chat::conversations::unread::post(
			self.client(),
			&api::v3::chat::conversations::unread::Request { uuid: chat.uuid },
		)
		.await?
		.unread)
	}

	pub async fn get_all_conversations_unread_count(&self) -> Result<u64, Error> {
		Ok(api::v3::chat::unread::get(self.client()).await?.unread)
	}

	pub async fn update_conversation_online_status(
		&self,
		conversation: &mut ChatConversation,
	) -> Result<(), Error> {
		let resp = api::v3::chat::conversations::online::post(
			self.client(),
			&api::v3::chat::conversations::online::Request {
				conversation: conversation.uuid,
			},
		)
		.await?;
		conversation.participants.iter_mut().for_each(|p| {
			let status = resp.0.iter().find(|s| s.user_id == p.user_id);

			p.last_active = status.map_or(p.last_active, |s| s.last_active);
			p.appear_offline = status.map_or(p.appear_offline, |s| s.appear_offline);
		});

		Ok(())
	}

	pub async fn leave_conversation(&self, chat: &ChatConversation) -> Result<(), Error> {
		if self.user_id == chat.owner_id {
			return Err(
				Error::custom(ErrorKind::Server, "Owner cannot leave the chat")
					.with_context("leave conversation"),
			);
		}

		let _lock = self.lock_chats().await?;
		api::v3::chat::conversations::leave::post(
			self.client(),
			&api::v3::chat::conversations::leave::Request { uuid: chat.uuid },
		)
		.await
	}

	pub async fn get_last_chat_focus_times(&self) -> Result<Vec<ChatLastFocusValues>, Error> {
		Ok(api::v3::chat::last_focus::get(self.client()).await?.0)
	}

	pub async fn mute_chat(
		&self,
		conversation: &mut ChatConversation,
		mute: bool,
	) -> Result<(), Error> {
		let _lock = self.lock_chats().await?;
		api::v3::chat::mute::post(
			self.client(),
			&api::v3::chat::mute::Request {
				uuid: conversation.uuid,
				mute,
			},
		)
		.await?;

		conversation.muted = mute;

		Ok(())
	}
}

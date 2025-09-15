use std::borrow::Cow;

use chrono::{DateTime, Utc};
use filen_types::fs::UuidStr;
use rsa::RsaPublicKey;
use serde::{Deserialize, Serialize};
use tsify::Tsify;
use wasm_bindgen::{JsValue, prelude::wasm_bindgen};

use crate::{
	Error,
	auth::Client,
	connect::{DirPublicLink, FilePublicLink},
	fs::dir::DirectoryMetaType,
	js::{Dir, DirWithMetaEnum, File, SharedDir, SharedFile},
	tuple_to_jsvalue,
};

#[derive(Serialize, Deserialize, Tsify)]
#[tsify(into_wasm_abi, from_wasm_abi, large_number_types_as_bigints)]
#[serde(rename_all = "camelCase")]
pub struct Contact {
	pub uuid: UuidStr,
	pub user_id: u64,
	pub email: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	#[tsify(type = "string")]
	pub avatar: Option<String>,
	pub nick_name: String,
	#[serde(with = "chrono::serde::ts_milliseconds")]
	#[tsify(type = "bigint")]
	pub last_active: DateTime<Utc>,
	#[tsify(type = "bigint")]
	#[serde(with = "chrono::serde::ts_milliseconds")]
	pub timestamp: DateTime<Utc>,
	#[tsify(type = "string")]
	#[serde(with = "filen_types::serde::rsa::public_key_der")]
	pub public_key: RsaPublicKey,
}

impl From<filen_types::api::v3::contacts::Contact<'_>> for Contact {
	fn from(c: filen_types::api::v3::contacts::Contact<'_>) -> Self {
		Self {
			uuid: c.uuid,
			user_id: c.user_id,
			email: c.email.into_owned(),
			avatar: c.avatar.map(|a| a.into_owned()),
			nick_name: c.nick_name.into_owned(),
			last_active: c.last_active,
			timestamp: c.timestamp,
			public_key: c.public_key,
		}
	}
}

impl From<Contact> for filen_types::api::v3::contacts::Contact<'static> {
	fn from(c: Contact) -> Self {
		Self {
			uuid: c.uuid,
			user_id: c.user_id,
			email: Cow::Owned(c.email),
			avatar: c.avatar.map(Cow::Owned),
			nick_name: Cow::Owned(c.nick_name),
			last_active: c.last_active,
			timestamp: c.timestamp,
			public_key: c.public_key,
		}
	}
}

#[derive(Serialize, Deserialize, Tsify)]
#[tsify(into_wasm_abi, from_wasm_abi, large_number_types_as_bigints)]
#[serde(rename_all = "camelCase")]
pub struct BlockedContact {
	pub uuid: UuidStr,
	pub user_id: u64,
	pub email: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	#[tsify(type = "string")]
	pub avatar: Option<String>,
	pub nick_name: String,
	#[tsify(type = "bigint")]
	#[serde(with = "chrono::serde::ts_milliseconds")]
	pub timestamp: DateTime<Utc>,
}

impl From<filen_types::api::v3::contacts::blocked::BlockedContact<'_>> for BlockedContact {
	fn from(c: filen_types::api::v3::contacts::blocked::BlockedContact<'_>) -> Self {
		Self {
			uuid: c.uuid,
			user_id: c.user_id,
			email: c.email.into_owned(),
			avatar: c.avatar.map(|a| a.into_owned()),
			nick_name: c.nick_name.into_owned(),
			timestamp: c.timestamp,
		}
	}
}

#[derive(Serialize, Deserialize, Tsify)]
#[tsify(into_wasm_abi, from_wasm_abi, large_number_types_as_bigints)]
#[serde(rename_all = "camelCase")]
pub struct ContactRequestIn {
	pub uuid: UuidStr,
	pub user_id: u64,
	pub email: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	#[tsify(type = "string")]
	pub avatar: Option<String>,
	pub nick_name: String,
}

impl From<filen_types::api::v3::contacts::requests::r#in::ContactRequestIn<'_>>
	for ContactRequestIn
{
	fn from(c: filen_types::api::v3::contacts::requests::r#in::ContactRequestIn<'_>) -> Self {
		Self {
			uuid: c.uuid,
			user_id: c.user_id,
			email: c.email.into_owned(),
			avatar: c.avatar.map(|a| a.into_owned()),
			nick_name: c.nick_name.into_owned(),
		}
	}
}

impl<'a> From<&'a ContactRequestIn>
	for filen_types::api::v3::contacts::requests::r#in::ContactRequestIn<'a>
{
	fn from(c: &'a ContactRequestIn) -> Self {
		Self {
			uuid: c.uuid,
			user_id: c.user_id,
			email: Cow::Borrowed(&c.email),
			avatar: c.avatar.as_deref().map(Cow::Borrowed),
			nick_name: Cow::Borrowed(&c.nick_name),
		}
	}
}

#[derive(Serialize, Deserialize, Tsify)]
#[tsify(into_wasm_abi, from_wasm_abi, large_number_types_as_bigints)]
#[serde(rename_all = "camelCase")]
pub struct ContactRequestOut {
	pub uuid: UuidStr,
	pub email: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	#[tsify(type = "string")]
	pub avatar: Option<String>,
	pub nick_name: String,
}

impl From<filen_types::api::v3::contacts::requests::out::ContactRequestOut<'_>>
	for ContactRequestOut
{
	fn from(c: filen_types::api::v3::contacts::requests::out::ContactRequestOut<'_>) -> Self {
		Self {
			uuid: c.uuid,
			email: c.email.into_owned(),
			avatar: c.avatar.map(|a| a.into_owned()),
			nick_name: c.nick_name.into_owned(),
		}
	}
}

impl<'a> From<&'a ContactRequestOut>
	for filen_types::api::v3::contacts::requests::out::ContactRequestOut<'a>
{
	fn from(c: &'a ContactRequestOut) -> Self {
		Self {
			uuid: c.uuid,
			email: Cow::Borrowed(&c.email),
			avatar: c.avatar.as_deref().map(Cow::Borrowed),
			nick_name: Cow::Borrowed(&c.nick_name),
		}
	}
}

#[wasm_bindgen]
impl Client {
	// Public Links

	#[wasm_bindgen(js_name = "publicLinkDir")]
	pub async fn public_link_dir_js(&self, dir: Dir) -> Result<DirPublicLink, Error> {
		self.public_link_dir(&dir.into()).await
	}

	#[wasm_bindgen(js_name = "publicLinkFile")]
	pub async fn public_link_file_js(&self, file: File) -> Result<FilePublicLink, Error> {
		self.public_link_file(&file.try_into()?).await
	}

	#[wasm_bindgen(js_name = "updateDirLink")]
	pub async fn update_dir_link_js(&self, dir: Dir, link: DirPublicLink) -> Result<(), Error> {
		self.update_dir_link(&dir.into(), &link).await
	}

	#[wasm_bindgen(js_name = "updateFileLink")]
	pub async fn update_file_link_js(&self, file: File, link: FilePublicLink) -> Result<(), Error> {
		self.update_file_link(&file.try_into()?, &link).await
	}

	#[wasm_bindgen(js_name = "getFileLinkStatus")]
	pub async fn get_file_link_status_js(
		&self,
		file: File,
	) -> Result<Option<FilePublicLink>, Error> {
		self.get_file_link_status(&file.try_into()?).await
	}

	// This is annoying because I can't map this to either of the basic file types
	// I probably have to make a new base file type and implement everything for it
	// then pass that around just for this one use case
	// #[wasm_bindgen(js_name = "getLinkedFile")]
	// pub async fn get_linked_file_js(
	// 	&self,
	// 	link: FilePublicLink,
	// ) -> Result<JsValue, Error> {
	// 	todo!()
	// }

	#[wasm_bindgen(js_name = "getDirLinkStatus")]
	pub async fn get_dir_link_status_js(&self, dir: Dir) -> Result<Option<DirPublicLink>, Error> {
		self.get_dir_link_status(&dir.into()).await
	}

	#[wasm_bindgen(js_name = "listLinkedDir", unchecked_return_type = "[Dir[], File[]]")]
	pub async fn list_linked_dir_js(
		&self,
		dir: DirWithMetaEnum,
		link: DirPublicLink,
	) -> Result<JsValue, Error> {
		let (dirs, files) = self
			.list_linked_dir(&DirectoryMetaType::from(dir), &link)
			.await?;
		Ok(tuple_to_jsvalue!(
			dirs.into_iter().map(Dir::from).collect::<Vec<_>>(),
			files.into_iter().map(File::from).collect::<Vec<_>>()
		))
	}

	#[wasm_bindgen(js_name = "removeDirLink")]
	pub async fn remove_dir_link_js(&self, link: DirPublicLink) -> Result<(), Error> {
		self.remove_dir_link(link).await
	}

	// Contacts

	#[wasm_bindgen(js_name = "getContacts", unchecked_return_type = "Contact[]")]
	pub async fn get_contacts_js(&self) -> Result<JsValue, JsValue> {
		let contacts = self.get_contacts().await?;
		let contacts = contacts.into_iter().map(Contact::from).collect::<Vec<_>>();
		Ok(serde_wasm_bindgen::to_value(&contacts)?)
	}

	#[wasm_bindgen(js_name = "sendContactRequest")]
	pub async fn send_contact_request_js(&self, email: &str) -> Result<UuidStr, Error> {
		self.send_contact_request(email).await
	}

	#[wasm_bindgen(js_name = "cancelContactRequest")]
	pub async fn cancel_contact_request_js(&self, contact_uuid: UuidStr) -> Result<(), Error> {
		self.cancel_contact_request(contact_uuid).await
	}

	#[wasm_bindgen(js_name = "acceptContactRequest")]
	pub async fn accept_contact_request_js(&self, contact_uuid: UuidStr) -> Result<UuidStr, Error> {
		self.accept_contact_request(contact_uuid).await
	}

	#[wasm_bindgen(js_name = "denyContactRequest")]
	pub async fn deny_contact_request_js(&self, contact_uuid: UuidStr) -> Result<(), Error> {
		self.deny_contact_request(contact_uuid).await
	}

	#[wasm_bindgen(js_name = "deleteContact")]
	pub async fn delete_contact_js(&self, contact_uuid: UuidStr) -> Result<(), Error> {
		self.delete_contact(contact_uuid).await
	}

	#[wasm_bindgen(
		js_name = "listIncomingContactRequests",
		unchecked_return_type = "ContactRequestIn[]"
	)]
	pub async fn list_incoming_contact_requests_js(&self) -> Result<JsValue, JsValue> {
		let requests = self.list_incoming_contact_requests().await?;
		let requests = requests
			.into_iter()
			.map(ContactRequestIn::from)
			.collect::<Vec<_>>();
		Ok(serde_wasm_bindgen::to_value(&requests)?)
	}

	#[wasm_bindgen(
		js_name = "listOutgoingContactRequests",
		unchecked_return_type = "ContactRequestOut[]"
	)]
	pub async fn list_outgoing_contact_requests_js(&self) -> Result<JsValue, JsValue> {
		let requests = self.list_outgoing_contact_requests().await?;
		let requests = requests
			.into_iter()
			.map(ContactRequestOut::from)
			.collect::<Vec<_>>();
		Ok(serde_wasm_bindgen::to_value(&requests)?)
	}

	#[wasm_bindgen(
		js_name = "getBlockedContacts",
		unchecked_return_type = "BlockedContact[]"
	)]
	pub async fn get_blocked_contacts_js(&self) -> Result<JsValue, JsValue> {
		let contacts = self.get_blocked_contacts().await?;
		let contacts = contacts
			.into_iter()
			.map(BlockedContact::from)
			.collect::<Vec<_>>();
		Ok(serde_wasm_bindgen::to_value(&contacts)?)
	}

	#[wasm_bindgen(js_name = "blockContact")]
	pub async fn block_contact_js(&self, email: &str) -> Result<UuidStr, Error> {
		self.block_contact(email).await
	}

	#[wasm_bindgen(js_name = "unblockContact")]
	pub async fn unblock_contact_js(&self, contact_uuid: UuidStr) -> Result<(), Error> {
		self.unblock_contact(contact_uuid).await
	}

	// Sharing

	#[wasm_bindgen(js_name = "shareDir")]
	pub async fn share_dir_js(&self, dir: Dir, contact: Contact) -> Result<(), Error> {
		self.share_dir(&dir.into(), &contact.into()).await
	}

	#[wasm_bindgen(js_name = "shareFile")]
	pub async fn share_file_js(&self, file: File, contact: Contact) -> Result<(), Error> {
		self.share_file(&file.try_into()?, &contact.into()).await
	}

	#[wasm_bindgen(
		js_name = "listOutShared",
		unchecked_return_type = "[SharedDirectory[], SharedFile[]]"
	)]
	pub async fn list_out_shared_js(
		&self,
		dir: Option<DirWithMetaEnum>,
		contact: Option<Contact>,
	) -> Result<JsValue, Error> {
		let (dirs, files) = self
			.inner_list_out_shared(
				dir.map(DirectoryMetaType::from).as_ref(),
				contact.map(Into::into).as_ref(),
			)
			.await?;
		Ok(tuple_to_jsvalue!(
			dirs.into_iter().map(SharedDir::from).collect::<Vec<_>>(),
			files.into_iter().map(SharedFile::from).collect::<Vec<_>>()
		))
	}

	#[wasm_bindgen(
		js_name = "listInShared",
		unchecked_return_type = "[SharedDirectory[], SharedFile[]]"
	)]
	pub async fn list_in_shared_js(&self, dir: Option<DirWithMetaEnum>) -> Result<JsValue, Error> {
		let (dirs, files) = self
			.inner_list_in_shared(dir.map(DirectoryMetaType::from).as_ref())
			.await?;
		Ok(tuple_to_jsvalue!(
			dirs.into_iter().map(SharedDir::from).collect::<Vec<_>>(),
			files.into_iter().map(SharedFile::from).collect::<Vec<_>>()
		))
	}

	#[wasm_bindgen(js_name = "removeSharedLinkIn")]
	pub async fn remove_shared_link_in_js(&self, link_uuid: UuidStr) -> Result<(), Error> {
		self.remove_shared_link_in(link_uuid).await
	}

	#[wasm_bindgen(js_name = "removeSharedLinkOut")]
	pub async fn remove_shared_link_out_js(
		&self,
		link_uuid: UuidStr,
		#[wasm_bindgen(unchecked_param_type = "bigint")] receiver_id: u64,
	) -> Result<(), Error> {
		self.remove_shared_link_out(link_uuid, receiver_id).await
	}
}

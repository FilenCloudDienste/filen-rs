use std::borrow::Cow;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use filen_types::fs::{ParentUuid, UuidStr};
use rsa::RsaPublicKey;
use serde::{Deserialize, Serialize};

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use crate::auth::js_impls::UnauthJsClient;
use crate::{
	Error,
	auth::{JsClient, http::UnauthorizedClient, shared_client::SharedClient},
	connect::{DirPublicLink, FilePublicLink, PublicLinkSharedClientExt},
	fs::dir::DirectoryMetaType,
	js::{Dir, DirWithMetaEnum, DirsAndFiles, File, SharedDir, SharedFile, SharedRootItem},
	runtime::{self, do_on_commander},
};
#[cfg(feature = "uniffi")]
use crate::{
	auth::js_impls::UnauthJsClient,
	connect::{DirPublicLinkU, FilePublicLinkU},
	fs::dir::js_impl::DirContentDownloadProgressCallback,
};

#[derive(Serialize, Deserialize)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(into_wasm_abi, from_wasm_abi, large_number_types_as_bigints)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "camelCase")]
pub struct Contact {
	pub uuid: UuidStr,
	pub user_id: u64,
	pub email: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "string")
	)]
	#[cfg_attr(feature = "uniffi", uniffi(default = None))]
	pub avatar: Option<String>,
	pub nick_name: String,
	#[serde(with = "chrono::serde::ts_milliseconds")]
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint")
	)]
	pub last_active: DateTime<Utc>,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint")
	)]
	#[serde(with = "chrono::serde::ts_milliseconds")]
	pub timestamp: DateTime<Utc>,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "string")
	)]
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

#[derive(Serialize, Deserialize)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(into_wasm_abi, from_wasm_abi, large_number_types_as_bigints)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "camelCase")]
pub struct BlockedContact {
	pub uuid: UuidStr,
	pub user_id: u64,
	pub email: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "string")
	)]
	#[cfg_attr(feature = "uniffi", uniffi(default = None))]
	pub avatar: Option<String>,
	pub nick_name: String,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint")
	)]
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

#[derive(Serialize, Deserialize)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(into_wasm_abi, from_wasm_abi, large_number_types_as_bigints)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "camelCase")]
pub struct ContactRequestIn {
	pub uuid: UuidStr,
	pub user_id: u64,
	pub email: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "string")
	)]
	#[cfg_attr(feature = "uniffi", uniffi(default = None))]
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

#[derive(Serialize, Deserialize)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(into_wasm_abi, from_wasm_abi, large_number_types_as_bigints)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "camelCase")]
pub struct ContactRequestOut {
	pub uuid: UuidStr,
	pub email: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "string")
	)]
	#[cfg_attr(feature = "uniffi", uniffi(default = None))]
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

#[derive(Serialize)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(into_wasm_abi, large_number_types_as_bigints)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct SharedDirsAndFiles {
	pub dirs: Vec<SharedDir>,
	pub files: Vec<SharedFile>,
}

#[cfg(feature = "uniffi")]
// We need to separate these out because otherwise we get
// implementation of `std::marker::Send` is not general enough
// errors.
// Probably due to compiler bugs with async.
impl JsClient {
	async fn inner_public_link_dir<F>(&self, dir: Dir, callback: F) -> Result<DirPublicLink, Error>
	where
		F: Fn(u64, Option<u64>) + Send + Sync + 'static,
	{
		let this = self.inner();
		runtime::do_on_commander(move || async move {
			let dir = dir.into();
			this.public_link_dir(&dir, &callback).await
		})
		.await
	}

	async fn inner_share_dir<F>(&self, dir: Dir, contact: Contact, callback: F) -> Result<(), Error>
	where
		F: Fn(u64, Option<u64>) + Send + Sync + 'static,
	{
		let this = self.inner();
		do_on_commander(move || {
			let contact = contact.into();
			let dir = dir.into();
			async move { this.share_dir(&dir, &contact, &callback).await }
		})
		.await
	}
}

#[cfg(feature = "uniffi")]
#[uniffi::export]
impl JsClient {
	pub async fn public_link_dir(
		&self,
		dir: Dir,
		callback: Arc<dyn DirContentDownloadProgressCallback>,
	) -> Result<DirPublicLinkU, Error> {
		self.inner_public_link_dir(dir, move |downloaded, total| {
			let callback = Arc::clone(&callback);
			tokio::task::spawn_blocking(move || {
				callback.on_progress(downloaded, total);
			});
		})
		.await
		.map(DirPublicLinkU::from)
	}

	pub async fn update_dir_link(&self, dir: Dir, link: Arc<DirPublicLinkU>) -> Result<(), Error> {
		let link = match Arc::try_unwrap(link) {
			Ok(link) => link.inner.into_inner().unwrap_or_else(|e| e.into_inner()),
			Err(e) => e.inner.lock().unwrap_or_else(|e| e.into_inner()).clone(),
		};
		self.update_dir_link_inner(dir, link).await
	}

	pub async fn get_dir_link_status(
		&self,
		dir: Dir,
	) -> Result<Option<Arc<DirPublicLinkU>>, Error> {
		self.get_dir_link_status_inner(dir)
			.await
			.map(|o| o.map(|link| Arc::new(DirPublicLinkU::from(link))))
	}

	pub async fn share_dir(
		&self,
		dir: Dir,
		contact: Contact,
		callback: Arc<dyn DirContentDownloadProgressCallback>,
	) -> Result<(), Error> {
		self.inner_share_dir(dir, contact, move |downloaded, total| {
			let callback = Arc::clone(&callback);
			tokio::task::spawn_blocking(move || {
				callback.on_progress(downloaded, total);
			});
		})
		.await
	}

	pub async fn update_file_link(
		&self,
		file: File,
		link: Arc<FilePublicLinkU>,
	) -> Result<(), Error> {
		let link = match Arc::try_unwrap(link) {
			Ok(link) => link.inner.into_inner().unwrap_or_else(|e| e.into_inner()),
			Err(e) => e.inner.lock().unwrap_or_else(|e| e.into_inner()).clone(),
		};

		self.update_file_link_inner(file, link).await
	}

	pub async fn remove_file_link(
		&self,
		file: File,
		link: Arc<FilePublicLinkU>,
	) -> Result<(), Error> {
		let link = match Arc::try_unwrap(link) {
			Ok(link) => link.inner.into_inner().unwrap_or_else(|e| e.into_inner()),
			Err(e) => e.inner.lock().unwrap_or_else(|e| e.into_inner()).clone(),
		};
		self.remove_file_link_inner(file, link).await
	}

	pub async fn get_file_link_status(
		&self,
		file: File,
	) -> Result<Option<Arc<FilePublicLinkU>>, Error> {
		self.get_file_link_status_inner(file)
			.await
			.map(|o| o.map(|link| Arc::new(FilePublicLinkU::from(link))))
	}

	pub async fn public_link_file(&self, file: File) -> Result<Arc<FilePublicLinkU>, Error> {
		self.public_link_file_inner(file)
			.await
			.map(|link| Arc::new(FilePublicLinkU::from(link)))
	}

	pub async fn remove_dir_link(&self, link: Arc<DirPublicLinkU>) -> Result<(), Error> {
		let link = match Arc::try_unwrap(link) {
			Ok(link) => link.inner.into_inner().unwrap_or_else(|e| e.into_inner()),
			Err(e) => e.inner.lock().unwrap_or_else(|e| e.into_inner()).clone(),
		};
		self.remove_dir_link_inner(link).await
	}
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
#[wasm_bindgen::prelude::wasm_bindgen(js_class = "Client")]
impl JsClient {
	#[wasm_bindgen::prelude::wasm_bindgen(js_name = "publicLinkDir")]
	pub async fn public_link_dir(
		&self,
		dir: Dir,
		#[wasm_bindgen(
			unchecked_param_type = "(downloadedBytes: number, totalBytes: number | undefined) => void"
		)]
		callback: web_sys::js_sys::Function,
	) -> Result<DirPublicLink, Error> {
		use crate::runtime;
		use wasm_bindgen::JsValue;
		let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();

		runtime::spawn_local(async move {
			while let Some((downloaded, total)) = receiver.recv().await {
				let _ = callback.call2(
					&JsValue::UNDEFINED,
					&JsValue::from_f64(downloaded as f64),
					&match total {
						Some(v) => JsValue::from_f64(v as f64),
						None => JsValue::UNDEFINED,
					},
				);
			}
		});

		let this = self.inner();
		runtime::do_on_commander(move || async move {
			let dir = dir.into();
			this.public_link_dir(&dir, &move |downloaded, total| {
				let _ = sender.send((downloaded, total));
			})
			.await
		})
		.await
	}

	#[wasm_bindgen::prelude::wasm_bindgen(js_name = "shareDir")]
	pub async fn share_dir(
		&self,
		dir: Dir,
		contact: Contact,
		#[wasm_bindgen(
			unchecked_param_type = "(downloadedBytes: number, totalBytes: number | undefined) => void"
		)]
		callback: web_sys::js_sys::Function,
	) -> Result<(), Error> {
		use crate::runtime;
		use wasm_bindgen::JsValue;
		let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();

		runtime::spawn_local(async move {
			while let Some((downloaded, total)) = receiver.recv().await {
				let _ = callback.call2(
					&JsValue::UNDEFINED,
					&JsValue::from_f64(downloaded as f64),
					&match total {
						Some(v) => JsValue::from_f64(v as f64),
						None => JsValue::UNDEFINED,
					},
				);
			}
		});

		let this = self.inner();

		do_on_commander(move || {
			let contact = contact.into();
			let dir = dir.into();
			async move {
				this.share_dir(&dir, &contact, &move |downloaded, total| {
					let _ = sender.send((downloaded, total));
				})
				.await
			}
		})
		.await
	}
}

#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	wasm_bindgen::prelude::wasm_bindgen(js_class = "Client")
)]
impl JsClient {
	// Public Links
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "updateFileLink")
	)]
	pub async fn update_file_link_inner(
		&self,
		file: File,
		link: FilePublicLink,
	) -> Result<(), Error> {
		let this = self.inner();
		runtime::do_on_commander(move || async move {
			this.update_file_link(&file.try_into()?, &link).await
		})
		.await
	}

	pub async fn remove_file_link_inner(
		&self,
		file: File,
		link: FilePublicLink,
	) -> Result<(), Error> {
		let this = self.inner();
		runtime::do_on_commander(move || async move {
			this.remove_file_link(&file.try_into()?, link).await
		})
		.await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "getFileLinkStatus")
	)]
	pub async fn get_file_link_status_inner(
		&self,
		file: File,
	) -> Result<Option<FilePublicLink>, Error> {
		let this = self.inner();
		runtime::do_on_commander(move || async move {
			this.get_file_link_status(&file.try_into()?).await
		})
		.await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "publicLinkFile")
	)]
	pub async fn public_link_file_inner(&self, file: File) -> Result<FilePublicLink, Error> {
		let this = self.inner();
		runtime::do_on_commander(
			move || async move { this.public_link_file(&file.try_into()?).await },
		)
		.await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "updateDirLink")
	)]
	pub async fn update_dir_link_inner(&self, dir: Dir, link: DirPublicLink) -> Result<(), Error> {
		let this = self.inner();
		runtime::do_on_commander(
			move || async move { this.update_dir_link(&dir.into(), &link).await },
		)
		.await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "getDirLinkStatus")
	)]
	pub async fn get_dir_link_status_inner(
		&self,
		dir: Dir,
	) -> Result<Option<DirPublicLink>, Error> {
		let this = self.inner();
		runtime::do_on_commander(move || async move { this.get_dir_link_status(&dir.into()).await })
			.await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "removeDirLink")
	)]
	pub async fn remove_dir_link_inner(&self, link: DirPublicLink) -> Result<(), Error> {
		let this = self.inner();
		runtime::do_on_commander(move || async move { this.remove_dir_link(link).await }).await
	}
}

#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	wasm_bindgen::prelude::wasm_bindgen(js_class = "Client")
)]
#[cfg_attr(feature = "uniffi", uniffi::export)]
impl JsClient {
	// This is annoying because I can't map this to either of the basic file types
	// I probably have to make a new base file type and implement everything for it
	// then pass that around just for this one use case
	// #[cfg_attr(
	// all(target_family = "wasm", target_os = "unknown"),
	// wasm_bindgen::prelude::wasm_bindgen(js_name = "getLinkedFile"))]
	// pub async fn get_linked_file(
	// 	&self,
	// 	link: FilePublicLink,
	// ) -> Result<JsValue, Error> {
	// 	todo!()
	// }
	//

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "listLinkedItems")
	)]
	pub async fn list_linked_items(&self) -> Result<DirsAndFiles, Error> {
		let this = self.inner();
		let (dirs, files) = runtime::do_on_commander(move || async move {
			this.list_dir(&ParentUuid::Links).await.map(|(d, f)| {
				(
					d.into_iter().map(Dir::from).collect(),
					f.into_iter().map(File::from).collect(),
				)
			})
		})
		.await?;

		Ok(DirsAndFiles { dirs, files })
	}

	// Contacts

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "getContacts")
	)]
	pub async fn get_contacts(&self) -> Result<Vec<Contact>, Error> {
		let this = self.inner();
		let contacts = runtime::do_on_commander(move || async move {
			this.get_contacts()
				.await
				.map(|contacts| contacts.into_iter().map(Contact::from).collect::<Vec<_>>())
		})
		.await?;
		Ok(contacts)
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "sendContactRequest")
	)]
	pub async fn send_contact_request(&self, email: String) -> Result<UuidStr, Error> {
		let this = self.inner();
		do_on_commander(move || async move { this.send_contact_request(&email).await }).await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "cancelContactRequest")
	)]
	pub async fn cancel_contact_request(&self, contact_uuid: UuidStr) -> Result<(), Error> {
		let this = self.inner();
		do_on_commander(move || async move { this.cancel_contact_request(contact_uuid).await })
			.await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "acceptContactRequest")
	)]
	pub async fn accept_contact_request(&self, contact_uuid: UuidStr) -> Result<UuidStr, Error> {
		let this = self.inner();
		do_on_commander(move || async move { this.accept_contact_request(contact_uuid).await })
			.await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "denyContactRequest")
	)]
	pub async fn deny_contact_request(&self, contact_uuid: UuidStr) -> Result<(), Error> {
		let this = self.inner();
		do_on_commander(move || async move { this.deny_contact_request(contact_uuid).await }).await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "deleteContact")
	)]
	pub async fn delete_contact(&self, contact_uuid: UuidStr) -> Result<(), Error> {
		let this = self.inner();
		do_on_commander(move || async move { this.delete_contact(contact_uuid).await }).await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "listIncomingContactRequests",)
	)]
	pub async fn list_incoming_contact_requests(&self) -> Result<Vec<ContactRequestIn>, Error> {
		let this = self.inner();
		let requests = do_on_commander(move || async move {
			this.list_incoming_contact_requests().await.map(|requests| {
				requests
					.into_iter()
					.map(ContactRequestIn::from)
					.collect::<Vec<_>>()
			})
		})
		.await?;
		Ok(requests)
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "listOutgoingContactRequests",)
	)]
	pub async fn list_outgoing_contact_requests(&self) -> Result<Vec<ContactRequestOut>, Error> {
		let this = self.inner();
		let requests = do_on_commander(move || async move {
			this.list_outgoing_contact_requests().await.map(|requests| {
				requests
					.into_iter()
					.map(ContactRequestOut::from)
					.collect::<Vec<_>>()
			})
		})
		.await?;
		Ok(requests)
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "getBlockedContacts")
	)]
	pub async fn get_blocked_contacts(&self) -> Result<Vec<BlockedContact>, Error> {
		let this = self.inner();
		let contacts = do_on_commander(move || async move {
			this.get_blocked_contacts().await.map(|contacts| {
				contacts
					.into_iter()
					.map(BlockedContact::from)
					.collect::<Vec<_>>()
			})
		})
		.await?;
		Ok(contacts)
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "blockContact")
	)]
	pub async fn block_contact(&self, email: String) -> Result<UuidStr, Error> {
		let this = self.inner();
		do_on_commander(move || async move { this.block_contact(&email).await }).await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "unblockContact")
	)]
	pub async fn unblock_contact(&self, contact_uuid: UuidStr) -> Result<(), Error> {
		let this = self.inner();
		do_on_commander(move || async move { this.unblock_contact(contact_uuid).await }).await
	}

	// Sharing

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "shareFile")
	)]
	pub async fn share_file(&self, file: File, contact: Contact) -> Result<(), Error> {
		let this = self.inner();
		do_on_commander(
			move || async move { this.share_file(&file.try_into()?, &contact.into()).await },
		)
		.await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "listOutShared")
	)]
	pub async fn list_out_shared(
		&self,
		dir: Option<DirWithMetaEnum>,
		contact: Option<Contact>,
	) -> Result<SharedDirsAndFiles, Error> {
		let this = self.inner();
		let (dirs, files) = runtime::do_on_commander(move || async move {
			this.inner_list_out_shared(
				dir.map(DirectoryMetaType::from).as_ref(),
				contact.map(Into::into).as_ref(),
			)
			.await
			.map(|(dirs, files)| {
				(
					dirs.into_iter().map(SharedDir::from).collect::<Vec<_>>(),
					files.into_iter().map(SharedFile::from).collect::<Vec<_>>(),
				)
			})
		})
		.await?;
		Ok(SharedDirsAndFiles { dirs, files })
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "listInShared",)
	)]
	pub async fn list_in_shared(
		&self,
		dir: Option<DirWithMetaEnum>,
	) -> Result<SharedDirsAndFiles, Error> {
		let this = self.inner();
		let (dirs, files) = runtime::do_on_commander(move || async move {
			this.inner_list_in_shared(dir.map(DirectoryMetaType::from).as_ref())
				.await
				.map(|(dirs, files)| {
					(
						dirs.into_iter().map(SharedDir::from).collect::<Vec<_>>(),
						files.into_iter().map(SharedFile::from).collect::<Vec<_>>(),
					)
				})
		})
		.await?;
		Ok(SharedDirsAndFiles { dirs, files })
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "removeSharedItem")
	)]
	pub async fn remove_shared_item(&self, item: SharedRootItem) -> Result<(), Error> {
		let this = self.inner();
		do_on_commander(move || async move { this.remove_shared_item(item.try_into()?).await })
			.await
	}
}

async fn list_linked_dir_inner_generic<F, T, C>(
	client: Arc<T>,
	dir: DirWithMetaEnum,
	link: DirPublicLink,
	callback: F,
) -> Result<DirsAndFiles, Error>
where
	F: Fn(u64, Option<u64>) + Send + Sync + 'static,
	T: SharedClient<C> + Send + Sync + 'static,
	C: UnauthorizedClient + 'static,
{
	let (dirs, files) = runtime::do_on_commander(move || async move {
		client
			.list_linked_dir(&DirectoryMetaType::from(dir), &link, &callback)
			.await
			.map(|(dirs, files)| {
				(
					dirs.into_iter().map(Dir::from).collect::<Vec<_>>(),
					files.into_iter().map(File::from).collect::<Vec<_>>(),
				)
			})
	})
	.await?;

	Ok(DirsAndFiles { dirs, files })
}

#[cfg(feature = "uniffi")]
async fn list_linked_dir_uniffi<T, C>(
	client: Arc<T>,
	dir: DirWithMetaEnum,
	link: Arc<DirPublicLinkU>,
	callback: Arc<dyn DirContentDownloadProgressCallback>,
) -> Result<DirsAndFiles, Error>
where
	T: SharedClient<C> + Send + Sync + 'static,
	C: UnauthorizedClient + 'static,
{
	let link = match Arc::try_unwrap(link) {
		Ok(link) => link.inner.into_inner().unwrap_or_else(|e| e.into_inner()),
		Err(e) => e.inner.lock().unwrap_or_else(|e| e.into_inner()).clone(),
	};
	list_linked_dir_inner_generic(client, dir, link, move |downloaded, total| {
		let callback = Arc::clone(&callback);
		tokio::task::spawn_blocking(move || {
			callback.on_progress(downloaded, total);
		});
	})
	.await
}

#[cfg(feature = "uniffi")]
#[uniffi::export]
impl JsClient {
	pub async fn list_linked_dir(
		&self,
		dir: DirWithMetaEnum,
		link: Arc<DirPublicLinkU>,
		callback: Arc<dyn DirContentDownloadProgressCallback>,
	) -> Result<DirsAndFiles, Error> {
		list_linked_dir_uniffi(self.inner(), dir, link, callback).await
	}
}

#[cfg(feature = "uniffi")]
#[uniffi::export]
impl UnauthJsClient {
	pub async fn list_linked_dir(
		&self,
		dir: DirWithMetaEnum,
		link: Arc<DirPublicLinkU>,
		callback: Arc<dyn DirContentDownloadProgressCallback>,
	) -> Result<DirsAndFiles, Error> {
		list_linked_dir_uniffi(self.inner(), dir, link, callback).await
	}
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
async fn list_linked_dir_wasm<T, C>(
	client: Arc<T>,
	dir: DirWithMetaEnum,
	link: DirPublicLink,
	callback: web_sys::js_sys::Function,
) -> Result<DirsAndFiles, Error>
where
	T: SharedClient<C> + Send + Sync + 'static,
	C: UnauthorizedClient + 'static,
{
	use crate::runtime;
	use wasm_bindgen::JsValue;
	let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();

	runtime::spawn_local(async move {
		while let Some((downloaded, total)) = receiver.recv().await {
			let _ = callback.call2(
				&JsValue::UNDEFINED,
				&JsValue::from_f64(downloaded as f64),
				&match total {
					Some(v) => JsValue::from_f64(v as f64),
					None => JsValue::UNDEFINED,
				},
			);
		}
	});

	list_linked_dir_inner_generic(client, dir, link, move |downloaded, total| {
		let _ = sender.send((downloaded, total));
	})
	.await
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
#[wasm_bindgen::prelude::wasm_bindgen(js_class = "Client")]
impl JsClient {
	#[wasm_bindgen::prelude::wasm_bindgen(js_name = "listLinkedDir")]
	pub async fn list_linked_dir(
		&self,
		dir: DirWithMetaEnum,
		link: DirPublicLink,
		#[wasm_bindgen(
			unchecked_param_type = "(downloadedBytes: number, totalBytes: number | undefined) => void"
		)]
		callback: web_sys::js_sys::Function,
	) -> Result<DirsAndFiles, Error> {
		list_linked_dir_wasm(self.inner(), dir, link, callback).await
	}
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
#[wasm_bindgen::prelude::wasm_bindgen(js_class = "UnauthClient")]
impl UnauthJsClient {
	#[wasm_bindgen::prelude::wasm_bindgen(js_name = "listLinkedDir")]
	pub async fn list_linked_dir(
		&self,
		dir: DirWithMetaEnum,
		link: DirPublicLink,
		#[wasm_bindgen(
			unchecked_param_type = "(downloadedBytes: number, totalBytes: number | undefined) => void"
		)]
		callback: web_sys::js_sys::Function,
	) -> Result<DirsAndFiles, Error> {
		list_linked_dir_wasm(self.inner(), dir, link, callback).await
	}
}

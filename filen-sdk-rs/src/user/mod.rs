pub mod events;
#[cfg(any(feature = "uniffi", feature = "wasm-full"))]
pub mod js;

use std::borrow::Cow;

use chrono::{DateTime, Utc};
use filen_macros::js_type;
use filen_types::{
	api::v3::user::{
		account::{Personal, UserAccountPlan, UserAccountSubs, UserAccountSubsInvoices},
		events::UserEventDeserializeError,
	},
	fs::UuidStr,
	serde::str::Base64EncodedBytes,
};
#[cfg(feature = "multi-threaded-crypto")]
use rayon::iter::ParallelIterator;
use url::Url;

use crate::{
	api,
	auth::Client,
	error::{Error, ResultExt},
	runtime::do_cpu_intensive,
	user::events::DecryptedUserEvent,
	util::IntoMaybeParallelIterator,
};

impl Client {
	pub async fn get_user_info(&self) -> Result<UserInfo, Error> {
		let (settings, info, account) = futures::try_join!(
			api::v3::user::settings::get(self.client()),
			api::v3::user::info::get(self.client()),
			api::v3::user::account::get(self.client())
		)?;

		Ok(UserInfo {
			id: info.id,
			email: info.email.into_owned(),
			is_premium: info.is_premium,
			storage_used: info.storage_used,
			max_storage: info.max_storage,
			avatar_url: info.avatar_url.into_owned(),
			root_dir_uuid: info.root_dir_uuid,

			two_factor_enabled: settings.two_factor_enabled,
			two_factor_key: if settings.two_factor_enabled {
				Some(settings.two_factor_key.into_owned())
			} else {
				None
			},
			unfinished_files: settings.unfinished_files,
			unfinished_storage: settings.unfinished_storage,
			versioned_files: settings.versioned_files,
			versioned_storage: settings.versioned_storage,
			versioning_enabled: settings.versioning_enabled,
			login_alerts_enabled: settings.login_alerts_enabled,

			aff_balance: account.aff_balance,
			aff_count: account.aff_count,
			aff_earnings: account.aff_earnings,
			aff_id: account.aff_id,
			aff_rate: account.aff_rate,
			personal: account.personal,
			plans: account.plans,
			ref_id: account.ref_id,
			ref_limit: account.ref_limit,
			ref_storage: account.ref_storage,
			refer_count: account.refer_count,
			refer_storage: account.refer_storage,
			nick_name: account.nick_name,
			display_name: account.display_name,
			appear_offline: account.appear_offline,
			subs: account.subs,
			subs_invoices: account.subs_invoices,
			did_export_master_keys: account.did_export_master_keys,
		})
	}

	pub async fn delete_all_items(&self) -> Result<(), Error> {
		api::v3::user::delete::all::post(self.client()).await
	}

	pub async fn delete_all_versions(&self) -> Result<(), Error> {
		api::v3::user::delete::versions::post(self.client()).await
	}

	pub async fn get_gdpr_info(&self) -> Result<GdprInfo, Error> {
		let resp = api::v3::user::gdpr::get(self.client()).await?;

		Ok(GdprInfo {
			user: GdprUser {
				email: resp.user.email.into_owned(),
				last_active: resp.user.last_active,
				last_active_chat: resp.user.last_active_chat,
				last_ip_address: resp.user.last_ip_address.into_owned(),
				nick_name: resp.user.nick_name.map(|v| v.into_owned()),
				first_name: resp.user.personal.first_name.map(|v| v.into_owned()),
				last_name: resp.user.personal.last_name.map(|v| v.into_owned()),
				company_name: resp.user.personal.company_name.map(|v| v.into_owned()),
				vat_id: resp.user.personal.vat_id.map(|v| v.into_owned()),
				street: resp.user.personal.street.map(|v| v.into_owned()),
				street_number: resp.user.personal.street_number.map(|v| v.into_owned()),
				city: resp.user.personal.city.map(|v| v.into_owned()),
				postal_code: resp.user.personal.postal_code.map(|v| v.into_owned()),
				country: resp.user.personal.country.map(|v| v.into_owned()),
			},
			events: GdprEvents {
				ip_addresses: resp
					.events
					.ip_addresses
					.into_iter()
					.map(|s| s.into_owned())
					.collect(),
				user_agents: resp
					.events
					.user_agents
					.into_iter()
					.map(|s| s.into_owned())
					.collect(),
			},
		})
	}

	pub async fn update_personal_info(&self, info: &UpdatePersonalInfo<'_>) -> Result<(), Error> {
		api::v3::user::personal::update::post(
			self.client(),
			&api::v3::user::personal::update::Request {
				city: info.city.map(Cow::Borrowed),
				company_name: info.company_name.map(Cow::Borrowed),
				country: info.country.map(Cow::Borrowed),
				first_name: info.first_name.map(Cow::Borrowed),
				last_name: info.last_name.map(Cow::Borrowed),
				postal_code: info.postal_code.map(Cow::Borrowed),
				street: info.street.map(Cow::Borrowed),
				street_number: info.street_number.map(Cow::Borrowed),
				vat_id: info.vat_id.map(Cow::Borrowed),
			},
		)
		.await
	}

	pub async fn set_nickname(&self, nickname: &str) -> Result<(), Error> {
		api::v3::user::nickname::post(
			self.client(),
			&api::v3::user::nickname::Request {
				nickname: Cow::Borrowed(nickname),
			},
		)
		.await
	}

	pub async fn upload_avatar(&self, buffer: &[u8]) -> Result<Url, Error> {
		let base64 = Base64EncodedBytes::from(buffer);

		api::v3::user::avatar::post(
			self.client(),
			&api::v3::user::avatar::Request {
				// The API requires the hash of the base64 string, not the raw bytes
				hash: base64.sha512_hash().context("hashing base64 of avatar")?,
				avatar: base64,
			},
		)
		.await
		.map(|resp| resp.avatar_url)
	}

	pub async fn set_versioning_enabled(&self, enabled: bool) -> Result<(), Error> {
		api::v3::user::versioning::post(
			self.client(),
			&api::v3::user::versioning::Request { enabled },
		)
		.await
	}

	pub async fn set_login_alerts_enabled(&self, enabled: bool) -> Result<(), Error> {
		api::v3::user::login_alerts::post(
			self.client(),
			&api::v3::user::login_alerts::Request { enabled },
		)
		.await
	}

	pub async fn get_user_events(
		&self,
		filter: Option<&str>,
		timestamp: Option<DateTime<Utc>>,
	) -> Result<Vec<Result<DecryptedUserEvent, UserEventDeserializeError>>, Error> {
		let filter = filter.unwrap_or("all");
		let timestamp = timestamp.unwrap_or_else(|| Utc::now() + chrono::Duration::seconds(60));
		let events = api::v3::user::events::post(
			self.client(),
			&api::v3::user::events::Request {
				filter: Cow::Borrowed(filter),
				timestamp,
			},
		)
		.await?
		.events;
		let crypter = self.crypter();
		Ok(do_cpu_intensive(move || {
			events
				.into_maybe_par_iter()
				.map(|result| {
					result
						.map(|event| DecryptedUserEvent::blocking_from_encrypted(&*crypter, event))
				})
				.collect()
		})
		.await)
	}

	pub async fn get_user_event(&self, uuid: UuidStr) -> Result<DecryptedUserEvent, Error> {
		let event =
			api::v3::user::event::post(self.client(), &api::v3::user::event::Request { uuid })
				.await?;
		let crypter = self.crypter();
		Ok(
			do_cpu_intensive(move || DecryptedUserEvent::blocking_from_encrypted(&*crypter, event))
				.await,
		)
	}
}

#[derive(Debug, Clone, PartialEq)]
#[js_type(export, no_default)]
pub struct UserInfo {
	// user/info
	pub id: u64,
	pub email: String,
	pub is_premium: bool,
	#[cfg_attr(feature = "wasm-full", tsify(type = "bigint"))]
	pub storage_used: u64,
	#[cfg_attr(feature = "wasm-full", tsify(type = "bigint"))]
	pub max_storage: u64,
	pub avatar_url: String,
	pub root_dir_uuid: UuidStr,

	// user/settings
	pub two_factor_enabled: bool,
	pub two_factor_key: Option<String>,
	#[cfg_attr(feature = "wasm-full", tsify(type = "bigint"))]
	pub unfinished_files: u64,
	#[cfg_attr(feature = "wasm-full", tsify(type = "bigint"))]
	pub unfinished_storage: u64,
	#[cfg_attr(feature = "wasm-full", tsify(type = "bigint"))]
	pub versioned_files: u64,
	#[cfg_attr(feature = "wasm-full", tsify(type = "bigint"))]
	pub versioned_storage: u64,
	pub versioning_enabled: bool,
	pub login_alerts_enabled: bool,

	// user/account
	pub aff_balance: f64,
	#[cfg_attr(feature = "wasm-full", tsify(type = "bigint"))]
	pub aff_count: u64,
	pub aff_earnings: f64,
	pub aff_id: String,
	pub aff_rate: f64,
	// TODO: Figure out what the invoice type is
	// pub invoices: Vec<()>,
	pub personal: Personal,
	pub plans: Vec<UserAccountPlan>,
	pub ref_id: String,
	#[cfg_attr(feature = "wasm-full", tsify(type = "bigint"))]
	pub ref_limit: u64,
	#[cfg_attr(feature = "wasm-full", tsify(type = "bigint"))]
	pub ref_storage: u64,
	#[cfg_attr(feature = "wasm-full", tsify(type = "bigint"))]
	pub refer_count: u64,
	#[cfg_attr(feature = "wasm-full", tsify(type = "bigint"))]
	pub refer_storage: u64,
	pub nick_name: String,
	pub display_name: String,
	pub appear_offline: bool,
	pub subs: Vec<UserAccountSubs>,
	pub subs_invoices: Vec<UserAccountSubsInvoices>,
	pub did_export_master_keys: bool,
}

#[js_type(export, no_deser)]
pub struct GdprInfo {
	pub user: GdprUser,
	pub events: GdprEvents,
}

#[js_type(export, no_deser)]
pub struct GdprUser {
	pub email: String,
	#[cfg_attr(
		feature = "wasm-full",
		serde(with = "chrono::serde::ts_milliseconds"),
		tsify(type = "bigint")
	)]
	pub last_active: DateTime<Utc>,
	#[cfg_attr(
		feature = "wasm-full",
		serde(with = "chrono::serde::ts_milliseconds"),
		tsify(type = "bigint")
	)]
	pub last_active_chat: DateTime<Utc>,
	pub last_ip_address: String,
	pub nick_name: Option<String>,
	pub first_name: Option<String>,
	pub last_name: Option<String>,
	pub company_name: Option<String>,
	pub vat_id: Option<String>,
	pub street: Option<String>,
	pub street_number: Option<String>,
	pub city: Option<String>,
	pub postal_code: Option<String>,
	pub country: Option<String>,
}

#[js_type(export, no_deser)]
pub struct GdprEvents {
	pub ip_addresses: Vec<String>,
	pub user_agents: Vec<String>,
}

#[derive(Debug, Default, Clone)]
pub struct UpdatePersonalInfo<'a> {
	pub city: Option<&'a str>,
	pub company_name: Option<&'a str>,
	pub country: Option<&'a str>,
	pub first_name: Option<&'a str>,
	pub last_name: Option<&'a str>,
	pub postal_code: Option<&'a str>,
	pub street: Option<&'a str>,
	pub street_number: Option<&'a str>,
	pub vat_id: Option<&'a str>,
}

#[cfg(any(feature = "uniffi", feature = "wasm-full"))]
#[js_type(import, no_ser)]
pub struct UserPersonalUpdateInfo {
	pub city: Option<String>,
	pub company_name: Option<String>,
	pub country: Option<String>,
	pub first_name: Option<String>,
	pub last_name: Option<String>,
	pub postal_code: Option<String>,
	pub street: Option<String>,
	pub street_number: Option<String>,
	pub vat_id: Option<String>,
}

#[cfg(any(feature = "uniffi", feature = "wasm-full"))]
mod js_impl {
	use crate::{auth::JsClient, runtime::do_on_commander};

	use super::*;

	#[cfg_attr(
		feature = "wasm-full",
		wasm_bindgen::prelude::wasm_bindgen(js_class = "Client")
	)]
	#[cfg_attr(feature = "uniffi", uniffi::export)]
	impl JsClient {
		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "getUserInfo")
		)]
		pub async fn get_user_info(&self) -> Result<UserInfo, crate::error::Error> {
			let client = self.inner();
			do_on_commander(move || async move { client.get_user_info().await }).await
		}

		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "deleteAllItems")
		)]
		pub async fn delete_all_items(&self) -> Result<(), crate::error::Error> {
			let client = self.inner();
			do_on_commander(move || async move { client.delete_all_items().await }).await
		}

		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "deleteAllVersions")
		)]
		pub async fn delete_all_versions(&self) -> Result<(), crate::error::Error> {
			let client = self.inner();
			do_on_commander(move || async move { client.delete_all_versions().await }).await
		}

		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "getGdprInfo")
		)]
		pub async fn get_gdpr_info(&self) -> Result<GdprInfo, crate::error::Error> {
			let client = self.inner();
			do_on_commander(move || async move { client.get_gdpr_info().await }).await
		}

		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "updatePersonalInfo")
		)]
		pub async fn update_personal_info(
			&self,
			info: UserPersonalUpdateInfo,
		) -> Result<(), Error> {
			let this = self.inner();

			do_on_commander(move || async move {
				this.update_personal_info(&UpdatePersonalInfo {
					city: info.city.as_deref(),
					company_name: info.company_name.as_deref(),
					country: info.country.as_deref(),
					first_name: info.first_name.as_deref(),
					last_name: info.last_name.as_deref(),
					postal_code: info.postal_code.as_deref(),
					street: info.street.as_deref(),
					street_number: info.street_number.as_deref(),
					vat_id: info.vat_id.as_deref(),
				})
				.await
			})
			.await
		}

		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "setNickname")
		)]
		pub async fn set_nickname(&self, nickname: String) -> Result<(), Error> {
			let this = self.inner();
			do_on_commander(move || async move { this.set_nickname(&nickname).await }).await
		}

		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "uploadAvatar")
		)]
		pub async fn upload_avatar(&self, buffer: Vec<u8>) -> Result<String, Error> {
			let this = self.inner();
			do_on_commander(move || async move { this.upload_avatar(&buffer).await })
				.await
				.map(|url| url.to_string())
		}

		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "setVersioningEnabled")
		)]
		pub async fn set_versioning_enabled(&self, enabled: bool) -> Result<(), Error> {
			let this = self.inner();
			do_on_commander(move || async move { this.set_versioning_enabled(enabled).await }).await
		}

		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "setLoginAlertsEnabled")
		)]
		pub async fn set_login_alerts_enabled(&self, enabled: bool) -> Result<(), Error> {
			let this = self.inner();
			do_on_commander(move || async move { this.set_login_alerts_enabled(enabled).await })
				.await
		}

		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "getUserEvents")
		)]
		pub async fn get_user_events(
			&self,
			filter: Option<String>,
			timestamp: Option<i64>,
		) -> Result<Vec<crate::user::js::events::UserEventResult>, Error> {
			let this = self.inner();
			do_on_commander(move || async move {
				let timestamp = timestamp.and_then(|t| DateTime::<Utc>::from_timestamp(t, 0));
				let events = this.get_user_events(filter.as_deref(), timestamp).await?;
				Ok(events.into_iter().map(Into::into).collect())
			})
			.await
		}

		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "getUserEvent")
		)]
		pub async fn get_user_event(
			&self,
			uuid: UuidStr,
		) -> Result<crate::user::js::events::UserEvent, Error> {
			let this = self.inner();
			do_on_commander(move || async move {
				let event = this.get_user_event(uuid).await?;
				Ok(event.into())
			})
			.await
		}
	}
}

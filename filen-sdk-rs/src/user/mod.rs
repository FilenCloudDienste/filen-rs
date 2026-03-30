use filen_macros::js_type;
use filen_types::{
	api::v3::user::account::{Personal, UserAccountPlan, UserAccountSubs, UserAccountSubsInvoices},
	fs::UuidStr,
};

use crate::{api, auth::Client};

impl Client {
	pub async fn get_user_info(&self) -> Result<UserInfo, crate::error::Error> {
		// api::v3::user::info::get(self.client()).await
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
	}
}

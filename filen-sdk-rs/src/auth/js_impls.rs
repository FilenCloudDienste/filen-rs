use std::sync::Arc;

use serde::Deserialize;
use tsify::Tsify;
use wasm_bindgen::prelude::*;

use crate::{
	Error,
	auth::{Client, RegisteredInfo, StringifiedClient, TwoFASecret, start_password_reset},
	runtime::do_on_commander,
};

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
#[wasm_bindgen(js_name = "Client")]
pub struct JsClient {
	client: Arc<Client>,
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
impl JsClient {
	pub(crate) fn new(client: Client) -> Self {
		Self {
			client: Arc::new(client),
		}
	}

	pub(crate) fn inner(&self) -> Arc<Client> {
		self.client.clone()
	}

	pub(crate) fn inner_ref(&self) -> &Client {
		&self.client
	}
}

#[derive(Deserialize, Tsify)]
#[tsify(from_wasm_abi)]
#[serde(rename_all = "camelCase")]
pub struct ChangePasswordParams {
	pub current_password: String,
	pub new_password: String,
}

#[wasm_bindgen(js_class = "Client")]
impl JsClient {
	#[wasm_bindgen(js_name = "toStringified")]
	pub async fn to_stringified(&self) -> StringifiedClient {
		let this = self.inner();
		crate::runtime::do_on_commander(move || async move { this.to_stringified() }).await
	}

	#[wasm_bindgen(js_name = "deleteAccount")]
	pub async fn delete_account(&mut self, two_factor_code: Option<String>) -> Result<(), Error> {
		let this = self.inner();
		do_on_commander(move || async move {
			this.delete_account(two_factor_code.as_deref().unwrap_or("XXXXXX"))
				.await
		})
		.await
	}

	#[wasm_bindgen(js_name = "changePassword")]
	pub async fn change_password(&self, params: ChangePasswordParams) -> Result<(), Error> {
		let this = self.inner();
		do_on_commander(move || async move {
			this.change_password(&params.current_password, &params.new_password)
				.await
		})
		.await
	}

	#[wasm_bindgen(js_name = "exportMasterKeys")]
	pub async fn export_master_keys(&self) -> Result<String, Error> {
		let this = self.inner();
		do_on_commander(move || async move { this.export_master_keys().await }).await
	}

	#[wasm_bindgen(js_name = "generate2FASecret")]
	pub async fn generate_2fa_secret(&self) -> Result<TwoFASecret, Error> {
		let this = self.inner();
		do_on_commander(move || async move { this.generate_2fa_secret().await }).await
	}

	#[wasm_bindgen(js_name = "enable2FAGetRecoveryKey")]
	pub async fn enable_2fa_get_recovery_key(
		&self,
		two_factor_code: String,
	) -> Result<String, Error> {
		let this = self.inner();
		do_on_commander(move || async move { this.enable_2fa(&two_factor_code).await }).await
	}

	#[wasm_bindgen(js_name = "disable2FA")]
	pub async fn disable_2fa(&self, two_factor_code: String) -> Result<(), Error> {
		let this = self.inner();
		do_on_commander(move || async move { this.disable_2fa(&two_factor_code).await }).await
	}
}

#[wasm_bindgen(js_name = "startPasswordReset")]
pub async fn start_password_reset_js(email: String) -> Result<(), Error> {
	do_on_commander(move || async move { start_password_reset(&email).await }).await
}

#[derive(Deserialize, Tsify)]
#[tsify(from_wasm_abi)]
#[serde(rename_all = "camelCase")]
pub struct CompletePasswordResetParams {
	pub token: String,
	pub email: String,
	pub new_password: String,
	#[serde(default)]
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "string")
	)]
	pub recover_key: Option<String>,
}

#[wasm_bindgen(js_name = "completePasswordReset")]
pub async fn complete_password_reset_js(
	params: CompletePasswordResetParams,
) -> Result<JsClient, Error> {
	let client = do_on_commander(move || async move {
		Client::complete_password_reset(
			&params.token,
			params.email,
			&params.new_password,
			params.recover_key.as_deref(),
		)
		.await
	})
	.await?;
	Ok(JsClient::new(client))
}

#[derive(Deserialize, Tsify)]
#[tsify(from_wasm_abi)]
#[serde(rename_all = "camelCase")]
pub struct RegisterParams {
	pub email: String,
	pub password: String,
	#[serde(default)]
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "string")
	)]
	pub ref_id: Option<String>,
	#[serde(default)]
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "string")
	)]
	pub aff_id: Option<String>,
}

#[wasm_bindgen(js_name = "register")]
pub async fn register_js(params: RegisterParams) -> Result<(), Error> {
	do_on_commander(move || async move {
		RegisteredInfo::register(
			params.email,
			&params.password,
			params.ref_id.as_deref(),
			params.aff_id.as_deref(),
		)
		.await
		.map(|_| ())
	})
	.await
}

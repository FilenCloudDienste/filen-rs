use std::{num::NonZeroU32, sync::Arc};

use serde::Deserialize;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use tsify::Tsify;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasm_bindgen::prelude::*;

use crate::{
	Error,
	auth::{
		Client, StringifiedClient, TwoFASecret,
		http::{ClientConfig, JsClientConfig},
		unauth::UnauthClient,
	},
	runtime::do_on_commander,
};

#[derive(Clone)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	wasm_bindgen(js_name = "Client")
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct JsClient {
	client: Arc<Client>,
}

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

#[derive(Deserialize)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(Tsify),
	tsify(from_wasm_abi)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "camelCase")]
pub struct ChangePasswordParams {
	pub current_password: String,
	pub new_password: String,
}

#[cfg(feature = "uniffi")]
#[uniffi::export]
pub fn init() {
	#[cfg(target_os = "android")]
	{
		android_logger::init_once(
			android_logger::Config::default()
				.with_max_level(log::LevelFilter::Info)
				.with_tag("filen-sdk-rs"),
		);
	}
	#[cfg(target_os = "ios")]
	{
		if let Err(e) = oslog::OsLogger::new("io.filen.filen-sdk-rs")
			.level_filter(log::LevelFilter::Info)
			.init()
		{
			eprintln!("Failed to initialize oslog logger: {}", e);
		}
	}
}

#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	wasm_bindgen(js_class = "Client")
)]
#[cfg_attr(feature = "uniffi", uniffi::export)]
impl JsClient {
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen(js_name = "toStringified")
	)]
	pub async fn to_stringified(&self) -> StringifiedClient {
		let this = self.inner();
		crate::runtime::do_on_commander(move || async move { this.to_stringified() }).await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen(js_name = "deleteAccount")
	)]
	pub async fn delete_account(&self, two_factor_code: Option<String>) -> Result<(), Error> {
		let this = self.inner();
		do_on_commander(move || async move {
			this.delete_account(two_factor_code.as_deref().unwrap_or("XXXXXX"))
				.await
		})
		.await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen(js_name = "changePassword")
	)]
	pub async fn change_password(&self, params: ChangePasswordParams) -> Result<(), Error> {
		let this = self.inner();
		do_on_commander(move || async move {
			this.change_password(&params.current_password, &params.new_password)
				.await
		})
		.await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen(js_name = "exportMasterKeys")
	)]
	pub async fn export_master_keys(&self) -> Result<String, Error> {
		let this = self.inner();
		do_on_commander(move || async move { this.export_master_keys().await }).await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen(js_name = "generate2FASecret")
	)]
	pub async fn generate_2fa_secret(&self) -> Result<TwoFASecret, Error> {
		let this = self.inner();
		do_on_commander(move || async move { this.generate_2fa_secret().await }).await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen(js_name = "enable2FAGetRecoveryKey")
	)]
	pub async fn enable_2fa_get_recovery_key(
		&self,
		two_factor_code: String,
	) -> Result<String, Error> {
		let this = self.inner();
		do_on_commander(move || async move { this.enable_2fa(&two_factor_code).await }).await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen(js_name = "disable2FA")
	)]
	pub async fn disable_2fa(&self, two_factor_code: String) -> Result<(), Error> {
		let this = self.inner();
		do_on_commander(move || async move { this.disable_2fa(&two_factor_code).await }).await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen(js_name = "setRequestRateLimit")
	)]
	pub async fn set_request_rate_limit(&self, requests_per_sec: u32) -> Result<(), Error> {
		let this = self.inner();

		let requests_per_sec = NonZeroU32::new(requests_per_sec).ok_or_else(|| {
			Error::custom(
				crate::ErrorKind::InvalidState,
				"requests per second rate limit needs to be > 0",
			)
		})?;
		do_on_commander(move || async move { this.set_request_rate_limit(requests_per_sec).await })
			.await;
		Ok(())
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	pub async fn set_bandwidth_limits(&self, upload_kbps: u32, download_kbps: u32) {
		let upload_kbps = NonZeroU32::new(upload_kbps);
		let download_kbps = NonZeroU32::new(download_kbps);

		let this = self.inner();
		do_on_commander(move || async move {
			this.set_bandwidth_limits(upload_kbps, download_kbps).await
		})
		.await;
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen(js_name = "getUnauthed")
	)]
	pub fn get_unauthed(&self) -> UnauthJsClient {
		UnauthJsClient::new(self.inner_ref().get_unauthed())
	}
}

#[derive(Deserialize)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(Tsify),
	tsify(from_wasm_abi)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
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
	#[cfg_attr(feature = "uniffi", uniffi(default = None))]
	pub recover_key: Option<String>,
}

#[derive(Deserialize)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(Tsify),
	tsify(from_wasm_abi)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "camelCase")]
pub struct RegisterParams {
	pub email: String,
	pub password: String,
	#[serde(default)]
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "string")
	)]
	#[cfg_attr(feature = "uniffi", uniffi(default = None))]
	pub ref_id: Option<String>,
	#[serde(default)]
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "string")
	)]
	#[cfg_attr(feature = "uniffi", uniffi(default = None))]
	pub aff_id: Option<String>,
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	wasm_bindgen::prelude::wasm_bindgen(js_name = "UnauthClient")
)]
pub struct UnauthJsClient {
	inner: Arc<UnauthClient>,
}

impl UnauthJsClient {
	pub(crate) fn new(unauth: UnauthClient) -> Self {
		Self {
			inner: Arc::new(unauth),
		}
	}

	pub(crate) fn inner(&self) -> Arc<UnauthClient> {
		Arc::clone(&self.inner)
	}

	pub(crate) fn inner_ref(&self) -> &UnauthClient {
		&self.inner
	}
}

#[cfg_attr(feature = "uniffi", uniffi::export)]
#[cfg(any(feature = "uniffi", all(target_family = "wasm", target_os = "unknown")))]
impl UnauthClient {}

#[cfg(any(all(target_family = "wasm", target_os = "unknown"), feature = "uniffi"))]
#[cfg_attr(feature = "uniffi", uniffi::export)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	wasm_bindgen::prelude::wasm_bindgen(js_class = "UnauthClient")
)]
impl UnauthJsClient {
	#[cfg_attr(feature = "uniffi", uniffi::constructor)]
	pub fn from_config(client_config: JsClientConfig) -> Result<Self, Error> {
		let config: ClientConfig = client_config.into();
		Ok(Self::new(UnauthClient::from_config(config)?))
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "fromStringified")
	)]
	pub fn from_stringified(
		&self,
		serialized: StringifiedClient,
	) -> Result<super::JsClient, Error> {
		Ok(super::JsClient::new(
			self.inner_ref().from_stringified(serialized)?,
		))
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen(js_name = "startPasswordReset")
	)]
	pub async fn start_password_reset(&self, email: String) -> Result<(), Error> {
		let this = self.inner();
		do_on_commander(move || async move { this.start_password_reset(&email).await }).await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen(js_name = "completePasswordReset")
	)]
	pub async fn complete_password_reset(
		&self,
		params: CompletePasswordResetParams,
	) -> Result<JsClient, Error> {
		let this = self.inner();
		let client = do_on_commander(move || async move {
			this.complete_password_reset(
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

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen(js_name = "register")
	)]
	pub async fn register(&self, params: RegisterParams) -> Result<(), Error> {
		let this = self.inner();
		do_on_commander(move || async move {
			this.register(
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

	#[cfg_attr(all(target_family = "wasm", target_os = "unknown"), wasm_bindgen)]
	pub async fn login(&self, params: crate::js::LoginParams) -> Result<JsClient, Error> {
		let this = self.inner();
		let client = do_on_commander(move || async move {
			this.login(
				params.email,
				&params.password,
				params.two_factor_code.as_deref().unwrap_or("XXXXXX"),
			)
			.await
		})
		.await?;
		Ok(JsClient::new(client))
	}
}

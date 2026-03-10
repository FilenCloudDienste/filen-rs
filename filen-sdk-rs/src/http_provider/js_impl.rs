use std::sync::Arc;

use crate::{
	Error,
	auth::{JsClient, js_impls::UnauthJsClient},
	http_provider::{HttpProviderHandle, client_impl::HttpProviderSharedClientExt},
	runtime::do_on_commander,
};

#[cfg_attr(feature = "uniffi", uniffi::export)]
impl JsClient {
	/// Start an HTTP provider on the commander side if one is not currently running,
	/// and return a handle to it.
	/// The provider will be automatically stopped when all handles for it are dropped.
	///
	/// Only one provider can be active at a time, so if you call this method multiple times,
	/// you will get another handle to the same provider.
	async fn start_http_provider(
		&self,
		port: Option<u16>,
	) -> Result<Arc<HttpProviderHandle>, Error> {
		let this = self.inner();
		do_on_commander(move || async move { this.start_http_provider(port).await }).await
	}
}

#[cfg_attr(feature = "uniffi", uniffi::export)]
impl UnauthJsClient {
	/// Start an HTTP provider on the commander side if one is not currently running,
	/// and return a handle to it.
	/// The provider will be automatically stopped when all handles for it are dropped.
	///
	/// Only one provider can be active at a time, so if you call this method multiple times,
	/// you will get another handle to the same provider.
	async fn start_http_provider(
		&self,
		port: Option<u16>,
	) -> Result<Arc<HttpProviderHandle>, Error> {
		let this = self.inner();
		do_on_commander(move || async move { this.start_http_provider(port).await }).await
	}
}

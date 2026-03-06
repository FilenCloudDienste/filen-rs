use crate::{
	Error,
	auth::JsClient,
	fs::categories::{NonRootItemType, Normal},
	js::{NonRootNormalItem, NonRootNormalItemTagged},
	runtime::do_on_commander,
};

#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	wasm_bindgen::prelude::wasm_bindgen(js_class = "Client")
)]
#[cfg_attr(feature = "uniffi", uniffi::export)]
impl JsClient {
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "setFavorite")
	)]
	pub async fn set_favorite(
		&self,
		item: NonRootNormalItem,
		favorited: bool,
	) -> Result<NonRootNormalItemTagged, Error> {
		let this = self.inner();
		do_on_commander(move || async move {
			let mut item: NonRootItemType<'static, Normal> = item.try_into()?;
			this.set_favorite(&mut item, favorited).await?;
			Ok(item.into())
		})
		.await
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "emptyTrash")
	)]
	pub async fn empty_trash(&self) -> Result<(), Error> {
		let this = self.inner();
		do_on_commander(move || async move {
			this.empty_trash().await?;
			Ok(())
		})
		.await
	}
}

use crate::{
	Error,
	auth::JsClient,
	fs::NonRootFSObject,
	js::{NonRootItem, NonRootItemTagged},
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
		item: NonRootItem,
		favorited: bool,
	) -> Result<NonRootItemTagged, Error> {
		let this = self.inner();
		do_on_commander(move || async move {
			let mut item: NonRootFSObject = item.try_into()?;
			match item {
				NonRootFSObject::Dir(ref mut dir) => {
					this.set_favorite(dir.to_mut(), favorited).await?
				}
				NonRootFSObject::File(ref mut file) => {
					this.set_favorite(file.to_mut(), favorited).await?
				}
			}
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

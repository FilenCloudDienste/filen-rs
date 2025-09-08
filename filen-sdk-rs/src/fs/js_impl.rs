use wasm_bindgen::prelude::wasm_bindgen;

use crate::{
	Error,
	auth::Client,
	fs::NonRootFSObject,
	js::{NonRootItem, NonRootItemTagged},
};

#[wasm_bindgen]
impl Client {
	#[wasm_bindgen(js_name = "setFavorite")]
	pub async fn set_favorite_js(
		&self,
		item: NonRootItem,
		favorited: bool,
	) -> Result<NonRootItemTagged, Error> {
		let mut item: NonRootFSObject = item.try_into()?;
		match item {
			NonRootFSObject::Dir(ref mut dir) => self.set_favorite(dir.to_mut(), favorited).await?,
			NonRootFSObject::File(ref mut file) => {
				self.set_favorite(file.to_mut(), favorited).await?
			}
		}
		Ok(item.into())
	}
}

mod dir;
mod file;
mod item;
mod managed_futures;
mod params;
mod returned_types;

pub use dir::*;
pub use file::*;
pub use item::*;
pub use managed_futures::*;
pub use params::*;
pub use returned_types::*;
use shared::*;

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasm_bindgen::prelude::*;

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use crate::{
	Error,
	auth::{Client, JsClient, StringifiedClient},
};

const HIDDEN_META_KEY: &str = "__hiddenMeta";

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
#[wasm_bindgen(start)]
pub fn main_js() -> Result<(), JsValue> {
	console_error_panic_hook::set_once();
	#[cfg(debug_assertions)]
	wasm_logger::init(wasm_logger::Config::new(log::Level::Debug));
	#[cfg(not(debug_assertions))]
	wasm_logger::init(wasm_logger::Config::new(log::Level::Info));
	Ok(())
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
#[wasm_bindgen]
pub async fn login(
	email: String,
	password: &str,
	#[wasm_bindgen(js_name = "twoFactorCode")] two_factor_code: Option<String>,
) -> Result<JsClient, JsValue> {
	Ok(JsClient::new(
		Client::login(
			email,
			password,
			two_factor_code.as_deref().unwrap_or("XXXXXX"),
		)
		.await?,
	))
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
#[wasm_bindgen(js_name = "fromStringified")]
pub fn from_stringified(serialized: StringifiedClient) -> Result<JsClient, JsValue> {
	Ok(JsClient::new(
		Client::from_stringified(serialized).map_err(Error::from)?,
	))
}

mod shared {
	pub(super) enum EncodedOrDecoded<E, D> {
		Encoded(E),
		Decoded(D),
	}

	pub(super) trait AsEncodedOrDecoded<'a, E, D, E1, D1>
	where
		E: 'a,
		D: 'a,
		E1: 'static,
		D1: 'static,
	{
		fn as_encoded_or_decoded(&'a self) -> EncodedOrDecoded<E, D>;
		fn from_encoded(encoded: E1) -> Self;
		fn from_decoded(decoded: D1) -> Self;
		fn from_encoded_or_decoded(encoded: Option<E1>, decoded: Option<D1>) -> Option<Self>
		where
			Self: Sized,
		{
			match (encoded, decoded) {
				// prefer decoded if both are present
				(_, Some(decoded)) => Some(Self::from_decoded(decoded)),
				(Some(encoded), None) => Some(Self::from_encoded(encoded)),
				(None, None) => None,
			}
		}
	}
}

#[cfg(all(test, target_family = "wasm", target_os = "unknown"))]
mod tests {
	use std::str::FromStr;

	use chrono::{DateTime, Utc};
	use filen_types::{
		auth::FileEncryptionVersion,
		fs::{ParentUuid, UuidStr},
	};
	use wasm_bindgen_test::wasm_bindgen_test;

	use super::*;

	#[wasm_bindgen_test]
	fn root_serde() {
		let root = Root {
			uuid: UuidStr::default(),
		};
		let js_value = JsValue::from(root.clone());
		let deserializer = serde_wasm_bindgen::Deserializer::from(js_value);
		let dir_enum: DirEnum = serde_path_to_error::deserialize(deserializer).unwrap();
		assert_eq!(dir_enum, DirEnum::Root(root));
	}

	#[wasm_bindgen_test]
	fn dir_serde() {
		let dir = Dir {
			uuid: UuidStr::default(),
			parent: ParentUuid::default(),
			color: DirColor::Blue,
			timestamp: Utc::now(),
			favorited: true,
			meta: DirMeta::Decoded(DecryptedDirMeta {
				name: "Test Directory".to_string(),
				created: Some(Utc::now()),
			}),
		};
		let js_value = JsValue::from(dir.clone());
		let deserializer = serde_wasm_bindgen::Deserializer::from(js_value);
		let dir_enum: DirEnum = serde_path_to_error::deserialize(deserializer).unwrap();
		assert_eq!(dir_enum, DirEnum::Dir(dir));
	}

	#[wasm_bindgen_test]
	fn non_root_object_serde() {
		let file = File {
			uuid: UuidStr::default(),
			meta: FileMeta::Decoded(DecryptedFileMeta {
				name: "Test File".to_string(),
				mime: "text/plain".to_string(),
				created: Some(Utc::now()),
				modified: Utc::now(),
				hash: None,
				size: 1024,
				key: "file_key".to_string(),
				version: FileEncryptionVersion::V1,
			}),
			parent: ParentUuid::default(),
			size: 1024,
			favorited: false,
			region: "us-west-1".to_string(),
			bucket: "test-bucket".to_string(),
			timestamp: Utc::now(),
			chunks: 1,
			can_make_thumbnail: false,
		};

		let dir = Dir {
			uuid: UuidStr::from_str("413c5087-cef2-468a-a7b0-3e4f597fffd3").unwrap(),
			parent: ParentUuid::from_str("32514e81-2753-4741-aac9-7da2400900c3").unwrap(),
			color: DirColor::Default,
			timestamp: DateTime::from_timestamp_millis(1755781567998).unwrap(),
			favorited: false,
			meta: DirMeta::Decoded(DecryptedDirMeta {
				name: "wasm-test-dir".to_string(),
				created: Some(DateTime::from_timestamp_millis(1755781567998).unwrap()),
			}),
		};

		let non_root_object = NonRootItemTagged::File(file.clone());
		let js_value = JsValue::from(non_root_object.clone());
		let deserializer = serde_wasm_bindgen::Deserializer::from(js_value);
		let deserialized_object: File = serde_path_to_error::deserialize(deserializer).unwrap();
		assert_eq!(deserialized_object, file);

		let non_root_object = NonRootItemTagged::Dir(dir.clone());
		let js_value = JsValue::from(non_root_object.clone());

		let js_value2 = js_value.clone();
		let deserializer = serde_wasm_bindgen::Deserializer::from(js_value);
		let deserialized_object: Dir = serde_path_to_error::deserialize(deserializer).unwrap();
		assert_eq!(deserialized_object, dir);

		let deserializer = serde_wasm_bindgen::Deserializer::from(js_value2);
		let deserialized_object: DirEnum = serde_path_to_error::deserialize(deserializer).unwrap();
		assert_eq!(deserialized_object, DirEnum::Dir(dir));
	}

	#[wasm_bindgen_test]
	fn dir_meta_serde() {
		let mut dir = Dir {
			uuid: UuidStr::default(),
			parent: ParentUuid::default(),
			color: DirColor::Blue,
			timestamp: Utc::now(),
			favorited: true,
			meta: DirMeta::Decoded(DecryptedDirMeta {
				name: "Test Directory".to_string(),
				created: Some(Utc::now()),
			}),
		};
		let js_value = JsValue::from(dir.clone());
		let deserializer = serde_wasm_bindgen::Deserializer::from(js_value);
		let deserialized_dir: Dir = serde_path_to_error::deserialize(deserializer).unwrap();
		assert_eq!(deserialized_dir, dir);

		dir.meta = DirMeta::DecryptedRaw(vec![1, 2, 3, 4]);
		let js_value = JsValue::from(dir.clone());
		let deserializer = serde_wasm_bindgen::Deserializer::from(js_value);
		let deserialized_dir: Dir = serde_path_to_error::deserialize(deserializer).unwrap();
		assert_eq!(deserialized_dir, dir);

		dir.meta = DirMeta::DecryptedUTF8("Test Directory".to_string());
		let js_value = JsValue::from(dir.clone());
		let deserializer = serde_wasm_bindgen::Deserializer::from(js_value);
		let deserialized_dir: Dir = serde_path_to_error::deserialize(deserializer).unwrap();
		assert_eq!(deserialized_dir, dir);

		dir.meta = DirMeta::Encrypted("encrypted_data".to_string());
		let js_value = JsValue::from(dir.clone());
		let deserializer = serde_wasm_bindgen::Deserializer::from(js_value);
		let deserialized_dir: Dir = serde_path_to_error::deserialize(deserializer).unwrap();
		assert_eq!(deserialized_dir, dir);
	}
}

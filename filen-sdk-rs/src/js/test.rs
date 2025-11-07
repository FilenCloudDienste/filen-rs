use std::str::FromStr;

use chrono::{DateTime, Utc};
use filen_types::{
	auth::FileEncryptionVersion,
	fs::{ParentUuid, UuidStr},
};
use wasm_bindgen::JsValue;
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

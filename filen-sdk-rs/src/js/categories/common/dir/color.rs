use std::borrow::Cow;

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(serde::Serialize, serde::Deserialize),
	serde(rename_all = "camelCase")
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum DirColor {
	Default,
	Blue,
	Green,
	Purple,
	Red,
	Gray,
	#[cfg_attr(all(target_family = "wasm", target_os = "unknown"), serde(untagged))]
	Custom(String),
}

impl From<String> for DirColor {
	fn from(s: String) -> Self {
		match s.as_str() {
			"default" => DirColor::Default,
			"blue" => DirColor::Blue,
			"green" => DirColor::Green,
			"purple" => DirColor::Purple,
			"red" => DirColor::Red,
			"gray" => DirColor::Gray,
			_ => DirColor::Custom(s),
		}
	}
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
impl wasm_bindgen::describe::WasmDescribe for DirColor {
	fn describe() {
		<String as wasm_bindgen::describe::WasmDescribe>::describe();
	}
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
impl wasm_bindgen::convert::FromWasmAbi for DirColor {
	type Abi = <String as wasm_bindgen::convert::FromWasmAbi>::Abi;

	unsafe fn from_abi(abi: Self::Abi) -> Self {
		let s = unsafe { <String as wasm_bindgen::convert::FromWasmAbi>::from_abi(abi) };
		DirColor::from(s)
	}
}

// tsify does not support untagged variants yet: https://github.com/madonoharu/tsify/issues/52
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
#[wasm_bindgen::prelude::wasm_bindgen(typescript_custom_section)]
const TS_DIR_COLOR: &'static str =
	r#"export type DirColor = "default" | "blue" | "green" | "purple" | "red" | "gray" | string;"#;

impl From<filen_types::api::v3::dir::color::DirColor<'_>> for DirColor {
	fn from(color: filen_types::api::v3::dir::color::DirColor) -> Self {
		match color {
			filen_types::api::v3::dir::color::DirColor::Default => DirColor::Default,
			filen_types::api::v3::dir::color::DirColor::Blue => DirColor::Blue,
			filen_types::api::v3::dir::color::DirColor::Green => DirColor::Green,
			filen_types::api::v3::dir::color::DirColor::Purple => DirColor::Purple,
			filen_types::api::v3::dir::color::DirColor::Red => DirColor::Red,
			filen_types::api::v3::dir::color::DirColor::Gray => DirColor::Gray,
			filen_types::api::v3::dir::color::DirColor::Custom(c) => {
				DirColor::Custom(c.into_owned())
			}
		}
	}
}

impl From<DirColor> for filen_types::api::v3::dir::color::DirColor<'static> {
	fn from(color: DirColor) -> Self {
		match color {
			DirColor::Default => filen_types::api::v3::dir::color::DirColor::Default,
			DirColor::Blue => filen_types::api::v3::dir::color::DirColor::Blue,
			DirColor::Green => filen_types::api::v3::dir::color::DirColor::Green,
			DirColor::Purple => filen_types::api::v3::dir::color::DirColor::Purple,
			DirColor::Red => filen_types::api::v3::dir::color::DirColor::Red,
			DirColor::Gray => filen_types::api::v3::dir::color::DirColor::Gray,
			DirColor::Custom(c) => {
				filen_types::api::v3::dir::color::DirColor::Custom(Cow::Owned(c))
			}
		}
	}
}

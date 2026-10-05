use std::str::FromStr;

use filen_types::fs::{StableUuid, UuidStr};
use serde::{
	Deserialize,
	de::{
		DeserializeOwned,
		value::{self, StringDeserializer},
	},
};
use web_sys::js_sys::JsString;

use crate::{Error, ErrorKind};

/// How an async wasm export reads a param: as its [`Wire`](JsParam::Wire) type, which
/// wasm-bindgen converts without failing, parsed by [`JsParam::parse`] inside the call.
///
/// wasm-bindgen converts an async export's params inside the promise it returns, so a param
/// that fails to convert there (a tsify type that does not deserialize, a malformed uuid, a list
/// of strings holding something else) throws out of the promise's task and leaves the promise
/// pending forever. Read through this trait, the same param rejects the call with
/// [`ErrorKind::Conversion`] instead.
///
/// `pub` only because the exports `#[filen_macros::js_exports]` writes name it.
#[doc(hidden)]
pub trait JsParam: Sized {
	/// What wasm-bindgen converts the JS value to; converting to it cannot fail.
	type Wire;

	fn parse(wire: Self::Wire) -> Result<Self, Error>;
}

/// Parses a tsify type from the JS value an export received for it.
///
/// `pub` only because the [`JsParam`] impls `#[filen_macros::js_type(import)]` writes call it.
#[doc(hidden)]
pub fn parse_tsify<T: tsify::Tsify + DeserializeOwned>(wire: T::JsType) -> Result<T, Error> {
	// serde_wasm_bindgen's error holds a JsValue, which is not Send, so only its message can be
	// kept
	T::from_js(wire).map_err(|e| Error::custom(ErrorKind::Conversion, e.to_string()))
}

/// Types whose own wasm-bindgen conversion cannot fail are their own wire type.
macro_rules! identity_js_param {
	($($ty:ty),+ $(,)?) => {
		$(
			impl JsParam for $ty {
				type Wire = Self;

				fn parse(wire: Self) -> Result<Self, Error> {
					Ok(wire)
				}
			}
		)+
	};
}

identity_js_param!(
	bool,
	u32,
	u64,
	i64,
	// Stays concrete so bytes are not converted one by one through the `Vec<T>` impl; that
	// impl does not overlap it, since `u8` is not a `JsParam`.
	Vec<u8>,
	web_sys::js_sys::Function,
	web_sys::WritableStream,
);

/// Read from a `JsString`, which wasm-bindgen converts without checking the value. A `String`
/// wire would be checked inside a list, where wasm-bindgen throws on an element that is not a
/// string.
impl JsParam for String {
	type Wire = JsString;

	fn parse(wire: JsString) -> Result<Self, Error> {
		wire.as_string().ok_or_else(|| {
			Error::custom(
				ErrorKind::Conversion,
				"non-string value passed from JS for a string",
			)
		})
	}
}

impl JsParam for crate::js::DirColor {
	type Wire = JsString;

	fn parse(wire: JsString) -> Result<Self, Error> {
		// Any string is a color.
		String::parse(wire).map(Self::from)
	}
}

/// Types that derive tsify themselves instead of through `#[js_type(import)]`.
macro_rules! tsify_js_param {
	($($ty:ty),+ $(,)?) => {
		$(
			impl JsParam for $ty {
				type Wire = <Self as tsify::Tsify>::JsType;

				fn parse(wire: Self::Wire) -> Result<Self, Error> {
					parse_tsify(wire)
				}
			}
		)+
	};
}

tsify_js_param!(
	filen_types::api::v3::chat::typing::ChatTypingType,
	filen_types::api::v3::notes::NoteType,
);

#[cfg(any(feature = "wasm-full", feature = "service-worker"))]
tsify_js_param!(crate::js::ManagedFuture);

impl<T: JsParam> JsParam for Option<T> {
	type Wire = Option<T::Wire>;

	fn parse(wire: Self::Wire) -> Result<Self, Error> {
		wire.map(T::parse).transpose()
	}
}

impl<T: JsParam> JsParam for Vec<T> {
	type Wire = Vec<T::Wire>;

	fn parse(wire: Self::Wire) -> Result<Self, Error> {
		wire.into_iter().map(T::parse).collect()
	}
}

impl JsParam for UuidStr {
	type Wire = JsString;

	fn parse(wire: JsString) -> Result<Self, Error> {
		Self::from_str(&String::parse(wire)?).map_err(|e| {
			Error::custom_with_source(
				ErrorKind::Conversion,
				e,
				Some("invalid UUID string passed from JS"),
			)
		})
	}
}

impl JsParam for StableUuid {
	type Wire = JsString;

	fn parse(wire: JsString) -> Result<Self, Error> {
		// Mint-only, so it has no constructor: it is read the way the server's value is.
		let wire = String::parse(wire)?;
		Self::deserialize(StringDeserializer::<value::Error>::new(wire)).map_err(|e| {
			Error::custom_with_source(
				ErrorKind::Conversion,
				e,
				Some("invalid stable UUID string passed from JS"),
			)
		})
	}
}

//! SDK errors on napi, thrown in the shape ubrn throws them.
//!
//! ubrn throws a `UniffiThrownObject<FilenSdkError>`: an `Error` whose `inner` is the
//! `FilenSdkError` object, with `__baseTypeName` set to `"UniffiThrownObject"` and the message
//! `"FilenSdkError: " + inner.toDebugString()`. napi classes cannot extend `Error`, so the thrown
//! value here is a plain JS `Error` carrying those properties, and `FilenSdkError` is a plain
//! class holding the Rust error, with the methods and statics ubrn's has.

use napi::{
	Env, JsValue, ValueType,
	bindgen_prelude::{
		Either, Function, JavaScriptClassExt, JsObjectValue, Object, Undefined, Unknown,
	},
};
use napi_derive::napi;

use super::{IntoWire, OptionReturn, Wire};
use crate::{Error, ErrorKind};

/// What ubrn's `UniffiThrownObject` carries in `__baseTypeName`.
const THROWN_OBJECT: &str = "UniffiThrownObject";

/// Custom error type for the SDK
#[napi(js_name = "FilenSdkError")]
pub(crate) struct FilenSdkErrorNapi(Error);

#[napi]
impl FilenSdkErrorNapi {
	/// The wrapped (inner) error's message, WITHOUT the `Error of kind ...` wrapper that
	/// `message()` adds. Returns `None` when there is no inner error. Prefer this over `message()`
	/// when surfacing a non-server error to a user.
	#[napi(
		js_name = "innerMessage",
		ts_args_type = "",
		ts_return_type = "string | undefined"
	)]
	pub fn inner_message(&self) -> Either<String, Undefined> {
		OptionReturn(self.0.inner_message()).into_wire()
	}

	#[napi(js_name = "kind", ts_args_type = "", ts_return_type = "ErrorKind")]
	pub fn kind(&self) -> <ErrorKind as Wire>::Wire {
		self.0.kind().into_wire()
	}

	#[napi(js_name = "message", ts_args_type = "", ts_return_type = "string")]
	pub fn message(&self) -> String {
		self.0.message()
	}

	/// The server-provided error code for server/API errors (e.g. `max_storage_reached`), if present.
	#[napi(
		js_name = "serverCode",
		ts_args_type = "",
		ts_return_type = "string | undefined"
	)]
	pub fn server_code(&self) -> Either<String, Undefined> {
		OptionReturn(self.0.server_code()).into_wire()
	}

	/// The server-provided, human-readable message for server/API errors, if present.
	///
	/// Returns `Some` only when the underlying error is a `ResponseError::ApiError` carrying a
	/// message (i.e. the error kinds that originate from a server response). Prefer this over
	/// `message()` when surfacing a server error to a user — `message()` wraps everything in a
	/// developer-oriented `Error of kind ...` string.
	#[napi(
		js_name = "serverMessage",
		ts_args_type = "",
		ts_return_type = "string | undefined"
	)]
	pub fn server_message(&self) -> Either<String, Undefined> {
		OptionReturn(self.0.server_message()).into_wire()
	}

	/// The error's `Display` text.
	#[napi(js_name = "toString", ts_args_type = "", ts_return_type = "string")]
	pub fn js_to_string(&self) -> String {
		self.0.to_string()
	}

	/// The error's `Debug` text, which the thrown error's message is built from.
	#[napi(
		js_name = "toDebugString",
		ts_args_type = "",
		ts_return_type = "string"
	)]
	pub fn to_debug_string(&self) -> String {
		format!("{:?}", self.0)
	}

	#[napi(
		js_name = "instanceOf",
		ts_args_type = "obj: any",
		ts_return_type = "obj is FilenSdkError"
	)]
	pub fn js_instance_of(env: &Env, obj: Unknown) -> napi::Result<bool> {
		Self::instance_of(env, &obj)
	}

	/// Whether `obj` is what an SDK call throws: as ubrn's `UniffiThrownObject.instanceOf(obj) &&
	/// FilenSdkError.instanceOf(obj.inner)`.
	#[napi(
		js_name = "hasInner",
		ts_args_type = "obj: any",
		ts_return_type = "obj is UniffiThrownObject<FilenSdkError>"
	)]
	pub fn has_inner(env: &Env, obj: Unknown) -> napi::Result<bool> {
		if obj.get_type()? != ValueType::Object {
			return Ok(false);
		}
		let obj = obj.coerce_to_object()?;
		let base_type: Unknown = obj.get_named_property("__baseTypeName")?;
		if base_type.get_type()? != ValueType::String
			|| base_type.coerce_to_string()?.into_utf8()?.as_str()? != THROWN_OBJECT
		{
			return Ok(false);
		}
		let error_class: Function = env.get_global()?.get_named_property("Error")?;
		if !obj.instanceof(error_class)? {
			return Ok(false);
		}
		let inner: Unknown = obj.get_named_property("inner")?;
		Self::instance_of(env, &inner)
	}

	#[napi(
		js_name = "getInner",
		ts_args_type = "err: UniffiThrownObject<FilenSdkError>",
		ts_return_type = "FilenSdkError"
	)]
	pub fn get_inner<'env>(err: Object<'env>) -> napi::Result<Unknown<'env>> {
		err.get_named_property("inner")
	}
}

// The helpers below are plain Rust, so they stay under the workspace's `unsafe_code` deny, which
// `js` lifts for this module only for napi-derive's glue.

/// The JS error an SDK error is thrown as, in ubrn's shape (see the module docs).
#[deny(unsafe_code)]
pub(crate) fn throw(env: &Env, err: Error) -> napi::Error {
	let message = format!("FilenSdkError: {err:?}");
	thrown_object(env, &message, err).unwrap_or_else(|failure| {
		tracing::warn!(
			"could not build the thrown SDK error, throwing its message only: {failure}"
		);
		napi::Error::from_reason(message)
	})
}

#[deny(unsafe_code)]
fn thrown_object(env: &Env, message: &str, err: Error) -> napi::Result<napi::Error> {
	let mut thrown = plain_error(env, message)?;
	thrown.set_named_property("inner", FilenSdkErrorNapi(err))?;
	thrown.set_named_property("__baseTypeName", THROWN_OBJECT)?;
	Ok(napi::Error::from(thrown.to_unknown()))
}

/// A JS `Error` with `message`, as `new Error(message)` makes it: napi's `create_error` also sets
/// a `code` (its status), which ubrn's errors do not have.
#[deny(unsafe_code)]
pub(super) fn plain_error<'env>(env: &'env Env, message: &str) -> napi::Result<Object<'env>> {
	let mut error = env.create_error(napi::Error::from_reason(message))?;
	error.delete_named_property("code")?;
	Ok(error)
}

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

pub const ENDPOINT: &str = "v3/user/personal/update";

/// Update the user's personal information. The backend requires every field and overwrites
/// all nine on every call: the `"__NONE__"` sentinel string, which `None` is sent as, CLEARS
/// its field, and an empty string is rejected with `invalid_params`. A custom serde adapter
/// hides the sentinel, so the SDK only sees `Option<Cow<str>>` where `None` means "clear".
/// To change one field, send every other field's current value too.
#[derive(Deserialize, Serialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	#[serde(borrow, with = "crate::serde::option::str_none_sentinel")]
	pub city: Option<Cow<'a, str>>,
	#[serde(borrow, with = "crate::serde::option::str_none_sentinel")]
	pub company_name: Option<Cow<'a, str>>,
	#[serde(borrow, with = "crate::serde::option::str_none_sentinel")]
	pub country: Option<Cow<'a, str>>,
	#[serde(borrow, with = "crate::serde::option::str_none_sentinel")]
	pub first_name: Option<Cow<'a, str>>,
	#[serde(borrow, with = "crate::serde::option::str_none_sentinel")]
	pub last_name: Option<Cow<'a, str>>,
	#[serde(borrow, with = "crate::serde::option::str_none_sentinel")]
	pub postal_code: Option<Cow<'a, str>>,
	#[serde(borrow, with = "crate::serde::option::str_none_sentinel")]
	pub street: Option<Cow<'a, str>>,
	#[serde(borrow, with = "crate::serde::option::str_none_sentinel")]
	pub street_number: Option<Cow<'a, str>>,
	#[serde(borrow, with = "crate::serde::option::str_none_sentinel")]
	pub vat_id: Option<Cow<'a, str>>,
}

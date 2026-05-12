use std::borrow::Cow;

use serde::{Deserialize, Serialize};

pub const ENDPOINT: &str = "v3/user/personal/update";

/// Update the user's personal information. The backend treats every field as
/// mandatory and uses the `"__NONE__"` sentinel string to mean "leave this
/// field unchanged"; we hide that detail with a custom serde adapter so the
/// SDK only sees `Option<Cow<str>>` where `None` means "leave unchanged".
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

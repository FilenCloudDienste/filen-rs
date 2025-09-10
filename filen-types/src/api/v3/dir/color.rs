use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::fs::UuidStr;

pub const ENDPOINT: &str = "v3/dir/color";

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Request<'a> {
	pub uuid: UuidStr,
	pub color: DirColor<'a>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum DirColor<'a> {
	#[default]
	Default,
	Blue,
	Green,
	Purple,
	Red,
	Gray,
	#[serde(untagged)]
	Custom(Cow<'a, str>),
}

impl<'a, 'de> Deserialize<'de> for DirColor<'a> {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		#[derive(Deserialize)]
		#[serde(rename_all = "camelCase")]
		enum Helper<'a> {
			Default,
			Blue,
			Green,
			Purple,
			Red,
			Gray,
			#[serde(untagged)]
			Custom(Cow<'a, str>),
		}
		let s = Option::<Helper<'a>>::deserialize(deserializer)?;
		Ok(match s {
			None | Some(Helper::Default) => DirColor::Default,
			Some(Helper::Blue) => DirColor::Blue,
			Some(Helper::Green) => DirColor::Green,
			Some(Helper::Purple) => DirColor::Purple,
			Some(Helper::Red) => DirColor::Red,
			Some(Helper::Gray) => DirColor::Gray,
			Some(Helper::Custom(c)) => DirColor::Custom(c),
		})
	}
}

impl<'a> DirColor<'a> {
	pub fn borrow_clone(&'a self) -> DirColor<'a> {
		match self {
			DirColor::Default => DirColor::Default,
			DirColor::Blue => DirColor::Blue,
			DirColor::Green => DirColor::Green,
			DirColor::Purple => DirColor::Purple,
			DirColor::Red => DirColor::Red,
			DirColor::Gray => DirColor::Gray,
			DirColor::Custom(c) => DirColor::Custom(Cow::Borrowed(c)),
		}
	}

	pub fn into_owned(self) -> DirColor<'static> {
		match self {
			DirColor::Default => DirColor::Default,
			DirColor::Blue => DirColor::Blue,
			DirColor::Green => DirColor::Green,
			DirColor::Purple => DirColor::Purple,
			DirColor::Red => DirColor::Red,
			DirColor::Gray => DirColor::Gray,
			DirColor::Custom(c) => DirColor::Custom(Cow::Owned(c.into_owned())),
		}
	}
}

impl<'a> From<DirColor<'a>> for Cow<'a, str> {
	fn from(color: DirColor<'a>) -> Self {
		match color {
			DirColor::Default => Cow::Borrowed("default"),
			DirColor::Blue => Cow::Borrowed("blue"),
			DirColor::Green => Cow::Borrowed("green"),
			DirColor::Purple => Cow::Borrowed("purple"),
			DirColor::Red => Cow::Borrowed("red"),
			DirColor::Gray => Cow::Borrowed("gray"),
			DirColor::Custom(c) => c,
		}
	}
}

impl AsRef<str> for DirColor<'_> {
	fn as_ref(&self) -> &str {
		match self {
			DirColor::Default => "default",
			DirColor::Blue => "blue",
			DirColor::Green => "green",
			DirColor::Purple => "purple",
			DirColor::Red => "red",
			DirColor::Gray => "gray",
			DirColor::Custom(c) => c.as_ref(),
		}
	}
}

impl From<DirColor<'_>> for Option<String> {
	fn from(color: DirColor<'_>) -> Self {
		match color {
			DirColor::Default => None,
			other => Some(Cow::from(other).into_owned()),
		}
	}
}

#[cfg(feature = "rusqlite")]
mod sqlite {

	use std::borrow::Cow;

	use rusqlite::{
		ToSql,
		types::{FromSql, ToSqlOutput},
	};

	use super::DirColor;

	impl ToSql for DirColor<'_> {
		fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
			Ok(ToSqlOutput::Borrowed(self.as_ref().into()))
		}
	}

	impl FromSql for DirColor<'static> {
		fn column_result(
			value: rusqlite::types::ValueRef<'_>,
		) -> rusqlite::types::FromSqlResult<Self> {
			match value {
				rusqlite::types::ValueRef::Text(s) => match std::str::from_utf8(s) {
					Ok("default") => Ok(DirColor::Default),
					Ok("blue") => Ok(DirColor::Blue),
					Ok("green") => Ok(DirColor::Green),
					Ok("purple") => Ok(DirColor::Purple),
					Ok("red") => Ok(DirColor::Red),
					Ok("gray") => Ok(DirColor::Gray),
					Ok(s) => Ok(DirColor::Custom(Cow::Owned(s.to_string()))),
					Err(_) => Err(rusqlite::types::FromSqlError::InvalidType),
				},
				_ => Err(rusqlite::types::FromSqlError::InvalidType),
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_dir_color_serde() {
		let colors = vec![
			(DirColor::Default, "\"default\""),
			(DirColor::Blue, "\"blue\""),
			(DirColor::Green, "\"green\""),
			(DirColor::Purple, "\"purple\""),
			(DirColor::Red, "\"red\""),
			(DirColor::Gray, "\"gray\""),
			(DirColor::Custom(Cow::Borrowed("#123456")), "\"#123456\""),
			(
				DirColor::Custom(Cow::Owned("#abcdef".to_string())),
				"\"#abcdef\"",
			),
		];
		for (color, expected) in colors {
			println!("Testing color: {:?}", color);
			let serialized = serde_json::to_string(&color).unwrap();
			assert_eq!(serialized, expected);
			println!("Serialized: {}", serialized);
			let deserialized: DirColor = serde_json::from_str(&serialized).unwrap();
			assert_eq!(color, deserialized);
		}
	}
}

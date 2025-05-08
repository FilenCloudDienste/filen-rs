use std::borrow::Cow;

use serde::Deserialize;

use crate::error::ResponseError;

#[derive(Deserialize, Debug)]
#[serde(bound = "T: Deserialize<'de>")]
pub struct FilenResponse<'a, T>
where
	T: std::fmt::Debug,
{
	pub status: Option<bool>,
	pub message: Option<Cow<'a, str>>,
	pub code: Option<Cow<'a, str>>,
	data: Option<T>,
}

impl<T> FilenResponse<'_, T>
where
	T: std::fmt::Debug,
{
	pub fn into_data(self) -> Result<T, ResponseError> {
		match (self.status, self.data) {
			(Some(true), Some(data)) => Ok(data),
			_ => Err(ResponseError::ApiError {
				message: self.message.map(|s| s.into_owned()),
				code: self.code.map(|s| s.into_owned()),
			}),
		}
	}

	pub fn ignore_data(self) -> Result<(), ResponseError> {
		match (self.status, self.data) {
			(Some(true), _) => Ok(()),
			_ => Err(ResponseError::ApiError {
				message: self.message.map(|s| s.into_owned()),
				code: self.code.map(|s| s.into_owned()),
			}),
		}
	}
}

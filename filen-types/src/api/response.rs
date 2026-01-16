use std::borrow::Cow;

use serde::Deserialize;

use crate::error::ResponseError;

#[derive(Deserialize)]
#[serde(bound = "T: Deserialize<'de>")]
pub struct FilenResponse<'a, T> {
	pub status: Option<bool>,
	pub message: Option<Cow<'a, str>>,
	pub code: Option<Cow<'a, str>>,
	data: Option<T>,
}

impl<T> std::fmt::Debug for FilenResponse<'_, T>
where
	T: std::fmt::Debug,
{
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("FilenResponse")
			.field("status", &self.status)
			.field("message", &self.message)
			.field("code", &self.code)
			.field("data", &self.data)
			.finish()
	}
}

impl<T> FilenResponse<'_, T> {
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

	pub fn as_error(&self) -> Option<ResponseError> {
		if let Some(false) = self.status {
			Some(ResponseError::ApiError {
				message: self.message.as_ref().map(|s| s.to_string()),
				code: self.code.as_ref().map(|s| s.to_string()),
			})
		} else {
			None
		}
	}
}

pub trait ResponseIntoData<T> {
	fn into_data(self) -> Result<T, ResponseError>;
}

impl<T> ResponseIntoData<T> for FilenResponse<'_, T> {
	default fn into_data(self) -> Result<T, ResponseError> {
		match (self.status, self.data) {
			(Some(true), Some(data)) => Ok(data),
			_ => Err(ResponseError::ApiError {
				message: self.message.map(|s| s.into_owned()),
				code: self.code.map(|s| s.into_owned()),
			}),
		}
	}
}

impl ResponseIntoData<()> for FilenResponse<'_, ()> {
	fn into_data(self) -> Result<(), ResponseError> {
		match self.status {
			Some(true) => Ok(()),
			_ => Err(ResponseError::ApiError {
				message: self.message.map(|s| s.into_owned()),
				code: self.code.map(|s| s.into_owned()),
			}),
		}
	}
}

use serde::Deserialize;

use crate::error::ResponseError;

#[derive(Deserialize, Debug)]
#[serde(bound = "T: Deserialize<'de>")]
pub struct FilenResponse<T>
where
	T: std::fmt::Debug,
{
	pub status: Option<bool>,
	pub message: Option<String>,
	pub code: Option<String>,
	data: Option<T>,
}

impl<T> FilenResponse<T>
where
	T: std::fmt::Debug,
{
	pub fn into_data(self) -> Result<T, ResponseError> {
		self.data.ok_or(ResponseError::ApiError {
			message: self.message,
			code: self.code,
		})
	}

	pub fn check_status(self) -> Result<(), ResponseError> {
		if self.status.is_some_and(|s| s) {
			Ok(())
		} else {
			Err(ResponseError::ApiError {
				message: self.message,
				code: self.code,
			})
		}
	}
}

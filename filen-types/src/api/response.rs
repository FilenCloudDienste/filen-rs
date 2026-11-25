use std::borrow::Cow;

use serde::Deserialize;

use crate::error::ResponseError;

#[derive(Clone, Deserialize)]
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

impl<'a, T> FilenResponse<'a, T> {
	/// This response with its `data`, if it has any, mapped by `f`, or the error `f` returned.
	pub fn try_map_data<U, E>(
		self,
		f: impl FnOnce(T) -> Result<U, E>,
	) -> Result<FilenResponse<'a, U>, E> {
		Ok(FilenResponse {
			status: self.status,
			message: self.message,
			code: self.code,
			data: self.data.map(f).transpose()?,
		})
	}

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

#[cfg(test)]
mod tests {
	use super::*;

	fn response(data: Option<&str>) -> FilenResponse<'static, &str> {
		FilenResponse {
			status: Some(true),
			message: Some(Cow::Borrowed("ok")),
			code: Some(Cow::Borrowed("done")),
			data,
		}
	}

	#[test]
	fn try_map_data_maps_the_data_and_keeps_the_envelope() {
		let mapped = response(Some("7")).try_map_data(str::parse::<u32>).unwrap();
		assert_eq!(mapped.status, Some(true));
		assert_eq!(mapped.message.as_deref(), Some("ok"));
		assert_eq!(mapped.code.as_deref(), Some("done"));
		assert_eq!(mapped.data, Some(7));
	}

	#[test]
	fn try_map_data_does_not_call_the_mapping_without_data() {
		let mapped = response(None)
			.try_map_data(|_| -> Result<u32, ()> { panic!("mapped absent data") })
			.unwrap();
		assert_eq!(mapped.data, None);
		assert_eq!(mapped.code.as_deref(), Some("done"));
	}

	#[test]
	fn try_map_data_returns_the_mapping_error() {
		let e = response(Some("seven"))
			.try_map_data(str::parse::<u32>)
			.unwrap_err();
		assert_eq!(e.to_string(), "invalid digit found in string");
	}
}

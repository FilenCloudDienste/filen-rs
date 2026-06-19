use thiserror::Error;

#[derive(Error, Debug)]
pub enum F64ToU64Error {
	#[error("value is negative")]
	Negative,
	#[error("value {0} is too large to fit in a u64")]
	TooLarge(f64),
	#[error("value is NaN")]
	NaN,
	#[error("value {0} is not an integer")]
	NotInteger(f64),
}

/// Converts a f64 to u64, returning an error if the value is negative, NaN, too large, or not an integer.
pub(crate) fn f64_to_u64(value: f64) -> Result<u64, F64ToU64Error> {
	if value.is_nan() {
		Err(F64ToU64Error::NaN)
	} else if value < 0.0 {
		Err(F64ToU64Error::Negative)
	} else if value > (u64::MAX as f64) {
		Err(F64ToU64Error::TooLarge(value))
	} else if value.fract() != 0.0 {
		Err(F64ToU64Error::NotInteger(value))
	} else {
		Ok(value as u64)
	}
}

#[derive(Error, Debug)]
pub enum StrToU64Error {
	#[error("failed to parse float: {0}")]
	ParseFloatError(std::num::ParseFloatError),
	#[error("failed to convert f64 to u64: {0}")]
	F64ToU64Error(F64ToU64Error),
}

/// Converts a string to u64, trying to parse it as a f64 and converting to u64.
/// This is necessary because some APIs return numbers as strings,
/// and some of those numbers are formatted as floats even though they represent integers.
pub(crate) fn str_to_u64(value: &str) -> Result<u64, StrToU64Error> {
	match value.parse::<f64>() {
		Ok(v) => f64_to_u64(v).map_err(StrToU64Error::F64ToU64Error),
		Err(e) => Err(StrToU64Error::ParseFloatError(e)),
	}
}

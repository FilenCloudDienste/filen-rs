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
	} else if value >= 18446744073709551616.0 {
		// `u64::MAX as f64` rounds up to exactly 2^64, so `value > u64::MAX as f64`
		// lets 2^64 through and `as u64` then saturates to u64::MAX. Compare
		// against 2^64 directly and reject it (the largest representable f64 below
		// 2^64 is 2^64 - 2048, which still converts).
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
	// Parse as an integer first so exact values above 2^53 (e.g. "9007199254740993")
	// aren't silently rounded by going through f64. Only fall back to f64 for the
	// legacy float-formatted integers this helper exists to accept.
	if let Ok(v) = value.parse::<u64>() {
		return Ok(v);
	}
	match value.parse::<f64>() {
		Ok(v) => f64_to_u64(v).map_err(StrToU64Error::F64ToU64Error),
		Err(e) => Err(StrToU64Error::ParseFloatError(e)),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn f64_to_u64_rejects_two_to_the_64() {
		// `u64::MAX as f64` rounds up to exactly 2^64; it must be rejected, not
		// saturated to u64::MAX.
		assert!(matches!(
			f64_to_u64(18446744073709551616.0),
			Err(F64ToU64Error::TooLarge(_))
		));
	}

	#[test]
	fn f64_to_u64_accepts_largest_representable_below_two_to_the_64() {
		// 2^64 - 2048 is the largest f64 strictly below 2^64 and still fits.
		let just_below = 18446744073709549568.0_f64;
		assert_eq!(f64_to_u64(just_below).unwrap(), 18_446_744_073_709_549_568);
	}

	#[test]
	fn str_to_u64_parses_large_integers_without_precision_loss() {
		// 2^53 + 1 cannot be represented exactly as f64; the integer path must
		// preserve it instead of rounding to 2^53.
		assert_eq!(
			str_to_u64("9007199254740993").unwrap(),
			9_007_199_254_740_993
		);
		assert_eq!(str_to_u64("18446744073709551615").unwrap(), u64::MAX);
	}

	#[test]
	fn str_to_u64_still_accepts_float_formatted_integers() {
		assert_eq!(str_to_u64("42.0").unwrap(), 42);
	}
}

//! The types values cross the Node-API boundary as.
//!
//! A type's wire type is what napi-derive converts to and from JS; `FromWire` and `IntoWire`
//! convert between it and the Rust type. Generated mirrors implement them for `#[js_type(napi)]`
//! items; this module covers the leaves. Where napi's own conversion of a Rust type would differ
//! from ubrn's, the wire type is a different one: `u64` and `i64` cross as `BigInt` (napi reads
//! no `u64`, and passes an `i64` as a `number`), a `Uuid` as a `String`. `f32` has no wire type:
//! napi reads none.

use napi::bindgen_prelude::{BigInt, Either, Undefined};
use uuid::Uuid;

use crate::{Error, ErrorKind};

/// A type that crosses the napi boundary as `Self::Wire`.
pub(crate) trait Wire {
	type Wire;
}

/// A type read from JS.
pub(crate) trait FromWire: Wire + Sized {
	fn from_wire(wire: Self::Wire) -> Result<Self, Error>;
}

/// A type written to JS.
pub(crate) trait IntoWire: Wire {
	fn into_wire(self) -> Self::Wire;
}

/// Types napi converts as ubrn does.
macro_rules! same_on_wire {
	($($ty:ty),*) => {$(
		impl Wire for $ty {
			type Wire = $ty;
		}

		impl FromWire for $ty {
			fn from_wire(wire: $ty) -> Result<Self, Error> {
				Ok(wire)
			}
		}

		impl IntoWire for $ty {
			fn into_wire(self) -> $ty {
				self
			}
		}
	)*};
}

same_on_wire!(String, bool, u8, u16, u32, i8, i16, i32, f64, ());

/// A JS value that does not fit the Rust type it is read as.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum WireError {
	#[error("the bigint does not fit in a {0}")]
	BigIntOutOfRange(&'static str),
}

fn conversion(e: impl std::error::Error + Send + Sync + 'static) -> Error {
	Error::custom_with_source(ErrorKind::Conversion, e, None::<&str>)
}

/// The magnitude of `wire` as a `u64`, if it has one: napi gives a zero bigint as one word or
/// none, and high zero words carry nothing.
fn magnitude(wire: &BigInt) -> Option<u64> {
	match wire.words.iter().rposition(|word| *word != 0) {
		None => Some(0),
		Some(0) => wire.words.first().copied(),
		Some(_) => None,
	}
}

impl Wire for u64 {
	type Wire = BigInt;
}

impl FromWire for u64 {
	fn from_wire(wire: BigInt) -> Result<Self, Error> {
		match magnitude(&wire) {
			Some(0) => Ok(0),
			Some(value) if !wire.sign_bit => Ok(value),
			_ => Err(conversion(WireError::BigIntOutOfRange("u64"))),
		}
	}
}

impl IntoWire for u64 {
	fn into_wire(self) -> BigInt {
		BigInt::from(self)
	}
}

impl Wire for i64 {
	type Wire = BigInt;
}

impl FromWire for i64 {
	fn from_wire(wire: BigInt) -> Result<Self, Error> {
		let out_of_range = || conversion(WireError::BigIntOutOfRange("i64"));
		let value = magnitude(&wire).ok_or_else(out_of_range)?;
		if wire.sign_bit {
			0_i64.checked_sub_unsigned(value).ok_or_else(out_of_range)
		} else {
			i64::try_from(value).map_err(|_| out_of_range())
		}
	}
}

impl IntoWire for i64 {
	fn into_wire(self) -> BigInt {
		BigInt::from(self)
	}
}

impl Wire for Uuid {
	type Wire = String;
}

impl FromWire for Uuid {
	fn from_wire(wire: String) -> Result<Self, Error> {
		Uuid::parse_str(&wire).map_err(conversion)
	}
}

impl IntoWire for Uuid {
	fn into_wire(self) -> String {
		self.to_string()
	}
}

impl<T: Wire> Wire for Option<T> {
	type Wire = Option<T::Wire>;
}

impl<T: FromWire> FromWire for Option<T> {
	fn from_wire(wire: Self::Wire) -> Result<Self, Error> {
		wire.map(T::from_wire).transpose()
	}
}

impl<T: IntoWire> IntoWire for Option<T> {
	fn into_wire(self) -> Self::Wire {
		self.map(T::into_wire)
	}
}

impl<T: Wire> Wire for Vec<T> {
	type Wire = Vec<T::Wire>;
}

impl<T: FromWire> FromWire for Vec<T> {
	fn from_wire(wire: Self::Wire) -> Result<Self, Error> {
		wire.into_iter().map(T::from_wire).collect()
	}
}

impl<T: IntoWire> IntoWire for Vec<T> {
	fn into_wire(self) -> Self::Wire {
		self.into_iter().map(T::into_wire).collect()
	}
}

/// A top-level `Option` return, written as `undefined` when `None` as ubrn returns it; napi
/// writes a returned `None` as `null`.
pub(crate) struct OptionReturn<T>(pub(crate) Option<T>);

impl<T: Wire> Wire for OptionReturn<T> {
	type Wire = Either<T::Wire, Undefined>;
}

impl<T: IntoWire> IntoWire for OptionReturn<T> {
	fn into_wire(self) -> Self::Wire {
		match self.0 {
			Some(value) => Either::A(value.into_wire()),
			None => Either::B(()),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn bigint(sign_bit: bool, words: &[u64]) -> BigInt {
		BigInt {
			sign_bit,
			words: words.to_vec(),
		}
	}

	fn read<T: FromWire<Wire = BigInt>>(sign_bit: bool, words: &[u64]) -> Result<T, ErrorKind> {
		T::from_wire(bigint(sign_bit, words)).map_err(|e| e.kind())
	}

	#[test]
	fn u64_reads_zero_in_any_napi_shape() {
		assert_eq!(read::<u64>(false, &[0]), Ok(0));
		assert_eq!(read::<u64>(false, &[]), Ok(0));
		assert_eq!(read::<u64>(true, &[0]), Ok(0));
	}

	#[test]
	fn u64_reads_its_maximum() {
		assert_eq!(read::<u64>(false, &[u64::MAX]), Ok(u64::MAX));
		assert_eq!(read::<u64>(false, &[u64::MAX, 0]), Ok(u64::MAX));
	}

	#[test]
	fn u64_rejects_a_negative_bigint() {
		assert_eq!(read::<u64>(true, &[1]), Err(ErrorKind::Conversion));
	}

	#[test]
	fn u64_rejects_a_bigint_of_two_words() {
		assert_eq!(read::<u64>(false, &[0, 1]), Err(ErrorKind::Conversion));
	}

	#[test]
	fn i64_reads_both_ends_of_its_range() {
		assert_eq!(read::<i64>(true, &[1 << 63]), Ok(i64::MIN));
		assert_eq!(read::<i64>(false, &[i64::MAX.unsigned_abs()]), Ok(i64::MAX));
		assert_eq!(read::<i64>(true, &[5]), Ok(-5));
	}

	#[test]
	fn i64_rejects_one_past_either_end() {
		assert_eq!(
			read::<i64>(true, &[(1 << 63) + 1]),
			Err(ErrorKind::Conversion)
		);
		assert_eq!(read::<i64>(false, &[1 << 63]), Err(ErrorKind::Conversion));
		assert_eq!(read::<i64>(true, &[0, 1]), Err(ErrorKind::Conversion));
	}

	#[test]
	fn i64_round_trips_through_its_wire_type() {
		for value in [i64::MIN, -1, 0, 1, i64::MAX] {
			assert_eq!(i64::from_wire(value.into_wire()).ok(), Some(value));
		}
	}

	#[test]
	fn uuid_rejects_a_string_that_is_not_one() {
		assert_eq!(
			Uuid::from_wire("not-a-uuid".to_owned()).map_err(|e| e.kind()),
			Err(ErrorKind::Conversion)
		);
		let uuid = Uuid::from_u128(7);
		assert_eq!(Uuid::from_wire(uuid.into_wire()).ok(), Some(uuid));
	}

	#[test]
	fn a_none_return_is_written_as_undefined() {
		assert!(matches!(
			OptionReturn::<u32>(None).into_wire(),
			Either::B(())
		));
		assert!(matches!(
			OptionReturn(Some(3_u32)).into_wire(),
			Either::A(3)
		));
	}
}

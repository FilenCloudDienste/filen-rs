use std::ops::Deref;

use generic_array::{ArrayLength, GenericArray, IntoArrayLength};
use serde::{Deserialize, Serialize};
use typenum::Const;

use crate::error::ConversionError;

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct StackSizedString<N: ArrayLength>(pub(super) GenericArray<u8, N>);

impl<N: ArrayLength> Copy for StackSizedString<N> where N::ArrayType<u8>: Copy {}

impl<N> StackSizedString<N>
where
	N: ArrayLength,
{
	pub fn into_bytes<const U: usize>(self) -> [u8; U]
	where
		Const<U>: IntoArrayLength<ArrayLength = N>,
	{
		self.0.into()
	}
}

impl<N: ArrayLength> Deref for StackSizedString<N> {
	type Target = str;
	fn deref(&self) -> &Self::Target {
		unsafe { std::str::from_utf8_unchecked(&self.0) }
	}
}

impl<N: ArrayLength> std::fmt::Debug for StackSizedString<N> {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "StackSizedString<{}>(\"{}\")", N::USIZE, self)
	}
}

impl<N: ArrayLength> std::fmt::Display for StackSizedString<N> {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		self.deref().fmt(f)
	}
}

impl<N: ArrayLength> TryFrom<&str> for StackSizedString<N>
where
	N::ArrayType<u8>: Copy,
{
	type Error = ConversionError;

	fn try_from(value: &str) -> Result<Self, Self::Error> {
		let bytes = value.as_bytes();
		let array = GenericArray::try_from_slice(bytes)
			.map_err(|_| ConversionError::InvalidLength(bytes.len(), N::USIZE))?;

		Ok(Self(*array))
	}
}

impl<N: ArrayLength> Serialize for StackSizedString<N> {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		self.deref().serialize(serializer)
	}
}

impl<'de, N: ArrayLength> Deserialize<'de> for StackSizedString<N>
where
	N::ArrayType<u8>: Copy,
{
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let s = crate::serde::cow::deserialize(deserializer)?;
		match Self::try_from(s.deref()) {
			Ok(sss) => Ok(sss),
			Err(e) => Err(serde::de::Error::custom(format!(
				"failed to deserialize StackSizedString<{}>: {}",
				N::USIZE,
				e
			))),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::error::ConversionError;
	use generic_array::GenericArray;
	use std::collections::hash_map::DefaultHasher;
	use std::hash::{Hash, Hasher};
	use typenum::{U0, U1, U3, U4, U5, U8, U16};

	// ---------- helpers -------------------------------------------------------

	fn hash_of<T: Hash>(t: &T) -> u64 {
		let mut h = DefaultHasher::new();
		t.hash(&mut h);
		h.finish()
	}

	// ---------- TryFrom<&str> -------------------------------------------------

	#[test]
	fn try_from_exact_length_succeeds() {
		let s = StackSizedString::<U5>::try_from("hello").expect("len matches N");
		assert_eq!(&*s, "hello");
	}

	#[test]
	fn try_from_too_short_fails_with_invalid_length() {
		let err = StackSizedString::<U5>::try_from("hi").unwrap_err();
		assert!(
			matches!(err, ConversionError::InvalidLength(actual, expected) if actual == 2 && expected == 5),
			"unexpected error: {err:?}",
		);
	}

	#[test]
	fn try_from_too_long_fails_with_invalid_length() {
		let err = StackSizedString::<U3>::try_from("hello").unwrap_err();
		assert!(
			matches!(err, ConversionError::InvalidLength(actual, expected) if actual == 5 && expected == 3),
			"unexpected error: {err:?}",
		);
	}

	#[test]
	fn try_from_empty_string_into_zero_length_succeeds() {
		let s = StackSizedString::<U0>::try_from("").expect("zero length should accept empty");
		assert_eq!(&*s, "");
		assert_eq!(s.len(), 0);
	}

	#[test]
	fn try_from_nonempty_into_zero_length_fails() {
		let err = StackSizedString::<U0>::try_from("x").unwrap_err();
		assert!(matches!(err, ConversionError::InvalidLength(1, 0)));
	}

	#[test]
	fn try_from_empty_into_nonzero_length_fails() {
		let err = StackSizedString::<U4>::try_from("").unwrap_err();
		assert!(matches!(err, ConversionError::InvalidLength(0, 4)));
	}

	#[test]
	fn try_from_single_byte() {
		let s = StackSizedString::<U1>::try_from("a").unwrap();
		assert_eq!(&*s, "a");
	}

	// Multi-byte UTF-8: "é" is 2 bytes, "🦀" is 4 bytes. The type is sized in
	// bytes, not chars, so these must fit byte-exactly.
	#[test]
	fn try_from_multibyte_utf8_byte_exact_succeeds() {
		// "é" = 0xC3 0xA9  →  needs N = 2
		let s = StackSizedString::<typenum::U2>::try_from("é").unwrap();
		assert_eq!(&*s, "é");

		// "🦀" = 4 bytes → needs N = 4
		let crab = StackSizedString::<U4>::try_from("🦀").unwrap();
		assert_eq!(&*crab, "🦀");
	}

	#[test]
	fn try_from_multibyte_utf8_wrong_byte_count_fails() {
		// One char, but 2 bytes — does not fit N = 1.
		let err = StackSizedString::<U1>::try_from("é").unwrap_err();
		assert!(matches!(err, ConversionError::InvalidLength(2, 1)));

		// 4-byte char does not fit N = 3.
		let err = StackSizedString::<U3>::try_from("🦀").unwrap_err();
		assert!(matches!(err, ConversionError::InvalidLength(4, 3)));
	}

	// ---------- Deref ---------------------------------------------------------

	#[test]
	fn deref_yields_original_str() {
		let s = StackSizedString::<U5>::try_from("world").unwrap();
		let as_str: &str = &s;
		assert_eq!(as_str, "world");
	}

	#[test]
	fn deref_enables_str_methods() {
		let s = StackSizedString::<U5>::try_from("HELLO").unwrap();
		assert_eq!(s.to_lowercase(), "hello");
		assert_eq!(s.len(), 5);
		assert!(s.starts_with("HE"));
		assert!(s.contains("LL"));
	}

	#[test]
	fn deref_preserves_multibyte_chars() {
		let s = StackSizedString::<U4>::try_from("🦀").unwrap();
		assert_eq!(s.chars().count(), 1);
		assert_eq!(s.len(), 4); // bytes
	}

	// ---------- Display / Debug ----------------------------------------------

	#[test]
	fn display_matches_inner_str() {
		let s = StackSizedString::<U3>::try_from("abc").unwrap();
		assert_eq!(format!("{s}"), "abc");
	}

	#[test]
	fn debug_includes_size_and_value() {
		let s = StackSizedString::<U3>::try_from("abc").unwrap();
		assert_eq!(format!("{s:?}"), "StackSizedString<3>(\"abc\")");
	}

	#[test]
	fn debug_for_zero_length() {
		let s = StackSizedString::<U0>::try_from("").unwrap();
		assert_eq!(format!("{s:?}"), "StackSizedString<0>(\"\")");
	}

	// ---------- into_bytes ----------------------------------------------------

	#[test]
	fn into_bytes_round_trip() {
		let s = StackSizedString::<U5>::try_from("hello").unwrap();
		let bytes: [u8; 5] = s.into_bytes();
		assert_eq!(&bytes, b"hello");
	}

	#[test]
	fn into_bytes_zero_length() {
		let s = StackSizedString::<U0>::try_from("").unwrap();
		let bytes: [u8; 0] = s.into_bytes();
		assert_eq!(bytes.len(), 0);
	}

	#[test]
	fn into_bytes_multibyte() {
		let s = StackSizedString::<U4>::try_from("🦀").unwrap();
		let bytes: [u8; 4] = s.into_bytes();
		assert_eq!(&bytes, &[0xF0, 0x9F, 0xA6, 0x80]);
	}

	// ---------- Copy / Clone --------------------------------------------------

	#[test]
	fn clone_produces_equal_value() {
		let a = StackSizedString::<U5>::try_from("hello").unwrap();
		#[allow(clippy::clone_on_copy)]
		let b = a.clone();
		assert_eq!(a, b);
		assert_eq!(&*a, &*b);
	}

	#[test]
	fn copy_does_not_move() {
		let a = StackSizedString::<U5>::try_from("hello").unwrap();
		let b = a; // copy
		// `a` is still usable because the type is `Copy` for typenum sizes.
		assert_eq!(&*a, "hello");
		assert_eq!(&*b, "hello");
	}

	// ---------- PartialEq / Eq / Hash ----------------------------------------

	#[test]
	fn equal_values_are_eq() {
		let a = StackSizedString::<U5>::try_from("hello").unwrap();
		let b = StackSizedString::<U5>::try_from("hello").unwrap();
		assert_eq!(a, b);
	}

	#[test]
	fn different_values_are_not_eq() {
		let a = StackSizedString::<U5>::try_from("hello").unwrap();
		let b = StackSizedString::<U5>::try_from("world").unwrap();
		assert_ne!(a, b);
	}

	#[test]
	fn equal_values_hash_equally() {
		let a = StackSizedString::<U8>::try_from("abcdefgh").unwrap();
		let b = StackSizedString::<U8>::try_from("abcdefgh").unwrap();
		assert_eq!(hash_of(&a), hash_of(&b));
	}

	#[test]
	fn different_values_hash_differently() {
		// Not strictly guaranteed by `Hash`, but overwhelmingly likely with
		// `DefaultHasher` and short distinct inputs.
		let a = StackSizedString::<U8>::try_from("abcdefgh").unwrap();
		let b = StackSizedString::<U8>::try_from("12345678").unwrap();
		assert_ne!(hash_of(&a), hash_of(&b));
	}

	// ---------- Serde: Serialize ---------------------------------------------

	#[test]
	fn serialize_to_json_string() {
		let s = StackSizedString::<U5>::try_from("hello").unwrap();
		let json = serde_json::to_string(&s).unwrap();
		assert_eq!(json, "\"hello\"");
	}

	#[test]
	fn serialize_zero_length() {
		let s = StackSizedString::<U0>::try_from("").unwrap();
		let json = serde_json::to_string(&s).unwrap();
		assert_eq!(json, "\"\"");
	}

	#[test]
	fn serialize_multibyte() {
		let s = StackSizedString::<U4>::try_from("🦀").unwrap();
		let json = serde_json::to_string(&s).unwrap();
		assert_eq!(json, "\"🦀\"");
	}

	// ---------- Serde: Deserialize -------------------------------------------

	#[test]
	fn deserialize_from_json_exact_length() {
		let s: StackSizedString<U5> = serde_json::from_str("\"hello\"").unwrap();
		assert_eq!(&*s, "hello");
	}

	#[test]
	fn deserialize_zero_length() {
		let s: StackSizedString<U0> = serde_json::from_str("\"\"").unwrap();
		assert_eq!(&*s, "");
	}

	#[test]
	fn deserialize_wrong_length_errors() {
		let res = serde_json::from_str::<StackSizedString<U5>>("\"hi\"");
		assert!(res.is_err());
		// The custom error message should mention the type size.
		let msg = res.unwrap_err().to_string();
		assert!(msg.contains("StackSizedString<5>"), "got: {msg}");
	}

	#[test]
	fn deserialize_too_long_errors() {
		let res = serde_json::from_str::<StackSizedString<U3>>("\"hello\"");
		assert!(res.is_err());
	}

	#[test]
	fn deserialize_non_string_errors() {
		let res = serde_json::from_str::<StackSizedString<U5>>("12345");
		assert!(res.is_err());
	}

	// ---------- Serde: round-trip --------------------------------------------

	#[test]
	fn serde_round_trip_ascii() {
		let original = StackSizedString::<U16>::try_from("abcdefghijklmnop").unwrap();
		let json = serde_json::to_string(&original).unwrap();
		let decoded: StackSizedString<U16> = serde_json::from_str(&json).unwrap();
		assert_eq!(original, decoded);
	}

	#[test]
	fn serde_round_trip_multibyte() {
		// 4 bytes of UTF-8 forming one crab.
		let original = StackSizedString::<U4>::try_from("🦀").unwrap();
		let json = serde_json::to_string(&original).unwrap();
		let decoded: StackSizedString<U4> = serde_json::from_str(&json).unwrap();
		assert_eq!(original, decoded);
	}

	// ---------- Internal representation sanity check -------------------------

	#[test]
	fn underlying_bytes_match_input() {
		let s = StackSizedString::<U5>::try_from("hello").unwrap();
		let expected: GenericArray<u8, U5> = *GenericArray::from_slice(b"hello");
		assert_eq!(s.0, expected);
	}
}

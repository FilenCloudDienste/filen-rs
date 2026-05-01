use std::{borrow::Cow, ops::Deref};

use generic_array::{ArrayLength, GenericArray};
use serde::{Deserialize, Serialize};

use super::BoxedSliceCow;
use crate::{error::ConversionError, traits::CowHelpers};

// a string guaranteed to be exactly N bytes long, and valid UTF-8
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct SizedString<'a, N: ArrayLength>(BoxedSliceCow<'a, N>);

impl<N: ArrayLength> CowHelpers for SizedString<'_, N>
where
	N::ArrayType<u8>: Copy,
{
	type CowBorrowed<'borrow>
		= SizedString<'borrow, N>
	where
		Self: 'borrow;

	type CowStatic = SizedString<'static, N>;

	fn as_borrowed_cow<'borrow>(&'borrow self) -> Self::CowBorrowed<'borrow>
	where
		Self: 'borrow,
	{
		SizedString(self.0.as_borrowed_cow())
	}

	fn into_owned_cow(self) -> Self::CowStatic {
		SizedString(self.0.into_owned_cow())
	}
}

impl<'a, N: ArrayLength> TryFrom<&'a str> for SizedString<'a, N> {
	type Error = ConversionError;

	fn try_from(value: &'a str) -> Result<Self, Self::Error> {
		let bytes = value.as_bytes();
		Ok(Self(BoxedSliceCow::try_from(bytes)?))
	}
}

impl<N: ArrayLength> TryFrom<String> for SizedString<'_, N> {
	type Error = ConversionError;

	fn try_from(value: String) -> Result<Self, Self::Error> {
		Ok(Self(BoxedSliceCow::try_from(value.into_bytes())?))
	}
}

impl<'a, N: ArrayLength> TryFrom<Cow<'a, str>> for SizedString<'a, N> {
	type Error = ConversionError;

	fn try_from(value: Cow<'a, str>) -> Result<Self, Self::Error> {
		match value {
			Cow::Borrowed(s) => Self::try_from(s),
			Cow::Owned(s) => Self::try_from(s),
		}
	}
}

impl<'a, N: ArrayLength> From<SizedString<'a, N>> for Cow<'a, str> {
	fn from(value: SizedString<'a, N>) -> Self {
		match value.0 {
			BoxedSliceCow::Borrowed(b) => {
				// SAFETY: SizedStrings can only be constructed from strings and the byte are therefore valid UTF-8
				Cow::Borrowed(unsafe { std::str::from_utf8_unchecked(b) })
			}
			BoxedSliceCow::Owned(o) => {
				// SAFETY: SizedStrings can only be constructed from strings and the byte are therefore valid UTF-8
				Cow::Owned(unsafe { String::from_utf8_unchecked(o.into_vec()) })
			}
		}
	}
}

impl<N: ArrayLength> std::fmt::Debug for SizedString<'_, N> {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "SizedString<{}>(\"{}\")", N::USIZE, self.deref())
	}
}

impl<N: ArrayLength> Deref for SizedString<'_, N> {
	type Target = str;

	fn deref(&self) -> &Self::Target {
		// SAFETY: SizedStrings can only be constructed from strings and the byte are therefore valid UTF-8
		unsafe { std::str::from_utf8_unchecked(self.0.deref()) }
	}
}

impl<'de, N: ArrayLength> Deserialize<'de> for SizedString<'de, N> {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let cow: Cow<'de, str> = crate::serde::cow::deserialize(deserializer)?;
		match Self::try_from(cow) {
			Ok(sized_string) => Ok(sized_string),
			Err(e) => Err(serde::de::Error::custom(format!(
				"failed to deserialize SizedString<{}>: {}",
				N::USIZE,
				e
			))),
		}
	}
}

impl<N: ArrayLength> Serialize for SizedString<'_, N> {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		self.deref().serialize(serializer)
	}
}

impl<N: ArrayLength> std::fmt::Display for SizedString<'_, N> {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		self.deref().fmt(f)
	}
}

impl<N: ArrayLength> SizedString<'_, N> {
	pub(crate) unsafe fn from_bytes_unchecked(bytes: Box<GenericArray<u8, N>>) -> Self {
		Self(BoxedSliceCow::Owned(bytes))
	}
}

#[cfg(test)]
mod tests {
	use std::{
		borrow::Cow,
		collections::hash_map::DefaultHasher,
		hash::{Hash, Hasher},
	};

	use generic_array::{GenericArray, typenum};

	use super::*;
	use crate::error::ConversionError;

	// Helper aliases to keep tests readable.
	type S5<'a> = SizedString<'a, typenum::U5>;
	type S0<'a> = SizedString<'a, typenum::U0>;
	type S1<'a> = SizedString<'a, typenum::U1>;
	type S4<'a> = SizedString<'a, typenum::U4>;

	fn hash_of<T: Hash>(value: &T) -> u64 {
		let mut hasher = DefaultHasher::new();
		value.hash(&mut hasher);
		hasher.finish()
	}

	// -----------------------------------------------------------------
	// TryFrom<&str>
	// -----------------------------------------------------------------

	#[test]
	fn try_from_str_exact_length_succeeds() {
		let s = S5::try_from("hello").expect("exact length should succeed");
		assert_eq!(&*s, "hello");
	}

	#[test]
	fn try_from_str_too_short_fails() {
		let err = S5::try_from("hi").unwrap_err();
		match err {
			ConversionError::InvalidLength(actual, expected) => {
				assert_eq!(actual, 2);
				assert_eq!(expected, 5);
			}
			other => panic!("expected InvalidLength, got {other:?}"),
		}
	}

	#[test]
	fn try_from_str_too_long_fails() {
		let err = S5::try_from("helloworld").unwrap_err();
		match err {
			ConversionError::InvalidLength(actual, expected) => {
				assert_eq!(actual, 10);
				assert_eq!(expected, 5);
			}
			other => panic!("expected InvalidLength, got {other:?}"),
		}
	}

	#[test]
	fn try_from_empty_str_with_n0_succeeds() {
		let s = S0::try_from("").expect("empty string with N=0 should succeed");
		assert_eq!(&*s, "");
		assert_eq!(s.len(), 0);
	}

	#[test]
	fn try_from_empty_str_with_nonzero_n_fails() {
		let err = S5::try_from("").unwrap_err();
		assert!(matches!(err, ConversionError::InvalidLength(0, 5)));
	}

	#[test]
	fn try_from_str_counts_bytes_not_chars() {
		// "héllo" is 6 bytes (é = 0xC3 0xA9) but 5 chars; with N=5 it must fail.
		let err = S5::try_from("héllo").unwrap_err();
		assert!(matches!(err, ConversionError::InvalidLength(6, 5)));

		// And it should succeed for N=6.
		type S6<'a> = SizedString<'a, typenum::U6>;
		let s = S6::try_from("héllo").expect("6-byte string with N=6 should succeed");
		assert_eq!(&*s, "héllo");
		assert_eq!(s.chars().count(), 5);
		assert_eq!(s.len(), 6);
	}

	#[test]
	fn try_from_str_handles_multibyte_utf8_at_exact_length() {
		// 4-byte emoji (U+1F600 "😀")
		let s = S4::try_from("😀").expect("4-byte emoji with N=4 should succeed");
		assert_eq!(&*s, "😀");
		assert_eq!(s.chars().count(), 1);
	}

	// -----------------------------------------------------------------
	// TryFrom<String>
	// -----------------------------------------------------------------

	#[test]
	fn try_from_string_exact_length_succeeds() {
		let owned = String::from("hello");
		let s = S5::try_from(owned).expect("owned exact length should succeed");
		assert_eq!(&*s, "hello");
	}

	#[test]
	fn try_from_string_wrong_length_fails() {
		let err = S5::try_from(String::from("oops!!")).unwrap_err();
		assert!(matches!(err, ConversionError::InvalidLength(6, 5)));
	}

	// -----------------------------------------------------------------
	// TryFrom<Cow<str>>
	// -----------------------------------------------------------------

	#[test]
	fn try_from_cow_borrowed_succeeds() {
		let cow: Cow<'_, str> = Cow::Borrowed("hello");
		let s = S5::try_from(cow).expect("Cow::Borrowed should succeed");
		assert_eq!(&*s, "hello");
	}

	#[test]
	fn try_from_cow_owned_succeeds() {
		let cow: Cow<'_, str> = Cow::Owned(String::from("hello"));
		let s = S5::try_from(cow).expect("Cow::Owned should succeed");
		assert_eq!(&*s, "hello");
	}

	#[test]
	fn try_from_cow_borrowed_wrong_length_fails() {
		let cow: Cow<'_, str> = Cow::Borrowed("nope");
		let err = S5::try_from(cow).unwrap_err();
		assert!(matches!(err, ConversionError::InvalidLength(4, 5)));
	}

	#[test]
	fn try_from_cow_owned_wrong_length_fails() {
		let cow: Cow<'_, str> = Cow::Owned(String::from("toolong"));
		let err = S5::try_from(cow).unwrap_err();
		assert!(matches!(err, ConversionError::InvalidLength(7, 5)));
	}

	// -----------------------------------------------------------------
	// From<SizedString> for Cow<str> — preserves Borrowed/Owned variant
	// -----------------------------------------------------------------

	#[test]
	fn into_cow_str_preserves_borrowed() {
		let src: &str = "hello";
		let s = S5::try_from(src).unwrap();
		let cow: Cow<'_, str> = s.into();
		assert!(matches!(cow, Cow::Borrowed(_)));
		assert_eq!(cow, "hello");
	}

	#[test]
	fn into_cow_str_preserves_owned() {
		let s = S5::try_from(String::from("world")).unwrap();
		let cow: Cow<'_, str> = s.into();
		assert!(matches!(cow, Cow::Owned(_)));
		assert_eq!(cow, "world");
	}

	#[test]
	fn into_cow_str_roundtrip_preserves_multibyte_utf8() {
		let s = S4::try_from(String::from("😀")).unwrap();
		let cow: Cow<'_, str> = s.into();
		assert_eq!(cow, "😀");
	}

	// -----------------------------------------------------------------
	// Deref / AsRef behaviour
	// -----------------------------------------------------------------

	#[test]
	fn deref_returns_str_slice() {
		let s = S5::try_from("hello").unwrap();
		let as_str: &str = &s;
		assert_eq!(as_str, "hello");
		// str methods should work via Deref.
		assert_eq!(s.to_uppercase(), "HELLO");
		assert!(s.starts_with("he"));
		assert_eq!(s.len(), 5);
	}

	// -----------------------------------------------------------------
	// Display / Debug
	// -----------------------------------------------------------------

	#[test]
	fn display_outputs_inner_string() {
		let s = S5::try_from("hello").unwrap();
		assert_eq!(format!("{s}"), "hello");
	}

	#[test]
	fn debug_includes_size_and_content() {
		let s = S5::try_from("hello").unwrap();
		let debug = format!("{s:?}");
		assert_eq!(debug, "SizedString<5>(\"hello\")");
	}

	#[test]
	fn debug_for_n0_is_empty() {
		let s = S0::try_from("").unwrap();
		assert_eq!(format!("{s:?}"), "SizedString<0>(\"\")");
	}

	// -----------------------------------------------------------------
	// PartialEq / Eq / Hash
	// -----------------------------------------------------------------

	#[test]
	fn equal_sized_strings_compare_equal() {
		let a = S5::try_from("hello").unwrap();
		let b = S5::try_from(String::from("hello")).unwrap();
		assert_eq!(a, b);
	}

	#[test]
	fn different_sized_strings_compare_unequal() {
		let a = S5::try_from("hello").unwrap();
		let b = S5::try_from("world").unwrap();
		assert_ne!(a, b);
	}

	#[test]
	fn equal_borrowed_and_owned_hash_identically() {
		let borrowed = S5::try_from("hello").unwrap();
		let owned = S5::try_from(String::from("hello")).unwrap();
		// Hash::Eq contract: a == b => hash(a) == hash(b).
		assert_eq!(borrowed, owned);
		assert_eq!(hash_of(&borrowed), hash_of(&owned));
	}

	#[test]
	fn different_strings_typically_hash_differently() {
		let a = S5::try_from("hello").unwrap();
		let b = S5::try_from("world").unwrap();
		// Not strictly required by Hash, but extremely likely for the default
		// hasher; if this ever fails it's almost certainly a bug.
		assert_ne!(hash_of(&a), hash_of(&b));
	}

	// -----------------------------------------------------------------
	// Clone
	// -----------------------------------------------------------------

	#[test]
	fn clone_of_borrowed_equals_original() {
		let s = S5::try_from("hello").unwrap();
		let c = s.clone();
		assert_eq!(s, c);
		assert_eq!(&*c, "hello");
	}

	#[test]
	fn clone_of_owned_equals_original() {
		let s = S5::try_from(String::from("hello")).unwrap();
		let c = s.clone();
		assert_eq!(s, c);
		assert_eq!(&*c, "hello");
	}

	// -----------------------------------------------------------------
	// CowHelpers
	// -----------------------------------------------------------------

	#[test]
	fn as_borrowed_cow_yields_equal_value() {
		let owned = S5::try_from(String::from("hello")).unwrap();
		let borrowed = owned.as_borrowed_cow();
		assert_eq!(&*borrowed, "hello");
		assert_eq!(owned, borrowed);
	}

	#[test]
	fn into_owned_cow_extends_lifetime_to_static() {
		// Build a SizedString that borrows from a local, then promote to 'static.
		let local = String::from("hello");
		let borrowed: S5<'_> = S5::try_from(local.as_str()).unwrap();
		let static_s: SizedString<'static, typenum::U5> = borrowed.into_owned_cow();
		// Drop the original source; the 'static value must remain valid.
		drop(local);
		assert_eq!(&*static_s, "hello");
	}

	#[test]
	fn into_owned_cow_on_already_owned_is_equivalent() {
		let owned = S5::try_from(String::from("hello")).unwrap();
		let static_s: SizedString<'static, typenum::U5> = owned.into_owned_cow();
		assert_eq!(&*static_s, "hello");
	}

	// -----------------------------------------------------------------
	// from_bytes_unchecked
	// -----------------------------------------------------------------

	#[test]
	fn from_bytes_unchecked_constructs_owned_static() {
		let bytes: Box<GenericArray<u8, typenum::U5>> =
			Box::new(GenericArray::from_array(*b"hello"));
		// SAFETY: the bytes "hello" are valid UTF-8.
		let s: SizedString<'static, typenum::U5> =
			unsafe { SizedString::from_bytes_unchecked(bytes) };
		assert_eq!(&*s, "hello");
	}

	#[test]
	fn from_bytes_unchecked_supports_multibyte_utf8() {
		// "😀" is exactly 4 bytes of valid UTF-8.
		let bytes: Box<GenericArray<u8, typenum::U4>> =
			Box::new(GenericArray::from_array([0xF0, 0x9F, 0x98, 0x80]));
		// SAFETY: these bytes are the valid UTF-8 encoding of U+1F600.
		let s: SizedString<'static, typenum::U4> =
			unsafe { SizedString::from_bytes_unchecked(bytes) };
		assert_eq!(&*s, "😀");
	}

	// -----------------------------------------------------------------
	// Serde
	// -----------------------------------------------------------------

	#[test]
	fn serialize_emits_plain_string() {
		let s = S5::try_from("hello").unwrap();
		let json = serde_json::to_string(&s).unwrap();
		assert_eq!(json, "\"hello\"");
	}

	#[test]
	fn deserialize_exact_length_succeeds() {
		let s: SizedString<'_, typenum::U5> = serde_json::from_str("\"hello\"").unwrap();
		assert_eq!(&*s, "hello");
	}

	#[test]
	fn deserialize_wrong_length_fails() {
		let res: Result<SizedString<'_, typenum::U5>, _> = serde_json::from_str("\"hi\"");
		let err = res.unwrap_err().to_string();
		assert!(
			err.contains("SizedString<5>"),
			"error should mention the size, got: {err}"
		);
	}

	#[test]
	fn serde_roundtrip_preserves_value() {
		let original = S5::try_from("hello").unwrap();
		let json = serde_json::to_string(&original).unwrap();
		let decoded: SizedString<'_, typenum::U5> = serde_json::from_str(&json).unwrap();
		assert_eq!(original, decoded);
	}

	#[test]
	fn serde_roundtrip_preserves_multibyte_utf8() {
		let original = S4::try_from("😀").unwrap();
		let json = serde_json::to_string(&original).unwrap();
		let decoded: SizedString<'_, typenum::U4> = serde_json::from_str(&json).unwrap();
		assert_eq!(original, decoded);
		assert_eq!(&*decoded, "😀");
	}

	// -----------------------------------------------------------------
	// Edge case: N = 1
	// -----------------------------------------------------------------

	#[test]
	fn n1_accepts_single_ascii_byte() {
		let s = S1::try_from("x").unwrap();
		assert_eq!(&*s, "x");
	}

	#[test]
	fn n1_rejects_multibyte_char() {
		// "é" is 2 bytes, can't fit in N=1.
		let err = S1::try_from("é").unwrap_err();
		assert!(matches!(err, ConversionError::InvalidLength(2, 1)));
	}
}

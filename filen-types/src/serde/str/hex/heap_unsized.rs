use std::borrow::Cow;

use generic_array::ArrayLength;
use serde::{Deserialize, Serialize};

use super::stack_sized::SizedHexString;

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct HexString<'a>(Cow<'a, [u8]>);

impl HexString<'_> {
	pub fn as_slice(&self) -> &[u8] {
		&self.0
	}
}

impl core::fmt::Display for HexString<'_> {
	fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
		write!(f, "{}", hex::display(&self.0))
	}
}

impl core::fmt::Debug for HexString<'_> {
	fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
		write!(f, "HexString({})", hex::display(&self.0))
	}
}

impl HexString<'_> {
	pub fn new_from_hex_str(hex_str: &str) -> Result<Self, hex::FromHexError> {
		Ok(Self(hex::decode(hex_str)?.into()))
	}
}

impl AsRef<[u8]> for HexString<'_> {
	fn as_ref(&self) -> &[u8] {
		&self.0
	}
}

impl From<Vec<u8>> for HexString<'_> {
	fn from(bytes: Vec<u8>) -> Self {
		Self(bytes.into())
	}
}

impl Serialize for HexString<'_> {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		hex::serde::no_prefix::serialize(&self.0, serializer)
	}
}

// use owned deserialization for now
impl<'de> Deserialize<'de> for HexString<'_> {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		Ok(Self(hex::serde::no_prefix::deserialize(deserializer)?))
	}
}

impl<N: ArrayLength> From<SizedHexString<N>> for HexString<'static> {
	fn from(sized_hex: SizedHexString<N>) -> Self {
		Self(sized_hex.0.to_vec().into())
	}
}

impl<'a, N: ArrayLength> From<&'a SizedHexString<N>> for HexString<'a> {
	fn from(sized_hex: &'a SizedHexString<N>) -> Self {
		Self(Cow::Borrowed(sized_hex.as_slice()))
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use generic_array::{GenericArray, typenum::U4};
	use std::collections::HashMap;

	// ============================================================
	// Construction: new_from_hex_str
	// ============================================================

	#[test]
	fn new_from_hex_str_empty() {
		let hs = HexString::new_from_hex_str("").unwrap();
		assert_eq!(hs.as_ref(), &[] as &[u8]);
	}

	#[test]
	fn new_from_hex_str_single_byte() {
		let hs = HexString::new_from_hex_str("ab").unwrap();
		assert_eq!(hs.as_ref(), &[0xab]);
	}

	#[test]
	fn new_from_hex_str_multiple_bytes() {
		let hs = HexString::new_from_hex_str("deadbeef").unwrap();
		assert_eq!(hs.as_ref(), &[0xde, 0xad, 0xbe, 0xef]);
	}

	#[test]
	fn new_from_hex_str_lowercase() {
		let hs = HexString::new_from_hex_str("0123456789abcdef").unwrap();
		assert_eq!(
			hs.as_ref(),
			&[0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]
		);
	}

	#[test]
	fn new_from_hex_str_uppercase() {
		let hs = HexString::new_from_hex_str("DEADBEEF").unwrap();
		assert_eq!(hs.as_ref(), &[0xde, 0xad, 0xbe, 0xef]);
	}

	#[test]
	fn new_from_hex_str_mixed_case() {
		let hs = HexString::new_from_hex_str("DeAdBeEf").unwrap();
		assert_eq!(hs.as_ref(), &[0xde, 0xad, 0xbe, 0xef]);
	}

	#[test]
	fn new_from_hex_str_all_zeros() {
		let hs = HexString::new_from_hex_str("0000").unwrap();
		assert_eq!(hs.as_ref(), &[0x00, 0x00]);
	}

	#[test]
	fn new_from_hex_str_all_ones() {
		let hs = HexString::new_from_hex_str("ffff").unwrap();
		assert_eq!(hs.as_ref(), &[0xff, 0xff]);
	}

	#[test]
	fn new_from_hex_str_with_0x_prefix_lowercase() {
		// Lowercase 0x prefix is ignored.
		let hs = HexString::new_from_hex_str("0xdeadbeef").unwrap();
		assert_eq!(hs.as_ref(), &[0xde, 0xad, 0xbe, 0xef]);
	}

	#[test]
	fn new_from_hex_str_with_0x_prefix_mixed_case_body() {
		let hs = HexString::new_from_hex_str("0xDeAdBeEf").unwrap();
		assert_eq!(hs.as_ref(), &[0xde, 0xad, 0xbe, 0xef]);
	}

	#[test]
	fn new_from_hex_str_with_0x_prefix_uppercase_body() {
		let hs = HexString::new_from_hex_str("0xDEADBEEF").unwrap();
		assert_eq!(hs.as_ref(), &[0xde, 0xad, 0xbe, 0xef]);
	}

	#[test]
	fn new_from_hex_str_prefix_only_is_empty() {
		// "0x" with nothing after should decode to an empty byte string.
		let hs = HexString::new_from_hex_str("0x").unwrap();
		assert_eq!(hs.as_ref(), &[] as &[u8]);
	}

	#[test]
	fn new_from_hex_str_prefix_and_no_prefix_match() {
		let with = HexString::new_from_hex_str("0xcafebabe").unwrap();
		let without = HexString::new_from_hex_str("cafebabe").unwrap();
		assert_eq!(with, without);
	}

	#[test]
	fn new_from_hex_str_uppercase_prefix_fails() {
		// Only lowercase "0x" is accepted as a prefix; "0X" is not stripped,
		// so the leading 'X' is treated as an invalid hex character.
		let err = HexString::new_from_hex_str("0XDEADBEEF").unwrap_err();
		assert!(matches!(err, hex::FromHexError::InvalidHexCharacter { .. }));
	}

	#[test]
	fn new_from_hex_str_uppercase_prefix_only_fails() {
		// "0X" alone: 'X' is not a valid hex digit.
		let err = HexString::new_from_hex_str("0X").unwrap_err();
		assert!(matches!(err, hex::FromHexError::InvalidHexCharacter { .. }));
	}

	#[test]
	fn new_from_hex_str_odd_length_fails() {
		let err = HexString::new_from_hex_str("abc").unwrap_err();
		assert!(matches!(err, hex::FromHexError::OddLength));
	}

	#[test]
	fn new_from_hex_str_odd_length_with_prefix_fails() {
		// After stripping "0x", "abc" still has odd length.
		let err = HexString::new_from_hex_str("0xabc").unwrap_err();
		assert!(matches!(err, hex::FromHexError::OddLength));
	}

	#[test]
	fn new_from_hex_str_invalid_char_fails() {
		let err = HexString::new_from_hex_str("zz").unwrap_err();
		assert!(matches!(err, hex::FromHexError::InvalidHexCharacter { .. }));
	}

	#[test]
	fn new_from_hex_str_invalid_char_in_middle_fails() {
		let err = HexString::new_from_hex_str("aabbggcc").unwrap_err();
		assert!(matches!(err, hex::FromHexError::InvalidHexCharacter { .. }));
	}

	#[test]
	fn new_from_hex_str_invalid_char_after_prefix_fails() {
		let err = HexString::new_from_hex_str("0xzz").unwrap_err();
		assert!(matches!(err, hex::FromHexError::InvalidHexCharacter { .. }));
	}

	#[test]
	fn new_from_hex_str_with_whitespace_fails() {
		let err = HexString::new_from_hex_str("de  ad").unwrap_err();
		assert!(matches!(err, hex::FromHexError::InvalidHexCharacter { .. }));
	}

	// ============================================================
	// From<Vec<u8>>
	// ============================================================

	#[test]
	fn from_vec_empty() {
		let hs: HexString = Vec::<u8>::new().into();
		assert_eq!(hs.as_ref(), &[] as &[u8]);
	}

	#[test]
	fn from_vec_non_empty() {
		let hs: HexString = vec![0x01, 0x02, 0x03].into();
		assert_eq!(hs.as_ref(), &[0x01, 0x02, 0x03]);
	}

	#[test]
	fn from_vec_roundtrip_via_hex_str() {
		let original = vec![0xde, 0xad, 0xbe, 0xef];
		let hs: HexString = original.clone().into();
		let parsed = HexString::new_from_hex_str(&hs.to_string()).unwrap();
		assert_eq!(parsed.as_ref(), original.as_slice());
	}

	// ============================================================
	// AsRef<[u8]>
	// ============================================================

	#[test]
	fn as_ref_returns_underlying_bytes() {
		let hs: HexString = vec![1, 2, 3, 4, 5].into();
		let slice: &[u8] = hs.as_ref();
		assert_eq!(slice, &[1, 2, 3, 4, 5]);
	}

	#[test]
	fn as_ref_consistent_across_calls() {
		let hs: HexString = vec![10, 20, 30].into();
		let a = hs.as_ref();
		let b = hs.as_ref();
		assert_eq!(a, b);
	}

	// ============================================================
	// Display
	// ============================================================

	#[test]
	fn display_empty() {
		let hs: HexString = Vec::<u8>::new().into();
		assert_eq!(format!("{}", hs), "");
	}

	#[test]
	fn display_renders_lowercase_hex_no_prefix() {
		let hs: HexString = vec![0xde, 0xad, 0xbe, 0xef].into();
		assert_eq!(format!("{}", hs), "deadbeef");
	}

	#[test]
	fn display_pads_single_digits() {
		// 0x0a should render as "0a", not "a".
		let hs: HexString = vec![0x0a, 0x00, 0x01].into();
		assert_eq!(format!("{}", hs), "0a0001");
	}

	#[test]
	fn display_roundtrip_with_parser() {
		let original = "0123456789abcdef";
		let hs = HexString::new_from_hex_str(original).unwrap();
		assert_eq!(format!("{}", hs), original);
	}

	#[test]
	fn display_strips_prefix_after_parse() {
		// Parsing "0x..." then displaying should yield the no-prefix form.
		let hs = HexString::new_from_hex_str("0xdeadbeef").unwrap();
		assert_eq!(format!("{}", hs), "deadbeef");
	}

	// ============================================================
	// Debug
	// ============================================================

	#[test]
	fn debug_format_includes_wrapper() {
		let hs: HexString = vec![0xab, 0xcd].into();
		assert_eq!(format!("{:?}", hs), "HexString(abcd)");
	}

	#[test]
	fn debug_format_empty() {
		let hs: HexString = Vec::<u8>::new().into();
		assert_eq!(format!("{:?}", hs), "HexString()");
	}

	// ============================================================
	// Equality, Hashing, Cloning
	// ============================================================

	#[test]
	fn equality_same_contents() {
		let a: HexString = vec![1, 2, 3].into();
		let b: HexString = vec![1, 2, 3].into();
		assert_eq!(a, b);
	}

	#[test]
	fn equality_different_contents() {
		let a: HexString = vec![1, 2, 3].into();
		let b: HexString = vec![1, 2, 4].into();
		assert_ne!(a, b);
	}

	#[test]
	fn equality_different_lengths() {
		let a: HexString = vec![1, 2, 3].into();
		let b: HexString = vec![1, 2].into();
		assert_ne!(a, b);
	}

	#[test]
	fn equality_empty_strings() {
		let a: HexString = Vec::<u8>::new().into();
		let b: HexString = Vec::<u8>::new().into();
		assert_eq!(a, b);
	}

	#[test]
	fn equality_prefix_vs_no_prefix_parsed() {
		let with = HexString::new_from_hex_str("0xdeadbeef").unwrap();
		let without = HexString::new_from_hex_str("deadbeef").unwrap();
		assert_eq!(with, without);
	}

	#[test]
	fn clone_yields_equal_value() {
		let a: HexString = vec![0xaa, 0xbb, 0xcc].into();
		let b = a.clone();
		assert_eq!(a, b);
		assert_eq!(a.as_ref(), b.as_ref());
	}

	#[test]
	fn clone_is_independent_in_memory() {
		// A clone must own its own buffer.
		let a: HexString = vec![1, 2, 3].into();
		let b = a.clone();
		assert!(!std::ptr::eq(a.as_ref().as_ptr(), b.as_ref().as_ptr()));
	}

	#[test]
	fn hash_equal_for_equal_values() {
		use std::collections::hash_map::DefaultHasher;
		use std::hash::{Hash, Hasher};

		let a: HexString = vec![1, 2, 3].into();
		let b: HexString = vec![1, 2, 3].into();

		let mut ha = DefaultHasher::new();
		let mut hb = DefaultHasher::new();
		a.hash(&mut ha);
		b.hash(&mut hb);

		assert_eq!(ha.finish(), hb.finish());
	}

	#[test]
	fn usable_as_hashmap_key() {
		let mut map: HashMap<HexString, i32> = HashMap::new();
		let key: HexString = vec![1, 2, 3].into();
		map.insert(key.clone(), 42);

		let lookup: HexString = vec![1, 2, 3].into();
		assert_eq!(map.get(&lookup), Some(&42));
	}

	// ============================================================
	// Serde: Serialize
	// ============================================================

	#[test]
	fn serialize_to_json_no_prefix() {
		let hs: HexString = vec![0xde, 0xad, 0xbe, 0xef].into();
		let json = serde_json::to_string(&hs).unwrap();
		// Serializer is the no_prefix variant, so output never includes "0x".
		assert_eq!(json, "\"deadbeef\"");
	}

	#[test]
	fn serialize_empty_to_json() {
		let hs: HexString = Vec::<u8>::new().into();
		let json = serde_json::to_string(&hs).unwrap();
		assert_eq!(json, "\"\"");
	}

	#[test]
	fn serialize_pads_single_digits() {
		let hs: HexString = vec![0x00, 0x0f, 0xa0].into();
		let json = serde_json::to_string(&hs).unwrap();
		assert_eq!(json, "\"000fa0\"");
	}

	// ============================================================
	// Serde: Deserialize
	// ============================================================

	#[test]
	fn deserialize_from_json() {
		let hs: HexString = serde_json::from_str("\"deadbeef\"").unwrap();
		assert_eq!(hs.as_ref(), &[0xde, 0xad, 0xbe, 0xef]);
	}

	#[test]
	fn deserialize_empty_from_json() {
		let hs: HexString = serde_json::from_str("\"\"").unwrap();
		assert_eq!(hs.as_ref(), &[] as &[u8]);
	}

	#[test]
	fn deserialize_uppercase_from_json() {
		let hs: HexString = serde_json::from_str("\"DEADBEEF\"").unwrap();
		assert_eq!(hs.as_ref(), &[0xde, 0xad, 0xbe, 0xef]);
	}

	#[test]
	fn deserialize_with_0x_prefix() {
		// Lowercase prefix is ignored on deserialize too.
		let hs: HexString = serde_json::from_str("\"0xdeadbeef\"").unwrap();
		assert_eq!(hs.as_ref(), &[0xde, 0xad, 0xbe, 0xef]);
	}

	#[test]
	fn deserialize_with_0x_prefix_uppercase_body() {
		let hs: HexString = serde_json::from_str("\"0xDEADBEEF\"").unwrap();
		assert_eq!(hs.as_ref(), &[0xde, 0xad, 0xbe, 0xef]);
	}

	#[test]
	fn deserialize_prefix_only_is_empty() {
		let hs: HexString = serde_json::from_str("\"0x\"").unwrap();
		assert_eq!(hs.as_ref(), &[] as &[u8]);
	}

	#[test]
	fn deserialize_with_and_without_prefix_match() {
		let with: HexString = serde_json::from_str("\"0xcafebabe\"").unwrap();
		let without: HexString = serde_json::from_str("\"cafebabe\"").unwrap();
		assert_eq!(with, without);
	}

	#[test]
	fn deserialize_uppercase_prefix_fails() {
		// "0X" is not a recognized prefix, so the 'X' is an invalid hex char.
		let result: Result<HexString, _> = serde_json::from_str("\"0XDEADBEEF\"");
		assert!(result.is_err());
	}

	#[test]
	fn deserialize_odd_length_fails() {
		let result: Result<HexString, _> = serde_json::from_str("\"abc\"");
		assert!(result.is_err());
	}

	#[test]
	fn deserialize_odd_length_with_prefix_fails() {
		let result: Result<HexString, _> = serde_json::from_str("\"0xabc\"");
		assert!(result.is_err());
	}

	#[test]
	fn deserialize_invalid_chars_fails() {
		let result: Result<HexString, _> = serde_json::from_str("\"zzzz\"");
		assert!(result.is_err());
	}

	#[test]
	fn deserialize_invalid_chars_after_prefix_fails() {
		let result: Result<HexString, _> = serde_json::from_str("\"0xzzzz\"");
		assert!(result.is_err());
	}

	#[test]
	fn deserialize_non_string_fails() {
		let result: Result<HexString, _> = serde_json::from_str("12345");
		assert!(result.is_err());
	}

	#[test]
	fn deserialize_null_fails() {
		let result: Result<HexString, _> = serde_json::from_str("null");
		assert!(result.is_err());
	}

	// ============================================================
	// Serde: Roundtrip
	// ============================================================

	#[test]
	fn serde_roundtrip_json() {
		let original: HexString = vec![0x00, 0x01, 0x7f, 0x80, 0xfe, 0xff].into();
		let json = serde_json::to_string(&original).unwrap();
		let decoded: HexString = serde_json::from_str(&json).unwrap();
		assert_eq!(original, decoded);
	}

	#[test]
	fn serde_roundtrip_empty() {
		let original: HexString = Vec::<u8>::new().into();
		let json = serde_json::to_string(&original).unwrap();
		let decoded: HexString = serde_json::from_str(&json).unwrap();
		assert_eq!(original, decoded);
	}

	#[test]
	fn serde_roundtrip_all_byte_values() {
		let original: HexString = (0u8..=255).collect::<Vec<u8>>().into();
		let json = serde_json::to_string(&original).unwrap();
		let decoded: HexString = serde_json::from_str(&json).unwrap();
		assert_eq!(original, decoded);
	}

	#[test]
	fn serde_prefixed_input_roundtrips_to_unprefixed_output() {
		// Deserializing a prefixed string then re-serializing should drop the prefix.
		let decoded: HexString = serde_json::from_str("\"0xdeadbeef\"").unwrap();
		let reserialized = serde_json::to_string(&decoded).unwrap();
		assert_eq!(reserialized, "\"deadbeef\"");
	}

	#[test]
	fn serde_in_struct() {
		#[derive(Serialize, Deserialize, PartialEq, Debug)]
		struct Wrapper {
			data: HexString<'static>,
		}

		let original = Wrapper {
			data: vec![0xca, 0xfe, 0xba, 0xbe].into(),
		};
		let json = serde_json::to_string(&original).unwrap();
		assert_eq!(json, r#"{"data":"cafebabe"}"#);

		let decoded: Wrapper = serde_json::from_str(&json).unwrap();
		assert_eq!(decoded, original);
	}

	#[test]
	fn serde_in_struct_accepts_prefixed_input() {
		#[derive(Deserialize)]
		struct Wrapper {
			data: HexString<'static>,
		}

		let decoded: Wrapper = serde_json::from_str(r#"{"data":"0xcafebabe"}"#).unwrap();
		assert_eq!(decoded.data.as_ref(), &[0xca, 0xfe, 0xba, 0xbe]);
	}

	// ============================================================
	// From<SizedHexString<N>>
	// ============================================================

	#[test]
	fn from_sized_hex_string() {
		let arr = GenericArray::<u8, U4>::from([0xde, 0xad, 0xbe, 0xef]);
		let sized = SizedHexString::<U4>(arr);
		let hs: HexString = sized.into();
		assert_eq!(hs.as_ref(), &[0xde, 0xad, 0xbe, 0xef]);
	}

	#[test]
	fn from_sized_hex_string_preserves_display() {
		let arr = GenericArray::<u8, U4>::from([0x01, 0x02, 0x03, 0x04]);
		let sized = SizedHexString::<U4>(arr);
		let hs: HexString = sized.into();
		assert_eq!(format!("{}", hs), "01020304");
	}

	// ============================================================
	// Large input
	// ============================================================

	#[test]
	fn large_input_roundtrip() {
		let bytes: Vec<u8> = (0..1024).map(|i| (i % 256) as u8).collect();
		let hs: HexString = bytes.clone().into();
		let s = hs.to_string();
		assert_eq!(s.len(), bytes.len() * 2);

		let parsed = HexString::new_from_hex_str(&s).unwrap();
		assert_eq!(parsed.as_ref(), bytes.as_slice());
	}

	#[test]
	fn large_input_with_prefix_roundtrip() {
		let bytes: Vec<u8> = (0..512usize)
			.map(|i| (i.wrapping_mul(7) % 256) as u8)
			.collect();
		let hs: HexString = bytes.clone().into();
		let prefixed = format!("0x{}", hs);

		let parsed = HexString::new_from_hex_str(&prefixed).unwrap();
		assert_eq!(parsed.as_ref(), bytes.as_slice());
	}
}

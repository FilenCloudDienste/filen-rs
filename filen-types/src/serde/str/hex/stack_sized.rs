use std::{marker::PhantomData, ops::Mul};

use generic_array::{ArrayLength, GenericArray, IntoArrayLength, typenum::Const};
use serde::{Deserialize, Deserializer, Serialize, de::Visitor};
use typenum::{Prod, U2};

use crate::serde::str::StackSizedString;

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct SizedHexString<N: ArrayLength>(pub(in super::super) GenericArray<u8, N>);
impl<N: ArrayLength> Copy for SizedHexString<N> where N::ArrayType<u8>: Copy {}

impl<N: ArrayLength> core::fmt::Display for SizedHexString<N> {
	fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
		write!(f, "{}", hex::display(&self.0))
	}
}

impl<N: ArrayLength> core::fmt::Debug for SizedHexString<N> {
	fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
		write!(f, "SizedHexString({})", hex::display(&self.0))
	}
}

impl<N: ArrayLength> SizedHexString<N> {
	pub fn new_from_hex_str(hex_str: &str) -> Result<Self, hex::FromHexError> {
		Ok(Self(super::hex_decode_to_generic_array(
			hex_str.as_bytes(),
		)?))
	}
}

impl<N> SizedHexString<N>
where
	N: ArrayLength + Mul<U2>,
	Prod<N, U2>: ArrayLength,
{
	pub fn to_str(&self) -> StackSizedString<Prod<N, U2>> {
		let mut out = GenericArray::<u8, Prod<N, U2>>::default();
		// SAFETY: out is exactly 2 * N bytes long, which is what encode_to_slice requires
		// It also encodes valid UTF-8, since it's just hex encoding of valid bytes
		unsafe {
			hex::encode_to_slice(&self.0, &mut out).unwrap_unchecked();
		}
		StackSizedString(out)
	}
}

impl<N> SizedHexString<N>
where
	N: ArrayLength,
{
	pub fn as_slice(&self) -> &[u8] {
		self.0.as_slice()
	}
}

impl<const U: usize, N: ArrayLength> AsRef<[u8; U]> for SizedHexString<N>
where
	Const<U>: IntoArrayLength<ArrayLength = N>,
{
	fn as_ref(&self) -> &[u8; U] {
		self.0.as_ref()
	}
}

impl<const U: usize, N: ArrayLength> From<[u8; U]> for SizedHexString<N>
where
	Const<U>: IntoArrayLength<ArrayLength = N>,
{
	fn from(bytes: [u8; U]) -> Self {
		Self(bytes.into())
	}
}

impl<N: ArrayLength> From<GenericArray<u8, N>> for SizedHexString<N> {
	fn from(bytes: GenericArray<u8, N>) -> Self {
		Self(bytes)
	}
}

impl<N: ArrayLength> Serialize for SizedHexString<N> {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		hex::serde::no_prefix::serialize(AsRef::<[u8]>::as_ref(&self.0), serializer)
	}
}

impl<'de, N: ArrayLength> Deserialize<'de> for SizedHexString<N> {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: Deserializer<'de>,
	{
		struct HexVisitor<N: ArrayLength>(PhantomData<N>);

		impl<'de, N: ArrayLength> Visitor<'de> for HexVisitor<N> {
			type Value = SizedHexString<N>;

			fn expecting(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
				write!(f, "a hex string of {} bytes", N::USIZE)
			}

			fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<Self::Value, E> {
				let mut out = GenericArray::<u8, N>::default();
				hex::decode_to_slice(s, &mut out).map_err(serde::de::Error::custom)?;
				Ok(SizedHexString(out))
			}
		}

		deserializer.deserialize_str(HexVisitor::<N>(PhantomData))
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use generic_array::GenericArray;
	use std::collections::hash_map::DefaultHasher;
	use std::hash::{Hash, Hasher};
	use typenum::{U0, U1, U2, U4, U8, U16, U32};

	fn hash_of<T: Hash>(t: &T) -> u64 {
		let mut h = DefaultHasher::new();
		t.hash(&mut h);
		h.finish()
	}

	// ---------- construction: new_from_hex_str ----------

	#[test]
	fn new_from_hex_str_decodes_valid_lowercase() {
		let s = SizedHexString::<U4>::new_from_hex_str("deadbeef").unwrap();
		assert_eq!(s.as_slice(), &[0xde, 0xad, 0xbe, 0xef]);
	}

	#[test]
	fn new_from_hex_str_decodes_valid_uppercase() {
		let s = SizedHexString::<U4>::new_from_hex_str("DEADBEEF").unwrap();
		assert_eq!(s.as_slice(), &[0xde, 0xad, 0xbe, 0xef]);
	}

	#[test]
	fn new_from_hex_str_decodes_mixed_case() {
		let s = SizedHexString::<U4>::new_from_hex_str("DeAdBeEf").unwrap();
		assert_eq!(s.as_slice(), &[0xde, 0xad, 0xbe, 0xef]);
	}

	#[test]
	fn new_from_hex_str_decodes_all_zeros() {
		let s = SizedHexString::<U8>::new_from_hex_str("0000000000000000").unwrap();
		assert_eq!(s.as_slice(), &[0u8; 8]);
	}

	#[test]
	fn new_from_hex_str_decodes_all_ones() {
		let s = SizedHexString::<U4>::new_from_hex_str("ffffffff").unwrap();
		assert_eq!(s.as_slice(), &[0xff; 4]);
	}

	#[test]
	fn new_from_hex_str_zero_length() {
		let s = SizedHexString::<U0>::new_from_hex_str("").unwrap();
		assert_eq!(s.as_slice(), &[] as &[u8]);
	}

	#[test]
	fn new_from_hex_str_single_byte() {
		let s = SizedHexString::<U1>::new_from_hex_str("a5").unwrap();
		assert_eq!(s.as_slice(), &[0xa5]);
	}

	#[test]
	fn new_from_hex_str_32_bytes() {
		let hex_in = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
		let s = SizedHexString::<U32>::new_from_hex_str(hex_in).unwrap();
		assert_eq!(s.as_slice().len(), 32);
		assert_eq!(s.as_slice()[0], 0x01);
		assert_eq!(s.as_slice()[31], 0xef);
	}

	#[test]
	fn new_from_hex_str_too_short_errors() {
		assert!(SizedHexString::<U4>::new_from_hex_str("deadbe").is_err());
	}

	#[test]
	fn new_from_hex_str_too_long_errors() {
		assert!(SizedHexString::<U4>::new_from_hex_str("deadbeef00").is_err());
	}

	#[test]
	fn new_from_hex_str_odd_length_errors() {
		assert!(SizedHexString::<U4>::new_from_hex_str("deadbee").is_err());
	}

	#[test]
	fn new_from_hex_str_invalid_chars_errors() {
		assert!(SizedHexString::<U4>::new_from_hex_str("deadbeeg").is_err());
	}

	#[test]
	fn new_from_hex_str_empty_for_nonzero_n_errors() {
		assert!(SizedHexString::<U4>::new_from_hex_str("").is_err());
	}

	#[test]
	fn new_from_hex_str_nonempty_for_zero_n_errors() {
		assert!(SizedHexString::<U0>::new_from_hex_str("00").is_err());
	}

	// ---------- Display / Debug ----------

	#[test]
	fn display_prints_lowercase_hex() {
		let s = SizedHexString::<U4>::from([0xde, 0xad, 0xbe, 0xef]);
		assert_eq!(format!("{}", s), "deadbeef");
	}

	#[test]
	fn display_zero_length() {
		let s = SizedHexString::<U0>::from([0u8; 0]);
		assert_eq!(format!("{}", s), "");
	}

	#[test]
	fn display_pads_with_leading_zeros_per_byte() {
		// 0x01 must render as "01", not "1".
		let s = SizedHexString::<U2>::from([0x01, 0x0a]);
		assert_eq!(format!("{}", s), "010a");
	}

	#[test]
	fn debug_format() {
		let s = SizedHexString::<U4>::from([0xde, 0xad, 0xbe, 0xef]);
		assert_eq!(format!("{:?}", s), "SizedHexString(deadbeef)");
	}

	#[test]
	fn debug_zero_length() {
		let s = SizedHexString::<U0>::from([0u8; 0]);
		assert_eq!(format!("{:?}", s), "SizedHexString()");
	}

	// ---------- to_str ----------

	#[test]
	fn to_str_basic_round_trip() {
		let original = "deadbeef";
		let s = SizedHexString::<U4>::new_from_hex_str(original).unwrap();
		assert_eq!(format!("{}", s.to_str()), original);
	}

	#[test]
	fn to_str_lowercase_output_for_uppercase_input() {
		let s = SizedHexString::<U4>::new_from_hex_str("DEADBEEF").unwrap();
		assert_eq!(format!("{}", s.to_str()), "deadbeef");
	}

	#[test]
	fn to_str_zero_length() {
		let s = SizedHexString::<U0>::from([0u8; 0]);
		assert_eq!(format!("{}", s.to_str()), "");
	}

	#[test]
	fn to_str_matches_display() {
		let s = SizedHexString::<U16>::from([
			0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
			0xee, 0xff,
		]);
		assert_eq!(format!("{}", s.to_str()), format!("{}", s));
	}

	// ---------- as_slice ----------

	#[test]
	fn as_slice_returns_underlying_bytes() {
		let bytes = [0x01u8, 0x02, 0x03, 0x04];
		let s = SizedHexString::<U4>::from(bytes);
		assert_eq!(s.as_slice(), &bytes);
		assert_eq!(s.as_slice().len(), 4);
	}

	#[test]
	fn as_slice_zero_length() {
		let s = SizedHexString::<U0>::from([0u8; 0]);
		assert_eq!(s.as_slice(), &[] as &[u8]);
	}

	// ---------- AsRef<[u8; U]> ----------

	#[test]
	fn as_ref_array_returns_fixed_size_array() {
		let s = SizedHexString::<U4>::from([0xaa, 0xbb, 0xcc, 0xdd]);
		let arr: &[u8; 4] = s.as_ref();
		assert_eq!(arr, &[0xaa, 0xbb, 0xcc, 0xdd]);
	}

	#[test]
	fn as_ref_array_32_bytes() {
		let bytes = [0x42u8; 32];
		let s = SizedHexString::<U32>::from(bytes);
		let arr: &[u8; 32] = s.as_ref();
		assert_eq!(arr, &bytes);
	}

	// ---------- From conversions ----------

	#[test]
	fn from_byte_array() {
		let s: SizedHexString<U4> = [0x11u8, 0x22, 0x33, 0x44].into();
		assert_eq!(s.as_slice(), &[0x11, 0x22, 0x33, 0x44]);
	}

	#[test]
	fn from_generic_array() {
		let ga = GenericArray::<u8, U4>::from([0x55u8, 0x66, 0x77, 0x88]);
		let s: SizedHexString<U4> = ga.into();
		assert_eq!(s.as_slice(), &[0x55, 0x66, 0x77, 0x88]);
	}

	#[test]
	fn from_byte_array_zero_length() {
		let s: SizedHexString<U0> = [].into();
		assert_eq!(s.as_slice(), &[] as &[u8]);
	}

	#[test]
	fn from_array_matches_from_generic_array() {
		let arr = [0xaa, 0xbb, 0xcc, 0xdd];
		let a = SizedHexString::<U4>::from(arr);
		let b: SizedHexString<U4> = GenericArray::<u8, U4>::from(arr).into();
		assert_eq!(a, b);
	}

	// ---------- Equality / Clone / Copy / Hash ----------

	#[test]
	fn equality_same_bytes() {
		let a = SizedHexString::<U4>::from([1u8, 2, 3, 4]);
		let b = SizedHexString::<U4>::from([1u8, 2, 3, 4]);
		assert_eq!(a, b);
	}

	#[test]
	fn equality_different_bytes() {
		let a = SizedHexString::<U4>::from([1u8, 2, 3, 4]);
		let b = SizedHexString::<U4>::from([1u8, 2, 3, 5]);
		assert_ne!(a, b);
	}

	#[test]
	fn clone_produces_equal_value() {
		let a = SizedHexString::<U4>::from([0x11, 0x22, 0x33, 0x44]);
		#[allow(clippy::clone_on_copy)]
		let b = a.clone();
		assert_eq!(a, b);
	}

	#[test]
	fn copy_semantics_compile_and_preserve_value() {
		// Compiles only because Copy is implemented for U4 (whose ArrayType<u8> = [u8; 4] is Copy).
		let a = SizedHexString::<U4>::from([0x11, 0x22, 0x33, 0x44]);
		let b = a;
		assert_eq!(a, b);
	}

	#[test]
	fn hash_equal_for_equal_values() {
		let a = SizedHexString::<U4>::from([1u8, 2, 3, 4]);
		let b = SizedHexString::<U4>::from([1u8, 2, 3, 4]);
		assert_eq!(hash_of(&a), hash_of(&b));
	}

	#[test]
	fn hash_typically_differs_for_different_values() {
		// Not formally guaranteed by Hash, but expected with DefaultHasher.
		let a = SizedHexString::<U4>::from([1u8, 2, 3, 4]);
		let b = SizedHexString::<U4>::from([4u8, 3, 2, 1]);
		assert_ne!(hash_of(&a), hash_of(&b));
	}

	// ---------- Serialize / Deserialize ----------

	#[test]
	fn serialize_to_json_string_no_prefix() {
		let s = SizedHexString::<U4>::from([0xde, 0xad, 0xbe, 0xef]);
		assert_eq!(serde_json::to_string(&s).unwrap(), "\"deadbeef\"");
	}

	#[test]
	fn serialize_zero_length_to_empty_json_string() {
		let s = SizedHexString::<U0>::from([0u8; 0]);
		assert_eq!(serde_json::to_string(&s).unwrap(), "\"\"");
	}

	#[test]
	fn deserialize_from_json_string() {
		let s: SizedHexString<U4> = serde_json::from_str("\"deadbeef\"").unwrap();
		assert_eq!(s.as_slice(), &[0xde, 0xad, 0xbe, 0xef]);
	}

	#[test]
	fn deserialize_accepts_uppercase() {
		let s: SizedHexString<U4> = serde_json::from_str("\"DEADBEEF\"").unwrap();
		assert_eq!(s.as_slice(), &[0xde, 0xad, 0xbe, 0xef]);
	}

	#[test]
	fn deserialize_zero_length_from_empty_string() {
		let s: SizedHexString<U0> = serde_json::from_str("\"\"").unwrap();
		assert_eq!(s.as_slice(), &[] as &[u8]);
	}

	#[test]
	fn deserialize_rejects_too_short() {
		let r: Result<SizedHexString<U4>, _> = serde_json::from_str("\"deadbe\"");
		assert!(r.is_err());
	}

	#[test]
	fn deserialize_rejects_too_long() {
		let r: Result<SizedHexString<U4>, _> = serde_json::from_str("\"deadbeef00\"");
		assert!(r.is_err());
	}

	#[test]
	fn deserialize_rejects_odd_length() {
		let r: Result<SizedHexString<U4>, _> = serde_json::from_str("\"deadbee\"");
		assert!(r.is_err());
	}

	#[test]
	fn deserialize_rejects_non_hex_chars() {
		let r: Result<SizedHexString<U4>, _> = serde_json::from_str("\"deadbeeg\"");
		assert!(r.is_err());
	}

	#[test]
	fn deserialize_rejects_non_string_input() {
		// Visitor implements only visit_str.
		assert!(serde_json::from_str::<SizedHexString<U4>>("[222, 173, 190, 239]").is_err());
		assert!(serde_json::from_str::<SizedHexString<U4>>("12345").is_err());
		assert!(serde_json::from_str::<SizedHexString<U4>>("null").is_err());
	}

	#[test]
	fn serde_round_trip_json() {
		let original = SizedHexString::<U16>::from([
			0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
			0xee, 0xff,
		]);
		let json = serde_json::to_string(&original).unwrap();
		let parsed: SizedHexString<U16> = serde_json::from_str(&json).unwrap();
		assert_eq!(original, parsed);
	}

	#[test]
	fn serde_round_trip_zero_length() {
		let original = SizedHexString::<U0>::from([0u8; 0]);
		let json = serde_json::to_string(&original).unwrap();
		let parsed: SizedHexString<U0> = serde_json::from_str(&json).unwrap();
		assert_eq!(original, parsed);
	}

	#[test]
	fn serde_round_trip_32_bytes() {
		let original = SizedHexString::<U32>::from([0xab; 32]);
		let json = serde_json::to_string(&original).unwrap();
		assert_eq!(json, format!("\"{}\"", "ab".repeat(32)));
		let parsed: SizedHexString<U32> = serde_json::from_str(&json).unwrap();
		assert_eq!(original, parsed);
	}

	// ---------- cross-API consistency ----------

	#[test]
	fn display_to_str_and_serialize_agree() {
		let s = SizedHexString::<U4>::from([0x12, 0x34, 0x56, 0x78]);
		assert_eq!(format!("{}", s), "12345678");
		assert_eq!(format!("{}", s.to_str()), "12345678");
		assert_eq!(serde_json::to_string(&s).unwrap(), "\"12345678\"");
	}

	#[test]
	fn new_from_hex_str_then_to_str_is_identity_lowercased() {
		fn check<N>(hex_in: &str)
		where
			N: generic_array::ArrayLength + std::ops::Mul<U2>,
			typenum::Prod<N, U2>: generic_array::ArrayLength,
		{
			let parsed = SizedHexString::<N>::new_from_hex_str(hex_in).unwrap();
			assert_eq!(format!("{}", parsed.to_str()), hex_in.to_lowercase());
		}
		check::<U1>("00");
		check::<U1>("FF");
		check::<U2>("0102");
		check::<U4>("DeAdBeEf");
		check::<U8>("0123456789abcdef");
	}

	// ---------- 0x-prefix tolerance ----------

	#[test]
	fn new_from_hex_str_accepts_0x_prefix() {
		let s = SizedHexString::<U4>::new_from_hex_str("0xdeadbeef").unwrap();
		assert_eq!(s.as_slice(), &[0xde, 0xad, 0xbe, 0xef]);
	}

	#[test]
	fn new_from_hex_str_prefix_matches_no_prefix() {
		let with_prefix = SizedHexString::<U4>::new_from_hex_str("0xdeadbeef").unwrap();
		let without = SizedHexString::<U4>::new_from_hex_str("deadbeef").unwrap();
		assert_eq!(with_prefix, without);
	}

	#[test]
	fn deserialize_accepts_0x_prefix() {
		let s: SizedHexString<U4> = serde_json::from_str("\"0xdeadbeef\"").unwrap();
		assert_eq!(s.as_slice(), &[0xde, 0xad, 0xbe, 0xef]);
	}

	#[test]
	fn deserialize_prefix_matches_no_prefix() {
		let with_prefix: SizedHexString<U4> = serde_json::from_str("\"0xdeadbeef\"").unwrap();
		let without: SizedHexString<U4> = serde_json::from_str("\"deadbeef\"").unwrap();
		assert_eq!(with_prefix, without);
	}
}

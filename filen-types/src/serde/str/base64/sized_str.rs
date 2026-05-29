use std::{mem::MaybeUninit, ops::Deref};

use filen_macros::rkyv_self;
use generic_array::{ArrayLength, GenericArray};
use rand::Rng;
use rkyv::{
	bytecheck::CheckBytes,
	rancor::{Fallible, ResultExt, Source},
};
use serde::{Deserialize, Serialize};

use crate::{
	error::{ConversionError, TransparentError},
	serde::str::SizedStr,
};

// `no_check_bytes`: the archived bytes must be valid UTF-8 *and* contain only
// base64url alphabet characters — stricter than the inner `SizedStr<N>` — so the
// hand-written `CheckBytes` below is kept rather than delegating to the field.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[rkyv_self(no_check_bytes)]
pub struct SizedStrBase64Chars<N: ArrayLength>(SizedStr<N>);

unsafe impl<N: ArrayLength, C: Fallible + ?Sized> CheckBytes<C> for SizedStrBase64Chars<N>
where
	C::Error: Source,
{
	unsafe fn check_bytes(value: *const Self, context: &mut C) -> Result<(), C::Error> {
		unsafe { SizedStr::<N>::check_bytes(value.cast(), context).into_error()? };
		let bytes = unsafe { &*(value as *const SizedStr<N>) };
		match <&Self>::try_from(bytes) {
			Err(e) => Err(TransparentError::new(e)).into_error(),
			Ok(_) => Ok(()),
		}
	}
}

impl<N: ArrayLength> SizedStrBase64Chars<N> {
	pub fn ref_from_str(s: &str) -> Result<&Self, ConversionError> {
		s.try_into()
	}

	pub fn owned_from_string(s: String) -> Result<Box<Self>, ConversionError> {
		s.try_into()
	}
}

impl<N: ArrayLength> SizedStrBase64Chars<N> {
	fn find_invalid_char(s: &str) -> Option<(usize, u8)> {
		s.bytes().enumerate().find(|(_, byte)| {
			!base64::alphabet::URL_SAFE
				.as_str()
				.as_bytes()
				.contains(byte)
		})
	}
}

impl<N: ArrayLength> Serialize for SizedStrBase64Chars<N> {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		self.0.serialize(serializer)
	}
}

impl<N: ArrayLength> TryFrom<&SizedStr<N>> for &SizedStrBase64Chars<N> {
	type Error = ConversionError;

	fn try_from(value: &SizedStr<N>) -> Result<Self, Self::Error> {
		if let Some((idx, byte)) = SizedStrBase64Chars::<N>::find_invalid_char(value.as_ref()) {
			Err(ConversionError::Base64DecodeError(
				base64::DecodeError::InvalidByte(idx, byte),
			))
		} else {
			// SAFETY: SizedStringBase64Chars is #[repr(transparent)] over SizedStr,
			// and the bytes have been validated to only contain valid base64 characters.
			Ok(unsafe { &*(value as *const SizedStr<N> as *const SizedStrBase64Chars<N>) })
		}
	}
}

impl<N: ArrayLength> TryFrom<SizedStr<N>> for SizedStrBase64Chars<N> {
	type Error = ConversionError;

	fn try_from(value: SizedStr<N>) -> Result<Self, Self::Error> {
		match <&SizedStrBase64Chars<N>>::try_from(&value) {
			// SAFETY: SizedStringBase64Chars is #[repr(transparent)] over SizedStr,
			// and the bytes have been validated to only contain valid base64 characters.
			Ok(_) => Ok(unsafe {
				generic_array::const_transmute::<SizedStr<N>, SizedStrBase64Chars<N>>(value)
			}),
			Err(e) => Err(e),
		}
	}
}

impl<N: ArrayLength> TryFrom<Box<SizedStr<N>>> for Box<SizedStrBase64Chars<N>> {
	type Error = ConversionError;

	fn try_from(value: Box<SizedStr<N>>) -> Result<Self, Self::Error> {
		match <&SizedStrBase64Chars<N>>::try_from(value.as_ref()) {
			Ok(_) => {
				Ok(unsafe { Box::from_raw(Box::into_raw(value) as *mut SizedStrBase64Chars<N>) })
			}
			Err(e) => Err(e),
		}
	}
}

impl<N: ArrayLength> TryFrom<&str> for &SizedStrBase64Chars<N> {
	type Error = ConversionError;

	fn try_from(value: &str) -> Result<Self, Self::Error> {
		<&SizedStr<N>>::try_from(value).and_then(Self::try_from)
	}
}

impl<N: ArrayLength> TryFrom<String> for Box<SizedStrBase64Chars<N>> {
	type Error = ConversionError;

	fn try_from(value: String) -> Result<Self, Self::Error> {
		<Box<SizedStr<N>>>::try_from(value).and_then(Self::try_from)
	}
}

impl<'de, N: ArrayLength> Deserialize<'de> for Box<SizedStrBase64Chars<N>> {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let str = String::deserialize(deserializer)?;

		match Self::try_from(str) {
			Ok(sized_string) => Ok(sized_string),
			Err(e) => Err(serde::de::Error::custom(format!(
				"failed to deserialize SizedStringBase64Chars<{}>: {}",
				N::USIZE,
				e
			))),
		}
	}
}

impl<N: ArrayLength> std::fmt::Display for SizedStrBase64Chars<N> {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		self.deref().fmt(f)
	}
}

impl<N: ArrayLength> Deref for SizedStrBase64Chars<N> {
	type Target = str;

	fn deref(&self) -> &Self::Target {
		self.0.deref()
	}
}

impl<N: ArrayLength> SizedStrBase64Chars<N> {
	const __ASSERT_EQ_SIZE: () = assert!(
		std::mem::size_of::<GenericArray<MaybeUninit<u8>, N>>()
			== std::mem::size_of::<GenericArray<u8, N>>()
	);

	pub fn new_random(rng: &mut rand::prelude::ThreadRng) -> Self {
		let mut out_bytes = GenericArray::uninit();

		let alphabet = base64::alphabet::URL_SAFE.as_str().as_bytes();
		for slot in out_bytes.iter_mut() {
			slot.write(alphabet[rng.random_range(0..alphabet.len())]);
		}

		// SAFETY: All written bytes are valid base64 URL-safe characters,
		// so the resulting bytes form a valid SizedString.
		Self(unsafe { SizedStr::from_bytes_unchecked(GenericArray::assume_init(out_bytes)) })
	}
}

#[cfg(test)]
mod tests {
	use std::{error::Error, str::Utf8Error};

	use super::*;
	use generic_array::typenum::{U0, U1, U5, U16, U64, U256, U512, U1000, U2048, U10000};

	#[test]
	fn returns_string_of_requested_length() {
		let mut rng = rand::rng();

		fn check<N: ArrayLength>(rng: &mut rand::prelude::ThreadRng)
		where
			N::ArrayType<u8>: Copy,
		{
			let s = SizedStrBase64Chars::<N>::new_random(rng);
			assert_eq!(
				s.chars().count(),
				N::USIZE,
				"expected length {}, got {} for output {:?}",
				N::USIZE,
				s.chars().count(),
				&*s
			);
		}

		check::<U0>(&mut rng);
		check::<U1>(&mut rng);
		check::<U5>(&mut rng);
		check::<U16>(&mut rng);
		check::<U64>(&mut rng);
		check::<U256>(&mut rng);
		check::<U1000>(&mut rng);
	}

	#[test]
	fn empty_input_produces_empty_string() {
		let mut rng = rand::rng();
		let s = SizedStrBase64Chars::<U0>::new_random(&mut rng);
		assert!(s.is_empty());
	}

	#[test]
	fn only_uses_base64_alphabet_characters() {
		let mut rng = rand::rng();
		let s = SizedStrBase64Chars::<U2048>::new_random(&mut rng);
		for c in s.chars() {
			assert!(
				base64::alphabet::URL_SAFE.as_str().contains(c),
				"character {:?} is not in the standard base64 alphabet",
				c
			);
		}
	}

	#[test]
	fn does_not_include_padding_character() {
		// The standard base64 alphabet does not include '='; padding is applied
		// by encoders, not part of the alphabet itself.
		let mut rng = rand::rng();
		let s = SizedStrBase64Chars::<U2048>::new_random(&mut rng);
		assert!(
			!s.contains('='),
			"output should not contain padding char '='"
		);
	}

	#[test]
	fn output_is_ascii() {
		let mut rng = rand::rng();
		let s = SizedStrBase64Chars::<U512>::new_random(&mut rng);
		assert!(s.is_ascii());
		// Each base64 char is 1 byte in UTF-8, so byte len == char count.
		assert_eq!(s.len(), s.chars().count());
	}

	#[test]
	fn produces_different_outputs_across_calls() {
		// Probability of collision for two independent 64-char strings over a
		// 64-symbol alphabet is 64^-64 — astronomically small. A failure here
		// means something is very wrong (or extraordinarily unlucky).
		let mut rng = rand::rng();
		let a = SizedStrBase64Chars::<U64>::new_random(&mut rng);
		let b = SizedStrBase64Chars::<U64>::new_random(&mut rng);
		assert_ne!(&*a, &*b);
	}

	#[test]
	fn distribution_covers_most_of_the_alphabet() {
		// With 10_000 draws over 64 symbols, every symbol should appear with
		// overwhelming probability.
		let mut rng = rand::rng();
		let s = SizedStrBase64Chars::<U10000>::new_random(&mut rng);
		let unique: std::collections::HashSet<char> = s.chars().collect();
		assert!(
			unique.len() >= 64,
			"expected full coverage of the alphabet, only saw {} distinct chars",
			unique.len()
		);
	}

	#[test]
	fn rkyv_round_trip_ascii() {
		let original = <&SizedStrBase64Chars<U5>>::try_from("12345").unwrap();
		let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(original).unwrap();
		let decoded =
			rkyv::from_bytes::<SizedStrBase64Chars<U5>, rkyv::rancor::Error>(&bytes).unwrap();
		assert_eq!(&decoded, original);
	}

	#[test]
	fn rkyv_round_trip_zero_length() {
		let original = <&SizedStrBase64Chars<U0>>::try_from("").unwrap();
		let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(original).unwrap();
		let decoded =
			rkyv::from_bytes::<SizedStrBase64Chars<U0>, rkyv::rancor::Error>(&bytes).unwrap();
		assert_eq!(&decoded, original);
	}

	#[test]
	fn rkyv_rejects_invalid_utf8() {
		// The archived form of `SizedStrBase64Chars<U5>` is the bare 5 UTF-8 bytes (no header).
		// `0xC3` starts a 2-byte sequence but `0x28` is not a valid continuation byte.
		let invalid: [u8; 5] = [0x00, 0x00, 0xC3, 0x28, 0x00];
		let res = rkyv::from_bytes::<SizedStrBase64Chars<U5>, rkyv::rancor::BoxedError>(&invalid);
		assert!(res.is_err(), "expected UTF-8 validation to reject input");
		let err = res.unwrap_err();
		println!("rkyv error: {err}");
		let source = err.source().unwrap();

		let utf8_err = source.downcast_ref::<Utf8Error>().unwrap();

		assert_eq!(
			utf8_err.valid_up_to(),
			2,
			"expected UTF-8 error to indicate failure at the first byte"
		);
	}

	#[test]
	fn rkyv_rejects_non_base64() {
		let invalid = b"1234$";
		let res = rkyv::from_bytes::<SizedStrBase64Chars<U5>, rkyv::rancor::BoxedError>(invalid);
		assert!(res.is_err(), "expected base64 validation to reject input");
		let err = res.unwrap_err();

		let source = err.source().unwrap();
		let conv_err = source.downcast_ref::<ConversionError>().unwrap();
		match conv_err {
			ConversionError::Base64DecodeError(base64::DecodeError::InvalidByte(idx, byte)) => {
				assert_eq!(
					*idx, 4,
					"expected invalid byte index to be 4 (the position of '$')"
				);
				assert_eq!(
					*byte, b'$',
					"expected invalid byte to be b'$' (the offending character)"
				);
			}
			_ => panic!("expected a Base64DecodeError with InvalidByte variant"),
		}
	}
}

use std::{mem::MaybeUninit, ops::Deref};

use generic_array::{ArrayLength, GenericArray};
use rand::Rng;
use serde::{Deserialize, Serialize};

use crate::{error::ConversionError, serde::str::SizedStr};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SizedStrBase64Chars<N: ArrayLength>(SizedStr<N>);

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
}

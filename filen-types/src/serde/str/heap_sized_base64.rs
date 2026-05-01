use std::{borrow::Cow, mem::MaybeUninit, ops::Deref};

use generic_array::{ArrayLength, GenericArray};
use rand::Rng;
use serde::{Deserialize, Serialize};

use crate::{error::ConversionError, traits::CowHelpers};

use super::SizedString;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SizedStringBase64Chars<'a, N: ArrayLength>(SizedString<'a, N>);

impl<'a, N: ArrayLength> CowHelpers for SizedStringBase64Chars<'a, N>
where
	N::ArrayType<u8>: Copy,
{
	type CowBorrowed<'borrow>
		= SizedStringBase64Chars<'borrow, N>
	where
		Self: 'borrow;

	type CowStatic = SizedStringBase64Chars<'static, N>;

	fn as_borrowed_cow<'borrow>(&'borrow self) -> Self::CowBorrowed<'borrow>
	where
		Self: 'borrow,
	{
		SizedStringBase64Chars(self.0.as_borrowed_cow())
	}

	fn into_owned_cow(self) -> Self::CowStatic {
		SizedStringBase64Chars(self.0.into_owned_cow())
	}
}

impl<N: ArrayLength> SizedStringBase64Chars<'_, N> {
	fn find_invalid_char(s: &str) -> Option<(usize, u8)> {
		s.bytes()
			.enumerate()
			.find(|(_, byte)| !base64::alphabet::URL_SAFE.as_str().contains(*byte as char))
	}
}

impl<N: ArrayLength> Serialize for SizedStringBase64Chars<'_, N> {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		self.0.serialize(serializer)
	}
}

impl<'a, N: ArrayLength> TryFrom<SizedString<'a, N>> for SizedStringBase64Chars<'a, N> {
	type Error = ConversionError;

	fn try_from(value: SizedString<'a, N>) -> Result<Self, Self::Error> {
		if let Some((idx, byte)) = Self::find_invalid_char(value.as_ref()) {
			Err(ConversionError::Base64DecodeError(
				base64::DecodeError::InvalidByte(idx, byte),
			))
		} else {
			Ok(SizedStringBase64Chars(value))
		}
	}
}

impl<'a, N: ArrayLength> TryFrom<&'a str> for SizedStringBase64Chars<'a, N> {
	type Error = ConversionError;

	fn try_from(value: &'a str) -> Result<Self, Self::Error> {
		SizedString::try_from(value).and_then(Self::try_from)
	}
}

impl<N: ArrayLength> TryFrom<String> for SizedStringBase64Chars<'_, N> {
	type Error = ConversionError;

	fn try_from(value: String) -> Result<Self, Self::Error> {
		SizedString::try_from(value).and_then(Self::try_from)
	}
}

impl<'a, N: ArrayLength> TryFrom<Cow<'a, str>> for SizedStringBase64Chars<'a, N> {
	type Error = ConversionError;

	fn try_from(value: Cow<'a, str>) -> Result<Self, Self::Error> {
		SizedString::try_from(value).and_then(Self::try_from)
	}
}

impl<'a, N: ArrayLength> From<SizedStringBase64Chars<'a, N>> for Cow<'a, str> {
	fn from(value: SizedStringBase64Chars<'a, N>) -> Self {
		value.0.into()
	}
}

impl<'de, N: ArrayLength> Deserialize<'de> for SizedStringBase64Chars<'de, N> {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let cow = crate::serde::cow::deserialize(deserializer)?;
		match Self::try_from(cow) {
			Ok(sized_string) => Ok(sized_string),
			Err(e) => Err(serde::de::Error::custom(format!(
				"failed to deserialize SizedStringBase64Chars<{}>: {}",
				N::USIZE,
				e
			))),
		}
	}
}

impl<N: ArrayLength> std::fmt::Display for SizedStringBase64Chars<'_, N> {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		self.deref().fmt(f)
	}
}

impl<N: ArrayLength> Deref for SizedStringBase64Chars<'_, N> {
	type Target = str;

	fn deref(&self) -> &Self::Target {
		self.0.deref()
	}
}

impl<N: ArrayLength> SizedStringBase64Chars<'_, N> {
	const __ASSERT_EQ_SIZE: () = assert!(
		std::mem::size_of::<GenericArray<MaybeUninit<u8>, N>>()
			== std::mem::size_of::<GenericArray<u8, N>>()
	);

	pub fn new_random(rng: &mut rand::prelude::ThreadRng) -> Self {
		let boxed: Box<[MaybeUninit<u8>]> = Box::new_uninit_slice(N::USIZE);
		// SAFETY: length matches N::USIZE by construction
		let mut out_bytes: Box<GenericArray<MaybeUninit<u8>, N>> =
			unsafe { GenericArray::try_from_boxed_slice(boxed).unwrap_unchecked() };

		let alphabet = base64::alphabet::URL_SAFE.as_str().as_bytes();
		for slot in out_bytes.iter_mut() {
			slot.write(alphabet[rng.random_range(0..alphabet.len())]);
		}

		// SAFETY: every element was initialized in the loop above.
		// GenericArray<MaybeUninit<u8>, N> and GenericArray<u8, N> have identical layout
		// (MaybeUninit<T> is repr(transparent) over T
		let out_bytes: Box<GenericArray<u8, N>> =
			unsafe { Box::from_raw(Box::into_raw(out_bytes) as *mut _) };

		// SAFETY: All written bytes are valid base64 URL-safe characters,
		// so the resulting bytes form a valid SizedString.
		Self(unsafe { SizedString::from_bytes_unchecked(out_bytes) })
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
			let s = SizedStringBase64Chars::<N>::new_random(rng);
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
		let s = SizedStringBase64Chars::<U0>::new_random(&mut rng);
		assert!(s.is_empty());
	}

	#[test]
	fn only_uses_base64_alphabet_characters() {
		let mut rng = rand::rng();
		let s = SizedStringBase64Chars::<U2048>::new_random(&mut rng);
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
		let s = SizedStringBase64Chars::<U2048>::new_random(&mut rng);
		assert!(
			!s.contains('='),
			"output should not contain padding char '='"
		);
	}

	#[test]
	fn output_is_ascii() {
		let mut rng = rand::rng();
		let s = SizedStringBase64Chars::<U512>::new_random(&mut rng);
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
		let a = SizedStringBase64Chars::<U64>::new_random(&mut rng);
		let b = SizedStringBase64Chars::<U64>::new_random(&mut rng);
		assert_ne!(&*a, &*b);
	}

	#[test]
	fn distribution_covers_most_of_the_alphabet() {
		// With 10_000 draws over 64 symbols, every symbol should appear with
		// overwhelming probability.
		let mut rng = rand::rng();
		let s = SizedStringBase64Chars::<U10000>::new_random(&mut rng);
		let unique: std::collections::HashSet<char> = s.chars().collect();
		assert!(
			unique.len() >= 64,
			"expected full coverage of the alphabet, only saw {} distinct chars",
			unique.len()
		);
	}
}

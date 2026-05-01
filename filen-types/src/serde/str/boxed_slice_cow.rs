use std::ops::Deref;

use generic_array::{ArrayLength, GenericArray, IntoArrayLength};
use typenum::Const;

use crate::{error::ConversionError, traits::CowHelpers};

#[derive(Clone, Eq, Debug)]
pub(super) enum BoxedSliceCow<'a, N: ArrayLength> {
	Borrowed(&'a GenericArray<u8, N>),
	Owned(Box<GenericArray<u8, N>>),
}

impl<N: ArrayLength> PartialEq for BoxedSliceCow<'_, N> {
	fn eq(&self, other: &Self) -> bool {
		self.deref() == other.deref()
	}
}

impl<N: ArrayLength> std::hash::Hash for BoxedSliceCow<'_, N> {
	fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
		self.deref().hash(state);
	}
}

impl<const U: usize, N: ArrayLength> AsRef<[u8; U]> for BoxedSliceCow<'_, N>
where
	Const<U>: IntoArrayLength<ArrayLength = N>,
{
	fn as_ref(&self) -> &[u8; U] {
		let ga: &GenericArray<u8, N> = match self {
			Self::Borrowed(b) => b,
			Self::Owned(o) => o,
		};
		ga.as_ref()
	}
}

impl<N: ArrayLength> Deref for BoxedSliceCow<'_, N> {
	type Target = [u8];

	fn deref(&self) -> &Self::Target {
		match self {
			Self::Borrowed(b) => b.as_slice(),
			Self::Owned(o) => o.as_slice(),
		}
	}
}

impl<'a, N: ArrayLength> TryFrom<&'a [u8]> for BoxedSliceCow<'a, N> {
	type Error = ConversionError;

	fn try_from(value: &'a [u8]) -> Result<Self, Self::Error> {
		Ok(Self::Borrowed(
			GenericArray::try_from_slice(value)
				.map_err(|_| ConversionError::InvalidLength(value.len(), N::USIZE))?,
		))
	}
}

impl<N: ArrayLength> TryFrom<Vec<u8>> for BoxedSliceCow<'_, N> {
	type Error = ConversionError;

	fn try_from(value: Vec<u8>) -> Result<Self, Self::Error> {
		let len = value.len();
		let boxed_array = GenericArray::try_from_vec(value)
			.map_err(|_| ConversionError::InvalidLength(len, N::USIZE))?;
		Ok(Self::Owned(boxed_array))
	}
}

// need a custom impl for this type for the into_owned_cow impl
impl<'a, N: ArrayLength> CowHelpers for BoxedSliceCow<'a, N>
where
	N::ArrayType<u8>: Copy,
{
	type CowBorrowed<'borrow>
		= BoxedSliceCow<'borrow, N>
	where
		Self: 'borrow;

	type CowStatic = BoxedSliceCow<'static, N>;

	fn as_borrowed_cow<'borrow>(&'borrow self) -> Self::CowBorrowed<'borrow>
	where
		Self: 'borrow,
	{
		BoxedSliceCow::<'borrow, N>::Borrowed(match self {
			Self::Borrowed(b) => b,
			Self::Owned(o) => o.as_ref(),
		})
	}

	fn into_owned_cow(self) -> Self::CowStatic {
		BoxedSliceCow::<'static, N>::Owned(match self {
			Self::Borrowed(b) => Box::new(*b),
			Self::Owned(o) => o,
		})
	}
}

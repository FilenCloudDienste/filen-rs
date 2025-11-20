use std::borrow::Cow;

pub use filen_macros::CowHelpers;

pub trait CowHelpers {
	type CowBorrowed<'borrow>
	where
		Self: 'borrow;

	type CowStatic;

	fn as_borrowed_cow<'borrow>(&'borrow self) -> Self::CowBorrowed<'borrow>
	where
		Self: 'borrow;

	fn into_owned_cow(self) -> Self::CowStatic;
}

impl<'a, T> CowHelpers for Cow<'a, T>
where
	T: ToOwned + ?Sized,
	T::Owned: Clone + AsRef<T>,
	Cow<'static, T>: 'static,
{
	type CowBorrowed<'borrow>
		= Cow<'borrow, T>
	where
		Self: 'borrow;

	type CowStatic = Cow<'static, T>;

	#[inline]
	fn as_borrowed_cow<'borrow>(&'borrow self) -> Self::CowBorrowed<'borrow>
	where
		Self: 'borrow,
	{
		match self {
			Cow::Borrowed(b) => Cow::Borrowed(*b),
			Cow::Owned(o) => Cow::Borrowed(o.as_ref()),
		}
	}

	#[inline]
	fn into_owned_cow(self) -> Self::CowStatic {
		Cow::Owned(self.into_owned())
	}
}

impl<T> CowHelpers for Vec<T>
where
	T: CowHelpers,
{
	type CowBorrowed<'borrow>
		= Vec<T::CowBorrowed<'borrow>>
	where
		Self: 'borrow;

	type CowStatic = Vec<T::CowStatic>;

	#[inline]
	fn as_borrowed_cow<'borrow>(&'borrow self) -> Self::CowBorrowed<'borrow>
	where
		Self: 'borrow,
	{
		self.iter().map(|item| item.as_borrowed_cow()).collect()
	}

	#[inline]
	fn into_owned_cow(self) -> Self::CowStatic {
		self.into_iter().map(|item| item.into_owned_cow()).collect()
	}
}

impl<T> CowHelpers for Option<T>
where
	T: CowHelpers,
{
	type CowBorrowed<'borrow>
		= Option<T::CowBorrowed<'borrow>>
	where
		Self: 'borrow;

	type CowStatic = Option<T::CowStatic>;

	#[inline]
	fn as_borrowed_cow<'borrow>(&'borrow self) -> Self::CowBorrowed<'borrow>
	where
		Self: 'borrow,
	{
		self.as_ref().map(|item| item.as_borrowed_cow())
	}

	#[inline]
	fn into_owned_cow(self) -> Self::CowStatic {
		self.map(|item| item.into_owned_cow())
	}
}

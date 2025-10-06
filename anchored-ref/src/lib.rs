#![cfg_attr(feature = "nightly", feature(closure_lifetime_binder))]
use std::fmt::Debug;
use std::mem::ManuallyDrop;
use std::mem::MaybeUninit;

// Thanks to t.dark for outlining the approach for this crate
// and getting me started on writing it.

#[cfg(test)]
mod test;

/// # Safety
/// This trait should never be used outside this crate
/// as the transmutation must be carefully controlled.
pub unsafe trait TransmuteLifetime {
	type Borrowed<'transmute_lifetime>;

	/// # Safety
	///
	/// This should only be used inside this crate
	/// and is generally unsound for any types with a non-static lifetime.
	unsafe fn transmute_to_static(borrowed: Self::Borrowed<'_>) -> Self;

	fn transmute_from_static<'transmute_lifetime>(
		static_val: Self,
	) -> Self::Borrowed<'transmute_lifetime>;
}

#[derive(Debug)]
pub struct AnchoredRef<T, U> {
	// we need a very specific drop order: `borrowed` _before_ `owned`.
	owned: ManuallyDrop<T>,
	borrowed: ManuallyDrop<U>,
}

pub trait MapFnHelper<'a, U, U1>
where
	U: TransmuteLifetime,
	U1: TransmuteLifetime,
{
	fn call(self, u: U::Borrowed<'a>) -> U1::Borrowed<'a>;
}

impl<'a, U, U1, F> MapFnHelper<'a, U, U1> for F
where
	U: TransmuteLifetime,
	U1: TransmuteLifetime,
	F: FnOnce(U::Borrowed<'a>) -> U1::Borrowed<'a>,
{
	fn call(self, u: U::Borrowed<'a>) -> U1::Borrowed<'a> {
		self(u)
	}
}

impl<T, U: TransmuteLifetime + 'static> AnchoredRef<T, U> {
	// Creates a new CombinedStruct
	// The closure is called with a reference to the owned value
	// and must return a borrowed version of U
	pub fn new<F>(owned: T, init_borrowed: F) -> Self
	where
		F: for<'a> FnOnce(&'a T) -> U::Borrowed<'a>,
	{
		let mut combined = MaybeUninit::<Self>::uninit();
		let combined_ptr = combined.as_mut_ptr();
		// Safety:
		// - This code assumes that `deserialized` will only refer to `owned`'s heap allocation, not the inline value. We enforce this
		// by only passing a pointer to said allocation
		// - `with_raw_parts` assumes that moving `owned` does not invalidate `deserialized`. The above guarantees that.
		// - `owned` and `deserialized` are both initialized before calling `assume_init`, even if `init_deserialized` panics.
		// - Neither field of `CombinedStruct` is accessible anymore after or during the call to `with_raw_parts` thanks to the `self` parameter,
		// so we don't need to worry about aliasing references.
		unsafe {
			let owned_ptr = &raw mut (*combined_ptr).owned;
			let borrowed_ptr = &raw mut (*combined_ptr).borrowed;
			owned_ptr.write(ManuallyDrop::new(owned));
			let deserialized = init_borrowed(&*owned_ptr);
			let static_deserialized = U::transmute_to_static(deserialized);
			borrowed_ptr.write(ManuallyDrop::new(static_deserialized));
			combined.assume_init()
		}
	}

	pub fn try_new<F, E>(owned: T, init_borrowed: F) -> Result<Self, E>
	where
		F: for<'a> FnOnce(&'a T) -> Result<U::Borrowed<'a>, E>,
	{
		let mut combined = MaybeUninit::<Self>::uninit();
		let combined_ptr = combined.as_mut_ptr();
		// Safety:
		// - This code assumes that `deserialized` will only refer to `owned`'s heap allocation, not the inline value. We enforce this
		// by only passing a pointer to said allocation
		// - `with_raw_parts` assumes that moving `owned` does not invalidate `deserialized`. The above guarantees that.
		// - `owned` and `deserialized` are both initialized before calling `assume_init`, even if `init_deserialized` panics.
		// - Neither field of `CombinedStruct` is accessible anymore after or during the call to `with_raw_parts` thanks to the `self` parameter,
		// so we don't need to worry about aliasing references.
		unsafe {
			let owned_ptr = &raw mut (*combined_ptr).owned;
			let borrowed_ptr = &raw mut (*combined_ptr).borrowed;
			owned_ptr.write(ManuallyDrop::new(owned));
			let deserialized = init_borrowed(&*owned_ptr)?;
			let static_deserialized = U::transmute_to_static(deserialized);
			borrowed_ptr.write(ManuallyDrop::new(static_deserialized));
			Ok(combined.assume_init())
		}
	}

	// Gets a reference to the owned part
	pub fn owned(&self) -> &T {
		&self.owned
	}

	// Gets a reference to the borrowed part
	pub fn borrowed(&self) -> &U::Borrowed<'_> {
		let ptr = &*self.borrowed as *const U as *const U::Borrowed<'_>;
		// Safety: We can hand out a reference to U::Borrowed<'_> because U::Borrowed<'_> is guaranteed to be a valid reference
		// for the lifetime of self, since self owns U and U::Borrowed<'_> is just a reference into U.
		unsafe { &*ptr }
	}

	// Calls the closure with the borrowed part, consuming self
	// The owned part is dropped after the closure returns
	pub fn with_ref<F, R>(self, f: F) -> R
	where
		F: for<'a> FnOnce(U::Borrowed<'a>) -> R,
	{
		// Safety: we are deconstructing the tuple and assigning both T and U so they are dropped in the right order
		let (_owned, borrowed) = unsafe { self.into_parts() };
		f(TransmuteLifetime::transmute_from_static(borrowed))
	}

	/// Consumes self and maps the borrowed part to another type, returning a new CombinedStruct
	/// The owned part is unchanged
	///
	/// Unfortunately, without the rust nightly feature `closure_lifetime_binder`
	/// there is no way for a closure to correctly Map from U<'a> to a U1<'a>
	/// so in stable rust it is generally required to pass a function instead of a closure here.
	pub fn map<F, U1>(self, f: F) -> AnchoredRef<T, U1>
	where
		U1: TransmuteLifetime,
		F: for<'a> MapFnHelper<'a, U, U1>,
	{
		// Safety: we are deconstructing the tuple and assigning both T and U so they are dropped in the right order
		let (owned, borrowed) = unsafe { self.into_parts() };

		let mapped = f.call(TransmuteLifetime::transmute_from_static(borrowed));
		let static_mapped = unsafe { U1::transmute_to_static(mapped) };
		AnchoredRef {
			owned: ManuallyDrop::new(owned),
			borrowed: ManuallyDrop::new(static_mapped),
		}
	}

	// Consumes self and returns the owned part, dropping the borrowed part
	pub fn into_owned(self) -> T {
		// Safety: we are deconstructing the tuple and only returning T, so U is dropped first
		let (owned, _) = unsafe { self.into_parts() };
		owned
	}

	/// Gets the owned and borrowed parts, consuming self, only for internal use
	///
	/// # Safety
	///
	/// The returned values must be deconstructed, and `T` must not be assigned to _
	/// if the tuple is dropped directly, or `T` is assigned to _
	/// the fields will be dropped in the wrong order.
	unsafe fn into_parts(self) -> (T, U) {
		// since we take out the fields, we don't want to run drop on self
		let mut this = ManuallyDrop::new(self);
		// SAFETY: we only take out the fields after setting self to ManuallyDrop,
		// this means that the drop implementation of CombinedStruct will not be called,
		let owned = unsafe { ManuallyDrop::take(&mut this.owned) };
		let borrowed = unsafe { ManuallyDrop::take(&mut this.borrowed) };
		(owned, borrowed)
	}
}

impl<T, U> Drop for AnchoredRef<T, U> {
	fn drop(&mut self) {
		// Safety
		// This is a drop implementation. It runs at most once and nobody will ever touch `self`'s fields again.
		// It will only be executed if into_parts is not called, so both fields are always initialized.
		unsafe {
			ManuallyDrop::drop(&mut self.borrowed);
			ManuallyDrop::drop(&mut self.owned);
		}
	}
}

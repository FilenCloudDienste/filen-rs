use std::sync::{Arc, atomic::Ordering};

pub struct PathIterator<'a> {
	path: &'a str,
	split: std::str::MatchIndices<'a, char>,
	last_idx: usize,
}

impl<'a> PathIterator<'a> {
	fn new(path: &'a str) -> Self {
		let path = path.trim_start_matches('/');
		Self {
			path,
			split: path.match_indices('/'),
			last_idx: 0,
		}
	}
}

impl<'a> Iterator for PathIterator<'a> {
	type Item = (&'a str, &'a str);

	fn next(&mut self) -> Option<Self::Item> {
		match self.split.next() {
			None if self.last_idx == self.path.len() => None,
			None => {
				let slice = &self.path[self.last_idx..];
				self.last_idx = self.path.len();
				Some((slice, ""))
			}
			Some((idx, _)) => {
				let slice = &self.path[self.last_idx..idx];
				let rest = &self.path[idx + 1..];
				self.last_idx = idx + 1;
				Some((slice, rest))
			}
		}
	}
}

pub trait PathIteratorExt {
	fn path_iter(&self) -> PathIterator<'_>;
}

impl PathIteratorExt for str {
	fn path_iter(&self) -> PathIterator<'_> {
		PathIterator::new(self)
	}
}

pub async fn sleep(until: std::time::Duration) {
	#[cfg(not(target_family = "wasm"))]
	{
		tokio::time::sleep(until).await;
	}
	#[cfg(target_family = "wasm")]
	{
		wasmtimer::tokio::sleep(until).await;
	}
}

#[cfg(not(target_family = "wasm"))]
pub type MaybeSendBoxFuture<'a, T> = futures::future::BoxFuture<'a, T>;
#[cfg(target_family = "wasm")]
pub type MaybeSendBoxFuture<'a, T> = futures::future::LocalBoxFuture<'a, T>;

#[cfg(not(target_family = "wasm"))]
pub trait MaybeSendSync: Send + Sync {}
#[cfg(target_family = "wasm")]
pub trait MaybeSendSync {}

#[cfg(not(target_family = "wasm"))]
impl<T: Send + Sync> MaybeSendSync for T {}
#[cfg(target_family = "wasm")]
impl<T> MaybeSendSync for T {}

#[cfg(not(target_family = "wasm"))]
pub trait MaybeSend: Send {}
#[cfg(target_family = "wasm")]
pub trait MaybeSend {}

#[cfg(not(target_family = "wasm"))]
impl<T: Send> MaybeSend for T {}
#[cfg(target_family = "wasm")]
impl<T> MaybeSend for T {}

#[cfg(not(target_family = "wasm"))]
pub type MaybeSendCallback<'a, T> = std::sync::Arc<dyn Fn(T) + Send + Sync + 'a>;
#[cfg(target_family = "wasm")]
pub type MaybeSendCallback<'a, T> = std::rc::Rc<dyn Fn(T) + 'a>;

#[cfg(not(target_family = "wasm"))]
pub type MaybeArc<T> = std::sync::Arc<T>;
#[cfg(target_family = "wasm")]
pub type MaybeArc<T> = std::rc::Rc<T>;

#[cfg(not(target_family = "wasm"))]
pub type MaybeArcWeak<T> = std::sync::Weak<T>;
#[cfg(target_family = "wasm")]
pub type MaybeArcWeak<T> = std::rc::Weak<T>;

pub(crate) trait WasmResultExt<T> {
	fn unwrap_or_throw(self) -> T;
	fn expect_or_throw(self, msg: &str) -> T;
}

impl<T, E: std::fmt::Debug> WasmResultExt<T> for Result<T, E> {
	fn unwrap_or_throw(self) -> T {
		#[cfg(all(target_family = "wasm", target_os = "unknown"))]
		{
			use wasm_bindgen::UnwrapThrowExt;
			self.unwrap_throw()
		}
		#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
		{
			self.unwrap()
		}
	}
	fn expect_or_throw(self, msg: &str) -> T {
		#[cfg(all(target_family = "wasm", target_os = "unknown"))]
		{
			use wasm_bindgen::UnwrapThrowExt;
			self.expect_throw(msg)
		}
		#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
		{
			self.expect(msg)
		}
	}
}

impl<T> WasmResultExt<T> for Option<T> {
	fn unwrap_or_throw(self) -> T {
		#[cfg(all(target_family = "wasm", target_os = "unknown"))]
		{
			use wasm_bindgen::UnwrapThrowExt;
			self.unwrap_throw()
		}
		#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
		{
			self.unwrap()
		}
	}
	fn expect_or_throw(self, msg: &str) -> T {
		#[cfg(all(target_family = "wasm", target_os = "unknown"))]
		{
			use wasm_bindgen::UnwrapThrowExt;
			self.expect_throw(msg)
		}
		#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
		{
			self.expect(msg)
		}
	}
}

type DateTime = chrono::DateTime<chrono::Utc>;
#[cfg(feature = "uniffi")]
uniffi::custom_type!(DateTime, i64, {
	remote,
	lower: |dt: &DateTime| dt.timestamp_millis(),
	try_lift: |millis: i64| {
		chrono::DateTime::<chrono::Utc>::from_timestamp_millis(millis).ok_or_else(|| uniffi::deps::anyhow::anyhow!("invalid timestamp millis: {}", millis))
	},
});

#[cfg(feature = "multi-threaded-crypto")]
pub(crate) trait IntoMaybeParallelIterator: rayon::iter::IntoParallelIterator {
	fn into_maybe_par_iter(self) -> Self::Iter
	where
		Self: Sized,
	{
		Self::into_par_iter(self)
	}
}
#[cfg(feature = "multi-threaded-crypto")]
impl<T> IntoMaybeParallelIterator for T where T: rayon::iter::IntoParallelIterator {}

#[cfg(not(feature = "multi-threaded-crypto"))]
pub(crate) trait IntoMaybeParallelIterator: IntoIterator {
	fn into_maybe_par_iter(self) -> Self::IntoIter
	where
		Self: Sized,
	{
		Self::into_iter(self)
	}
}
#[cfg(not(feature = "multi-threaded-crypto"))]
impl<T> IntoMaybeParallelIterator for T where T: IntoIterator {}

pub(crate) struct AtomicDropCanceller {
	cancelled: Arc<std::sync::atomic::AtomicBool>,
}

impl AtomicDropCanceller {
	pub fn cancelled(&self) -> &Arc<std::sync::atomic::AtomicBool> {
		&self.cancelled
	}
}

impl Default for AtomicDropCanceller {
	fn default() -> Self {
		Self {
			cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
		}
	}
}

impl Drop for AtomicDropCanceller {
	fn drop(&mut self) {
		self.cancelled.store(true, Ordering::Relaxed);
	}
}

mod hex {
	use core::ops::Mul;

	use generic_array::{
		ArrayLength, GenericArray,
		typenum::{Const, Prod, ToUInt, U, U2},
	};

	#[derive(Clone, PartialEq, Eq, Hash, Debug)]
	pub struct HexString<const N: usize>(GenericArray<u8, Prod<U<N>, U2>>)
	where
		Const<N>: ToUInt,
		U<N>: Mul<U2> + ArrayLength,
		Prod<U<N>, U2>: ArrayLength;

	impl<const N: usize> Copy for HexString<N>
	where
		Const<N>: ToUInt,
		U<N>: Mul<U2> + ArrayLength,
		Prod<U<N>, U2>: ArrayLength,
		<Prod<U<N>, U2> as ArrayLength>::ArrayType<u8>: Copy,
	{
	}

	impl<const N: usize> core::fmt::Display for HexString<N>
	where
		Const<N>: ToUInt,
		U<N>: Mul<U2> + ArrayLength,
		Prod<U<N>, U2>: ArrayLength,
	{
		fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
			write!(f, "{}", self.as_ref())
		}
	}

	impl<const N: usize> HexString<N>
	where
		Const<N>: ToUInt,
		U<N>: Mul<U2> + ArrayLength,
		Prod<U<N>, U2>: ArrayLength,
	{
		pub fn new(bytes: &[u8; N]) -> Self {
			Self(hex_encode_sized(bytes.into()))
		}

		pub fn new_from_hex_str(hex_str: &str) -> Result<Self, faster_hex::Error> {
			if hex_str.len() != N * 2 {
				return Err(faster_hex::Error::InvalidLength(hex_str.len()));
			}

			if !faster_hex::hex_check(hex_str.as_bytes()) {
				return Err(faster_hex::Error::InvalidChar);
			}

			let mut bytes = GenericArray::<u8, Prod<U<N>, U2>>::default();

			bytes.copy_from_slice(hex_str.as_bytes());
			Ok(Self(bytes))
		}

		pub fn decoded(&self) -> [u8; N] {
			// SAFETY: the output buffer is guaranteed to be large enough for the hex decoding of the input
			// and we know the input is valid hex because it was produced by hex_encode_sized.
			unsafe { hex_decode_sized(&self.0).unwrap_unchecked() }.into()
		}
	}

	impl<const N: usize> AsRef<str> for HexString<N>
	where
		Const<N>: ToUInt,
		U<N>: Mul<U2> + ArrayLength,
		Prod<U<N>, U2>: ArrayLength,
	{
		fn as_ref(&self) -> &str {
			// SAFETY: the hex encoding is always valid UTF-8.
			unsafe { std::str::from_utf8_unchecked(&self.0) }
		}
	}

	fn hex_encode_sized<N>(input: &GenericArray<u8, N>) -> GenericArray<u8, Prod<N, U2>>
	where
		N: ArrayLength + Mul<U2>,
		Prod<N, U2>: ArrayLength,
	{
		let mut bytes = GenericArray::<u8, Prod<N, U2>>::default();
		// SAFETY: output buffer is exactly 2*N bytes, which is the required size.
		unsafe { faster_hex::hex_encode(input, &mut bytes).unwrap_unchecked() };
		bytes
	}

	fn hex_decode_sized<N>(
		input: &GenericArray<u8, Prod<N, U2>>,
	) -> Result<GenericArray<u8, N>, faster_hex::Error>
	where
		N: ArrayLength + Mul<U2>,
		Prod<N, U2>: ArrayLength,
	{
		let mut bytes = GenericArray::<u8, N>::default();
		faster_hex::hex_decode(input, &mut bytes)?;
		Ok(bytes)
	}
}

pub use hex::HexString;

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn path_iterator() {
		assert_eq!(
			"root/dir/file.txt".path_iter().collect::<Vec<_>>(),
			vec![
				("root", "dir/file.txt"),
				("dir", "file.txt"),
				("file.txt", "")
			]
		);
		assert_eq!(
			"root/dir/".path_iter().collect::<Vec<_>>(),
			vec![("root", "dir/"), ("dir", "")]
		);
		assert_eq!(
			"/root/dir/".path_iter().collect::<Vec<_>>(),
			vec![("root", "dir/"), ("dir", "")]
		);
		assert_eq!("root".path_iter().collect::<Vec<_>>(), vec![("root", "")]);
		assert_eq!("/".path_iter().collect::<Vec<_>>(), vec![]);
		assert_eq!("".path_iter().collect::<Vec<_>>(), vec![]);
	}
}

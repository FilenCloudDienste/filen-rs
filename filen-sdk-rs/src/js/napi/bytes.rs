//! Byte buffers on napi, which cross as a `Uint8Array` where ubrn has an `ArrayBuffer`: napi has
//! no owned `ArrayBuffer`.
//!
//! A `Vec<u8>` has no `Wire` impl (it would overlap `Vec<T>`'s, which writes an `Array` of
//! numbers), so the generated mirrors and wrappers convert a byte vector through [`read`] and
//! [`write`] instead.

use napi::bindgen_prelude::Uint8Array;

/// A byte vector's wire type.
// No napi export takes or returns a byte vector yet.
#[allow(dead_code)]
pub(crate) type Bytes = Uint8Array;

/// Copies the bytes out of `bytes`.
///
/// Call it on the JS thread, where `bytes` was read: napi's `Uint8Array` borrows memory JS may
/// change at any time, so it is not safely `Send` (napi marks it so anyway), and only the copy
/// may move to another thread.
// No napi export takes a byte vector yet.
#[allow(dead_code)]
pub(crate) fn read(bytes: Bytes) -> Vec<u8> {
	bytes.to_vec()
}

/// `bytes` as a `Uint8Array` that owns them.
// No napi export returns a byte vector yet.
#[allow(dead_code)]
pub(crate) fn write(bytes: Vec<u8>) -> Bytes {
	Uint8Array::new(bytes)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn bytes_round_trip_through_read_and_write() {
		assert_eq!(read(write(Vec::new())), Vec::<u8>::new());
		assert_eq!(read(write((0..33).collect())), (0..33).collect::<Vec<u8>>());
	}
}

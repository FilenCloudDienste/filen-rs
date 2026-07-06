#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

use std::{
	ffi::{CStr, CString, c_int, c_void},
	io::{Read, Seek, SeekFrom},
	marker::PhantomData,
};

use image::RgbaImage;

struct HeicContext<'a> {
	inner: *mut heif_context,
	_lifetime: PhantomData<&'a [u8]>,
}

impl HeicContext<'_> {
	fn from_slice(data: &[u8]) -> Result<Self, HeifError> {
		let context = unsafe { heif_context_alloc() };
		let result = unsafe {
			heif_context_read_from_memory_without_copy(
				context,
				data.as_ptr() as *const c_void,
				data.len(),
				std::ptr::null(),
			)
		};
		if result.code != heif_error_code_heif_error_Ok {
			return Err(HeifError::from_raw(result));
		}

		Ok(HeicContext {
			inner: context,
			_lifetime: PhantomData,
		})
	}

	fn from_file(path: &str) -> Result<HeicContext<'static>, HeifError> {
		let file_name = CString::new(path)
			.map_err(|_| HeifError::invalid_input("file path contains an interior NUL byte"))?;
		let context = unsafe { heif_context_alloc() };
		let result =
			unsafe { heif_context_read_from_file(context, file_name.as_ptr(), std::ptr::null()) };
		if result.code != heif_error_code_heif_error_Ok {
			return Err(HeifError::from_raw(result));
		}

		Ok(HeicContext {
			inner: context,
			_lifetime: PhantomData,
		})
	}

	fn from_reader<T: Read + Seek>(reader: &mut HeifReader<T>) -> Result<Self, HeifError> {
		let context = unsafe { heif_context_alloc() };
		let result = unsafe {
			heif_context_read_from_reader(
				context,
				&reader.as_heif_reader(),
				reader as *mut _ as *mut c_void,
				std::ptr::null(),
			)
		};
		if result.code != heif_error_code_heif_error_Ok {
			return Err(HeifError::from_raw(result));
		}

		Ok(HeicContext {
			inner: context,
			_lifetime: PhantomData,
		})
	}
}

impl Drop for HeicContext<'_> {
	fn drop(&mut self) {
		unsafe { heif_context_free(self.inner) };
	}
}

struct ImageHandle<'a> {
	inner: *mut heif_image_handle,
	_lifetime: PhantomData<&'a HeicContext<'a>>,
}

impl ImageHandle<'_> {
	fn new(ctx: &HeicContext) -> Result<Self, HeifError> {
		let mut handle = std::ptr::null_mut();
		let result = unsafe { heif_context_get_primary_image_handle(ctx.inner, &mut handle) };
		if result.code != heif_error_code_heif_error_Ok {
			// copy the message while `ctx`, which owns the buffer it points into, is alive
			return Err(HeifError::from_raw(result));
		}

		Ok(ImageHandle {
			inner: handle,
			_lifetime: PhantomData,
		})
	}
}

impl Drop for ImageHandle<'_> {
	fn drop(&mut self) {
		unsafe { heif_image_handle_release(self.inner) };
	}
}

struct OutImage<'a> {
	inner: *mut heif_image,
	_lifetime: PhantomData<&'a ImageHandle<'a>>,
}

impl OutImage<'_> {
	fn new(handle: &ImageHandle) -> Result<Self, HeifError> {
		let mut heif_image_ptr = std::ptr::null_mut();
		let result = unsafe {
			heif_decode_image(
				handle.inner,
				(&mut heif_image_ptr) as *mut *mut heif_image,
				heif_colorspace_heif_colorspace_RGB,
				heif_chroma_heif_chroma_interleaved_RGBA,
				std::ptr::null(),
			)
		};

		if result.code != heif_error_code_heif_error_Ok {
			// copy the message while `handle`, which owns the buffer it points into, is alive
			return Err(HeifError::from_raw(result));
		}

		Ok(OutImage {
			inner: heif_image_ptr,
			_lifetime: PhantomData,
		})
	}

	fn make_rgba(&self) -> Result<RgbaImage, HeifError> {
		let mut stride = 0usize;
		let plane_data = unsafe {
			heif_image_get_plane_readonly2(
				self.inner,
				heif_channel_heif_channel_interleaved,
				&mut stride as *mut usize,
			)
		};

		let width =
			unsafe { heif_image_get_width(self.inner, heif_channel_heif_channel_interleaved) };
		let height =
			unsafe { heif_image_get_height(self.inner, heif_channel_heif_channel_interleaved) };

		if plane_data.is_null() {
			return Err(HeifError::invalid_decoded_image());
		}
		let layout = validate_rgba_layout(width, height, stride)
			.ok_or_else(HeifError::invalid_decoded_image)?;

		let mut rgba_data = Vec::with_capacity(layout.capacity);

		for y in 0..layout.height {
			let row_start = y * stride;
			rgba_data.extend_from_slice(unsafe {
				std::slice::from_raw_parts(plane_data.add(row_start), layout.row_bytes)
			});
		}

		image::RgbaImage::from_vec(width as u32, height as u32, rgba_data)
			.ok_or_else(HeifError::invalid_decoded_image)
	}
}

impl Drop for OutImage<'_> {
	fn drop(&mut self) {
		unsafe { heif_image_release(self.inner) };
	}
}

struct RgbaLayout {
	height: usize,
	row_bytes: usize,
	capacity: usize,
}

fn validate_rgba_layout(width: c_int, height: c_int, stride: usize) -> Option<RgbaLayout> {
	if width <= 0 || height <= 0 {
		return None;
	}
	let width = width as usize;
	let height = height as usize;
	let row_bytes = width.checked_mul(4)?;
	if stride < row_bytes {
		return None;
	}
	let capacity = row_bytes.checked_mul(height)?;
	// the end of the last row must be addressable without overflowing usize
	let last_row_end = (height - 1).checked_mul(stride)?.checked_add(row_bytes)?;
	// Vec allocations and raw-pointer offsets are limited to isize::MAX bytes
	if capacity > isize::MAX as usize || last_row_end > isize::MAX as usize {
		return None;
	}
	Some(RgbaLayout {
		height,
		row_bytes,
		capacity,
	})
}

pub fn try_get_rgba_image_from_slice(data: &[u8]) -> Result<RgbaImage, HeifError> {
	let context = HeicContext::from_slice(data)?;
	let image_handle = ImageHandle::new(&context)?;
	let out_image = OutImage::new(&image_handle)?;
	out_image.make_rgba()
}

pub fn try_get_rgba_image_from_file(path: &str) -> Result<RgbaImage, HeifError> {
	let context = HeicContext::from_file(path)?;
	let image_handle = ImageHandle::new(&context)?;
	let out_image = OutImage::new(&image_handle)?;
	out_image.make_rgba()
}

pub fn try_get_rgba_image_from_reader<T: Read + Seek>(
	reader: T,
	file_size: u64,
) -> Result<RgbaImage, HeifError> {
	let mut heif_reader = HeifReader::new(reader, file_size);
	let context = HeicContext::from_reader(&mut heif_reader)?;
	let image_handle = ImageHandle::new(&context)?;
	let out_image = OutImage::new(&image_handle)?;
	out_image.make_rgba()
}

struct HeifReader<T>
where
	T: Read + Seek,
{
	inner: T,
	file_size: u64,
}

impl<T: Read + Seek> HeifReader<T> {
	fn new(inner: T, file_size: u64) -> Self {
		HeifReader { inner, file_size }
	}

	fn as_heif_reader(&mut self) -> heif_reader {
		heif_reader {
			reader_api_version: 1,
			get_position: Some(get_position_impl::<T>),
			read: Some(read_impl::<T>),
			seek: Some(seek_impl::<T>),
			wait_for_file_size: Some(wait_for_file_size_impl::<T>),
			request_range: None,
			preload_range_hint: None,
			release_file_range: None,
			release_error_msg: None,
		}
	}

	fn get_position(&mut self) -> Result<u64, std::io::Error> {
		self.inner.stream_position()
	}

	// Helper method to read data
	fn read(&mut self, buffer: &mut [u8]) -> Result<usize, std::io::Error> {
		self.inner.read(buffer)
	}

	// Helper method to seek
	fn seek(&mut self, position: i64) -> Result<(), std::io::Error> {
		self.inner.seek(SeekFrom::Start(position as u64))?;
		Ok(())
	}

	// Helper method to check if we can read up to target_size
	fn wait_for_file_size(&mut self, target_size: i64) -> heif_reader_grow_status {
		if target_size as u64 <= self.file_size {
			heif_reader_grow_status_heif_reader_grow_status_size_reached
		} else {
			heif_reader_grow_status_heif_reader_grow_status_size_beyond_eof
		}
	}
}

unsafe extern "C" fn get_position_impl<T: Read + Seek>(userdata: *mut c_void) -> i64 {
	let reader = unsafe { &mut *(userdata as *mut HeifReader<T>) };
	reader.get_position().map(|pos| pos as i64).unwrap_or(-1)
}

unsafe extern "C" fn read_impl<T: Read + Seek>(
	data: *mut c_void,
	size: usize,
	userdata: *mut c_void,
) -> c_int {
	// from_raw_parts_mut requires a non-null pointer even for size == 0;
	// libheif treats any non-zero return as a read failure
	if data.is_null() {
		return -1;
	}
	let reader = unsafe { &mut *(userdata as *mut HeifReader<T>) };
	let buffer = unsafe { std::slice::from_raw_parts_mut(data as *mut u8, size) };

	match reader.read(buffer) {
		Ok(bytes_read) => {
			// Fill remaining buffer with zeros if we read less than requested
			if bytes_read < size {
				buffer[bytes_read..].fill(0);
			}
			0 // Success
		}
		Err(_) => -1, // Error
	}
}

unsafe extern "C" fn seek_impl<T: Read + Seek>(position: i64, userdata: *mut c_void) -> c_int {
	let reader = unsafe { &mut *(userdata as *mut HeifReader<T>) };
	match reader.seek(position) {
		Ok(_) => 0,   // Success
		Err(_) => -1, // Error
	}
}

unsafe extern "C" fn wait_for_file_size_impl<T: Read + Seek>(
	target_size: i64,
	userdata: *mut c_void,
) -> heif_reader_grow_status {
	let reader = unsafe { &mut *(userdata as *mut HeifReader<T>) };
	reader.wait_for_file_size(target_size)
}

#[derive(Debug)]
pub struct HeifError {
	code: heif_error_code,
	#[allow(dead_code)] // diagnostic detail, surfaced via the Debug impl
	subcode: heif_suberror_code,
	message: String,
}

impl HeifError {
	/// Copies the message eagerly: libheif error messages point into a buffer
	/// owned by the producing context/handle, so the pointer dangles once that
	/// object is freed. Call this while the producing object is still alive.
	fn from_raw(error: heif_error) -> Self {
		let message = if error.message.is_null() {
			String::from("unknown error")
		} else {
			unsafe { CStr::from_ptr(error.message) }
				.to_string_lossy()
				.into_owned()
		};
		HeifError {
			code: error.code,
			subcode: error.subcode,
			message,
		}
	}

	fn invalid_input(message: &str) -> Self {
		HeifError {
			code: heif_error_code_heif_error_Invalid_input,
			subcode: heif_suberror_code_heif_suberror_Unspecified,
			message: String::from(message),
		}
	}

	fn invalid_decoded_image() -> Self {
		Self::invalid_input("decoded image has invalid plane, dimensions, or stride")
	}
}

impl std::fmt::Display for HeifError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(
			f,
			"heif error: code: {}, message: {}",
			self.code, self.message
		)
	}
}

impl std::error::Error for HeifError {}

// #[cfg(test)]
// mod tests {
// 	use super::*;

// 	const TEST_HEIC_FILE: &str = "/Users/end/Documents/tmp/image1.heic"; // Update this path to a valid HEIC file for testing
// 	const TEST_OUTPUT_DIR: &str = "/Users/end/Documents/tmp/"; // Update this path to a valid output directory

// 	// very basic tests for now

// 	#[test]
// 	fn test_reader() {
// 		let heic_file = std::fs::File::open(TEST_HEIC_FILE).unwrap();
// 		let file_size = heic_file.metadata().unwrap().len();
// 		let image = try_get_rgba_image_from_reader(heic_file, file_size).unwrap();
// 		let mut file = std::fs::File::create(format!("{TEST_OUTPUT_DIR}/from_reader.png")).unwrap();
// 		image.write_to(&mut file, image::ImageFormat::Png).unwrap();
// 	}

// 	#[test]
// 	fn test_file() {
// 		let image = try_get_rgba_image_from_file(TEST_HEIC_FILE).unwrap();
// 		let mut file = std::fs::File::create(format!("{TEST_OUTPUT_DIR}/from_file.png")).unwrap();
// 		image.write_to(&mut file, image::ImageFormat::Png).unwrap();
// 	}

// 	#[test]
// 	fn test_slice() {
// 		let heic_data = std::fs::read(TEST_HEIC_FILE).unwrap();
// 		let image = try_get_rgba_image_from_slice(&heic_data).unwrap();
// 		let mut file = std::fs::File::create(format!("{TEST_OUTPUT_DIR}/from_slice.png")).unwrap();
// 		image.write_to(&mut file, image::ImageFormat::Png).unwrap();
// 	}
// }

#[cfg(test)]
mod layout_tests {
	use super::validate_rgba_layout;

	#[test]
	fn rejects_non_positive_dimensions() {
		assert!(validate_rgba_layout(0, 10, 40).is_none());
		assert!(validate_rgba_layout(10, 0, 40).is_none());
		assert!(validate_rgba_layout(-1, 10, 40).is_none());
		assert!(validate_rgba_layout(10, -1, 40).is_none());
		assert!(validate_rgba_layout(i32::MIN, i32::MIN, usize::MAX).is_none());
	}

	#[test]
	fn rejects_stride_smaller_than_row() {
		assert!(validate_rgba_layout(10, 10, 39).is_none());
		assert!(validate_rgba_layout(10, 10, 0).is_none());
	}

	#[test]
	fn rejects_row_offset_overflow() {
		assert!(validate_rgba_layout(2, 2, usize::MAX).is_none());
		assert!(validate_rgba_layout(i32::MAX, i32::MAX, usize::MAX).is_none());
	}

	#[test]
	fn accepts_dimensions_whose_byte_size_overflows_i32() {
		let side = 23_171_i32;
		let layout = validate_rgba_layout(side, side, side as usize * 4).unwrap();
		assert_eq!(layout.height, side as usize);
		assert_eq!(layout.row_bytes, side as usize * 4);
		assert_eq!(layout.capacity, side as usize * side as usize * 4);
	}

	#[test]
	fn accepts_padded_stride() {
		let layout = validate_rgba_layout(3, 2, 16).unwrap();
		assert_eq!(layout.height, 2);
		assert_eq!(layout.row_bytes, 12);
		assert_eq!(layout.capacity, 24);
	}

	#[test]
	fn rejects_capacity_exceeding_isize_max() {
		// width == height == i32::MAX with an unpadded stride: the byte counts
		// still fit in usize (so the checked_mul overflow guards above don't
		// trigger), but the resulting capacity exceeds isize::MAX, which
		// Vec::with_capacity cannot allocate.
		let side = i32::MAX;
		let row_bytes = side as usize * 4;
		assert!((row_bytes * side as usize) > isize::MAX as usize);
		assert!(validate_rgba_layout(side, side, row_bytes).is_none());
	}
}

#[cfg(test)]
mod api_tests {
	use std::io::Cursor;

	use super::*;

	#[test]
	fn from_file_rejects_path_with_interior_nul() {
		let path = "does/not/exist\0evil.heic";
		let err = try_get_rgba_image_from_file(path).unwrap_err();
		// must fail before any filesystem access is attempted
		assert!(err.to_string().contains("interior NUL byte"));
	}

	#[test]
	fn read_impl_rejects_null_data_pointer() {
		let mut reader = HeifReader::new(Cursor::new(Vec::<u8>::new()), 0);
		let result = unsafe {
			read_impl::<Cursor<Vec<u8>>>(
				std::ptr::null_mut(),
				4,
				&mut reader as *mut _ as *mut c_void,
			)
		};
		assert_eq!(result, -1);
	}
}

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
			return Err(HeifError { inner: result });
		}

		Ok(HeicContext {
			inner: context,
			_lifetime: PhantomData,
		})
	}

	fn from_file(path: &str) -> Result<HeicContext<'static>, HeifError> {
		let context = unsafe { heif_context_alloc() };
		let file_name: CString = CString::new(path).unwrap();
		let result =
			unsafe { heif_context_read_from_file(context, file_name.as_ptr(), std::ptr::null()) };
		if result.code != heif_error_code_heif_error_Ok {
			return Err(HeifError { inner: result });
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
			return Err(HeifError { inner: result });
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
			return Err(HeifError { inner: result });
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
			return Err(HeifError { inner: result });
		}

		Ok(OutImage {
			inner: heif_image_ptr,
			_lifetime: PhantomData,
		})
	}

	fn make_rgba(&self) -> Option<RgbaImage> {
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

		let mut rgba_data = Vec::with_capacity((width * height * 4) as usize);

		for y in 0..height {
			let row_start = (y as usize) * stride;
			rgba_data.extend_from_slice(unsafe {
				std::slice::from_raw_parts(plane_data.add(row_start), width as usize * 4)
			});
		}

		image::RgbaImage::from_vec(width as u32, height as u32, rgba_data)
	}
}

impl Drop for OutImage<'_> {
	fn drop(&mut self) {
		unsafe { heif_image_release(self.inner) };
	}
}

pub fn try_get_rgba_image_from_slice(data: &[u8]) -> Result<RgbaImage, HeifError> {
	let context = HeicContext::from_slice(data)?;
	let image_handle = ImageHandle::new(&context)?;
	let out_image = OutImage::new(&image_handle)?;
	Ok(out_image.make_rgba().unwrap())
}

pub fn try_get_rgba_image_from_file(path: &str) -> Result<RgbaImage, HeifError> {
	let context = HeicContext::from_file(path)?;
	let image_handle = ImageHandle::new(&context)?;
	let out_image = OutImage::new(&image_handle)?;
	Ok(out_image.make_rgba().unwrap())
}

pub fn try_get_rgba_image_from_reader<T: Read + Seek>(
	reader: T,
	file_size: u64,
) -> Result<RgbaImage, HeifError> {
	let mut heif_reader = HeifReader::new(reader, file_size);
	let context = HeicContext::from_reader(&mut heif_reader)?;
	let image_handle = ImageHandle::new(&context)?;
	let out_image = OutImage::new(&image_handle)?;
	Ok(out_image.make_rgba().unwrap())
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
	inner: heif_error,
}

impl std::fmt::Display for HeifError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(
			f,
			"heif error: code: {}, message: {}",
			self.inner.code,
			unsafe { CStr::from_ptr(self.inner.message) }
				.to_str()
				.unwrap_or("unknown error")
		)
	}
}
impl std::error::Error for HeifError {}

#[cfg(test)]
mod tests {
	use super::*;

	const TEST_HEIC_FILE: &str = "/path/to/your/test.heic"; // Update this path to a valid HEIC file for testing
	const TEST_OUTPUT_DIR: &str = "/path/to/output/"; // Update this path to a valid output directory

	// very basic tests for now

	#[test]
	fn test_reader() {
		let heic_file = std::fs::File::open(TEST_HEIC_FILE).unwrap();
		let file_size = heic_file.metadata().unwrap().len();
		let image = try_get_rgba_image_from_reader(heic_file, file_size).unwrap();
		let mut file = std::fs::File::create(format!("{TEST_OUTPUT_DIR}/from_reader.png")).unwrap();
		image.write_to(&mut file, image::ImageFormat::Png).unwrap();
	}

	#[test]
	fn test_file() {
		let image = try_get_rgba_image_from_file(TEST_HEIC_FILE).unwrap();
		let mut file = std::fs::File::create(format!("{TEST_OUTPUT_DIR}/from_file.png")).unwrap();
		image.write_to(&mut file, image::ImageFormat::Png).unwrap();
	}

	#[test]
	fn test_slice() {
		let heic_data = std::fs::read(TEST_HEIC_FILE).unwrap();
		let image = try_get_rgba_image_from_slice(&heic_data).unwrap();
		let mut file = std::fs::File::create(format!("{TEST_OUTPUT_DIR}/from_slice.png")).unwrap();
		image.write_to(&mut file, image::ImageFormat::Png).unwrap();
	}
}

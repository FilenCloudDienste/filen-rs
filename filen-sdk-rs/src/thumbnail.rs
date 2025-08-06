use std::io::{BufRead, Seek, Write};

use image::{DynamicImage, ImageReader, codecs::webp::WebPEncoder, imageops::FilterType};

use crate::error::Error;

pub fn make_thumbnail<R, W>(
	mime: Option<&str>,
	_image_file_size: u64,
	image_reader: R,
	target_width: u32,
	target_height: u32,
	out: &mut W,
) -> Result<(u32, u32), Error>
where
	R: BufRead + Seek,
	W: Write,
{
	let should_use_heic = cfg!(feature = "heic") && (mime == Some("image/heic") || mime == Some("image/heif"));
	let img = if should_use_heic {
		#[cfg(feature = "heic")]
		{
			DynamicImage::ImageRgba8(heic_decoder::try_get_rgba_image_from_reader(
				image_reader,
				_image_file_size,
			)?)
		}
		#[cfg(not(feature = "heic"))]
		{
			// heic check above will prevent this from being called
			unsafe { std::hint::unreachable_unchecked() }
		}
	} else {
		let reader = ImageReader::new(image_reader).with_guessed_format()?;
		let img: DynamicImage = reader.decode()?;
		img
	};
	let created_width = target_width.min(img.width());
	let created_height = target_height.min(img.height());
	let thumbnail = img.resize_to_fill(created_width, created_height, FilterType::CatmullRom);
	let encoder = WebPEncoder::new_lossless(out);
	thumbnail.write_with_encoder(encoder)?;
	Ok((created_width, created_height))
}

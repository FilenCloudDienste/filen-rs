use std::io::{BufRead, Seek, Write};

use image::{ImageError, ImageReader, codecs::webp::WebPEncoder, imageops::FilterType};

pub fn make_thumbnail<R, W>(
	image_reader: R,
	target_width: u32,
	target_height: u32,
	out: &mut W,
) -> Result<(u32, u32), ImageError>
where
	R: BufRead + Seek,
	W: Write,
{
	let reader = ImageReader::new(image_reader).with_guessed_format()?;
	let img: image::DynamicImage = reader.decode()?;
	let created_width = target_width.min(img.width());
	let created_height = target_height.min(img.height());
	let thumbnail = img.resize_to_fill(created_width, created_height, FilterType::CatmullRom);
	let encoder = WebPEncoder::new_lossless(out);
	thumbnail.write_with_encoder(encoder)?;
	Ok((created_width, created_height))
}

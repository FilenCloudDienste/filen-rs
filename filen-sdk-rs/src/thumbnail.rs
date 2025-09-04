use std::io::{BufRead, Seek, Write};

use image::{DynamicImage, ImageReader, codecs::webp::WebPEncoder, imageops::FilterType};

use crate::{
	ErrorKind,
	auth::Client,
	error::{Error, MetadataWasNotDecryptedError},
	fs::file::{RemoteFile, traits::HasFileInfo},
};

const SUPPORTED_THUMBNAIL_MIME_TYPES: &[&str] = &[
	#[cfg(feature = "avif-decoder")]
	"image/avif",
	#[cfg(feature = "heif-decoder")]
	"image/heic",
	#[cfg(feature = "heif-decoder")]
	"image/heif",
	"image/jpeg",
	"image/gif",
	"image/png",
	"image/tiff",
	"image/webp",
	"image/qoi",
	"image/x-qoi",
];

pub fn is_supported_thumbnail_mime(mime: &str) -> bool {
	SUPPORTED_THUMBNAIL_MIME_TYPES.contains(&mime)
}

impl Client {
	pub async fn make_thumbnail_in_memory(
		&self,
		file: &RemoteFile,
		max_width: u32,
		max_height: u32,
	) -> Result<DynamicImage, Error> {
		let mime = file.mime().ok_or(MetadataWasNotDecryptedError)?;
		if !is_supported_thumbnail_mime(mime) {
			return Err(Error::custom(
				ErrorKind::ImageError,
				format!("unsupported thumbnail mime type: {mime}"),
			));
		}
		let image_data = self.download_file(file).await?;

		let image = if mime == "image/heic" || mime == "image/heif" {
			#[cfg(feature = "heif-decoder")]
			{
				DynamicImage::ImageRgba8(heif_decoder::try_get_rgba_image_from_slice(&image_data)?)
			}
			#[cfg(not(feature = "heif-decoder"))]
			{
				unreachable!(
					"heif/heic support not enabled, should be handled by is_supported_thumbnail_mime"
				)
			}
		} else {
			image::load_from_memory(&image_data)?
		};

		Ok(image.resize(max_width, max_height, FilterType::CatmullRom))
	}
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod js_impls {
	use serde::{Deserialize, Serialize};
	use serde_bytes::ByteBuf;
	use tsify::Tsify;
	use wasm_bindgen::prelude::wasm_bindgen;

	use crate::{Error, auth::Client, fs::file::RemoteFile, js::File};

	#[derive(Deserialize, Tsify)]
	#[serde(rename_all = "camelCase")]
	#[tsify(from_wasm_abi)]
	pub struct MakeThumbnailInMemoryParams {
		pub file: File,
		pub max_width: u32,
		pub max_height: u32,
	}

	#[derive(Serialize, Tsify)]
	#[serde(rename_all = "camelCase")]
	#[tsify(into_wasm_abi)]
	pub struct MakeThumbnailInMemoryResult {
		pub image_data: ByteBuf,
		pub width: u32,
		pub height: u32,
	}

	#[wasm_bindgen]
	impl Client {
		#[wasm_bindgen(js_name = "makeThumbnailInMemory")]
		pub async fn make_thumbnail_in_memory_js(
			&self,
			params: MakeThumbnailInMemoryParams,
		) -> Result<Option<MakeThumbnailInMemoryResult>, Error> {
			let image = match self
				.make_thumbnail_in_memory(
					&RemoteFile::try_from(params.file)?,
					params.max_width,
					params.max_height,
				)
				.await
			{
				Ok(image) => image,
				Err(e) => {
					log::warn!("failed to create thumbnail: {}", e);
					return Ok(None);
				}
			};
			let width = image.width();
			let height = image.height();

			Ok(Some(MakeThumbnailInMemoryResult {
				image_data: ByteBuf::from(image.into_rgba8().into_vec()),
				width,
				height,
			}))
		}
	}
}

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
	let should_use_heic = cfg!(feature = "heif-decoder")
		&& (mime == Some("image/heic") || mime == Some("image/heif"));
	let img = if should_use_heic {
		#[cfg(feature = "heif-decoder")]
		{
			DynamicImage::ImageRgba8(heif_decoder::try_get_rgba_image_from_reader(
				image_reader,
				_image_file_size,
			)?)
		}
		#[cfg(not(feature = "heif-decoder"))]
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

//! Thumbnails straight off a remote file's chunks, against the live backend.
//! The fixture is generated here (no binary files): a JPEG of several chunks
//! with no embedded preview, so the pipeline has to read it end to end — the
//! path that streams ahead once the decode commits.

use std::io::Cursor;

use filen_macros::shared_test_runtime;
use filen_sdk_rs::{
	fs::{HasUUID, file::traits::HasFileInfo},
	thumbnail::{
		RemoteChunkSource, ThumbSource, ThumbnailFit, ThumbnailOutcome, make_thumbnail_from_source,
	},
};
use image::{ImageFormat, RgbImage};

/// Deterministic noise over a gradient: the one thing a JPEG encoder cannot
/// compress away, so the bytes span several chunks at a modest size.
fn noisy_jpeg(width: u32, height: u32) -> Vec<u8> {
	let mut state = 0x2545_f491_u32;
	let image = RgbImage::from_fn(width, height, |x, y| {
		state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
		let noise = (state >> 24) as u8;
		image::Rgb([
			((x % 256) as u8).wrapping_add(noise),
			((y % 256) as u8).wrapping_add(noise),
			noise,
		])
	});
	let mut bytes = Vec::new();
	image
		.write_to(&mut Cursor::new(&mut bytes), ImageFormat::Jpeg)
		.unwrap();
	bytes
}

#[shared_test_runtime]
async fn a_multi_chunk_remote_jpeg_thumbnails_through_the_streaming_source() {
	let bytes = noisy_jpeg(3000, 2000);
	assert!(
		bytes.len() > 3 * 1024 * 1024,
		"the fixture must span several chunks, got {} bytes",
		bytes.len()
	);
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = resources.client.clone();
	let file = client
		.make_file_builder("streamed.jpg", resources.dir.uuid())
		.unwrap();
	let file = client.upload_file(file, &bytes).await.unwrap();

	let spec = client.thumbnails().spec_remote(256, 256, file.size());
	assert!(spec.allow_full_decode);
	let source = RemoteChunkSource::new(
		client.clone(),
		file.clone(),
		tokio::runtime::Handle::current(),
		None,
	);
	let (outcome, webp) = tokio::task::spawn_blocking(move || {
		let mut webp = Vec::new();
		let outcome = make_thumbnail_from_source(
			Box::new(source),
			&spec,
			ThumbnailFit::Contain,
			None,
			&mut webp,
		)?;
		Ok::<_, filen_sdk_rs::error::Error>((outcome, webp))
	})
	.await
	.unwrap()
	.unwrap();

	let ThumbnailOutcome::Thumbnail(info) = outcome else {
		panic!("expected a thumbnail, got {outcome:?}");
	};
	// No embedded preview exists, so this went through the full decode: chunk
	// 0 fetched on demand for the header, the rest streamed. A misordered or
	// corrupted chunk would have failed the JPEG decode outright.
	assert_eq!(info.source, ThumbSource::Decoded);
	assert_eq!((info.width, info.height), (256, 171));
	assert!(!webp.is_empty());
}

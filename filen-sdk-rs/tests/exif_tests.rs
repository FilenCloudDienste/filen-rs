use chrono::{DateTime, TimeZone, Utc};
use filen_macros::shared_test_runtime;
use filen_sdk_rs::fs::{HasName, HasUUID};

/// JPEG with `EXIF:DateTimeOriginal = 2020:06:15 10:30:00`
/// (`CreateDate` and `ModifyDate` also set to the same value).
const EXIF_SAMPLE_JPEG: &[u8] = include_bytes!("fixtures/exif_sample.jpg");

/// `DateTimeOriginal` value baked into `EXIF_SAMPLE_JPEG`.
fn known_exif_time() -> DateTime<Utc> {
	Utc.with_ymd_and_hms(2020, 6, 15, 10, 30, 0).unwrap()
}

/// Returns the `created` time from a `RemoteFile` after re-fetching it
/// through a decoded `FileMeta`. Mirrors how the SDK exposes it post-upload.
fn file_created(file: &filen_sdk_rs::fs::file::RemoteFile) -> Option<DateTime<Utc>> {
	use filen_sdk_rs::fs::file::meta::FileMeta;
	match &file.meta {
		FileMeta::Decoded(d) => d.created,
		FileMeta::DecryptedRaw(_)
		| FileMeta::DecryptedUTF8(_)
		| FileMeta::Encrypted(_)
		| FileMeta::RSAEncrypted(_) => None,
	}
}

// -- Self-test: confirms the fixture's EXIF actually parses with nom-exif, so
// failures in the integration tests below can be attributed to the SDK and not
// to a broken fixture. Runs without credentials. ------------------------------

#[tokio::test]
async fn fixture_produces_parseable_exif() {
	use nom_exif::{AsyncMediaSource, EntryValue, Exif, ExifTag, MediaParser};
	use tokio::io::BufReader;

	let known = known_exif_time();
	let reader = BufReader::new(std::io::Cursor::new(EXIF_SAMPLE_JPEG));
	let ms = AsyncMediaSource::unseekable(reader)
		.await
		.expect("AsyncMediaSource");
	let mut parser = MediaParser::new();
	let iter = parser.parse_exif_async(ms).await.expect("parse_exif");
	let exif: Exif = iter.into();
	let dto = exif
		.get(ExifTag::DateTimeOriginal)
		.expect("DateTimeOriginal present");
	match dto {
		EntryValue::DateTime(dt) => {
			assert_eq!(dt.with_timezone(&Utc), known, "fixture date mismatch");
		}
		EntryValue::NaiveDateTime(ndt) => {
			assert_eq!(ndt.and_utc(), known, "fixture date mismatch");
		}
		other => panic!("unexpected EntryValue variant: {:?}", other),
	}
}

// -- Integration tests against a live test account. ---------------------------

#[shared_test_runtime]
async fn exif_default_overrides_created() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let dir = &resources.dir;

	let builder = client
		.make_file_builder("exif_override.jpg", dir.uuid())
		.unwrap();
	let file = client.upload_file(builder, EXIF_SAMPLE_JPEG).await.unwrap();
	assert_eq!(
		file_created(&file),
		Some(known_exif_time()),
		"EXIF DateTimeOriginal should override default created time"
	);
}

#[shared_test_runtime]
async fn exif_no_exif_flag_skips_parsing() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let dir = &resources.dir;

	let builder = client
		.make_file_builder("exif_skipped.jpg", dir.uuid())
		.unwrap()
		.no_exif();
	let file = client.upload_file(builder, EXIF_SAMPLE_JPEG).await.unwrap();
	let created = file_created(&file).expect("created decoded");
	assert!(
		created != known_exif_time(),
		"with .no_exif() the EXIF time must NOT be applied (got {created})"
	);
}

#[shared_test_runtime]
async fn exif_no_override_preserves_user_set_time() {
	let user_t = Utc.with_ymd_and_hms(2015, 1, 1, 0, 0, 0).unwrap();
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let dir = &resources.dir;

	let builder = client
		.make_file_builder("exif_no_override.jpg", dir.uuid())
		.unwrap()
		.created(user_t)
		.modified(user_t)
		.no_exif_override();
	let file = client.upload_file(builder, EXIF_SAMPLE_JPEG).await.unwrap();
	assert_eq!(
		file_created(&file),
		Some(user_t),
		"with .no_exif_override() and explicit .created(), user time must win"
	);
}

#[shared_test_runtime]
async fn exif_overrides_user_set_time_by_default() {
	let user_t = Utc.with_ymd_and_hms(2015, 1, 1, 0, 0, 0).unwrap();
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let dir = &resources.dir;

	let builder = client
		.make_file_builder("exif_overrides_user.jpg", dir.uuid())
		.unwrap()
		.created(user_t)
		.modified(user_t);
	let file = client.upload_file(builder, EXIF_SAMPLE_JPEG).await.unwrap();
	assert_eq!(
		file_created(&file),
		Some(known_exif_time()),
		"default flags: EXIF should override caller-supplied created time"
	);
}

#[shared_test_runtime]
async fn non_image_mime_ignores_exif_payload() {
	// JPEG bytes but stored under a non-image MIME — the EXIF parser must
	// never run since the gate is `image/*`, `video/*`, `audio/*`.
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let dir = &resources.dir;

	let builder = client
		.make_file_builder("exif_disguised.bin", dir.uuid())
		.unwrap()
		.mime("application/octet-stream".to_string());
	let file = client.upload_file(builder, EXIF_SAMPLE_JPEG).await.unwrap();
	let created = file_created(&file).expect("created decoded");
	assert!(
		created != known_exif_time(),
		"non-image MIME should not trigger EXIF parse (got {created})"
	);
	// Sanity: filename is preserved.
	assert_eq!(file.name(), Some("exif_disguised.bin"));
}

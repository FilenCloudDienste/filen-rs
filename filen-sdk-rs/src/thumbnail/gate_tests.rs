use super::might_be_thumbnailable;

#[test]
fn the_extension_rescues_what_the_stored_mime_lost() {
	// RAW, absent from the JS table entirely.
	assert!(might_be_thumbnailable(
		Some("DSC_0001.NEF"),
		Some("application/octet-stream")
	));
	assert!(might_be_thumbnailable(
		Some("IMG_0001.CR3"),
		Some("application/octet-stream")
	));
	// Uppercase is the norm straight off a camera.
	assert!(might_be_thumbnailable(Some("IMG_1234.JPG"), None));
	// Gzipped SVG is refused by the pipeline (no inflate), so svgz is not
	// in the table. (A stored mime naming plain SVG still admits it — a
	// chunk spent on a hand-supplied mime that lied, and the sniff's no.)
	assert!(!might_be_thumbnailable(
		Some("logo.svgz"),
		Some("application/octet-stream")
	));
}

/// The gate admits a format only when this build decodes it; a mime
/// cannot say otherwise. Without the decoder the answer is "no" up front,
/// not a chunk spent to be refused by the sniff.
#[test]
fn the_gate_follows_the_decoders_this_build_carries() {
	// The case the extension gate exists for: HEIC stored as octet-stream
	// by a Go client on macOS, or by any Rust client predating mime_guess
	// 2.0.5. AVIF rides the same libheif path since dav1d was vendored —
	// if either ever goes back to refusing, the decoder went away with it.
	let heif = cfg!(feature = "heif-decoder");
	for name in ["IMG_0042.heic", "IMG_0042.HIF", "photo.avif"] {
		assert_eq!(
			might_be_thumbnailable(Some(name), Some("application/octet-stream")),
			heif
		);
	}
	assert_eq!(
		might_be_thumbnailable(Some("photo.avif"), Some("image/avif")),
		heif
	);
	// SVG rasterises through resvg, which only the native target carries.
	let svg = cfg!(not(all(target_family = "wasm", target_os = "unknown")));
	assert_eq!(
		might_be_thumbnailable(Some("logo.svg"), Some("image/svg+xml")),
		svg
	);
	// The mime goes through the same gates as the extension: a rename
	// that dropped the extension does not smuggle a format past them.
	assert_eq!(
		might_be_thumbnailable(Some("logo"), Some("image/svg+xml")),
		svg
	);
	assert_eq!(
		might_be_thumbnailable(Some("scan"), Some("image/heic")),
		heif
	);
}

#[test]
fn a_known_mime_admits_what_the_name_does_not() {
	// No extension at all, or a last dot-segment that is none: the
	// picker-supplied mime is the only signal, and it is honoured.
	assert!(might_be_thumbnailable(Some("scan"), Some("image/jpeg")));
	assert!(might_be_thumbnailable(None, Some("image/png")));
	assert!(might_be_thumbnailable(
		Some("IMG_2026.06.01"),
		Some("image/jpeg")
	));
	assert!(might_be_thumbnailable(
		Some("photo.final"),
		Some("image/jpeg")
	));
	// A mime the table does not know admits nothing, extension or not —
	// looking would cost a chunk to learn nothing.
	assert!(!might_be_thumbnailable(
		Some("art.psd"),
		Some("image/vnd.adobe.photoshop")
	));
	assert!(!might_be_thumbnailable(Some("icon"), Some("image/x-icon")));
}

#[test]
fn everything_else_is_left_alone() {
	assert!(!might_be_thumbnailable(
		Some("archive.zip"),
		Some("application/zip")
	));
	// A wrapper suffix hides the real extension from every mime library
	// too, so the stored mime is octet-stream and this stays skipped.
	assert!(!might_be_thumbnailable(
		Some("photo.jpg.enc"),
		Some("application/octet-stream")
	));
	// No dot, nothing to go on.
	assert!(!might_be_thumbnailable(
		Some("IMG_1234"),
		Some("application/octet-stream")
	));
	assert!(!might_be_thumbnailable(None, None));
	// A dotfile is treated as its extension, deliberately.
	assert!(might_be_thumbnailable(Some(".jpg"), None));
}

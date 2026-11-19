use std::path::PathBuf;

use filen_mobile_native_cache::CacheError;
use filen_types::fs::Uuid;

use super::*;
use crate::test_support::{DriveCall, DriveOp, FakeDrive, FakeLocal, root_path};

#[cfg(windows)]
const ROOT: &str = r"C:\Users\u\Filen";
#[cfg(not(windows))]
const ROOT: &str = "/home/u/Filen";

const ACCOUNT: Uuid = Uuid::from_u128(0xacc);
const STABLE: Uuid = Uuid::from_u128(1);
const VERSION: Uuid = Uuid::from_u128(2);

/// A 24-bit uncompressed BMP of `width` x `height` pixels, every one `bgr`.
fn bmp(width: u32, height: u32, bgr: [u8; 3]) -> Vec<u8> {
	let row = (width * 3).div_ceil(4) * 4;
	let pixels = row * height;
	let mut out = Vec::new();
	out.extend_from_slice(b"BM");
	out.extend_from_slice(&(54 + pixels).to_le_bytes());
	out.extend_from_slice(&[0; 4]);
	out.extend_from_slice(&54_u32.to_le_bytes());
	out.extend_from_slice(&40_u32.to_le_bytes());
	out.extend_from_slice(&i32::try_from(width).unwrap().to_le_bytes());
	out.extend_from_slice(&i32::try_from(height).unwrap().to_le_bytes());
	out.extend_from_slice(&1_u16.to_le_bytes());
	out.extend_from_slice(&24_u16.to_le_bytes());
	out.extend_from_slice(&0_u32.to_le_bytes());
	out.extend_from_slice(&pixels.to_le_bytes());
	out.extend_from_slice(&[0; 16]);
	for _ in 0..height {
		for _ in 0..width {
			out.extend_from_slice(&bgr);
		}
		out.resize(out.len() + usize::try_from(row - width * 3).unwrap(), 0);
	}
	out
}

/// The full path of `relative`, components separated by `\`, as Explorer names it.
fn full(relative: &str) -> PathBuf {
	relative
		.split('\\')
		.fold(PathBuf::from(ROOT), |path, part| path.join(part))
}

fn file_identity() -> PlaceholderIdentity {
	PlaceholderIdentity::File {
		stable: STABLE,
		version: VERSION,
	}
}

struct Setup {
	local: FakeLocal,
	drive: Arc<FakeDrive>,
	service: ThumbnailService<FakeLocal, FakeDrive>,
}

fn setup(config: ThumbnailConfig) -> Setup {
	let local = FakeLocal::new(PlaceholderIdentity::Root { root: ACCOUNT });
	let drive = Arc::new(FakeDrive::new(ACCOUNT));
	let service = ThumbnailService::new(config, Handle::current());
	service.serve(
		RootDir::parse(ROOT.to_owned()).unwrap(),
		Arc::new(local.clone()),
		Arc::clone(&drive),
	);
	Setup {
		local,
		drive,
		service,
	}
}

fn served() -> Setup {
	setup(ThumbnailConfig::default())
}

/// A placeholder `a.bmp` of the drive file, holding `bytes`, all on disk when `hydrated`; the drive
/// holds `remote`.
fn placed(setup: &Setup, bytes: &[u8], hydrated: bool, remote: &[u8]) {
	setup
		.local
		.place_file_with(&root_path("a.bmp"), file_identity(), bytes, hydrated);
	setup.drive.add_file(STABLE, VERSION, remote);
}

fn dimensions(answer: Result<Thumbnail, ThumbnailRefusal>) -> (u32, u32) {
	let thumbnail = answer.expect("a thumbnail");
	assert_eq!(
		thumbnail.bgra().len(),
		usize::try_from(thumbnail.width() * thumbnail.height() * 4).unwrap()
	);
	(thumbnail.width(), thumbnail.height())
}

#[tokio::test(start_paused = true)]
async fn a_file_all_on_disk_is_thumbnailed_from_its_local_bytes_with_no_network() {
	let setup = served();
	placed(
		&setup,
		&bmp(200, 100, [0, 0, 255]),
		true,
		&bmp(10, 10, [0; 3]),
	);
	setup.drive.set_offline(true);
	let before = setup.local.entry(&root_path("a.bmp"));

	let thumbnail = setup.service.request(&full("a.bmp"), 96).await.unwrap();

	assert_eq!((thumbnail.width(), thumbnail.height()), (96, 48));
	assert_eq!(thumbnail.alpha(), ThumbnailAlpha::Opaque);
	// Red, as BGRA.
	assert_eq!(thumbnail.bgra().get(..4), Some(&[0, 0, 255, 255][..]));
	assert_eq!(
		setup.drive.ops(),
		vec![DriveOp::LocalThumbnail("a.bmp".to_owned())]
	);
	assert_eq!(setup.local.entry(&root_path("a.bmp")), before);
	assert_eq!(setup.local.ops(), vec![]);
}

#[tokio::test(start_paused = true)]
async fn an_online_only_file_is_thumbnailed_from_the_drive_and_left_as_it_is() {
	let setup = served();
	let image = bmp(100, 200, [255, 0, 0]);
	placed(&setup, &image, false, &image);
	let before = setup.local.entry(&root_path("a.bmp"));

	let answer = setup.service.request(&full("a.bmp"), 256).await;

	assert_eq!(dimensions(answer), (100, 200));
	assert_eq!(
		setup.drive.ops(),
		vec![DriveOp::Thumbnail(format!("stable/{STABLE}"))]
	);
	// Never hydrated, nor read through the provider's range reads, nor queued for a hydration.
	assert_eq!(setup.drive.reads(), vec![]);
	assert_eq!(setup.local.entry(&root_path("a.bmp")), before);
	assert_eq!(before.map(|entry| entry.on_disk), Some(0));
	assert_eq!(setup.local.ops(), vec![]);
}

#[tokio::test(start_paused = true)]
async fn a_partly_hydrated_file_is_thumbnailed_from_the_drive() {
	let setup = served();
	let image = bmp(50, 50, [1, 2, 3]);
	placed(&setup, &image, true, &image);
	setup.local.set_on_disk(&root_path("a.bmp"), 4096);

	let answer = setup.service.request(&full("a.bmp"), 96).await;

	assert_eq!(dimensions(answer), (50, 50));
	assert_eq!(
		setup.drive.ops(),
		vec![DriveOp::Thumbnail(format!("stable/{STABLE}"))]
	);
}

#[tokio::test(start_paused = true)]
async fn a_file_edited_locally_is_thumbnailed_from_its_edit() {
	let setup = served();
	placed(&setup, &bmp(40, 40, [0; 3]), true, &bmp(40, 40, [0; 3]));
	setup
		.local
		.user_write(&root_path("a.bmp"), &bmp(80, 20, [9; 3]))
		.unwrap();

	let answer = setup.service.request(&full("a.bmp"), 96).await;

	assert_eq!(dimensions(answer), (80, 20));
	assert_eq!(
		setup.drive.ops(),
		vec![DriveOp::LocalThumbnail("a.bmp".to_owned())]
	);
}

#[tokio::test(start_paused = true)]
async fn an_ordinary_file_not_taken_in_yet_is_thumbnailed_from_its_bytes() {
	let setup = served();
	setup
		.local
		.user_create(&root_path("new.bmp"), &bmp(30, 60, [5; 3]));

	let answer = setup.service.request(&full("new.bmp"), 96).await;

	assert_eq!(dimensions(answer), (30, 60));
}

#[tokio::test(start_paused = true)]
async fn directories_and_the_root_are_refused() {
	let setup = served();
	setup.local.place_dir(
		&root_path("Photos"),
		PlaceholderIdentity::Dir {
			uuid: Uuid::from_u128(7),
		},
	);

	assert_eq!(
		setup.service.request(&full("Photos"), 96).await,
		Err(ThumbnailRefusal::Directory)
	);
	assert_eq!(
		setup.service.request(Path::new(ROOT), 96).await,
		Err(ThumbnailRefusal::Directory)
	);
	assert_eq!(setup.drive.ops(), vec![]);
}

#[tokio::test(start_paused = true)]
async fn a_path_outside_the_root_is_refused() {
	let setup = served();
	let outside = PathBuf::from(ROOT).with_file_name("Other").join("a.bmp");

	assert_eq!(
		setup.service.request(&outside, 96).await,
		Err(ThumbnailRefusal::NotInRoot)
	);
}

#[tokio::test(start_paused = true)]
async fn a_detached_file_not_on_disk_is_not_a_drive_file() {
	let setup = served();
	setup.local.place_file_with(
		&root_path("a.bmp"),
		PlaceholderIdentity::Detached {
			nonce: Uuid::from_u128(3),
		},
		&bmp(4, 4, [0; 3]),
		false,
	);

	assert_eq!(
		setup.service.request(&full("a.bmp"), 96).await,
		Err(ThumbnailRefusal::NotAPlaceholder)
	);
}

#[tokio::test(start_paused = true)]
async fn nothing_is_served_until_a_root_is_and_after_it_stops() {
	let setup = served();
	let image = bmp(8, 8, [0; 3]);
	placed(&setup, &image, false, &image);
	setup.service.stop_serving();

	assert_eq!(
		setup.service.request(&full("a.bmp"), 96).await,
		Err(ThumbnailRefusal::SignedOut)
	);
	assert_eq!(setup.drive.ops(), vec![]);
}

#[tokio::test(start_paused = true)]
async fn a_signed_out_drive_is_signed_out() {
	let setup = served();
	let image = bmp(8, 8, [0; 3]);
	placed(&setup, &image, false, &image);
	setup.drive.fail_next(
		DriveCall::Thumbnail,
		CacheError::Unauthenticated("Unauthenticated".into()),
	);

	assert_eq!(
		setup.service.request(&full("a.bmp"), 96).await,
		Err(ThumbnailRefusal::SignedOut)
	);
}

#[tokio::test(start_paused = true)]
async fn offline_is_answered_and_not_remembered() {
	let setup = served();
	let image = bmp(8, 8, [0; 3]);
	placed(&setup, &image, false, &image);
	setup.drive.set_offline(true);

	assert_eq!(
		setup.service.request(&full("a.bmp"), 96).await,
		Err(ThumbnailRefusal::Offline)
	);
	setup.drive.set_offline(false);
	assert_eq!(
		dimensions(setup.service.request(&full("a.bmp"), 96).await),
		(8, 8)
	);
}

#[tokio::test(start_paused = true)]
async fn a_settled_no_is_remembered_per_version_and_size_for_a_while() {
	let setup = served();
	placed(&setup, b"not an image", false, b"not an image");
	let asked = || {
		setup
			.drive
			.ops()
			.iter()
			.filter(|op| matches!(op, DriveOp::Thumbnail(_)))
			.count()
	};

	assert_eq!(
		setup.service.request(&full("a.bmp"), 96).await,
		Err(ThumbnailRefusal::Unsupported)
	);
	assert_eq!(
		setup.service.request(&full("a.bmp"), 40).await,
		Err(ThumbnailRefusal::Unsupported)
	);
	assert_eq!(asked(), 1, "the same bucket is answered from memory");
	assert_eq!(
		setup.service.request(&full("a.bmp"), 256).await,
		Err(ThumbnailRefusal::Unsupported)
	);
	assert_eq!(asked(), 2, "another bucket asks the drive");

	tokio::time::advance(ThumbnailConfig::default().remember).await;
	assert_eq!(
		setup.service.request(&full("a.bmp"), 96).await,
		Err(ThumbnailRefusal::Unsupported)
	);
	assert_eq!(asked(), 3, "a verdict is forgotten after a while");
}

#[tokio::test(start_paused = true)]
async fn a_request_past_its_deadline_is_busy_and_holds_up_the_next_no_longer() {
	let setup = served();
	let image = bmp(8, 8, [0; 3]);
	placed(&setup, &image, false, &image);
	let gate = setup.drive.gate_calls(DriveCall::Thumbnail);
	let started = Instant::now();

	assert_eq!(
		setup.service.request(&full("a.bmp"), 96).await,
		Err(ThumbnailRefusal::Busy)
	);
	assert_eq!(started.elapsed(), ThumbnailConfig::default().deadline);

	gate.add_permits(1);
	assert_eq!(
		dimensions(setup.service.request(&full("a.bmp"), 96).await),
		(8, 8)
	);
}

#[tokio::test(start_paused = true)]
async fn one_thumbnail_is_made_at_a_time_by_default() {
	let setup = served();
	let image = bmp(8, 8, [0; 3]);
	placed(&setup, &image, false, &image);
	let _gate = setup.drive.gate_calls(DriveCall::Thumbnail);

	let path = full("a.bmp");
	let (first, second) = tokio::join!(
		setup.service.request(&path, 96),
		setup.service.request(&path, 256),
	);

	assert_eq!(first, Err(ThumbnailRefusal::Busy));
	assert_eq!(second, Err(ThumbnailRefusal::Busy));
	assert_eq!(
		setup.drive.ops(),
		vec![DriveOp::Thumbnail(format!("stable/{STABLE}"))],
		"the second request never got a slot"
	);
}

#[tokio::test(start_paused = true)]
async fn a_wider_limit_makes_several_at_once() {
	let setup = setup(ThumbnailConfig {
		concurrency: 2,
		..ThumbnailConfig::default()
	});
	let image = bmp(8, 8, [0; 3]);
	placed(&setup, &image, false, &image);
	let _gate = setup.drive.gate_calls(DriveCall::Thumbnail);

	let path = full("a.bmp");
	let _ = tokio::join!(
		setup.service.request(&path, 96),
		setup.service.request(&path, 256),
	);

	assert_eq!(setup.drive.ops().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_small_size_is_made_at_a_bucket_and_scaled_down_to_it() {
	let setup = served();
	let image = bmp(200, 100, [0; 3]);
	placed(&setup, &image, false, &image);

	assert_eq!(
		dimensions(setup.service.request(&full("a.bmp"), 32).await),
		(32, 16)
	);
	assert_eq!(
		dimensions(setup.service.request(&full("a.bmp"), 4).await),
		(16, 8),
		"clamped to 16"
	);
}

#[test]
fn sizes_are_clamped_and_bucketed() {
	let made_at = [1, 16, 32, 96, 97, 256, 257, 1024, 5000].map(bucket);
	assert_eq!(made_at, [96, 96, 96, 96, 256, 256, 1024, 1024, 1024]);
}

#[test]
fn transparency_is_premultiplied_and_reported() {
	let (bgra, alpha) = to_bgra(&[255, 128, 0, 255, 200, 100, 50, 0, 255, 255, 255, 128], 4);
	assert_eq!(alpha, ThumbnailAlpha::Premultiplied);
	assert_eq!(bgra, [0, 128, 255, 255, 0, 0, 0, 0, 128, 128, 128, 128]);
	let (bgra, alpha) = to_bgra(&[1, 2, 3], 3);
	assert_eq!(alpha, ThumbnailAlpha::Opaque);
	assert_eq!(bgra, [3, 2, 1, 255]);
}

#[test]
fn scaling_down_averages_the_pixels_covered() {
	let thumbnail = Thumbnail {
		width: 2,
		height: 1,
		bgra: vec![0, 0, 0, 255, 200, 100, 50, 255],
		alpha: ThumbnailAlpha::Opaque,
	};
	let scaled = fit_within(thumbnail, 1);
	assert_eq!((scaled.width, scaled.height), (1, 1));
	assert_eq!(scaled.bgra, [100, 50, 25, 255]);
}

#[test]
fn the_blocking_entry_point_answers_from_a_thread_outside_the_runtime() {
	let runtime = tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.build()
		.unwrap();
	let setup = runtime.block_on(async { served() });
	let image = bmp(20, 10, [0; 3]);
	placed(&setup, &image, false, &image);
	let service = Arc::new(setup.service);
	let handler = service.handler();

	let answer = std::thread::spawn(move || handler(&full("a.bmp"), 96))
		.join()
		.unwrap();

	assert_eq!(dimensions(answer), (20, 10));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_blocking_entry_point_refuses_on_the_runtime() {
	let setup = served();
	assert_eq!(
		setup.service.thumbnail_for(&full("a.bmp"), 96),
		Err(ThumbnailRefusal::Busy)
	);
}

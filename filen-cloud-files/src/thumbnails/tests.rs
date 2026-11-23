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
async fn an_online_only_file_the_drive_holds_a_newer_version_of_is_not_thumbnailed_from_it() {
	let setup = served();
	setup.local.place_file_with(
		&root_path("a.bmp"),
		file_identity(),
		&bmp(50, 50, [1, 2, 3]),
		false,
	);
	let newer = Uuid::from_u128(3);
	setup.drive.add_file(STABLE, newer, &bmp(20, 10, [4, 5, 6]));

	let answer = setup.service.request(&full("a.bmp"), 96).await;

	assert_eq!(answer.err(), Some(ThumbnailRefusal::Failed));
	assert_eq!(
		setup.drive.ops(),
		vec![DriveOp::Thumbnail(format!("stable/{STABLE}"))]
	);
	assert_eq!(
		setup
			.drive
			.cached_fitted_thumbnail(newer, 96)
			.await
			.unwrap(),
		None
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
async fn what_the_path_refuses_waits_for_no_slot() {
	let setup = served();
	let image = bmp(8, 8, [0; 3]);
	placed(&setup, &image, false, &image);
	let _gate = setup.drive.gate_calls(DriveCall::Thumbnail);
	let outside = PathBuf::from(ROOT).with_file_name("Other").join("a.bmp");

	let path = full("a.bmp");
	let refused = async {
		// Lets the first request take the slot.
		tokio::task::yield_now().await;
		let started = Instant::now();
		let answers = (
			setup.service.request(&outside, 96).await,
			setup.service.request(Path::new(ROOT), 96).await,
		);
		(answers, started.elapsed())
	};
	let (held, (answers, waited)) = tokio::join!(setup.service.request(&path, 96), refused);

	assert_eq!(held, Err(ThumbnailRefusal::Pending));
	assert_eq!(
		answers,
		(
			Err(ThumbnailRefusal::NotInRoot),
			Err(ThumbnailRefusal::Directory)
		)
	);
	assert_eq!(waited, Duration::ZERO);
	assert_eq!(
		setup.drive.ops(),
		vec![DriveOp::Thumbnail(format!("stable/{STABLE}"))],
		"the first request held the slot throughout"
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
	assert_eq!(
		setup.service.request(&full("a.bmp"), 256).await,
		Err(ThumbnailRefusal::Unsupported)
	);
	assert_eq!(asked(), 1, "the same bucket is answered from memory");
	assert_eq!(
		setup.service.request(&full("a.bmp"), 1024).await,
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
async fn a_request_past_its_deadline_is_pending_and_holds_up_the_next_no_longer() {
	let setup = served();
	let image = bmp(8, 8, [0; 3]);
	placed(&setup, &image, false, &image);
	let gate = setup.drive.gate_calls(DriveCall::Thumbnail);
	let started = Instant::now();

	assert_eq!(
		setup.service.request(&full("a.bmp"), 96).await,
		Err(ThumbnailRefusal::Pending)
	);
	assert_eq!(started.elapsed(), ThumbnailConfig::default().deadline);

	gate.add_permits(1);
	assert_eq!(
		dimensions(setup.service.request(&full("a.bmp"), 96).await),
		(8, 8)
	);
}

#[tokio::test(start_paused = true)]
async fn two_thumbnails_are_made_at_once_by_default() {
	let setup = served();
	let image = bmp(8, 8, [0; 3]);
	placed(&setup, &image, false, &image);
	let _gate = setup.drive.gate_calls(DriveCall::Thumbnail);

	let path = full("a.bmp");
	let (first, second) = tokio::join!(
		setup.service.request(&path, 256),
		setup.service.request(&path, 1024),
	);

	assert_eq!(first, Err(ThumbnailRefusal::Pending));
	assert_eq!(second, Err(ThumbnailRefusal::Pending));
	assert_eq!(setup.drive.ops().len(), 2, "neither waited for the other");
}

#[tokio::test(start_paused = true)]
async fn a_narrower_limit_makes_one_at_a_time() {
	let deadline = ThumbnailConfig::default().deadline;
	let setup = setup(ThumbnailConfig {
		running: 1,
		..ThumbnailConfig::default()
	});
	let image = bmp(8, 8, [0; 3]);
	placed(&setup, &image, false, &image);
	let _gate = setup.drive.gate_calls(DriveCall::Thumbnail);

	let path = full("a.bmp");
	let (_, _, asked_meanwhile) = tokio::join!(
		setup.service.request(&path, 96),
		setup.service.request(&path, 256),
		async {
			tokio::time::sleep(deadline / 2).await;
			setup.drive.ops().len()
		},
	);

	assert_eq!(asked_meanwhile, 1, "the second request waited for the slot");
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
	assert_eq!(made_at, [256, 256, 256, 256, 256, 256, 1024, 1024, 1024]);
}

#[test]
fn transparency_is_reported_and_its_colours_left_straight() {
	let (bgra, alpha) = to_bgra(
		&[
			255, 128, 0, 255, // opaque orange
			200, 100, 50, 0, // transparent
			255, 255, 255, 128, // white at half alpha
			255, 0, 0, 127, // red at half alpha
		],
		4,
	);
	assert_eq!(alpha, ThumbnailAlpha::Straight);
	// Not premultiplied: Explorer would darken white at 128 to #808080 a second time, which
	// showed as #BFBFBF over a white window.
	assert_eq!(
		bgra,
		[
			0, 128, 255, 255, //
			50, 100, 200, 0, //
			255, 255, 255, 128, //
			0, 0, 255, 127,
		]
	);
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
fn scaling_down_weighs_each_colour_by_its_alpha() {
	// A transparent black pixel beside an opaque white one: white at half alpha, not grey.
	let thumbnail = Thumbnail {
		width: 2,
		height: 1,
		bgra: vec![0, 0, 0, 0, 255, 255, 255, 255],
		alpha: ThumbnailAlpha::Straight,
	};
	let scaled = fit_within(thumbnail, 1);
	assert_eq!(scaled.bgra, [255, 255, 255, 127]);
	assert_eq!(scaled.alpha, ThumbnailAlpha::Straight);
	// Transparent throughout: no colour.
	let thumbnail = Thumbnail {
		width: 2,
		height: 1,
		bgra: vec![9, 9, 9, 0, 7, 7, 7, 0],
		alpha: ThumbnailAlpha::Straight,
	};
	assert_eq!(fit_within(thumbnail, 1).bgra, [0, 0, 0, 0]);
}

#[tokio::test(start_paused = true)]
async fn a_cancelled_request_is_busy_at_once_and_its_work_goes_on() {
	let setup = served();
	let image = bmp(8, 8, [0; 3]);
	placed(&setup, &image, false, &image);
	let gate = setup.drive.gate_calls(DriveCall::Thumbnail);
	let path = full("a.bmp");
	let cancel = ThumbnailCancel::default();

	let started = Instant::now();
	let (held, ()) = tokio::join!(setup.service.answer(&path, 96, &cancel), async {
		// Lets the request take the slot and wait in the drive.
		let waited = std::time::Instant::now();
		while setup.drive.ops().is_empty() {
			assert!(
				waited.elapsed() < Duration::from_secs(60),
				"the request never reached the drive"
			);
			tokio::task::yield_now().await;
		}
		cancel.cancel();
	});

	assert_eq!(held, Err(ThumbnailRefusal::Busy));
	assert_eq!(started.elapsed(), Duration::ZERO);
	assert_eq!(
		setup.drive.ops(),
		vec![DriveOp::Thumbnail(format!("stable/{STABLE}"))],
		"the request had reached the drive"
	);
	gate.add_permits(1);
	let next = setup.service.request(&path, 96).await;
	assert_eq!(dimensions(next), (8, 8));
	assert_eq!(started.elapsed(), Duration::ZERO);
	assert_eq!(
		made_of(&setup, STABLE),
		1,
		"answered by the work the cancelled request started, not started again"
	);
	assert_eq!(told(&setup, "a.bmp"), 0, "nothing was promised");
}

#[tokio::test(start_paused = true)]
async fn a_request_cancelled_while_waiting_for_the_slot_leaves_the_queue_at_once() {
	let setup = setup(ThumbnailConfig {
		running: 1,
		..ThumbnailConfig::default()
	});
	let image = bmp(8, 8, [0; 3]);
	placed(&setup, &image, false, &image);
	let gate = setup.drive.gate_calls(DriveCall::Thumbnail);
	let path = full("a.bmp");
	let (holder, waiter) = (ThumbnailCancel::default(), ThumbnailCancel::default());

	let started = Instant::now();
	let (first, second, ()) = tokio::join!(
		setup.service.answer(&path, 96, &holder),
		setup.service.answer(&path, 256, &waiter),
		async {
			tokio::task::yield_now().await;
			waiter.cancel();
			// The holder's answer is not cancelled, and goes on once the drive is.
			gate.add_permits(1);
		},
	);

	assert_eq!(second, Err(ThumbnailRefusal::Busy));
	assert_eq!(dimensions(first), (8, 8));
	assert_eq!(started.elapsed(), Duration::ZERO);
	assert!(!holder.is_cancelled());
	assert_eq!(
		setup.drive.ops(),
		vec![DriveOp::Thumbnail(format!("stable/{STABLE}"))],
		"the cancelled request never reached the drive"
	);
}

#[tokio::test(start_paused = true)]
async fn a_request_cancelled_before_it_starts_is_busy_without_reading_anything() {
	let setup = served();
	let image = bmp(8, 8, [0; 3]);
	placed(&setup, &image, false, &image);
	let cancel = ThumbnailCancel::default();
	cancel.cancel();

	assert_eq!(
		setup.service.answer(&full("a.bmp"), 96, &cancel).await,
		Err(ThumbnailRefusal::Busy)
	);
	assert_eq!(setup.drive.ops(), vec![]);
	assert_eq!(setup.local.ops(), vec![]);
}

#[tokio::test(start_paused = true)]
async fn a_request_from_file_explorer_is_pending_at_the_shorter_deadline() {
	let setup = served();
	let image = bmp(8, 8, [0; 3]);
	placed(&setup, &image, false, &image);
	let _gate = setup.drive.gate_calls(DriveCall::Thumbnail);
	let path = full("a.bmp");
	assert_eq!(EXPLORER_DEADLINE, Duration::from_millis(1500));
	assert!(EXPLORER_DEADLINE < ThumbnailConfig::default().deadline);

	let started = Instant::now();
	assert_eq!(
		setup
			.service
			.answer_for_explorer(&path, 96, &ThumbnailCancel::default())
			.await,
		Err(ThumbnailRefusal::Pending)
	);
	assert_eq!(started.elapsed(), EXPLORER_DEADLINE);

	// Any other caller keeps the service's deadline, waiting on the same work.
	let started = Instant::now();
	assert_eq!(
		setup.service.request(&path, 96).await,
		Err(ThumbnailRefusal::Pending)
	);
	assert_eq!(started.elapsed(), ThumbnailConfig::default().deadline);
}

#[tokio::test(start_paused = true)]
async fn a_service_deadline_shorter_than_file_explorers_holds_for_its_requests_too() {
	let deadline = Duration::from_secs(1);
	let setup = setup(ThumbnailConfig {
		deadline,
		..ThumbnailConfig::default()
	});
	let image = bmp(8, 8, [0; 3]);
	placed(&setup, &image, false, &image);
	let _gate = setup.drive.gate_calls(DriveCall::Thumbnail);

	let started = Instant::now();
	assert_eq!(
		setup
			.service
			.answer_for_explorer(&full("a.bmp"), 96, &ThumbnailCancel::default())
			.await,
		Err(ThumbnailRefusal::Pending)
	);
	assert_eq!(started.elapsed(), deadline);
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

	let answer =
		std::thread::spawn(move || handler(&full("a.bmp"), 96, &ThumbnailCancel::default()))
			.join()
			.unwrap();

	assert_eq!(dimensions(answer), (20, 10));
}

#[test]
fn the_blocking_entry_point_ends_once_another_thread_cancels() {
	let runtime = tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.build()
		.unwrap();
	let setup = runtime.block_on(async { served() });
	let image = bmp(20, 10, [0; 3]);
	placed(&setup, &image, false, &image);
	let gate = setup.drive.gate_calls(DriveCall::Thumbnail);
	let drive = Arc::clone(&setup.drive);
	let service = Arc::new(setup.service);
	let handler = service.handler();
	let cancel = ThumbnailCancel::default();

	let asking = std::thread::spawn({
		let cancel = cancel.clone();
		move || handler(&full("a.bmp"), 96, &cancel)
	});
	// Cancelled once the request waits in the drive, as the COM layer's poll does.
	let waited = std::time::Instant::now();
	while drive.ops().is_empty() {
		assert!(
			waited.elapsed() < Duration::from_secs(60),
			"the request never reached the drive"
		);
		std::thread::yield_now();
	}
	cancel.cancel();
	let answer = asking.join().unwrap();

	// Answered at the cancel: File Explorer's deadline answers Pending.
	assert_eq!(answer, Err(ThumbnailRefusal::Busy));
	gate.add_permits(1);
	let handler = service.handler();
	let next = std::thread::spawn(move || handler(&full("a.bmp"), 96, &ThumbnailCancel::default()))
		.join()
		.unwrap();
	assert_eq!(dimensions(next), (20, 10));
}

#[test]
fn the_blocking_entry_point_answers_at_file_explorers_deadline() {
	let runtime = tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.build()
		.unwrap();
	let setup = runtime.block_on(async { served() });
	let image = bmp(8, 8, [0; 3]);
	placed(&setup, &image, false, &image);
	let _gate = setup.drive.gate_calls(DriveCall::Thumbnail);
	let service = Arc::new(setup.service);
	let handler = service.handler();

	let answer =
		std::thread::spawn(move || handler(&full("a.bmp"), 96, &ThumbnailCancel::default()))
			.join()
			.unwrap();

	// Nothing cancels it, as File Explorer never does (measured): a deadline ends it. That it is
	// File Explorer's, not the service's, is
	// `a_request_from_file_explorer_is_pending_at_the_shorter_deadline`'s, on a paused clock.
	assert_eq!(answer, Err(ThumbnailRefusal::Pending));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_blocking_entry_point_refuses_on_the_runtime() {
	let setup = served();
	assert_eq!(
		setup
			.service
			.thumbnail_for(&full("a.bmp"), 96, &ThumbnailCancel::default()),
		Err(ThumbnailRefusal::Busy)
	);
}

/// Yields until `done` holds, for a real minute at most, then fails saying `what`. Yields rather
/// than sleeps, so the paused clock stays where it is while the background works.
async fn eventually(what: &str, done: impl Fn() -> bool) {
	let started = std::time::Instant::now();
	while !done() {
		assert!(
			started.elapsed() < Duration::from_secs(60),
			"never happened: {what}"
		);
		tokio::task::yield_now().await;
	}
}

/// Places `name`, a placeholder of the drive file `stable` at the version `stable + 100` holding
/// `bytes`, not on disk; the version.
fn placed_other(setup: &Setup, name: &str, stable: u128, bytes: &[u8]) -> Uuid {
	let (stable, version) = (Uuid::from_u128(stable), Uuid::from_u128(stable + 100));
	setup.local.place_file_with(
		&root_path(name),
		PlaceholderIdentity::File { stable, version },
		bytes,
		false,
	);
	setup.drive.add_file(stable, version, bytes);
	version
}

/// How many times the drive was asked to make a thumbnail of the file `stable`.
fn made_of(setup: &Setup, stable: Uuid) -> usize {
	let asked = DriveOp::Thumbnail(format!("stable/{stable}"));
	setup.drive.ops().iter().filter(|op| **op == asked).count()
}

#[tokio::test(start_paused = true)]
async fn a_kept_thumbnail_is_answered_while_both_slots_are_held() {
	let setup = served();
	let image = bmp(200, 100, [0; 3]);
	placed(&setup, &image, false, &image);
	placed_other(&setup, "b.bmp", 11, &image);
	placed_other(&setup, "c.bmp", 12, &image);
	assert_eq!(
		dimensions(setup.service.request(&full("a.bmp"), 96).await),
		(96, 48)
	);
	assert!(setup.drive.keeps_thumbnail(VERSION, ICON_SIDE));
	let _gate = setup.drive.gate_calls(DriveCall::Thumbnail);

	let (b_path, c_path) = (full("b.bmp"), full("c.bmp"));
	let started = Instant::now();
	let (b, c, (kept, waited)) = tokio::join!(
		setup.service.request(&b_path, 96),
		setup.service.request(&c_path, 96),
		async {
			eventually("both slots are held in the drive", || {
				setup.drive.thumbnails_in_flight().0 == 2
			})
			.await;
			let kept = setup.service.request(&full("a.bmp"), 32).await;
			(kept, started.elapsed())
		},
	);

	assert_eq!(dimensions(kept), (32, 16), "the kept 256 fitted to 32");
	assert_eq!(waited, Duration::ZERO, "it waited for no slot");
	assert_eq!(made_of(&setup, STABLE), 1, "it was not made again");
	assert_eq!(
		(b, c),
		(
			Err(ThumbnailRefusal::Pending),
			Err(ThumbnailRefusal::Pending)
		)
	);
}

#[tokio::test(start_paused = true)]
async fn a_request_past_its_deadline_is_finished_in_the_background_and_answered_from_the_cache() {
	let setup = served();
	let image = bmp(8, 8, [0; 3]);
	placed(&setup, &image, false, &image);
	let gate = setup.drive.gate_calls(DriveCall::Thumbnail);

	let started = Instant::now();
	assert_eq!(
		setup.service.request(&full("a.bmp"), 96).await,
		Err(ThumbnailRefusal::Pending)
	);
	assert_eq!(started.elapsed(), ThumbnailConfig::default().deadline);
	assert_eq!(setup.drive.thumbnails_in_flight().0, 1, "the work goes on");
	assert_eq!(setup.local.notified_items(), vec![]);

	gate.add_permits(1);
	eventually("File Explorer was told", || {
		!setup.local.notified_items().is_empty()
	})
	.await;
	assert!(setup.drive.keeps_thumbnail(VERSION, ICON_SIDE));

	let lookups = setup.drive.thumbnail_cache_lookups();
	let started = Instant::now();
	assert_eq!(
		dimensions(setup.service.request(&full("a.bmp"), 96).await),
		(8, 8)
	);
	assert_eq!(started.elapsed(), Duration::ZERO);
	assert_eq!(setup.drive.thumbnail_cache_lookups(), lookups + 1);
	assert_eq!(made_of(&setup, STABLE), 1, "the drive was asked once");
	tokio::task::yield_now().await;
	assert_eq!(setup.local.notified_items(), vec![root_path("a.bmp")]);
	// Never hydrated, nor counted as a read.
	assert_eq!(setup.drive.reads(), vec![]);
	assert_eq!(setup.local.ops(), vec![]);
}

#[tokio::test(start_paused = true)]
async fn asking_again_while_the_background_makes_it_joins_the_work() {
	let setup = served();
	let image = bmp(8, 8, [0; 3]);
	placed(&setup, &image, false, &image);
	let gate = setup.drive.gate_calls(DriveCall::Thumbnail);

	assert_eq!(
		setup.service.request(&full("a.bmp"), 96).await,
		Err(ThumbnailRefusal::Pending)
	);
	assert_eq!(
		setup.service.request(&full("a.bmp"), 40).await,
		Err(ThumbnailRefusal::Pending),
		"the same bucket"
	);
	assert_eq!(made_of(&setup, STABLE), 1, "joined, not made again");
	assert_eq!(setup.drive.thumbnails_in_flight(), (1, 1));

	// A request waiting on it is answered once it is made.
	let path = full("a.bmp");
	let (joined, ()) = tokio::join!(setup.service.request(&path, 96), async {
		eventually("the request waits on the job", || {
			setup
				.service
				.lane
				.state()
				.jobs
				.values()
				.any(|job| job.made.receiver_count() == 1)
		})
		.await;
		gate.add_permits(1);
	});
	assert_eq!(dimensions(joined), (8, 8));
	// It was answered Pending before: File Explorer is told all the same.
	eventually("File Explorer was told", || told(&setup, "a.bmp") == 1).await;
	settle().await;
	assert_eq!(made_of(&setup, STABLE), 1);
	assert_eq!(told(&setup, "a.bmp"), 1, "told once");
}

/// How many of the lane's jobs hold a slot now.
fn running(setup: &Setup) -> usize {
	setup
		.service
		.lane
		.state()
		.jobs
		.values()
		.filter(|job| matches!(job.run, Run::Running { .. }))
		.count()
}

#[tokio::test(start_paused = true)]
async fn the_lane_is_bounded_and_refuses_rather_than_drops_what_was_pending() {
	let config = ThumbnailConfig {
		running: 3,
		queued: 2,
		deadline: Duration::from_secs(1),
		..ThumbnailConfig::default()
	};
	let setup = setup(config);
	let image = bmp(8, 8, [0; 3]);
	let names = ["f1.bmp", "f2.bmp", "f3.bmp", "f4.bmp", "f5.bmp", "f6.bmp"];
	let versions: Vec<Uuid> = (11..)
		.zip(names)
		.map(|(stable, name)| placed_other(&setup, name, stable, &image))
		.collect();
	let gate = setup.drive.gate_calls(DriveCall::Thumbnail);

	let mut answers = Vec::new();
	for name in names {
		let started = Instant::now();
		answers.push((
			setup.service.request(&full(name), 96).await,
			started.elapsed(),
		));
		let state = setup.service.lane.state();
		assert!(state.queued.len() <= config.queued, "{:?}", state.queued);
	}
	assert!(
		answers[..5]
			.iter()
			.all(|(answer, _)| *answer == Err(ThumbnailRefusal::Pending)),
		"{answers:?}"
	);
	assert_eq!(
		answers[5],
		(Err(ThumbnailRefusal::Busy), Duration::ZERO),
		"no room: refused at once, not promised and dropped later"
	);

	let key = |at: usize| {
		(
			Bytes::Remote {
				version: versions[at],
			},
			ICON_SIDE,
		)
	};
	{
		let state = setup.service.lane.state();
		assert_eq!(
			state.queued,
			[key(3), key(4)],
			"nothing promised was dropped"
		);
		assert_eq!(state.jobs.len(), 5);
	}
	assert_eq!(running(&setup), config.running, "each where it started");
	assert_eq!(setup.drive.thumbnails_in_flight().0, config.running);

	gate.add_permits(100);
	eventually("every item answered Pending was told", || {
		setup.local.notified_items().len() == 5
	})
	.await;
	settle().await;
	let told = setup.local.notified_items();
	assert_eq!(told.len(), 5, "each told once: {told:?}");
	for (at, name) in names[..5].iter().enumerate() {
		assert!(told.contains(&root_path(name)), "{name}: {told:?}");
		assert!(
			setup.drive.keeps_thumbnail(versions[at], ICON_SIDE),
			"{name}"
		);
	}
	let stable = |at: u128| DriveOp::Thumbnail(format!("stable/{}", Uuid::from_u128(at)));
	assert_eq!(
		setup.drive.ops(),
		[stable(11), stable(12), stable(13), stable(15), stable(14)],
		"each started once, the newest waiting first; f6 never"
	);
	let (_, most) = setup.drive.thumbnails_in_flight();
	assert!(most <= config.running, "{most} at once");
	assert!(setup.service.lane.state().jobs.is_empty());
}

#[tokio::test(start_paused = true)]
async fn file_explorers_asks_on_a_held_drive_start_one_read_per_item() {
	let setup = served();
	let image = bmp(8, 8, [0; 3]);
	let names: Vec<String> = (0..30).map(|at| format!("s{at:02}.bmp")).collect();
	let versions: Vec<Uuid> = (11..)
		.zip(&names)
		.map(|(stable, name)| placed_other(&setup, name, stable, &image))
		.collect();
	let gate = setup.drive.gate_calls(DriveCall::Thumbnail);
	let limit = ThumbnailConfig::default().running;
	// File Explorer asks each item at 96 and then at 256 (measured), with two calls in flight, each
	// caller taking the next ask once its last was answered; the second starts later, so each ask
	// overlaps the other caller's, the item's other ask or the next item's.
	let asks: Mutex<VecDeque<(usize, u32)>> = Mutex::new(
		(0..names.len())
			.flat_map(|at| [(at, 96), (at, 256)])
			.collect(),
	);
	let caller = |after: Duration| {
		let (asks, names, setup) = (&asks, &names, &setup);
		async move {
			tokio::time::sleep(after).await;
			let (mut answers, mut most) = (Vec::new(), 0);
			loop {
				let next = asks.lock().unwrap().pop_front();
				let Some((at, side)) = next else {
					break;
				};
				let answer = setup
					.service
					.answer_for_explorer(&full(&names[at]), side, &ThumbnailCancel::default())
					.await;
				most = most.max(running(setup));
				answers.push((at, side, answer));
			}
			(answers, most)
		}
	};
	let ((first, most_first), (second, most_second)) =
		tokio::join!(caller(Duration::ZERO), caller(EXPLORER_DEADLINE / 2));

	let answers: Vec<_> = first.into_iter().chain(second).collect();
	assert_eq!(answers.len(), 2 * names.len());
	for (at, side, answer) in &answers {
		assert_eq!(*answer, Err(ThumbnailRefusal::Pending), "{at} at {side}");
	}
	assert!(most_first.max(most_second) <= limit);
	assert_eq!(running(&setup), limit, "every slot held, the rest queued");
	assert_eq!(setup.local.notified_items(), vec![]);
	for (at, name) in names.iter().enumerate() {
		let started = made_of(&setup, Uuid::from_u128(11 + u128::try_from(at).unwrap()));
		assert!(started <= 1, "{name}: {started} reads started while held");
	}

	gate.add_permits(names.len());
	eventually("every item was told", || {
		setup.local.notified_items().len() == names.len()
	})
	.await;
	settle().await;
	for (at, (name, version)) in names.iter().zip(&versions).enumerate() {
		let stable = Uuid::from_u128(11 + u128::try_from(at).unwrap());
		assert_eq!(made_of(&setup, stable), 1, "{name}: one read");
		assert_eq!(told(&setup, name), 1, "{name}: told once");
		assert!(setup.drive.keeps_thumbnail(*version, ICON_SIDE), "{name}");
	}
	let (_, most) = setup.drive.thumbnails_in_flight();
	assert!(most <= limit, "{most} reads at once");
	let state = setup.service.lane.state();
	assert!(state.jobs.is_empty());
	assert!(state.queued.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_cancelled_request_whose_job_waits_for_a_slot_leaves_nothing_queued() {
	let setup = setup(ThumbnailConfig {
		running: 1,
		..ThumbnailConfig::default()
	});
	let image = bmp(8, 8, [0; 3]);
	placed(&setup, &image, false, &image);
	placed_other(&setup, "b.bmp", 11, &image);
	let gate = setup.drive.gate_calls(DriveCall::Thumbnail);
	let (a_path, b_path) = (full("a.bmp"), full("b.bmp"));
	let cancel = ThumbnailCancel::default();

	let (held, cancelled) = tokio::join!(setup.service.request(&a_path, 96), async {
		eventually("a.bmp holds the slot", || {
			setup.drive.thumbnails_in_flight().0 == 1
		})
		.await;
		let (answer, ()) = tokio::join!(setup.service.answer(&b_path, 96, &cancel), async {
			eventually("b.bmp waits for the slot", || {
				setup.service.lane.state().queued.len() == 1
			})
			.await;
			cancel.cancel();
		});
		let state = setup.service.lane.state();
		(answer, state.queued.len(), state.jobs.len())
	});

	assert_eq!(cancelled, (Err(ThumbnailRefusal::Busy), 0, 1));
	assert_eq!(held, Err(ThumbnailRefusal::Pending));
	gate.add_permits(1);
	eventually("File Explorer was told of a.bmp", || {
		told(&setup, "a.bmp") == 1
	})
	.await;
	settle().await;
	assert!(setup.service.lane.state().jobs.is_empty());
	assert_eq!(made_of(&setup, Uuid::from_u128(11)), 0, "b.bmp never read");
	assert_eq!(told(&setup, "b.bmp"), 0);
}

#[tokio::test(start_paused = true)]
async fn stopping_cancels_the_background_at_once() {
	let setup = served();
	let image = bmp(8, 8, [0; 3]);
	placed(&setup, &image, false, &image);
	let gate = setup.drive.gate_calls(DriveCall::Thumbnail);
	assert_eq!(
		setup.service.request(&full("a.bmp"), 96).await,
		Err(ThumbnailRefusal::Pending)
	);
	assert_eq!(setup.drive.thumbnails_in_flight().0, 1, "the work goes on");

	let started = Instant::now();
	setup.service.stop_serving();
	eventually("the work stopped", || {
		setup.drive.thumbnails_in_flight().0 == 0
	})
	.await;
	assert_eq!(started.elapsed(), Duration::ZERO);
	assert!(setup.service.lane.state().jobs.is_empty());

	// Its gate's permit is left for the next session's request.
	gate.add_permits(1);
	tokio::task::yield_now().await;
	assert!(!setup.drive.keeps_thumbnail(VERSION, ICON_SIDE));
	assert_eq!(setup.local.notified_items(), vec![]);
	setup.service.serve(
		RootDir::parse(ROOT.to_owned()).unwrap(),
		Arc::new(setup.local.clone()),
		Arc::clone(&setup.drive),
	);
	assert_eq!(
		dimensions(setup.service.request(&full("a.bmp"), 96).await),
		(8, 8)
	);
	assert_eq!(made_of(&setup, STABLE), 2, "made anew");
}

#[tokio::test(start_paused = true)]
async fn a_background_job_cut_off_offline_ends_and_keeps_nothing() {
	let setup = served();
	let image = bmp(8, 8, [0; 3]);
	placed(&setup, &image, false, &image);
	let gate = setup.drive.gate_calls(DriveCall::Thumbnail);
	assert_eq!(
		setup.service.request(&full("a.bmp"), 96).await,
		Err(ThumbnailRefusal::Pending)
	);

	setup.drive.set_offline(true);
	gate.add_permits(1);
	eventually("the work ended", || {
		setup.drive.thumbnails_in_flight().0 == 0
	})
	.await;
	eventually("the job is gone", || {
		setup.service.lane.state().jobs.is_empty()
	})
	.await;
	assert!(!setup.drive.keeps_thumbnail(VERSION, ICON_SIDE));
	assert_eq!(setup.local.notified_items(), vec![]);

	setup.drive.set_offline(false);
	setup.drive.stop_gating(DriveCall::Thumbnail);
	assert_eq!(
		dimensions(setup.service.request(&full("a.bmp"), 96).await),
		(8, 8),
		"offline is not remembered"
	);
	assert_eq!(made_of(&setup, STABLE), 2);
}

/// How many times File Explorer was told the item `name` changed.
fn told(setup: &Setup, name: &str) -> usize {
	let at = root_path(name);
	setup
		.local
		.notified_items()
		.iter()
		.filter(|item| **item == at)
		.count()
}

/// Lets every task ready to run, the background's included, run on the paused clock.
async fn settle() {
	for _ in 0..64 {
		tokio::task::yield_now().await;
	}
}

#[tokio::test(start_paused = true)]
async fn a_larger_size_is_answered_at_once_from_a_smaller_kept_one_and_made_in_the_background() {
	let setup = served();
	let image = bmp(600, 300, [0, 0, 255]);
	placed(&setup, &image, false, &image);
	assert_eq!(
		dimensions(setup.service.request(&full("a.bmp"), 256).await),
		(256, 128)
	);
	let gate = setup.drive.gate_calls(DriveCall::Thumbnail);

	let started = Instant::now();
	let answer = setup.service.request(&full("a.bmp"), 1024).await;

	assert_eq!(
		dimensions(answer),
		(256, 128),
		"the kept 256 as it is, not upscaled"
	);
	assert_eq!(
		started.elapsed(),
		Duration::ZERO,
		"not Pending after a wait"
	);
	eventually("the larger size is made in the background", || {
		setup.drive.thumbnails_in_flight().0 == 1
	})
	.await;
	assert_eq!(made_of(&setup, STABLE), 2);
	assert_eq!(told(&setup, "a.bmp"), 0);

	gate.add_permits(1);
	eventually("File Explorer was told", || told(&setup, "a.bmp") == 1).await;
	assert!(setup.drive.keeps_thumbnail(VERSION, MAX_SIDE));
	let started = Instant::now();
	assert_eq!(
		dimensions(setup.service.request(&full("a.bmp"), 1024).await),
		(600, 300)
	);
	assert_eq!(started.elapsed(), Duration::ZERO);
	settle().await;
	assert_eq!(made_of(&setup, STABLE), 2, "answered from what is kept");
	assert_eq!(told(&setup, "a.bmp"), 1, "told once");
	assert_eq!(setup.drive.reads(), vec![]);
	assert_eq!(setup.local.ops(), vec![]);
}

#[tokio::test(start_paused = true)]
async fn a_smaller_size_is_scaled_down_from_a_larger_kept_one_with_nothing_made() {
	let setup = served();
	let image = bmp(600, 300, [0; 3]);
	placed(&setup, &image, false, &image);
	assert_eq!(
		dimensions(setup.service.request(&full("a.bmp"), 1024).await),
		(600, 300)
	);
	let _gate = setup.drive.gate_calls(DriveCall::Thumbnail);

	let started = Instant::now();
	assert_eq!(
		dimensions(setup.service.request(&full("a.bmp"), 96).await),
		(96, 48)
	);
	assert_eq!(started.elapsed(), Duration::ZERO);
	settle().await;
	assert_eq!(made_of(&setup, STABLE), 1, "nothing made");
	assert!(setup.service.lane.state().jobs.is_empty());
}

#[tokio::test(start_paused = true)]
async fn one_slow_read_serves_both_icon_sizes_and_tells_file_explorer_once() {
	let setup = served();
	let image = bmp(600, 300, [0; 3]);
	placed(&setup, &image, false, &image);
	let gate = setup.drive.gate_calls(DriveCall::Thumbnail);

	// File Explorer asks 96 and then 256, both in flight at once (measured).
	let path = full("a.bmp");
	let (first, second) = (ThumbnailCancel::default(), ThumbnailCancel::default());
	let (small, large) = tokio::join!(
		setup.service.answer_for_explorer(&path, 96, &first),
		setup.service.answer_for_explorer(&path, 256, &second),
	);
	assert_eq!(
		(small, large),
		(
			Err(ThumbnailRefusal::Pending),
			Err(ThumbnailRefusal::Pending)
		)
	);
	assert_eq!(made_of(&setup, STABLE), 1, "one job for both");

	gate.add_permits(1);
	eventually("File Explorer was told", || told(&setup, "a.bmp") == 1).await;
	// It asks again at 256 once told (measured), and the 96 is answered from the same.
	for (side, made) in [(256, (256, 128)), (96, (96, 48))] {
		let answer = setup
			.service
			.answer_for_explorer(&path, side, &ThumbnailCancel::default())
			.await;
		assert_eq!(dimensions(answer), made);
	}
	settle().await;
	assert_eq!(made_of(&setup, STABLE), 1, "one read of the remote file");
	assert_eq!(told(&setup, "a.bmp"), 1, "told once");
}

#[tokio::test(start_paused = true)]
async fn file_explorers_asks_on_a_slow_drive_end_with_bounded_reads_jobs_and_notifications() {
	let setup = served();
	let image = bmp(1200, 600, [0; 3]);
	placed(&setup, &image, false, &image);
	let gate = setup.drive.gate_calls(DriveCall::Thumbnail);
	let path = full("a.bmp");
	let ask = |side: u32| {
		let (service, path) = (&setup.service, &path);
		async move {
			service
				.answer_for_explorer(path, side, &ThumbnailCancel::default())
				.await
		}
	};

	// An icon view: 96 then 256, both Pending on the slow drive.
	let (first, second) = tokio::join!(ask(96), ask(256));
	assert_eq!(first, Err(ThumbnailRefusal::Pending));
	assert_eq!(second, Err(ThumbnailRefusal::Pending));
	let mut answers = Vec::new();
	let mut rounds = 0;
	let mut seen = 0;
	// The drive stays slow: one read a round, each long past Explorer's deadline. After each
	// notification File Explorer asks again at 256, and the preview pane, open since the first
	// one, at 1024.
	loop {
		rounds += 1;
		assert!(rounds < 20, "the asks never ended: {answers:?}");
		tokio::time::advance(EXPLORER_DEADLINE * 4).await;
		gate.add_permits(1);
		settle().await;
		let now = told(&setup, "a.bmp");
		if now == seen {
			if setup.drive.thumbnails_in_flight().0 == 0
				&& setup.service.lane.state().jobs.is_empty()
			{
				break;
			}
			continue;
		}
		seen = now;
		answers.push(ask(256).await);
		answers.push(ask(1024).await);
	}

	assert!(
		answers.iter().all(Result::is_ok),
		"never Pending once a thumbnail is kept: {answers:?}"
	);
	assert_eq!(
		answers.last().cloned().map(dimensions),
		Some((1024, 512)),
		"the preview pane's size was made in the end"
	);
	assert_eq!(made_of(&setup, STABLE), 2, "one read per size kept");
	assert_eq!(
		told(&setup, "a.bmp"),
		2,
		"once per size made in the background"
	);
	assert!(setup.drive.keeps_thumbnail(VERSION, ICON_SIDE));
	assert!(setup.drive.keeps_thumbnail(VERSION, MAX_SIDE));
}

#[tokio::test(start_paused = true)]
async fn every_item_answered_pending_is_told_once_its_thumbnail_is_kept() {
	let setup = served();
	let image = bmp(8, 8, [0; 3]);
	// More than two running and sixteen waiting, the background's former bound.
	let names: Vec<String> = (0..40).map(|at| format!("p{at}.bmp")).collect();
	let versions: Vec<Uuid> = (11..)
		.zip(&names)
		.map(|(stable, name)| placed_other(&setup, name, stable, &image))
		.collect();
	let gate = setup.drive.gate_calls(DriveCall::Thumbnail);
	let config = ThumbnailConfig::default();

	for name in &names {
		assert_eq!(
			setup
				.service
				.answer_for_explorer(&full(name), 96, &ThumbnailCancel::default())
				.await,
			Err(ThumbnailRefusal::Pending),
			"{name}"
		);
		let state = setup.service.lane.state();
		assert!(state.queued.len() <= config.queued);
		assert!(state.jobs.len() <= config.queued + config.running);
	}
	assert_eq!(setup.local.notified_items(), vec![]);

	gate.add_permits(names.len());
	eventually("every item was told", || {
		setup.local.notified_items().len() == names.len()
	})
	.await;
	settle().await;
	for (name, version) in names.iter().zip(&versions) {
		assert_eq!(told(&setup, name), 1, "{name}");
		assert!(setup.drive.keeps_thumbnail(*version, ICON_SIDE), "{name}");
	}
	let state = setup.service.lane.state();
	assert!(state.jobs.is_empty());
	assert!(state.queued.is_empty());
	assert!(state.kept.is_empty(), "nothing of local bytes");
}

#[tokio::test(start_paused = true)]
async fn a_thumbnail_the_cache_did_not_keep_is_not_told() {
	let setup = served();
	let image = bmp(8, 8, [0; 3]);
	placed(&setup, &image, false, &image);
	let gate = setup.drive.gate_calls(DriveCall::Thumbnail);
	assert_eq!(
		setup.service.request(&full("a.bmp"), 256).await,
		Err(ThumbnailRefusal::Pending)
	);
	setup.drive.keep_no_thumbnails(true);

	gate.add_permits(1);
	eventually("the work ended", || {
		setup.service.lane.state().jobs.is_empty()
	})
	.await;
	settle().await;

	assert_eq!(made_of(&setup, STABLE), 1);
	assert!(!setup.drive.keeps_thumbnail(VERSION, ICON_SIDE));
	assert_eq!(
		setup.local.notified_items(),
		vec![],
		"the ask that followed would find nothing kept"
	);
}

#[tokio::test(start_paused = true)]
async fn a_request_waiting_on_the_background_is_answered_and_file_explorer_is_still_told() {
	let setup = served();
	let image = bmp(8, 8, [0; 3]);
	placed(&setup, &image, false, &image);
	let gate = setup.drive.gate_calls(DriveCall::Thumbnail);
	assert_eq!(
		setup.service.request(&full("a.bmp"), 256).await,
		Err(ThumbnailRefusal::Pending)
	);

	let path = full("a.bmp");
	let (answer, ()) = tokio::join!(setup.service.request(&path, 256), async {
		eventually("the request waits on the background job", || {
			setup
				.service
				.lane
				.state()
				.jobs
				.values()
				.any(|job| job.made.receiver_count() == 1)
		})
		.await;
		gate.add_permits(1);
	});

	assert_eq!(dimensions(answer), (8, 8));
	// File Explorer leaves the tile of an item answered Pending blank until it is told, whatever
	// a later ask is answered (measured).
	eventually("File Explorer was told", || told(&setup, "a.bmp") == 1).await;
	// The ask the notification brings is answered from what is kept.
	let started = Instant::now();
	assert_eq!(dimensions(setup.service.request(&path, 256).await), (8, 8));
	assert_eq!(started.elapsed(), Duration::ZERO);
	settle().await;
	assert_eq!(made_of(&setup, STABLE), 1, "one read");
	assert_eq!(told(&setup, "a.bmp"), 1, "told once");
}

/// File Explorer's ask of the item `name` at 256, as it asks once told an item changed.
async fn explorer_ask(setup: &Setup, name: &str) -> Result<Thumbnail, ThumbnailRefusal> {
	setup
		.service
		.answer_for_explorer(&full(name), ICON_SIDE, &ThumbnailCancel::default())
		.await
}

#[tokio::test(start_paused = true)]
async fn items_answered_inside_a_burst_of_notifications_for_others_are_told_once_and_the_asks_end()
{
	const PENDING: usize = 6;
	const OTHERS: usize = 4;
	let setup = served();
	let image = bmp(8, 8, [0; 3]);
	let pending: Vec<String> = (0..PENDING).map(|n| format!("p{n}.bmp")).collect();
	let others: Vec<String> = (0..OTHERS).map(|n| format!("k{n}.bmp")).collect();
	for (stable, name) in (11..).zip(&pending) {
		placed_other(&setup, name, stable, &image);
	}
	for (stable, name) in (51..).zip(&others) {
		placed_other(&setup, name, stable, &image);
	}
	// The other items' thumbnails are made and kept, long before any notification.
	for name in &others {
		assert!(explorer_ask(&setup, name).await.is_ok(), "{name}");
	}
	tokio::time::advance(BURST_WINDOW * 2).await;
	let gate = setup.drive.gate_calls(DriveCall::Thumbnail);
	for name in &pending {
		assert_eq!(
			explorer_ask(&setup, name).await,
			Err(ThumbnailRefusal::Pending),
			"{name}"
		);
	}

	// The measured pattern: the background finishes the items answered Pending and tells File
	// Explorer of each in a burst; it asks other items meanwhile, answered from what is kept, and
	// asks each item it was told of again.
	gate.add_permits(PENDING);
	eventually("every item answered Pending was told", || {
		setup.local.notified_items().len() == PENDING
	})
	.await;
	for name in others.iter().chain(&pending) {
		assert!(explorer_ask(&setup, name).await.is_ok(), "{name}");
	}
	assert_eq!(
		setup.local.notified_items().len(),
		PENDING,
		"nothing more while the burst goes on"
	);
	tokio::time::advance(BURST_SETTLE).await;
	eventually("each item answered inside the burst was told", || {
		setup.local.notified_items().len() == PENDING + OTHERS
	})
	.await;
	// Each item told asks again, inside the notifications of the others, and is answered from what
	// is kept; and again once more, later.
	for round in 0..2 {
		for name in others.iter().chain(&pending) {
			assert!(
				explorer_ask(&setup, name).await.is_ok(),
				"{name}, round {round}"
			);
		}
		tokio::time::advance(BURST_WINDOW * 2).await;
		settle().await;
	}

	assert_eq!(setup.local.notified_items().len(), PENDING + OTHERS);
	for name in others.iter().chain(&pending) {
		assert_eq!(told(&setup, name), 1, "{name}");
	}
	assert!(setup.service.lane.state().burst.sender.is_none());
}

#[tokio::test(start_paused = true)]
async fn an_item_answered_outside_a_burst_is_not_told_and_a_stop_cancels_what_is_owed() {
	let setup = served();
	let image = bmp(8, 8, [0; 3]);
	placed_other(&setup, "quiet.bmp", 11, &image);
	placed_other(&setup, "p.bmp", 12, &image);
	placed_other(&setup, "k.bmp", 13, &image);
	for name in ["quiet.bmp", "k.bmp"] {
		assert!(explorer_ask(&setup, name).await.is_ok(), "{name}");
	}
	// Asked again with no notification near: never told.
	tokio::time::advance(BURST_WINDOW * 2).await;
	assert!(explorer_ask(&setup, "quiet.bmp").await.is_ok());
	tokio::time::advance(BURST_WINDOW * 2).await;
	settle().await;
	assert_eq!(setup.local.notified_items(), vec![]);

	let gate = setup.drive.gate_calls(DriveCall::Thumbnail);
	assert_eq!(
		explorer_ask(&setup, "p.bmp").await,
		Err(ThumbnailRefusal::Pending)
	);
	gate.add_permits(1);
	eventually("the item answered Pending was told", || {
		told(&setup, "p.bmp") == 1
	})
	.await;
	assert!(explorer_ask(&setup, "k.bmp").await.is_ok());
	setup.service.stop_serving();
	tokio::time::advance(BURST_WINDOW * 2).await;
	settle().await;

	assert_eq!(setup.local.notified_items(), vec![root_path("p.bmp")]);
}

#[tokio::test(start_paused = true)]
async fn an_item_answered_just_before_a_burst_is_told_once_it_settles() {
	let setup = served();
	let image = bmp(8, 8, [0; 3]);
	placed_other(&setup, "p.bmp", 11, &image);
	placed_other(&setup, "k.bmp", 12, &image);
	assert!(explorer_ask(&setup, "k.bmp").await.is_ok());
	tokio::time::advance(BURST_WINDOW * 2).await;
	let gate = setup.drive.gate_calls(DriveCall::Thumbnail);
	assert_eq!(
		explorer_ask(&setup, "p.bmp").await,
		Err(ThumbnailRefusal::Pending)
	);
	// Answered inside the window before the notification for `p.bmp`.
	assert!(explorer_ask(&setup, "k.bmp").await.is_ok());
	gate.add_permits(1);
	eventually("the item answered Pending was told", || {
		told(&setup, "p.bmp") == 1
	})
	.await;
	assert_eq!(told(&setup, "k.bmp"), 0, "not inside the burst");
	tokio::time::advance(BURST_SETTLE).await;
	eventually("the item answered before the burst was told", || {
		told(&setup, "k.bmp") == 1
	})
	.await;
	assert!(explorer_ask(&setup, "k.bmp").await.is_ok());
	tokio::time::advance(BURST_WINDOW * 2).await;
	settle().await;
	assert_eq!(setup.local.notified_items().len(), 2);
}

/// HEIF and AVIF images the SDK's and microthumb's tests already read, as nothing in the tree
/// encodes either: a 1000 x 667 photo coded both ways, and a 120 x 80 grid carrying a `thmb` item.
const HEIF_IMAGES: [(&str, &[u8]); 3] = [
	(
		"parrot.heic",
		include_bytes!("../../../filen-sdk-rs/web/test-assets/imgs/parrot.heif"),
	),
	(
		"parrot.avif",
		include_bytes!("../../../filen-sdk-rs/web/test-assets/imgs/parrot.avif"),
	),
	(
		"grid.heif",
		include_bytes!("../../../microthumb/tests/fixtures/heif/hevc-thumb.heic"),
	),
];

/// What a HEIF image's thumbnail at the icon size measures: the photos decoded and fitted into 256
/// pixels, the grid answered from its embedded 160 x 106 `thmb` item, with no tile decoded.
#[cfg(feature = "heif-decoder")]
fn heif_dimensions(name: &str) -> (u32, u32) {
	if name.starts_with("parrot") {
		(256, 170)
	} else {
		(160, 106)
	}
}

#[cfg(feature = "heif-decoder")]
#[tokio::test(start_paused = true)]
async fn heif_and_avif_files_online_only_are_thumbnailed_from_the_drive() {
	let setup = served();
	for (stable, (name, bytes)) in (20..).zip(HEIF_IMAGES) {
		placed_other(&setup, name, stable, bytes);
		let before = setup.local.entry(&root_path(name));

		let answer = setup.service.request(&full(name), 256).await;

		assert_eq!(dimensions(answer), heif_dimensions(name), "{name}");
		// Kept: asked again, it is answered with nothing made.
		let again = setup.service.request(&full(name), 96).await;
		assert!(again.is_ok(), "{name}");
		assert_eq!(made_of(&setup, Uuid::from_u128(stable)), 1, "{name}");
		assert_eq!(setup.local.entry(&root_path(name)), before, "{name}");
	}
	assert_eq!(setup.drive.reads(), vec![]);
	assert_eq!(setup.local.ops(), vec![]);
}

#[cfg(feature = "heif-decoder")]
#[tokio::test(start_paused = true)]
async fn heif_and_avif_files_on_disk_are_thumbnailed_from_their_bytes() {
	let setup = served();
	for (name, bytes) in HEIF_IMAGES {
		setup.local.user_create(&root_path(name), bytes);

		let answer = setup.service.request(&full(name), 256).await;

		assert_eq!(dimensions(answer), heif_dimensions(name), "{name}");
	}
}

#[cfg(not(feature = "heif-decoder"))]
#[tokio::test(start_paused = true)]
async fn heif_and_avif_files_are_unsupported_without_the_decoder() {
	let setup = served();
	for (stable, (name, bytes)) in (20..).zip(HEIF_IMAGES) {
		placed_other(&setup, name, stable, bytes);
		assert_eq!(
			setup.service.request(&full(name), 256).await,
			Err(ThumbnailRefusal::Unsupported),
			"{name} online only"
		);
		let local = format!("local-{name}");
		setup.local.user_create(&root_path(&local), bytes);
		assert_eq!(
			setup.service.request(&full(&local), 256).await,
			Err(ThumbnailRefusal::Unsupported),
			"{name} on disk"
		);
	}
}

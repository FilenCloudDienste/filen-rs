use std::{thread, time::Duration};

use filen_mobile_native_cache::ffi::FfiNonRootObject;
use filen_sdk_rs::io::DeviceRejection;
use filen_types::fs::Uuid;
use tokio::{runtime::RuntimeFlavor, time::sleep};

use super::*;
use crate::{
	names::NameRejection,
	os::RequestedRange,
	populate::SkipReason,
	test_support::{FakeDrive, FakeEvent, FakeOs, ffi_file, pattern, place_files},
};

const ROOT_UUID: Uuid = Uuid::from_u128(0x100);
const ROOT: PlaceholderIdentity = PlaceholderIdentity::Root { root: ROOT_UUID };
const STABLE: Uuid = Uuid::from_u128(0x200);
const VERSION: Uuid = Uuid::from_u128(0x201);
const FILE: PlaceholderIdentity = PlaceholderIdentity::File {
	stable: STABLE,
	version: VERSION,
};
const FILE_VERSION: FileVersion = FileVersion {
	stable: STABLE,
	version: VERSION,
};

/// Records every skip report.
#[derive(Default)]
struct Skips(Mutex<Vec<(PlaceholderIdentity, Vec<SkippedItem>)>>);

impl CloudFilesCallback for Skips {
	fn on_skipped_items(&self, directory: PlaceholderIdentity, items: &[SkippedItem]) {
		lock(&self.0).push((directory, items.to_vec()));
	}

	fn on_local_report(&self, _report: &LocalReport) {}
}

/// A drive holding `FILE` as 10,000 bytes, and an OS holding its placeholder.
fn placed() -> (Arc<FakeDrive>, FakeOs) {
	let drive = FakeDrive::new(ROOT_UUID);
	drive.add_file(STABLE, VERSION, &pattern(10_000));
	let os = FakeOs::with_root(ROOT);
	place_files(&os, &ROOT, &[(STABLE, VERSION, 10_000)]);
	(Arc::new(drive), os)
}

fn dispatcher(drive: Arc<FakeDrive>, permits: usize) -> (Dispatcher<FakeDrive>, Arc<Skips>) {
	let skips = Arc::new(Skips::default());
	let dispatcher = Dispatcher::new(
		drive,
		Handle::current(),
		DispatchConfig {
			max_concurrent_reads: NonZeroUsize::new(permits).unwrap(),
		},
		Arc::clone(&skips) as Arc<dyn CloudFilesCallback>,
	);
	(dispatcher, skips)
}

/// Waits for every task spawned so far to end.
async fn settle(dispatcher: &Dispatcher<FakeDrive>) {
	let mut tasks = mem::take(&mut *lock(&dispatcher.inner.tasks));
	while let Some(result) = tasks.join_next().await {
		result.unwrap();
	}
}

/// Lets every task run until all of them wait: the paused clock advances only once the runtime
/// is idle, so no real time passes.
async fn run_until_idle() {
	sleep(Duration::from_secs(1)).await;
}

/// How many reads the dispatcher holds registered.
fn registered(dispatcher: &Dispatcher<FakeDrive>) -> usize {
	lock(&dispatcher.inner.in_flight)
		.values()
		.map(Vec::len)
		.sum()
}

/// The events after the population that placed the file.
fn events(os: &FakeOs) -> Vec<FakeEvent> {
	os.events().split_off(1)
}

/// The events ending the requests under `key`.
fn ends(os: &FakeOs, key: TransferKey) -> Vec<FakeEvent> {
	events(os)
		.into_iter()
		.filter(|event| match event {
			FakeEvent::Completed(k)
			| FakeEvent::Incomplete(k)
			| FakeEvent::Failed(k, _)
			| FakeEvent::Cancelled(k)
			| FakeEvent::Abandoned(k) => *k == key,
			_ => false,
		})
		.collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_data_returns_while_the_read_is_gated() {
	let (drive, os) = placed();
	let gate = drive.gate_reads();
	let (dispatcher, _) = dispatcher(Arc::clone(&drive), 8);
	let dispatcher = Arc::new(dispatcher);
	let (request, responder) = os.fetch_data(&FILE, (0, 10_000), (0, 0));
	let key = request.transfer();
	// The OS's callback thread: the call returns although the read cannot progress.
	let os_thread = {
		let dispatcher = Arc::clone(&dispatcher);
		thread::spawn(move || dispatcher.on_fetch_data(request, responder))
	};
	os_thread.join().unwrap();
	drive.reads_started(1).await;
	assert_eq!(registered(&dispatcher), 1);
	assert_eq!(events(&os), []);
	gate.add_permits(1);
	settle(&dispatcher).await;
	assert_eq!(ends(&os, key), [FakeEvent::Completed(key)]);
	assert_eq!(registered(&dispatcher), 0);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn a_cancel_aborts_only_the_read_whose_range_it_names() {
	let (drive, os) = placed();
	let gate = drive.gate_reads();
	let (dispatcher, _) = dispatcher(Arc::clone(&drive), 8);
	let (first, first_responder) = os.fetch_data(&FILE, (0, 4096), (0, 0));
	let key = first.transfer();
	// Both reads under one key, as the OS would send two ranges of one transfer.
	let (second, second_responder) = os.fetch_data_under(key, &FILE, (8192, 1808), (0, 0));
	dispatcher.on_fetch_data(first, first_responder);
	dispatcher.on_fetch_data(second, second_responder);
	drive.reads_started(2).await;
	dispatcher.on_cancel_fetch_data(CancelRequest::new(key, 0, 4096));
	run_until_idle().await;
	assert_eq!(ends(&os, key), [FakeEvent::Cancelled(key)]);
	gate.add_permits(1);
	settle(&dispatcher).await;
	assert_eq!(
		ends(&os, key),
		[FakeEvent::Cancelled(key), FakeEvent::Completed(key)]
	);
	let hydrated: Vec<_> = os
		.contents(&FILE)
		.1
		.iter()
		.map(|range| (range.start, range.end))
		.collect();
	assert_eq!(
		hydrated,
		[(8192, 10_000)],
		"only the second read transferred"
	);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn a_cancel_under_another_key_aborts_nothing() {
	let (drive, os) = placed();
	let gate = drive.gate_reads();
	let (dispatcher, _) = dispatcher(Arc::clone(&drive), 8);
	let (request, responder) = os.fetch_data(&FILE, (0, 10_000), (0, 0));
	let key = request.transfer();
	dispatcher.on_fetch_data(request, responder);
	drive.reads_started(1).await;
	dispatcher.on_cancel_fetch_data(CancelRequest::new(TransferKey::new(999), 0, 10_000));
	gate.add_permits(1);
	settle(&dispatcher).await;
	assert_eq!(ends(&os, key), [FakeEvent::Completed(key)]);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn a_cancel_leaves_an_overlapping_read_of_another_range_running() {
	let (drive, os) = placed();
	let gate = drive.gate_reads();
	let (dispatcher, _) = dispatcher(Arc::clone(&drive), 8);
	let (first, first_responder) = os.fetch_data(&FILE, (0, 8192), (0, 0));
	let key = first.transfer();
	// Two readers' requests, overlapping but not equal, share the file's key.
	let (second, second_responder) = os.fetch_data_under(key, &FILE, (4096, 5904), (0, 0));
	dispatcher.on_fetch_data(first, first_responder);
	dispatcher.on_fetch_data(second, second_responder);
	drive.reads_started(2).await;
	dispatcher.on_cancel_fetch_data(CancelRequest::new(key, 0, 8192));
	run_until_idle().await;
	assert_eq!(ends(&os, key), [FakeEvent::Cancelled(key)]);
	gate.add_permits(1);
	settle(&dispatcher).await;
	assert_eq!(
		ends(&os, key),
		[FakeEvent::Cancelled(key), FakeEvent::Completed(key)]
	);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn a_cancel_naming_no_range_fails_every_read_under_its_key_as_transient() {
	let (drive, os) = placed();
	let gate = drive.gate_reads();
	let (dispatcher, _) = dispatcher(Arc::clone(&drive), 8);
	let (first, first_responder) = os.fetch_data(&FILE, (0, 4096), (0, 0));
	let key = first.transfer();
	let (second, second_responder) = os.fetch_data_under(key, &FILE, (8192, 1808), (0, 0));
	dispatcher.on_fetch_data(first, first_responder);
	dispatcher.on_fetch_data(second, second_responder);
	drive.reads_started(2).await;
	dispatcher.on_cancel_fetch_data(CancelRequest::new(key, 0, -1));
	settle(&dispatcher).await;
	assert_eq!(
		ends(&os, key),
		[
			FakeEvent::Failed(key, Failure::Transient),
			FakeEvent::Failed(key, Failure::Transient)
		]
	);
	drop(gate);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn a_cancel_naming_the_range_of_two_reads_fails_both_as_transient() {
	let (drive, os) = placed();
	let gate = drive.gate_reads();
	let (dispatcher, _) = dispatcher(Arc::clone(&drive), 8);
	let (first, first_responder) = os.fetch_data(&FILE, (0, 4096), (0, 0));
	let key = first.transfer();
	// Two requests of one range under the file's key: which one the OS cancelled is unknown.
	let (second, second_responder) = os.fetch_data_under(key, &FILE, (0, 4096), (0, 0));
	dispatcher.on_fetch_data(first, first_responder);
	dispatcher.on_fetch_data(second, second_responder);
	drive.reads_started(2).await;
	dispatcher.on_cancel_fetch_data(CancelRequest::new(key, 0, 4096));
	settle(&dispatcher).await;
	assert_eq!(
		ends(&os, key),
		[
			FakeEvent::Failed(key, Failure::Transient),
			FakeEvent::Failed(key, Failure::Transient)
		]
	);
	drop(gate);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn a_cancel_after_completion_is_a_no_op() {
	let (drive, os) = placed();
	let (dispatcher, _) = dispatcher(drive, 8);
	let (request, responder) = os.fetch_data(&FILE, (0, 10_000), (0, 0));
	let key = request.transfer();
	dispatcher.on_fetch_data(request, responder);
	settle(&dispatcher).await;
	let before = os.events();
	dispatcher.on_cancel_fetch_data(CancelRequest::new(key, 0, 10_000));
	run_until_idle().await;
	assert_eq!(os.events(), before);
	assert_eq!(ends(&os, key), [FakeEvent::Completed(key)]);
	assert_eq!(registered(&dispatcher), 0);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn the_registry_is_empty_after_success_failure_and_cancel() {
	let (drive, os) = placed();
	let gate = drive.gate_reads();
	let (dispatcher, _) = dispatcher(Arc::clone(&drive), 8);
	let (succeeds, responder) = os.fetch_data(&FILE, (0, 10_000), (0, 0));
	let success = succeeds.transfer();
	dispatcher.on_fetch_data(succeeds, responder);
	let (cancelled, responder) = os.fetch_data(&FILE, (0, 4096), (0, 0));
	let cancel = cancelled.transfer();
	dispatcher.on_fetch_data(cancelled, responder);
	let directory = PlaceholderIdentity::Dir {
		uuid: Uuid::from_u128(0x300),
	};
	let (fails, responder) = os.fetch_data_raw(
		directory.encode(),
		RequestedRange::new(10_000, 0, 4096, None),
	);
	let failure = fails.transfer();
	dispatcher.on_fetch_data(fails, responder);
	assert_eq!(registered(&dispatcher), 3);
	drive.reads_started(2).await;
	dispatcher.on_cancel_fetch_data(CancelRequest::new(cancel, 0, 4096));
	// The cancelled read ends before the gate opens: a read whose bytes are ready when the cancel
	// lands completes, as the cache's does.
	run_until_idle().await;
	gate.add_permits(1);
	settle(&dispatcher).await;
	assert_eq!(ends(&os, success), [FakeEvent::Completed(success)]);
	assert_eq!(ends(&os, cancel), [FakeEvent::Cancelled(cancel)]);
	assert_eq!(
		ends(&os, failure),
		[FakeEvent::Failed(failure, Failure::InvalidRequest)]
	);
	assert_eq!(registered(&dispatcher), 0);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn reads_never_exceed_the_configured_permits() {
	let (drive, os) = placed();
	let gate = drive.gate_reads();
	let (dispatcher, _) = dispatcher(Arc::clone(&drive), 2);
	let keys: Vec<_> = (0..4)
		.map(|_| {
			let (request, responder) = os.fetch_data(&FILE, (0, 10_000), (0, 0));
			let key = request.transfer();
			dispatcher.on_fetch_data(request, responder);
			key
		})
		.collect();
	run_until_idle().await;
	assert_eq!(drive.reads().len(), 2);
	gate.add_permits(4);
	settle(&dispatcher).await;
	assert_eq!(drive.reads().len(), 4);
	assert_eq!(drive.max_concurrent_reads(), 2);
	for key in keys {
		assert_eq!(ends(&os, key), [FakeEvent::Completed(key)]);
	}
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn skipped_items_reach_the_callback() {
	let drive = FakeDrive::new(ROOT_UUID);
	drive.list(
		"00000000-0000-0000-0000-000000000100",
		vec![
			FfiNonRootObject::File(ffi_file(STABLE, VERSION, "a:b", 1)),
			FfiNonRootObject::File(ffi_file(
				Uuid::from_u128(0x300),
				Uuid::from_u128(0x301),
				"ok.txt",
				1,
			)),
		],
	);
	let os = FakeOs::with_root(ROOT);
	let (dispatcher, skips) = dispatcher(Arc::new(drive), 8);
	let (request, responder) = os.fetch_placeholders(&ROOT);
	dispatcher.on_fetch_placeholders(request, responder);
	settle(&dispatcher).await;
	let skips = lock(&skips.0);
	let [(directory, items)] = skips.as_slice() else {
		panic!("one report expected: {:?}", skips.len());
	};
	assert_eq!(*directory, ROOT);
	let items: Vec<_> = items
		.iter()
		.map(|item| (item.id(), item.name(), item.reason().clone()))
		.collect();
	assert_eq!(
		items,
		[(
			"00000000-0000-0000-0000-000000000200",
			Some("a:b"),
			SkipReason::Name(NameRejection::Windows(DeviceRejection::ForbiddenChar(':')))
		)]
	);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn shutdown_fails_every_pending_request() {
	let (drive, os) = placed();
	let _gate = drive.gate_reads();
	let (dispatcher, _) = dispatcher(Arc::clone(&drive), 8);
	let keys: Vec<_> = (0..2)
		.map(|_| {
			let (request, responder) = os.fetch_data(&FILE, (0, 10_000), (0, 0));
			let key = request.transfer();
			dispatcher.on_fetch_data(request, responder);
			key
		})
		.collect();
	drive.reads_started(2).await;
	dispatcher.shutdown().await;
	for key in keys {
		assert_eq!(ends(&os, key), [FakeEvent::Abandoned(key)]);
	}
	assert_eq!(registered(&dispatcher), 0);
}

#[tokio::test(start_paused = true)]
async fn a_cancel_adjacent_to_the_read_aborts_nothing() {
	let (drive, os) = placed();
	let gate = drive.gate_reads();
	let (dispatcher, _) = dispatcher(Arc::clone(&drive), 8);
	let (request, responder) = os.fetch_data(&FILE, (0, 4096), (0, 0));
	let key = request.transfer();
	dispatcher.on_fetch_data(request, responder);
	drive.reads_started(1).await;
	dispatcher.on_cancel_fetch_data(CancelRequest::new(key, 4096, 4096));
	gate.add_permits(1);
	settle(&dispatcher).await;
	assert_eq!(ends(&os, key), [FakeEvent::Completed(key)]);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn a_cancel_sharing_one_byte_with_the_read_aborts_nothing() {
	let (drive, os) = placed();
	let gate = drive.gate_reads();
	let (dispatcher, _) = dispatcher(Arc::clone(&drive), 8);
	let (request, responder) = os.fetch_data(&FILE, (0, 4096), (0, 0));
	let key = request.transfer();
	dispatcher.on_fetch_data(request, responder);
	drive.reads_started(1).await;
	dispatcher.on_cancel_fetch_data(CancelRequest::new(key, 4095, 2));
	run_until_idle().await;
	assert_eq!(ends(&os, key), []);
	gate.add_permits(1);
	settle(&dispatcher).await;
	assert_eq!(ends(&os, key), [FakeEvent::Completed(key)]);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn a_cancel_whose_range_was_not_read_fails_every_read_under_its_key_as_transient() {
	let (drive, os) = placed();
	let _gate = drive.gate_reads();
	let (dispatcher, _) = dispatcher(Arc::clone(&drive), 8);
	let (request, responder) = os.fetch_data(&FILE, (0, 4096), (0, 0));
	let key = request.transfer();
	dispatcher.on_fetch_data(request, responder);
	drive.reads_started(1).await;
	dispatcher.on_cancel_fetch_data(CancelRequest::unranged(key));
	settle(&dispatcher).await;
	assert_eq!(ends(&os, key), [FakeEvent::Failed(key, Failure::Transient)]);
	assert_eq!(registered(&dispatcher), 0);
}

#[tokio::test(start_paused = true)]
async fn a_cancel_whose_end_overflows_fails_every_read_under_its_key_as_transient() {
	let (drive, os) = placed();
	let _gate = drive.gate_reads();
	let (dispatcher, _) = dispatcher(Arc::clone(&drive), 8);
	let (first, first_responder) = os.fetch_data(&FILE, (0, 4096), (0, 0));
	let key = first.transfer();
	let (second, second_responder) = os.fetch_data_under(key, &FILE, (8192, 1808), (0, 0));
	dispatcher.on_fetch_data(first, first_responder);
	dispatcher.on_fetch_data(second, second_responder);
	drive.reads_started(2).await;
	dispatcher.on_cancel_fetch_data(CancelRequest::new(key, i64::MAX, 1));
	settle(&dispatcher).await;
	assert_eq!(
		ends(&os, key),
		[
			FakeEvent::Failed(key, Failure::Transient),
			FakeEvent::Failed(key, Failure::Transient)
		]
	);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn a_cancel_of_a_queued_read_ends_it_without_reading() {
	let (drive, os) = placed();
	let gate = drive.gate_reads();
	let (dispatcher, _) = dispatcher(Arc::clone(&drive), 1);
	let (running, responder) = os.fetch_data(&FILE, (0, 10_000), (0, 0));
	let running_key = running.transfer();
	dispatcher.on_fetch_data(running, responder);
	let (queued, responder) = os.fetch_data(&FILE, (0, 4096), (0, 0));
	let queued_key = queued.transfer();
	dispatcher.on_fetch_data(queued, responder);
	drive.reads_started(1).await;
	dispatcher.on_cancel_fetch_data(CancelRequest::new(queued_key, 0, 4096));
	run_until_idle().await;
	// Answered while the read ahead of it still holds the only permit.
	assert_eq!(ends(&os, queued_key), [FakeEvent::Cancelled(queued_key)]);
	assert_eq!(ends(&os, running_key), []);
	assert_eq!(registered(&dispatcher), 1);
	gate.add_permits(1);
	settle(&dispatcher).await;
	assert_eq!(ends(&os, running_key), [FakeEvent::Completed(running_key)]);
	assert_eq!(drive.reads(), [(0, 10_000)]);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn a_cancel_ends_a_held_read_as_cancelled() {
	let (drive, os) = placed();
	drive.hold_file(STABLE);
	let (dispatcher, _) = dispatcher(Arc::clone(&drive), 8);
	let (request, responder) = os.fetch_data(&FILE, (0, 10_000), (0, 0));
	let key = request.transfer();
	dispatcher.on_fetch_data(request, responder);
	drive.reads_started(1).await;
	run_until_idle().await;
	// No gate to open: the read waits until it is cancelled.
	assert_eq!(ends(&os, key), []);
	dispatcher.on_cancel_fetch_data(CancelRequest::new(key, 0, 10_000));
	settle(&dispatcher).await;
	assert_eq!(ends(&os, key), [FakeEvent::Cancelled(key)]);
	assert_eq!(registered(&dispatcher), 0);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn a_cancel_before_the_task_first_runs_aborts_the_read() {
	let (drive, os) = placed();
	let (dispatcher, _) = dispatcher(Arc::clone(&drive), 8);
	let (request, responder) = os.fetch_data(&FILE, (0, 10_000), (0, 0));
	let key = request.transfer();
	// No await between the two: the task has not been polled yet.
	dispatcher.on_fetch_data(request, responder);
	dispatcher.on_cancel_fetch_data(CancelRequest::new(key, 0, 10_000));
	settle(&dispatcher).await;
	assert_eq!(ends(&os, key), [FakeEvent::Cancelled(key)]);
	assert_eq!(drive.reads(), []);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn shutdown_abandons_a_pending_population() {
	let drive = FakeDrive::new(ROOT_UUID);
	let os = FakeOs::with_root(ROOT);
	let (dispatcher, skips) = dispatcher(Arc::new(drive), 8);
	let (request, responder) = os.fetch_placeholders(&ROOT);
	dispatcher.on_fetch_placeholders(request, responder);
	dispatcher.shutdown().await;
	assert_eq!(os.events(), [FakeEvent::PopulateAbandoned { dir: ROOT }]);
	assert!(lock(&skips.0).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_leaves_a_running_read_unanswered_rather_than_cancelled() {
	let (drive, os) = placed();
	let _gate = drive.gate_reads();
	let (dispatcher, _) = dispatcher(Arc::clone(&drive), 8);
	let (request, responder) = os.fetch_data(&FILE, (0, 10_000), (0, 0));
	let key = request.transfer();
	dispatcher.on_fetch_data(request, responder);
	drive.reads_started(1).await;
	dispatcher.shutdown().await;
	assert_eq!(ends(&os, key), [FakeEvent::Abandoned(key)]);
	assert_eq!(registered(&dispatcher), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_callback_returns_from_a_thread_outside_the_runtime() {
	let drive = FakeDrive::new(ROOT_UUID);
	drive.add_file(STABLE, VERSION, &pattern(10_000));
	drive.list(
		"00000000-0000-0000-0000-000000000100",
		vec![FfiNonRootObject::File(ffi_file(
			STABLE, VERSION, "f.bin", 10_000,
		))],
	);
	let drive = Arc::new(drive);
	let gate = drive.gate_reads();
	let os = FakeOs::with_root(ROOT);
	let (dispatcher, _) = dispatcher(Arc::clone(&drive), 8);
	let dispatcher = Arc::new(dispatcher);
	let (request, responder) = os.fetch_placeholders(&ROOT);
	{
		let dispatcher = Arc::clone(&dispatcher);
		thread::spawn(move || dispatcher.on_fetch_placeholders(request, responder))
	}
	.join()
	.unwrap();
	os.wait_for(|event| matches!(event, FakeEvent::Populated { .. }))
		.await;
	let (request, responder) = os.fetch_data(&FILE, (0, 10_000), (0, 0));
	let key = request.transfer();
	{
		let dispatcher = Arc::clone(&dispatcher);
		thread::spawn(move || dispatcher.on_fetch_data(request, responder))
	}
	.join()
	.unwrap();
	drive.reads_started(1).await;
	{
		let dispatcher = Arc::clone(&dispatcher);
		thread::spawn(move || {
			dispatcher.on_cancel_fetch_data(CancelRequest::new(key, 0, 10_000));
		})
	}
	.join()
	.unwrap();
	os.wait_for(|event| *event == FakeEvent::Cancelled(key))
		.await;
	settle(&dispatcher).await;
	drop(gate);
	assert_eq!(registered(&dispatcher), 0);
	os.assert_clean();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dispatcher_on_a_multi_thread_runtime_reports_it() {
	let (dispatcher, _) = dispatcher(Arc::new(FakeDrive::new(ROOT_UUID)), 1);
	assert_eq!(dispatcher.runtime_flavor(), RuntimeFlavor::MultiThread);
}

#[tokio::test]
async fn a_dispatcher_on_a_current_thread_runtime_reports_it() {
	let (dispatcher, _) = dispatcher(Arc::new(FakeDrive::new(ROOT_UUID)), 1);
	assert_eq!(dispatcher.runtime_flavor(), RuntimeFlavor::CurrentThread);
}

/// A dispatcher over `drive` telling a fresh set of hints, which it returns.
fn hinted(drive: Arc<FakeDrive>) -> (Dispatcher<FakeDrive>, Arc<RemoteHints>) {
	let hints = Arc::new(RemoteHints::default());
	let (dispatcher, _) = dispatcher(drive, 8);
	(dispatcher.with_hints(Arc::clone(&hints)), hints)
}

#[tokio::test(start_paused = true)]
async fn a_whole_file_read_marks_its_version_verified() {
	let (drive, os) = placed();
	let (dispatcher, hints) = hinted(drive);
	let (request, responder) = os.fetch_data(&FILE, (0, 10_000), (0, 0));
	dispatcher.on_fetch_data(request, responder);
	settle(&dispatcher).await;
	assert!(hints.is_verified(FILE_VERSION));
	assert_eq!(hints.reads_in_flight(STABLE), 0);
	assert!(hints.last_read(FILE_VERSION).is_some());
}

#[tokio::test(start_paused = true)]
async fn a_windowed_read_is_not_marked_verified() {
	let (drive, os) = placed();
	let (dispatcher, hints) = hinted(drive);
	let (request, responder) = os.fetch_data(&FILE, (0, 4096), (0, 0));
	dispatcher.on_fetch_data(request, responder);
	settle(&dispatcher).await;
	assert!(!hints.is_verified(FILE_VERSION));
}

#[tokio::test(start_paused = true)]
async fn a_read_counts_in_flight_until_it_ends() {
	let (drive, os) = placed();
	let gate = drive.gate_reads();
	let (dispatcher, hints) = hinted(Arc::clone(&drive));
	let (request, responder) = os.fetch_data(&FILE, (0, 4096), (0, 0));
	dispatcher.on_fetch_data(request, responder);
	drive.reads_started(1).await;
	assert_eq!(hints.reads_in_flight(STABLE), 1);
	assert_eq!(hints.last_read(FILE_VERSION), None);
	gate.add_permits(1);
	settle(&dispatcher).await;
	assert_eq!(hints.reads_in_flight(STABLE), 0);
	assert!(hints.last_read(FILE_VERSION).is_some());
}

#[tokio::test(start_paused = true)]
async fn an_own_process_read_leaves_no_last_read() {
	let (drive, os) = placed();
	let (dispatcher, hints) = hinted(drive);
	let (request, responder) = os.fetch_data_from_self(&FILE, (0, 4096), (0, 0));
	dispatcher.on_fetch_data(request, responder);
	settle(&dispatcher).await;
	assert_eq!(hints.last_read(FILE_VERSION), None);
}

#[tokio::test(start_paused = true)]
async fn a_whole_file_mismatch_marks_the_version_damaged() {
	let (drive, os) = placed();
	drive.declare(STABLE, Some([0; 32]));
	let (dispatcher, hints) = hinted(drive);
	let (request, responder) = os.fetch_data(&FILE, (0, 10_000), (0, 0));
	dispatcher.on_fetch_data(request, responder);
	settle(&dispatcher).await;
	assert!(hints.is_damaged(FILE_VERSION));
	assert!(!hints.is_verified(FILE_VERSION));
}

#[tokio::test(start_paused = true)]
async fn a_read_of_a_damaged_version_fails_damaged_without_reading() {
	let (drive, os) = placed();
	let (dispatcher, hints) = hinted(Arc::clone(&drive));
	hints.damaged(FILE_VERSION);
	let (request, responder) = os.fetch_data(&FILE, (0, 4096), (0, 0));
	let key = request.transfer();
	dispatcher.on_fetch_data(request, responder);
	settle(&dispatcher).await;
	assert_eq!(
		ends(&os, key),
		[FakeEvent::Failed(key, Failure::ContentDamaged)]
	);
	assert_eq!(drive.reads(), Vec::new());
	os.assert_clean();
}

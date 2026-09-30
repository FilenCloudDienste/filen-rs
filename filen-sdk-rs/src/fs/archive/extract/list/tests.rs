//! Listings against the fake drive, with the real codec on its thread.

use std::{
	collections::HashSet,
	io::{Cursor, Write},
};

use filen_types::fs::Uuid;

use super::*;
use crate::{
	ErrorKind,
	consts::CHUNK_SIZE,
	fs::{
		archive::{
			extract::test_support::{list, setup, test_config},
			format::StreamCodec,
			test_support::{gzip, incompressible, tar_of, zip_of},
		},
		drive_job::test_support::{Request, wait_until},
	},
	job::test_support::controls,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_zip_is_listed_from_its_index_alone() {
	let big = incompressible(4 * CHUNK_SIZE, 0x77);
	let zip = zip_of(
		&[
			("big.bin", Some(&big)),
			("docs", None),
			("docs/a.txt", Some(b"a")),
		],
		None,
	);
	let setup = setup("bundle.zip", zip.clone(), |_| {});
	let listing = list(&setup, JobControl::default(), test_config());
	let listed = listing.running.await.unwrap().unwrap();

	assert_eq!(listed.format, Some(ArchiveFormat::Zip));
	assert_eq!(listed.password, PasswordCheck::NotNeeded);
	assert_eq!(
		listed
			.entries
			.iter()
			.map(|entry| (
				entry.id.index,
				entry.path.as_ref().map(|path| path.path.as_str()),
				entry.size
			))
			.collect::<Vec<_>>(),
		[
			(0, Some("big.bin"), Some(big.len() as u64)),
			(1, Some("docs"), None),
			(2, Some("docs/a.txt"), Some(1)),
		]
	);
	assert_eq!(
		listed.totals,
		ListTotals {
			entries: 3,
			dirs: 1,
			files: 2,
			bytes: big.len() as u64 + 1,
			skipped: 0,
			bytes_skipped: 0,
		}
	);
	assert_eq!(*listing.recorder.entries.lock().unwrap(), listed.entries);
	// the head to tell the format, and the index in the last chunks: none of the big entry's
	// middle
	let fetched: HashSet<u64> = setup
		.backend
		.log()
		.fetched
		.iter()
		.map(|(_, index)| *index)
		.collect();
	assert!(
		!fetched.contains(&1) && !fetched.contains(&2),
		"{fetched:?}"
	);
	let last = listing
		.recorder
		.updates
		.lock()
		.unwrap()
		.last()
		.unwrap()
		.clone();
	assert_eq!((last.phase, last.entries), (ListPhase::Done, 3));
	assert!(last.bytes_read < zip.len() as u64);
	assert_eq!(listing.reporter.ops_in_flight(), 0);
	assert_eq!(
		setup.backend.memory.available_permits(),
		setup.backend.budget
	);
}

/// A zip of `runs` runs of `per_run` symlinks, each run stored before a chunk of other data,
/// whose index lists the runs' links in turns: taken in index order, every link is in another
/// chunk from the one before it.
fn zip_of_scattered_symlinks(runs: usize, per_run: usize) -> Vec<u8> {
	const EOCD_LEN: usize = 22;
	const CENTRAL_HEADER_LEN: usize = 46;
	let stored = ::zip::write::SimpleFileOptions::default()
		.compression_method(::zip::CompressionMethod::Stored);
	let mut writer = ::zip::ZipWriter::new(Cursor::new(Vec::new()));
	for run in 0..runs {
		for link in 0..per_run {
			writer
				.add_symlink(format!("run{run}/link{link}"), "target", stored)
				.unwrap();
		}
		writer.start_file(format!("filler{run}"), stored).unwrap();
		writer
			.write_all(&incompressible(CHUNK_SIZE, run as u64))
			.unwrap();
	}
	let zip = writer.finish().unwrap().into_inner();
	let u16_at = |at: usize| usize::from(u16::from_le_bytes([zip[at], zip[at + 1]]));
	let u32_at = |at: usize| u32::from_le_bytes(zip[at..at + 4].try_into().unwrap()) as usize;
	let end = zip.len() - EOCD_LEN;
	let (directory_len, directory) = (u32_at(end + 12), u32_at(end + 16));
	let mut records = Vec::new();
	let mut at = directory;
	while at < directory + directory_len {
		let len = CENTRAL_HEADER_LEN + u16_at(at + 28) + u16_at(at + 30) + u16_at(at + 32);
		records.push(&zip[at..at + len]);
		at += len;
	}
	// the n-th entry of every run, then the n+1-th: run r's entries are records r * per..
	let per = per_run + 1;
	let mut scattered = zip[..directory].to_vec();
	for nth in 0..per {
		for run in 0..runs {
			scattered.extend_from_slice(records[run * per + nth]);
		}
	}
	scattered.extend_from_slice(&zip[end..]);
	scattered
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_zips_symlinks_are_listed_reading_it_front_to_back() {
	let zip = zip_of_scattered_symlinks(3, 500);
	let chunks = zip.len().div_ceil(CHUNK_SIZE);
	let setup = setup("links.zip", zip, |_| {});
	let listing = list(&setup, JobControl::default(), test_config());
	let listed = listing.running.await.unwrap().unwrap();

	let links: Vec<&ArchiveEntryKind> = listed
		.entries
		.iter()
		.map(|entry| &entry.kind)
		.filter(|kind| matches!(kind, ArchiveEntryKind::Symlink { .. }))
		.collect();
	let target = ArchiveEntryKind::Symlink {
		target: "target".into(),
	};
	assert_eq!(links, vec![&target; 1500]);
	// its index in the last chunks, then each chunk holding links once: not a fetch per link
	let fetched = setup.backend.log().fetched.len();
	assert!(
		fetched <= 2 * chunks,
		"{fetched} fetches of {chunks} chunks"
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tar_listing_reads_it_all_and_can_be_paused_and_cancelled() {
	let data = incompressible(3 * CHUNK_SIZE, 0x99);
	let tar = gzip(&tar_of(&[("a.bin", &data), ("b.txt", b"b")]));
	let config = test_config();

	// paused while it reads: it gives back its memory, and goes on once resumed
	let paused = setup("bundle.tar.gz", tar.clone(), |_| {});
	paused
		.backend
		.hold_requests(Request::Fetch, [paused.archive.uuid()]);
	let (pause, _cancel, control) = controls();
	let listing = list(&paused, control, config.clone());
	wait_until("a fetch is held", || !paused.backend.log().held.is_empty()).await;
	pause.send_replace(true);
	paused.backend.release_all();
	wait_until("the listing is paused", || listing.reporter.is_paused()).await;
	assert_eq!(
		paused.backend.memory.available_permits(),
		paused.backend.budget
	);
	pause.send_replace(false);
	let listed = listing.running.await.unwrap().unwrap();
	assert_eq!(
		listed.format,
		Some(ArchiveFormat::Tar {
			codec: Some(StreamCodec::Gzip)
		})
	);
	assert_eq!(listed.totals.files, 2);
	let last = listing
		.recorder
		.updates
		.lock()
		.unwrap()
		.last()
		.unwrap()
		.clone();
	assert_eq!(last.bytes_read, tar.len() as u64);

	// cancelled while it reads: what was listed so far comes back with the cancel
	let cancelled = setup("bundle.tar.gz", tar, |_| {});
	cancelled
		.backend
		.hold_requests(Request::Fetch, [cancelled.archive.uuid()]);
	let (_pause, cancel, control) = controls();
	let listing = list(&cancelled, control, config.clone());
	wait_until("a fetch is held", || {
		!cancelled.backend.log().held.is_empty()
	})
	.await;
	cancel.send_replace(true);
	let failed = listing.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::Cancelled);
	let last = listing
		.recorder
		.updates
		.lock()
		.unwrap()
		.last()
		.unwrap()
		.clone();
	assert_eq!(
		(last.phase, last.eta),
		(ListPhase::Cancelled, Some(Duration::ZERO))
	);
	assert_eq!(listing.reporter.ops_in_flight(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_listing_keeps_the_first_entries_and_hands_over_them_all() {
	let names: Vec<String> = (0..MAX_LISTED_ENTRIES + 3)
		.map(|i| format!("f{i:05}"))
		.collect();
	let members: Vec<(&str, &[u8])> = names.iter().map(|name| (name.as_str(), &b""[..])).collect();
	let many = setup("many.tar", tar_of(&members), |_| {});
	let mut config = test_config();
	config.max_members = 2 * MAX_LISTED_ENTRIES as u64;
	let listing = list(&many, JobControl::default(), config);
	let listed = listing.running.await.unwrap().unwrap();

	assert_eq!(listed.entries.len(), MAX_LISTED_ENTRIES);
	assert_eq!(listed.omitted_entries, 3);
	assert_eq!(listed.totals.files, names.len() as u64);
	assert_eq!(listing.recorder.entries.lock().unwrap().len(), names.len());
	let batches = listing.recorder.batches.lock().unwrap().clone();
	assert!(
		batches.iter().all(|&batch| batch <= CALLBACK_BATCH),
		"{batches:?}"
	);

	// long paths fill the listing's bytes before its count
	let dir = vec!["d".repeat(250); 16].join("/");
	let names: Vec<String> = (0..2200).map(|i| format!("{dir}/f{i:04}")).collect();
	let members: Vec<(&str, &[u8])> = names.iter().map(|name| (name.as_str(), &b""[..])).collect();
	let long = setup("long.tar", tar_of(&members), |_| {});
	// a GNU long-name record before each
	let mut config = test_config();
	config.max_members = 3 * names.len() as u64;
	let listing = list(&long, JobControl::default(), config);
	let listed = listing.running.await.unwrap().unwrap();
	// each keeps its stored path and its drive path, some 8 KB in all
	let kept = listed.entries.len();
	assert!(
		kept < names.len() && kept > MAX_LISTED_BYTES / (3 * names[0].len()),
		"{kept}"
	);
	assert_eq!(kept as u64 + listed.omitted_entries, names.len() as u64);
	assert_eq!(listing.recorder.entries.lock().unwrap().len(), names.len());
}

#[test]
fn stated_sizes_add_up_without_overflowing() {
	let entry = |skip| ArchiveEntry {
		id: ArchiveEntryId {
			archive: Uuid::nil(),
			index: 0,
		},
		stored_path: "big.bin".to_owned(),
		stored_path_truncated: false,
		path: Some(ListedPath::plain("big.bin")),
		size: Some(1 << 63),
		modified: None,
		encrypted: false,
		method: None,
		skip,
		mac_metadata: false,
		kind: ArchiveEntryKind::File,
	};
	let mut totals = ListTotals::default();
	for skip in [None, Some(ExtractSkipReason::UnsupportedMethod)] {
		totals.count(&entry(skip.clone()));
		totals.count(&entry(skip));
	}
	assert_eq!((totals.bytes, totals.bytes_skipped), (u64::MAX, u64::MAX));
}

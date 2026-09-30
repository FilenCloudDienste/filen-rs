//! Encrypted zips and 7zs: extracted with their password, refused up front without it or with a
//! wrong one, and cleaned up after when a wrong one only shows once entries were read.

use super::{
	dispose::{disposable, disposition},
	*,
};
use crate::fs::archive::extract::engine::finish::LATE_TRASH_LOCK_WAIT;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extracts_an_encrypted_zip_and_removes_it() {
	let big = pattern(2 * CHUNK_SIZE + 5, 6);
	let zip = zip_of(
		&[
			("docs", None),
			("docs/a.txt", Some(b"alpha")),
			("docs/big.bin", Some(&big)),
		],
		Some("pw"),
	);
	// a zip's entries are checked one by one, so no hash of the whole archive is needed
	let (setup, parent) = disposable(zip, None, |_| {});
	let options = Options {
		dispose: Some(ArchiveDisposal::Remove {
			how: SourceDisposal::DeletePermanently,
			parent,
		}),
		password: Some(ArchivePassword::new("pw".into()).unwrap()),
		..Options::default()
	};
	let job = start(&setup, options);
	let report = job.running.await.unwrap().unwrap();
	assert_eq!(created_dirs(&setup), ["bundle", "docs"]);
	assert_eq!(
		finished(&setup),
		BTreeMap::from([
			("bundle/docs/a.txt".to_owned(), (5, 1, hash(b"alpha"))),
			(
				"bundle/docs/big.bin".to_owned(),
				(big.len() as u64, 3, hash(&big))
			),
		])
	);
	assert!(matches!(
		disposition(&report),
		DisposalOutcome::Disposed { .. }
	));
	assert_eq!(setup.backend.log().deleted_files, [setup.archive.uuid()]);
	assert_released(&setup, &job.reporter, &job.recorder);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_zip_without_its_password_creates_nothing() {
	let zip = zip_of(&[("a.txt", Some(b"a"))], Some("pw"));
	let setup = setup("s.zip", zip, |_| {});
	let failed = start(&setup, Options::default())
		.running
		.await
		.unwrap()
		.unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::ArchivePasswordRequired);
	assert!(created_dirs(&setup).is_empty());
	let options = Options {
		password: Some(ArchivePassword::new("nope".into()).unwrap()),
		..Options::default()
	};
	let failed = start(&setup, options).running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::ArchiveWrongPassword);
	assert!(created_dirs(&setup).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extracts_an_encrypted_7z_and_removes_it() {
	let big = pattern(2 * CHUNK_SIZE + 5, 6);
	let archive = sevenz_of(
		&[
			("docs", None),
			("docs/a.txt", Some(b"alpha")),
			("docs/big.bin", Some(&big)),
		],
		LZMA2,
		true,
		Some((SevenZEncryption::EntriesAndHeaders, "pw")),
	);
	// a 7z's entries are checked one by one, so no hash of the whole archive is needed
	let (setup, parent) = disposable(archive, None, |_| {});
	let options = Options {
		dispose: Some(ArchiveDisposal::Remove {
			how: SourceDisposal::DeletePermanently,
			parent,
		}),
		password: Some(ArchivePassword::new("pw".into()).unwrap()),
		..Options::default()
	};
	let job = start(&setup, options);
	let report = job.running.await.unwrap().unwrap();
	// the files come before their directory's own entry, which then merges with it
	assert_eq!(created_dirs(&setup), ["bundle", "docs"]);
	assert_eq!(
		finished(&setup),
		BTreeMap::from([
			("bundle/docs/a.txt".to_owned(), (5, 1, hash(b"alpha"))),
			(
				"bundle/docs/big.bin".to_owned(),
				(big.len() as u64, 3, hash(&big))
			),
		])
	);
	assert!(matches!(
		disposition(&report),
		DisposalOutcome::Disposed { .. }
	));
	assert_eq!(setup.backend.log().deleted_files, [setup.archive.uuid()]);
	assert_released(&setup, &job.reporter, &job.recorder);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_7z_with_a_wrong_password_creates_nothing() {
	let archive = sevenz_of(
		&[("a.txt", Some(b"a"))],
		LZMA2,
		true,
		Some((SevenZEncryption::EntriesAndHeaders, "pw")),
	);
	let setup = setup("s.7z", archive, |_| {});
	let options = Options {
		password: Some(ArchivePassword::new("nope".into()).unwrap()),
		..Options::default()
	};
	let failed = start(&setup, options).running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::ArchiveWrongPassword);
	assert!(created_dirs(&setup).is_empty());
	assert!(finished(&setup).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_stops_a_codec_deriving_a_7z_key() {
	// the longest password there is, in UTF-16 surrogate pairs: 4 KiB hashed 2^22 times, most of
	// a minute on wasm and seconds here
	let password = "\u{1F600}".repeat(1024);
	let mut archive = sevenz_of(
		&[("a.txt", Some(b"a"))],
		LZMA2,
		true,
		Some((SevenZEncryption::EntriesAndHeaders, &password)),
	);
	// written at 2^4 rounds to be quick to make; the header key is read at 2^22. Its AES coder's
	// properties sit in the plain part of the header: the rounds, then salt and IV
	let aes = [0x06, 0xF1, 0x07, 0x01, 34, 0xC0 | 4, 0xFF];
	let at = archive
		.windows(aes.len())
		.rposition(|window| window == aes)
		.expect("the header's AES coder");
	archive[at + 5] = 0xC0 | MAX_CYCLES_POWER;
	// the start header's CRC-32 of the header, then its own
	let next =
		32 + usize::try_from(u64::from_le_bytes(archive[12..20].try_into().unwrap())).unwrap();
	let len = usize::try_from(u64::from_le_bytes(archive[20..28].try_into().unwrap())).unwrap();
	let crc = crc32fast::hash(&archive[next..next + len]);
	archive[28..32].copy_from_slice(&crc.to_le_bytes());
	let crc = crc32fast::hash(&archive[12..32]);
	archive[8..12].copy_from_slice(&crc.to_le_bytes());

	let setup = setup("slow.7z", archive, |_| {});
	let (_pause, cancel, control) = controls();
	let codec = Arc::new(Mutex::new(None));
	let job = {
		let codec = Arc::clone(&codec);
		let options = Options {
			control,
			password: Some(ArchivePassword::new(password).unwrap()),
			..Options::default()
		};
		let job = stream_job(&setup, &options);
		start_with(
			&setup,
			options,
			Box::new(move || {
				let link = worker::start(move |port| extract_stream(&port, job))?;
				*codec.lock().unwrap() = Some(Arc::clone(&link.shared));
				Ok(link)
			}),
		)
	};
	wait_until("the codec starts", || codec.lock().unwrap().is_some()).await;
	let shared = codec.lock().unwrap().take().unwrap();
	// the codec shows it is alive while it derives, which exchanges nothing with the driver:
	// past the two exchanges each of the archive's chunks takes
	let fetching = 2 * setup.archive.size().div_ceil(CHUNK_SIZE as u64);
	wait_until("the codec derives the key", || shared.progress() > fetching).await;
	cancel.send_replace(true);
	let failed = job.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::Cancelled);
	// and stops deriving once the job ended, whatever the hashing speed: its thread lets go of
	// what it shared (a derivation run to its end would too, eventually), having shown progress
	// at most once more, for the 2^16 rounds under way when the job ended
	let at_end = shared.progress();
	wait_until("the codec stops", || Arc::strong_count(&shared) == 1).await;
	assert!(
		shared.progress() - at_end <= 1,
		"the codec derived on for {} more checks",
		shared.progress() - at_end
	);
	assert!(created_dirs(&setup).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_password_found_late_trashes_the_directories_it_left() {
	// too large (incompressible, so compressed too) to check the password on up front: it
	// shows once the entry is opened
	let big = incompressible(17 << 20, 0x9E37_79B9_7F4A_7C15);
	let zip = zip_of(
		&[("docs", None), ("docs/big.bin", Some(&big))],
		Some("right"),
	);
	// the entry's first chunk comes slowest, long after the folder is created
	let setup = setup("bundle.zip", zip, |backend| {
		backend.quirks.insert(Quirk::ReverseChunks);
	});
	let options = Options {
		password: Some(ArchivePassword::new("wrong".into()).unwrap()),
		..Options::default()
	};
	let job = start(&setup, options);
	let failed = job.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::ArchiveWrongPassword);
	assert!(finished(&setup).is_empty());
	// the entry failed to open before it was announced, so the job never started it; the
	// folders count as created, trashed since
	assert_eq!(
		failed.report.counts,
		ItemCounts {
			dirs_created: 2,
			..ItemCounts::default()
		}
	);
	let log = setup.backend.log();
	let root = log
		.created_dirs
		.iter()
		.find(|(_, name)| name == "bundle")
		.map(|(uuid, _)| *uuid)
		.expect("the new folder was created before the password showed wrong");
	assert_eq!(
		log.trashed_dirs,
		[root],
		"the folder, with everything in it, is trashed"
	);
	assert_eq!(
		job.recorder.top_level.lock().unwrap().len(),
		1,
		"the callback got the folder"
	);
	assert!(
		failed.report.top_level.is_empty(),
		"the report no longer lists it as created"
	);
	let trashed: Vec<Uuid> = job
		.recorder
		.updates
		.lock()
		.unwrap()
		.iter()
		.flat_map(|update| &update.events)
		.filter_map(|event| match event {
			ExtractEvent::TopLevelTrashed { dest_uuid } => Some(*dest_uuid),
			_ => None,
		})
		.collect();
	assert_eq!(trashed, [root], "and is told it went to the trash");
}

#[tokio::test(start_paused = true)]
async fn a_wrong_password_found_late_keeps_the_directories_when_the_lock_does_not_come() {
	let setup = setup("bundle.tar", tar_of(&[("a.txt", b"a")]), |_| {});
	let (events, result, link) = worker::test_support::scripted::<CodecResult>();
	let job = start_with(&setup, Options::default(), Box::new(move || Ok(link)));
	events
		.send(WorkerEvent::Opened(ArchiveFormat::Tar { codec: None }))
		.await
		.unwrap();
	events.send(dir_entry(0, "docs")).await.unwrap();
	wait_until("the folders are created", || {
		created_dirs(&setup) == ["bundle", "docs"]
	})
	.await;
	// another client holds the lock from now on
	let calls = setup.backend.lock_calls.load(Ordering::SeqCst);
	setup.backend.block_locks_from.send_replace(Some(calls));
	drop(events);
	result
		.send(Err(Error::custom(ErrorKind::ArchiveWrongPassword, "wrong")))
		.unwrap();
	let failed = tokio::time::timeout(2 * LATE_TRASH_LOCK_WAIT, job.running)
		.await
		.expect("the wait for the lock ends")
		.unwrap()
		.unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::ArchiveWrongPassword);
	let log = setup.backend.log();
	assert_eq!(log.lock_waits, 1, "the job waited for the lock");
	assert!(log.trashed_dirs.is_empty());
	assert_eq!(
		failed.report.top_level.len(),
		1,
		"the report still lists the folder it kept"
	);
}

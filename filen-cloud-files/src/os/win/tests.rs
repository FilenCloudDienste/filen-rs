//! The struct reads of `ffi` against structs a test builds, with each size cfapi might report, and
//! the security descriptors and bitmaps it makes: run on Windows only, as the cfapi types exist
//! only there.

use std::{
	env,
	fs::{self, OpenOptions},
	mem::{offset_of, size_of},
	os::windows::fs::{FileExt, OpenOptionsExt},
	path::{Path, PathBuf},
	process::Command,
	sync::Arc,
};

use filen_types::fs::Uuid;

use windows_sys::Win32::Storage::{
	CloudFilters::{
		CF_CALLBACK_INFO, CF_CALLBACK_PARAMETERS, CF_CALLBACK_PARAMETERS_0,
		CF_CALLBACK_PARAMETERS_0_0, CF_CALLBACK_PARAMETERS_0_0_0, CF_CALLBACK_PARAMETERS_0_0_0_0,
		CF_CALLBACK_PARAMETERS_0_1, CF_CALLBACK_PARAMETERS_0_6,
	},
	FileSystem::FILE_READ_ATTRIBUTES,
};

use super::{
	ConnectedRoot, ProviderInfo, SyncRootPath, WinLocalFs,
	callbacks::Cancel,
	ffi::{self, AbsoluteDescriptor, DescriptorEntries, DibError},
	register, unregister_path,
};
use crate::{
	dispatch::{CloudFilesCallback, DispatchConfig, Dispatcher},
	identity::PlaceholderIdentity,
	local::{LocalConfig, LocalEngine, LocalReport},
	names::PlaceholderName,
	os::{ByteRange, LocalEntry, LocalError, LocalFileId, LocalFs, OptionalRange, RequestedRange},
	placeholder::{FileSize, FileTime, PlaceholderSpec},
	populate::SkippedItem,
	root_path::RootPath,
	test_support::{FakeDrive, pattern},
};

/// Set in the child process [`run_alone`] runs a test in.
const ALONE: &str = "FILEN_CLOUD_FILES_COM_TEST_ALONE";

/// Whether this is the child process [`run_alone`] started, which then runs the test's body.
/// Otherwise runs `test`, the test of that name in `module` (its `module_path!()`), alone in a
/// child process of this test binary, checks that it passed, and answers false. For the tests that
/// need the process to themselves: nothing else in it runs meanwhile.
pub(super) fn run_alone(module: &str, test: &str) -> bool {
	if env::var_os(ALONE).is_some() {
		return true;
	}
	let name = format!("{}::{test}", module.split_once("::").unwrap().1);
	let output = Command::new(env::current_exe().unwrap())
		.args([name.as_str(), "--exact", "--test-threads=1"])
		.env(ALONE, "1")
		.output()
		.unwrap();
	let stdout = String::from_utf8_lossy(&output.stdout);
	assert!(output.status.success(), "{stdout}");
	assert!(stdout.contains("1 passed"), "{stdout}");
	false
}

/// `size` as the `u32` a cfapi struct's size field takes.
fn size(size: usize) -> u32 {
	u32::try_from(size).unwrap()
}

/// A callback info reporting `struct_size` bytes, with keys 7, 9 and 3 and a file of 10,000 bytes.
fn info(struct_size: usize) -> CF_CALLBACK_INFO {
	CF_CALLBACK_INFO {
		StructSize: size(struct_size),
		ConnectionKey: 7,
		TransferKey: 9,
		RequestKey: 3,
		FileSize: 10_000,
		..CF_CALLBACK_INFO::default()
	}
}

/// A `FETCH_DATA` callback's parameters reporting `param_size` bytes.
fn fetch(param_size: usize) -> CF_CALLBACK_PARAMETERS {
	CF_CALLBACK_PARAMETERS {
		ParamSize: size(param_size),
		Anonymous: CF_CALLBACK_PARAMETERS_0 {
			FetchData: CF_CALLBACK_PARAMETERS_0_1 {
				Flags: 0,
				RequiredFileOffset: 4096,
				RequiredLength: 8192,
				OptionalFileOffset: 12_288,
				OptionalLength: 1000,
				LastDehydrationTime: 0,
				LastDehydrationReason: 0,
			},
		},
	}
}

/// A `CANCEL_FETCH_DATA` callback's parameters reporting `param_size` bytes.
fn cancel(param_size: usize) -> CF_CALLBACK_PARAMETERS {
	CF_CALLBACK_PARAMETERS {
		ParamSize: size(param_size),
		Anonymous: CF_CALLBACK_PARAMETERS_0 {
			Cancel: CF_CALLBACK_PARAMETERS_0_0 {
				Flags: 2,
				Anonymous: CF_CALLBACK_PARAMETERS_0_0_0 {
					FetchData: CF_CALLBACK_PARAMETERS_0_0_0_0 {
						FileOffset: 4096,
						Length: 8192,
					},
				},
			},
		},
	}
}

/// Where the union of the parameters starts.
const PARAMS: usize = offset_of!(CF_CALLBACK_PARAMETERS, Anonymous);

#[test]
fn a_full_callback_info_gives_its_keys_and_size() {
	let (keys, file_size) = ffi::keys_of(&info(size_of::<CF_CALLBACK_INFO>())).unwrap();
	assert_eq!((keys.all(), file_size), ([7, 9, 3], 10_000));
}

#[test]
fn a_callback_info_short_of_its_request_key_takes_the_default_one() {
	let (keys, _) = ffi::keys_of(&info(offset_of!(CF_CALLBACK_INFO, RequestKey))).unwrap();
	assert_eq!(keys.all(), [7, 9, 0]);
}

#[test]
fn a_callback_info_short_of_its_transfer_key_cannot_be_answered() {
	let struct_size = offset_of!(CF_CALLBACK_INFO, TransferKey) + 4;
	assert!(ffi::keys_of(&info(struct_size)).is_none());
}

#[test]
fn fetch_parameters_reaching_the_optional_length_give_the_range() {
	let wanted = Some(RequestedRange::new(
		10_000,
		4096,
		8192,
		OptionalRange::new(12_288, 1000),
	));
	let through_optional = PARAMS + offset_of!(CF_CALLBACK_PARAMETERS_0_1, OptionalLength) + 8;
	assert_eq!(
		ffi::fetch_params_of(&fetch(through_optional), 10_000),
		wanted
	);
	assert_eq!(
		ffi::fetch_params_of(&fetch(size_of::<CF_CALLBACK_PARAMETERS>()), 10_000),
		wanted
	);
}

#[test]
fn fetch_parameters_short_of_the_optional_length_give_no_range() {
	let short = PARAMS + offset_of!(CF_CALLBACK_PARAMETERS_0_1, OptionalLength) + 7;
	assert_eq!(ffi::fetch_params_of(&fetch(short), 10_000), None);
	assert_eq!(ffi::fetch_params_of(&fetch(4), 10_000), None);
}

#[test]
fn cancel_parameters_reaching_the_length_give_the_range() {
	let through_length = PARAMS + 8 + 16;
	assert_eq!(
		ffi::cancel_params_of(&cancel(through_length)),
		Some(Cancel {
			flags: 2,
			offset: 4096,
			length: 8192
		})
	);
	assert_eq!(ffi::cancel_params_of(&cancel(through_length - 1)), None);
}

/// A `NOTIFY_DEHYDRATE` callback's parameters reporting `param_size` bytes: background, low space.
fn dehydrate(param_size: usize) -> CF_CALLBACK_PARAMETERS {
	CF_CALLBACK_PARAMETERS {
		ParamSize: size(param_size),
		Anonymous: CF_CALLBACK_PARAMETERS_0 {
			Dehydrate: CF_CALLBACK_PARAMETERS_0_6 {
				Flags: 1,
				Reason: 2,
			},
		},
	}
}

#[test]
fn dehydrate_params_read_only_when_the_struct_holds_flags_and_reason() {
	let base = offset_of!(CF_CALLBACK_PARAMETERS, Anonymous);
	let whole = base + size_of::<CF_CALLBACK_PARAMETERS_0_6>();
	assert_eq!(ffi::dehydrate_params_of(&dehydrate(whole)), Some((1, 2)));
	assert_eq!(ffi::dehydrate_params_of(&dehydrate(whole - 1)), None);
	assert_eq!(ffi::dehydrate_params_of(&dehydrate(base + 4)), None);
}

#[test]
fn a_directory_in_no_sync_root_has_no_registration() {
	let temp = tempfile::tempdir().unwrap();
	let path = SyncRootPath::from_path(temp.path()).unwrap();
	assert_eq!(super::sync_root::sync_root_info(&path).unwrap(), None);
}

#[test]
fn a_missing_directory_is_read_on_its_nearest_existing_ancestor() {
	let temp = tempfile::tempdir().unwrap();
	let missing = temp.path().join("a").join("b");
	let (file_system, at) = super::volume_file_system(&missing).unwrap();
	assert_eq!(at, temp.path());
	// The temporary directory of every Windows this runs on is on the system volume.
	assert_eq!(file_system, "NTFS");
}

#[test]
fn an_sddl_string_becomes_the_absolute_descriptor_it_spells() {
	let system: Vec<u16> = "S-1-5-18".encode_utf16().collect();
	let descriptor = AbsoluteDescriptor::from_sddl("O:SYG:SYD:(A;;0x1;;;SY)").unwrap();
	assert_eq!(
		ffi::descriptor_entries_for_test(&descriptor),
		DescriptorEntries {
			owner: system.clone(),
			group: system.clone(),
			dacl: vec![(0, 0x1, system)],
		}
	);
}

#[test]
fn a_string_that_is_no_sddl_is_refused() {
	assert!(AbsoluteDescriptor::from_sddl("no descriptor").is_err());
}

#[test]
fn a_dib_section_holds_the_pixels_top_down() {
	// Two rows of one pixel, each its own colour.
	let pixels = [1, 2, 3, 4, 5, 6, 7, 8];
	let bitmap = ffi::dib_section(1, 2, &pixels).unwrap().into_raw();
	let bits = ffi::dib_bits_for_test(bitmap).unwrap();
	assert_eq!((bits.width, bits.height, bits.bits_per_pixel), (1, 2, 32));
	// The source's first row is the first row in memory, and the top row drawn.
	assert_eq!(bits.bytes, pixels);
	assert_eq!(
		ffi::first_column_for_test(bitmap, 2),
		[0x0001_0203, 0x0005_0607]
	);
	// Handed out, so the test's.
	assert!(ffi::delete_bitmap_for_test(bitmap));
}

#[test]
fn a_dib_section_refuses_pixels_that_do_not_match_the_size() {
	assert_eq!(ffi::dib_section(2, 2, &[0; 8]).err(), Some(DibError::Size));
	assert_eq!(ffi::dib_section(0, 1, &[]).err(), Some(DibError::Size));
}

#[test]
fn a_section_not_handed_out_is_deleted() {
	// Alone: the deleted handle is read again, which a section another test makes meanwhile could
	// take.
	if !run_alone(module_path!(), "a_section_not_handed_out_is_deleted") {
		return;
	}
	let section = ffi::dib_section(1, 1, &[1, 2, 3, 4]).unwrap();
	let bitmap = section.bitmap();
	assert!(ffi::dib_bits_for_test(bitmap).is_some());
	drop(section);
	assert_eq!(ffi::dib_bits_for_test(bitmap), None);
}

/// The provider id of the range test's own registration, so it never collides with Filen's.
const RANGE_TEST_PROVIDER: u128 = 0x6A0E_3B1D_57C2_4E19_9F43_0D2B_8C7A_51E6;

/// Ignores what a served root's provider reports.
struct Quiet;

impl CloudFilesCallback for Quiet {
	fn on_skipped_items(&self, _directory: PlaceholderIdentity, _items: &[SkippedItem]) {}

	fn on_local_report(&self, _report: &LocalReport) {}
}

/// Unregisters the sync root at its path when dropped, so a failed test leaves none behind.
struct Registration(SyncRootPath);

impl Drop for Registration {
	fn drop(&mut self) {
		if let Err(error) = unregister_path(&self.0) {
			eprintln!("unregistering the test's sync root failed: {error}");
		}
	}
}

/// Reads `len` bytes of `path` at `offset`, through a handle shared with every other.
fn read_at(path: &Path, offset: u64, len: usize) {
	let file = OpenOptions::new()
		.read(true)
		.share_mode(7)
		.open(path)
		.unwrap();
	let mut buffer = vec![0; len];
	let mut done = 0;
	while done < len {
		let read = file
			.seek_read(&mut buffer[done..], offset + u64::try_from(done).unwrap())
			.unwrap();
		assert!(read > 0, "the file ended at {offset} + {done}");
		done += read;
	}
	assert_eq!(
		buffer,
		pattern(usize::try_from(offset).unwrap() + len)[usize::try_from(offset).unwrap()..]
	);
}

/// A file placeholder of a sync root of its own in the user's temporary directory, served from the
/// fake drive: registered, connected, unregistered once dropped.
struct ServedFile {
	connected: ConnectedRoot,
	local: Arc<WinLocalFs>,
	/// The file's full path.
	full: PathBuf,
	path: RootPath,
	id: LocalFileId,
	// Dropped in this order: the registration before the directory it is of.
	_registration: Registration,
	_temp: tempfile::TempDir,
	_runtime: tokio::runtime::Runtime,
}

impl ServedFile {
	/// A placeholder of `size` bytes nothing read yet, whose drive bytes end at `served`: every
	/// fetch past it fails.
	fn new(size: u64, served: u64) -> Self {
		let runtime = tokio::runtime::Builder::new_multi_thread()
			.worker_threads(2)
			.enable_all()
			.build()
			.unwrap();
		let temp = tempfile::tempdir().unwrap();
		let dir = temp.path().join("root");
		fs::create_dir(&dir).unwrap();
		let account = Uuid::from_u128(0x5100);
		let (stable, version) = (Uuid::from_u128(0x5101), Uuid::from_u128(0x5102));
		let provider = ProviderInfo::new("FilenRangeTest", "0.1", RANGE_TEST_PROVIDER).unwrap();
		let registered =
			register(SyncRootPath::from_path(&dir).unwrap(), &provider, account).unwrap();
		let registration = Registration(SyncRootPath::from_path(&dir).unwrap());
		let drive = Arc::new(FakeDrive::new(account));
		drive.add_file(stable, version, &pattern(usize::try_from(size).unwrap()));
		drive.truncate_at(stable, served);
		let local = Arc::new(WinLocalFs::new(registered.path()));
		let engine = Arc::new(LocalEngine::new(
			Arc::clone(&drive),
			Arc::clone(&local),
			runtime.handle().clone(),
			LocalConfig::default(),
			Arc::new(Quiet),
		));
		let dispatcher = Arc::new(Dispatcher::new(
			Arc::clone(&drive),
			runtime.handle().clone(),
			DispatchConfig::default(),
			Arc::new(Quiet),
		));
		let connected = registered.connect_with_local(dispatcher, engine).unwrap();
		let at = FileTime::from_unix_millis(1_700_000_000_000).unwrap();
		let spec = PlaceholderSpec::file(
			PlaceholderName::parse("a.bin").unwrap(),
			stable,
			version,
			FileSize::from_i64(i64::try_from(size).unwrap()).unwrap(),
			at,
			at,
		);
		local.create_placeholder(&RootPath::root(), &spec).unwrap();
		let path = RootPath::parse(["a.bin"]).unwrap();
		let LocalEntry::Placeholder { id, .. } = local.inspect(&path).unwrap() else {
			panic!("a placeholder");
		};
		Self {
			connected,
			local,
			full: dir.join("a.bin"),
			path,
			id,
			_registration: registration,
			_temp: temp,
			_runtime: runtime,
		}
	}

	/// How many bytes of the file the platform says are on disk.
	fn on_disk(&self) -> u64 {
		let LocalEntry::Placeholder { state, .. } = self.local.inspect(&self.path).unwrap() else {
			panic!("a placeholder");
		};
		state.on_disk()
	}

	/// The ranges on disk, as the abandon reads them.
	fn ranges(&self) -> Result<Vec<ByteRange>, LocalError> {
		self.local.on_disk_ranges(&self.path, self.id)
	}
}

// Registers a sync root of its own in the user's temporary directory and serves it from the fake
// drive, which a build host's account may not be allowed to: run in the VM with `--ignored`.
#[test]
#[ignore = "registers a sync root; run in a Windows VM as the logged-on user"]
fn a_placeholder_read_in_part_reports_the_range_on_disk() {
	const SIZE: u64 = 16 << 20;
	// The drive's bytes end here, so the platform's progressive hydration, which goes on fetching
	// after a read is served, stops with the file partly on disk.
	const SERVED: u64 = 3 << 20;
	let file = ServedFile::new(SIZE, SERVED);
	assert_eq!(file.ranges(), Ok(vec![]));
	// Read up to where the drive's bytes end: on disk once the read returns, and nothing after it
	// ever is, as every fetch past it fails.
	read_at(&file.full, 0, usize::try_from(SERVED).unwrap());
	assert_eq!(file.on_disk(), SERVED);
	assert_eq!(
		file.ranges(),
		Ok(vec![ByteRange {
			offset: 0,
			len: SERVED
		}])
	);
	file.connected.disconnect().unwrap();
}

// As above: run in the VM with `--ignored`.
#[test]
#[ignore = "registers a sync root; run in a Windows VM as the logged-on user"]
fn a_placeholder_of_a_size_short_of_a_whole_block_reports_its_range_on_disk_up_to_its_end() {
	const SIZE: u64 = (3 << 20) + 5000;
	let file = ServedFile::new(SIZE, SIZE);
	read_at(&file.full, 0, usize::try_from(SIZE).unwrap());
	let whole = vec![ByteRange {
		offset: 0,
		len: SIZE,
	}];
	// The last block counts only its bytes in the file, on disk as in the range.
	assert_eq!(file.on_disk(), SIZE);
	assert_eq!(file.ranges(), Ok(whole.clone()));
	// Asked past the end, the platform reports the range as it is: never rounded up to the block.
	let attributes = OpenOptions::new()
		.access_mode(FILE_READ_ATTRIBUTES)
		.share_mode(7)
		.open(&file.full)
		.unwrap();
	assert_eq!(
		ffi::on_disk_ranges(&attributes, SIZE + 8192),
		Ok(whole.clone())
	);
	drop(attributes);
	// Disconnected, the provider sees the file disguised and the range query answers the same.
	let ServedFile {
		connected,
		local,
		path,
		id,
		..
	} = file;
	connected.disconnect().unwrap();
	assert_eq!(local.on_disk_ranges(&path, id), Ok(whole));
}

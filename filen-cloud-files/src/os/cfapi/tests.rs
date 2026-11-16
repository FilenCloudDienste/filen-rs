use filen_types::fs::Uuid;

use super::*;
use crate::{
	identity::PlaceholderIdentity,
	names::PlaceholderName,
	placeholder::{FileSize, FileTime},
};

fn wide(s: &str) -> Vec<u16> {
	s.encode_utf16().collect()
}

#[test]
fn a_win32_code_becomes_its_facility_7_hresult() {
	assert_eq!(
		Hresult::from_win32(398).get(),
		0x8007_018E_u32.cast_signed()
	);
	assert_eq!(Hresult::from_win32(0).get(), 0);
	assert_eq!(
		Hresult::from_win32(0x0012_0057).get(),
		0x8007_0057_u32.cast_signed()
	);
}

#[test]
fn an_hresult_is_ok_while_its_severity_bit_is_clear() {
	assert!(Hresult::new(0).is_ok());
	assert!(Hresult::new(1).is_ok());
	assert!(!Hresult::new(E_INVALIDARG).is_ok());
}

#[test]
fn an_hresult_prints_in_hex_with_its_name_when_known() {
	assert_eq!(
		Hresult::new(E_INVALIDARG).to_string(),
		"0x80070057 (E_INVALIDARG)"
	);
	assert_eq!(
		Hresult::from_win32(398).to_string(),
		"0x8007018e (ERROR_CLOUD_FILE_REQUEST_CANCELED)"
	);
	assert_eq!(Hresult::from_win32(1312).to_string(), "0x80070520");
	assert_eq!(Hresult::new(0).to_string(), "0x00000000");
}

#[test]
fn a_win_error_names_its_call_and_code() {
	let error = WinError {
		call: CfCall::Register,
		code: Hresult::new(E_INVALIDARG),
	};
	assert_eq!(
		error.to_string(),
		"Register failed: 0x80070057 (E_INVALIDARG)"
	);
}

#[test]
fn a_drive_path_is_a_sync_root_with_one_nul_appended() {
	let root = sync_root_units(&wide(r"C:\Users\me\Filen")).unwrap();
	assert_eq!(root.without_nul(), wide(r"C:\Users\me\Filen").as_slice());
	assert_eq!(root.with_nul().last(), Some(&0));
	assert_eq!(root.with_nul().len(), r"C:\Users\me\Filen".len() + 1);
}

#[test]
fn a_verbatim_drive_path_is_a_sync_root() {
	let root = sync_root_units(&wide(r"\\?\d:\Filen")).unwrap();
	assert_eq!(root.without_nul(), wide(r"\\?\d:\Filen").as_slice());
}

#[test]
fn a_path_off_a_local_drive_is_no_sync_root() {
	for path in [
		r"Filen",
		r"C:Filen",
		r"C:/Filen",
		r"\Filen",
		r"\\server\share\Filen",
		r"\\.\C:\Filen",
		r"\\?\UNC\server\share",
		r"1:\Filen",
	] {
		assert_eq!(
			sync_root_units(&wide(path)),
			Err(SyncRootPathError::NotLocalAbsolute),
			"{path}"
		);
	}
}

#[test]
fn an_empty_path_is_no_sync_root() {
	assert_eq!(sync_root_units(&[]), Err(SyncRootPathError::Empty));
}

#[test]
fn a_path_holding_a_nul_is_no_sync_root() {
	let mut units = wide(r"C:\Fi");
	units.push(0);
	units.extend(wide("len"));
	assert_eq!(sync_root_units(&units), Err(SyncRootPathError::InteriorNul));
}

#[test]
fn a_provider_string_takes_up_to_its_limit_in_utf16_units() {
	let at_limit = "a".repeat(MAX_PROVIDER_NAME_UNITS);
	let units = provider_units(&at_limit, MAX_PROVIDER_NAME_UNITS).unwrap();
	assert_eq!(units.with_nul().len(), MAX_PROVIDER_NAME_UNITS + 1);
	let over = "a".repeat(MAX_PROVIDER_NAME_UNITS + 1);
	assert_eq!(
		provider_units(&over, MAX_PROVIDER_NAME_UNITS),
		Err(ProviderInfoError::TooLong { units: 256 })
	);
}

#[test]
fn a_provider_string_counts_a_surrogate_pair_as_two_units() {
	// 128 emoji are 256 units, though only 128 characters.
	let emoji = "\u{1F600}".repeat(128);
	assert_eq!(
		provider_units(&emoji, MAX_PROVIDER_VERSION_UNITS),
		Err(ProviderInfoError::TooLong { units: 256 })
	);
}

#[test]
fn an_empty_or_nul_holding_provider_string_is_refused() {
	assert_eq!(provider_units("", 255), Err(ProviderInfoError::Empty));
	assert_eq!(
		provider_units("Fil\0en", 255),
		Err(ProviderInfoError::InteriorNul)
	);
}

#[test]
fn a_wide_string_debugs_as_its_text() {
	let units = provider_units("Filen", 255).unwrap();
	assert_eq!(format!("{units:?}"), "\"Filen\"");
}

#[test]
fn every_failure_is_answered_with_a_cloud_file_status() {
	let failures = [
		Failure::NotAuthenticated,
		Failure::NotFound,
		Failure::Unreachable,
		Failure::Unsupported,
		Failure::ContentDamaged,
		Failure::Undecryptable,
		Failure::StalePlaceholder,
		Failure::InvalidRequest,
		Failure::Transient,
		Failure::Internal,
	];
	for failure in failures {
		let status = failure_status(failure).cast_unsigned();
		assert_eq!(status >> 16, 0xC000, "{failure:?}");
		assert_eq!((status >> 8) & 0xFF, 0xCF, "{failure:?}");
	}
}

#[test]
fn failures_map_to_the_recorded_statuses() {
	assert_eq!(
		failure_status(Failure::NotAuthenticated).cast_unsigned(),
		0xC000_CF0F
	);
	assert_eq!(
		failure_status(Failure::NotFound).cast_unsigned(),
		0xC000_CF08
	);
	assert_eq!(
		failure_status(Failure::StalePlaceholder).cast_unsigned(),
		0xC000_CF08
	);
	assert_eq!(
		failure_status(Failure::Unreachable).cast_unsigned(),
		0xC000_CF11
	);
	assert_eq!(
		failure_status(Failure::Unsupported).cast_unsigned(),
		0xC000_CF0A
	);
	assert_eq!(
		failure_status(Failure::ContentDamaged).cast_unsigned(),
		0xC000_CF0E
	);
	assert_eq!(
		failure_status(Failure::InvalidRequest).cast_unsigned(),
		0xC000_CF0B
	);
	for failure in [
		Failure::Undecryptable,
		Failure::Transient,
		Failure::Internal,
	] {
		assert_eq!(failure_status(failure).cast_unsigned(), 0xC000_CF12);
	}
}

#[test]
fn a_cancelled_or_aborted_request_is_gone() {
	assert_eq!(
		os_error(Hresult::new(0x8007_018E_u32.cast_signed())),
		OsError::Cancelled
	);
	assert_eq!(os_error(Hresult::from_win32(393)), OsError::Cancelled);
}

#[test]
fn any_other_failure_is_a_rejection_with_its_code() {
	assert_eq!(
		os_error(Hresult::new(0x8007_017C_u32.cast_signed())),
		OsError::Rejected {
			code: 0x8007_017C_u32.cast_signed()
		}
	);
	assert_eq!(
		os_error(Hresult::from_win32(376)),
		OsError::Rejected {
			code: 0x8007_0178_u32.cast_signed()
		}
	);
}

#[test]
fn a_transfer_refused_as_not_a_cloud_file_ends_the_request() {
	assert_eq!(
		transfer_error(Hresult::new(0x8007_0178_u32.cast_signed())),
		OsError::Cancelled
	);
	assert_eq!(transfer_error(Hresult::from_win32(398)), OsError::Cancelled);
	assert_eq!(
		transfer_error(Hresult::new(E_INVALIDARG)),
		OsError::Rejected { code: E_INVALIDARG }
	);
}

#[test]
fn a_field_is_present_when_the_struct_reaches_its_end() {
	assert!(field_present(16, 8, 8));
	assert!(!field_present(15, 8, 8));
	assert!(!field_present(u32::MAX, usize::MAX, 1));
}

#[test]
fn an_identity_length_is_checked_before_it_is_read() {
	assert_eq!(identity_len(true, 34), Err(IdentityError::Empty));
	assert_eq!(identity_len(false, 0), Err(IdentityError::Empty));
	assert_eq!(identity_len(false, 34), Ok(34));
	assert_eq!(identity_len(false, 4096), Ok(4096));
	assert_eq!(
		identity_len(false, 4097),
		Err(IdentityError::TooLong { len: 4097 })
	);
}

#[test]
fn a_string_is_read_up_to_its_nul() {
	assert_eq!(
		take_until_nul([0x41, 0x42, 0, 0x43].into_iter(), 10),
		Some(vec![0x41, 0x42])
	);
	assert_eq!(take_until_nul([0].into_iter(), 1), Some(vec![]));
}

#[test]
fn a_string_with_no_nul_within_the_bound_is_refused() {
	assert_eq!(take_until_nul([0x41, 0x42, 0].into_iter(), 2), None);
	assert_eq!(take_until_nul(iter::repeat(0x41), 100), None);
}

#[test]
fn a_volume_relative_path_gets_its_drive() {
	assert_eq!(
		volume_path(Some(&wide("C:")), &wide(r"\cfroot\sub\inner.bin")),
		Some(wide(r"C:\cfroot\sub\inner.bin"))
	);
}

#[test]
fn an_absolute_path_is_kept_as_it_is() {
	assert_eq!(volume_path(None, &wide(r"D:\x")), Some(wide(r"D:\x")));
	assert_eq!(
		volume_path(Some(&wide("C:")), &wide(r"\\?\D:\x")),
		Some(wide(r"\\?\D:\x"))
	);
}

#[test]
fn a_relative_path_without_a_drive_has_no_full_path() {
	assert_eq!(volume_path(None, &wide(r"\x")), None);
	assert_eq!(volume_path(Some(&wide("C")), &wide(r"\x")), None);
	assert_eq!(volume_path(Some(&wide(r"\\?\Volume")), &wide(r"\x")), None);
	assert_eq!(volume_path(Some(&wide("C:")), &wide("x")), None);
}

#[test]
fn a_failure_names_the_measured_negative_range_clamped_to_the_file() {
	assert_eq!(failure_range(Some((-4096, 8192)), 10_000), (0, 4096));
}

#[test]
fn a_failure_names_an_aligned_in_file_range_as_sent() {
	assert_eq!(
		failure_range(Some((999_424, 8192)), 5_242_880),
		(999_424, 8192)
	);
	assert_eq!(failure_range(Some((0, 300_000)), 300_000), (0, 300_000));
}

#[test]
fn a_failure_range_past_the_end_is_cut_at_the_end() {
	assert_eq!(failure_range(Some((8192, 8192)), 10_000), (8192, 1808));
	assert_eq!(failure_range(Some((1000, 100)), 10_000), (0, 4096));
}

#[test]
fn a_failure_with_no_range_read_names_the_whole_file() {
	assert_eq!(failure_range(None, 10_000), (0, 10_000));
	assert_eq!(failure_range(None, 5_242_880), (0, 5_242_880));
}

#[test]
fn a_transfer_covering_the_aligned_required_range_covers_it() {
	assert!(covers_required(Some((0, 10_000)), (0, 10_000), 10_000));
	assert!(covers_required(
		Some((0, 1_048_576)),
		(1000, 100),
		5_242_880
	));
	assert!(covers_required(
		Some((999_424, 1_048_576)),
		(999_424, 8192),
		5_242_880
	));
}

#[test]
fn a_transfer_short_of_the_required_range_does_not_cover_it() {
	assert!(!covers_required(None, (0, 4096), 10_000));
	assert!(!covers_required(Some((0, 4096)), (0, 10_000), 10_000));
	assert!(!covers_required(Some((4096, 10_000)), (1000, 100), 10_000));
	assert!(!covers_required(Some((0, 8192)), (8192, 1808), 10_000));
}

#[test]
fn a_required_range_outside_the_file_needs_nothing() {
	assert!(covers_required(None, (20_480, 4096), 10_000));
	assert!(covers_required(None, (0, 0), 10_000));
}

#[test]
fn a_failure_range_outside_the_file_is_named_as_sent() {
	assert_eq!(failure_range(Some((20_480, 4096)), 10_000), (20_480, 4096));
	assert_eq!(failure_range(Some((0, 0)), 10_000), (0, 0));
	assert_eq!(
		failure_range(Some((i64::MAX, 4096)), i64::MAX),
		(i64::MAX, 4096)
	);
}

#[test]
fn processed_entries_report_their_own_results() {
	let refused = 0x8007_007B_u32.cast_signed();
	assert_eq!(
		entry_outcomes(&[0, refused], 2),
		vec![
			EntryOutcome::Created,
			EntryOutcome::Rejected(OsError::Rejected { code: refused })
		]
	);
}

#[test]
fn an_entry_whose_name_a_local_item_holds_exists() {
	let exists = 0x8007_00B7_u32.cast_signed();
	assert_eq!(
		entry_outcomes(&[exists, 0], 2),
		vec![EntryOutcome::Exists, EntryOutcome::Created]
	);
}

#[test]
fn entries_past_the_processed_count_are_refused() {
	assert_eq!(
		entry_outcomes(&[0, 0, 0], 1),
		vec![
			EntryOutcome::Created,
			EntryOutcome::Rejected(OsError::Rejected {
				code: 0x8007_017C_u32.cast_signed()
			}),
			EntryOutcome::Rejected(OsError::Rejected {
				code: 0x8007_017C_u32.cast_signed()
			}),
		]
	);
}

fn time(millis: i64) -> FileTime {
	FileTime::from_unix_millis(millis).unwrap()
}

#[test]
fn a_file_is_created_normal_with_its_size_and_in_sync() {
	let spec = PlaceholderSpec::file(
		PlaceholderName::parse("a.bin").unwrap(),
		Uuid::from_u128(1),
		Uuid::from_u128(2),
		FileSize::from_i64(10_000).unwrap(),
		time(0),
		time(1),
	);
	assert_eq!(
		create_values(&spec),
		CreateValues {
			attributes: 0x80,
			size: 10_000,
			created: 116_444_736_000_000_000,
			modified: 116_444_736_000_010_000,
			flags: 2,
		}
	);
}

#[test]
fn a_directory_is_created_with_size_0_and_population_on() {
	let spec = PlaceholderSpec::dir(
		PlaceholderName::parse("sub").unwrap(),
		Uuid::from_u128(3),
		time(0),
		time(0),
	);
	assert_eq!(
		create_values(&spec),
		CreateValues {
			attributes: 0x10,
			size: 0,
			created: 116_444_736_000_000_000,
			modified: 116_444_736_000_000_000,
			flags: 2,
		}
	);
}

#[test]
fn a_reparse_tag_tells_a_placeholder_from_a_link() {
	// The attributes and tags measured on Windows 11 for each placeholder state.
	for tag in [0x9000_401A, 0x9000_001A, 0x9000_601A, 0x9000_201A] {
		assert_eq!(reparse_kind(0x0040_1620, tag), ReparseKind::Placeholder);
	}
	assert_eq!(reparse_kind(0x20, 0), ReparseKind::Plain);
	// The tag of an item that is not a reparse point means nothing.
	assert_eq!(
		reparse_kind(0x10, IO_REPARSE_TAG_SYMLINK),
		ReparseKind::Plain
	);
	assert_eq!(
		reparse_kind(0x420, IO_REPARSE_TAG_SYMLINK),
		ReparseKind::Unsupported(UnsupportedKind::Symlink)
	);
	assert_eq!(
		reparse_kind(0x410, IO_REPARSE_TAG_MOUNT_POINT),
		ReparseKind::Unsupported(UnsupportedKind::Junction)
	);
	// OneDrive-style tags of another cloud driver differ in the low bits.
	assert_eq!(
		reparse_kind(0x420, 0x9000_001B),
		ReparseKind::Unsupported(UnsupportedKind::ForeignReparsePoint)
	);
}

#[test]
fn a_local_operation_error_maps_to_what_the_core_acts_on() {
	assert_eq!(win32_local_error(2), LocalError::NotFound);
	assert_eq!(win32_local_error(3), LocalError::NotFound);
	assert_eq!(win32_local_error(32), LocalError::Busy);
	assert_eq!(win32_local_error(391), LocalError::Busy);
	assert_eq!(win32_local_error(392), LocalError::Pinned);
	assert_eq!(win32_local_error(80), LocalError::Exists);
	assert_eq!(win32_local_error(183), LocalError::Exists);
	assert_eq!(win32_local_error(145), LocalError::Exists);
	assert_eq!(win32_local_error(377), LocalError::Modified);
	assert_eq!(
		win32_local_error(5),
		LocalError::Os(OsError::Rejected {
			code: 0x8007_0005_u32.cast_signed()
		})
	);
	assert_eq!(local_error(Hresult::from_win32(183)), LocalError::Exists);
	assert_eq!(local_error(Hresult::from_win32(32)), LocalError::Busy);
	assert_eq!(local_error(Hresult::from_win32(391)), LocalError::Busy);
	// Measured: converting a placeholder.
	assert_eq!(
		local_error(Hresult::new(0x8007_017C_u32.cast_signed())),
		LocalError::Os(OsError::Rejected {
			code: 0x8007_017C_u32.cast_signed()
		})
	);
	// Measured: DEHYDRATE on a file not in sync, and VERIFY_IN_SYNC on a file written since.
	assert_eq!(
		local_error(Hresult::new(0x8007_0179_u32.cast_signed())),
		LocalError::Modified
	);
	assert_eq!(
		local_error(Hresult::new(E_FAIL)),
		LocalError::Os(OsError::Rejected { code: E_FAIL })
	);
	assert_eq!(
		local_error(Hresult::from_win32(ERROR_CLOUD_FILE_REQUEST_CANCELED)),
		LocalError::Os(OsError::Rejected {
			code: Hresult::from_win32(ERROR_CLOUD_FILE_REQUEST_CANCELED).get()
		})
	);
}

#[test]
fn a_path_below_the_root_keeps_its_components_whatever_the_root_spelling() {
	let parts = |path: &str| {
		root_relative(&wide(r"C:\cfroot"), &wide(path)).map(|components| {
			components
				.iter()
				.map(|c| String::from_utf16_lossy(c))
				.collect::<Vec<_>>()
		})
	};
	assert_eq!(parts(r"C:\cfroot"), Some(vec![]));
	assert_eq!(
		parts(r"C:\CFROOT\Docs\a.txt"),
		Some(vec!["Docs".to_owned(), "a.txt".to_owned()])
	);
	assert_eq!(
		parts(r"\\?\c:\cfroot\a.txt"),
		Some(vec!["a.txt".to_owned()])
	);
	assert_eq!(parts(r"C:\cfroot2\a.txt"), None);
	assert_eq!(parts(r"C:\other\a.txt"), None);
	assert_eq!(parts(r"C:\$Recycle.Bin\S-1-5\$R1.bin"), None);
	assert_eq!(parts("C:"), None);
	assert_eq!(
		root_relative(&wide(r"\\?\C:\cfroot\"), &wide(r"C:\cfroot\a")),
		Some(vec![wide("a")])
	);
	assert_eq!(root_relative(&[], &wide(r"C:\a")), None);
}

#[test]
fn a_callback_path_becomes_a_root_path_only_when_the_core_can_name_it() {
	let root = wide(r"C:\cfroot");
	assert_eq!(
		callback_root_path(&root, &wide(r"C:\cfroot\Docs\a.txt")),
		Some(RootPath::parse(["Docs", "a.txt"]).expect("valid"))
	);
	assert_eq!(
		callback_root_path(&root, &wide(r"C:\cfroot")),
		Some(RootPath::root())
	);
	// Measured: a CLOSE_COMPLETION after a delete on ARM64 named the file under `$Extend`.
	assert_eq!(
		callback_root_path(&root, &wide(r"C:\$Extend\$Deleted\0001")),
		None
	);
	// An unpaired surrogate.
	let mut bad = wide(r"C:\cfroot\");
	bad.push(0xD800);
	assert_eq!(callback_root_path(&root, &bad), None);
	assert_eq!(
		root_path_of(&[vec![0xD800]]),
		Err(RootPathError::NotUnicode)
	);
}

#[test]
fn a_rename_target_is_in_the_root_only_by_its_flags_and_its_path_together() {
	let root = wide(r"C:\cfroot");
	let target = wide(r"C:\cfroot\exp\renamed.txt");
	// Measured flags: 6 within the root, 7 for a directory, 2 out of it, 3 for a directory.
	assert_eq!(
		rename_scope(6, &root, Some(&target)),
		RenameScope::InRoot(RootPath::parse(["exp", "renamed.txt"]).expect("valid"))
	);
	assert_eq!(
		rename_scope(7, &root, Some(&target)),
		RenameScope::InRoot(RootPath::parse(["exp", "renamed.txt"]).expect("valid"))
	);
	let bin = wide(r"C:\$Recycle.Bin\S-1-5-21\$R0ABC.bin");
	assert_eq!(rename_scope(2, &root, Some(&bin)), RenameScope::OutOfRoot);
	assert_eq!(rename_scope(3, &root, Some(&bin)), RenameScope::OutOfRoot);
	assert_eq!(rename_scope(2, &root, None), RenameScope::OutOfRoot);
	// In scope by its flags but not below the root by its path: never taken for a move out.
	assert_eq!(
		rename_scope(6, &root, Some(&bin)),
		RenameScope::Refused(Failure::Internal)
	);
	assert_eq!(
		rename_scope(6, &root, None),
		RenameScope::Refused(Failure::Internal)
	);
	assert_eq!(
		rename_scope(6, &root, Some(&root)),
		RenameScope::Refused(Failure::InvalidRequest)
	);
	assert_eq!(
		rename_scope(6, &root, Some(&wide(r"C:\cfroot\bad."))),
		RenameScope::Refused(Failure::InvalidRequest)
	);
}

#[test]
fn only_a_long_drive_path_gets_the_verbatim_prefix() {
	let short = wide(r"C:\cfroot\a.txt");
	assert_eq!(long_path(&short), short);
	let long = wide(&format!(r"C:\cfroot\{}", "a".repeat(250)));
	let mut expected = wide(r"\\?\");
	expected.extend(&long);
	assert_eq!(long_path(&long), expected);
	let verbatim = wide(&format!(r"\\?\C:\cfroot\{}", "a".repeat(250)));
	assert_eq!(long_path(&verbatim), verbatim);
	let slashed = wide(&format!("C:/cfroot/{}", "a".repeat(250)));
	assert_eq!(long_path(&slashed), slashed);
	assert!(path_nul(&wide("a\0b")).is_none());
	assert_eq!(
		path_nul(&short).expect("no NUL").with_nul().last(),
		Some(&0)
	);
}

#[test]
fn notification_flags_read_as_measured() {
	// NOTIFY_DELETE: 0 a file, 1 a directory, 2 the recycle bin's undelete.
	assert_eq!(delete_flags(0), (false, false));
	assert_eq!(delete_flags(1), (true, false));
	assert_eq!(delete_flags(2), (false, true));
	// NOTIFY_RENAME: 6 a file within the root, 7 a directory, 2 and 3 out of it.
	assert!(!rename_is_directory(6));
	assert!(rename_is_directory(7));
	assert!(!rename_is_directory(2));
	assert!(rename_is_directory(3));
	// CLOSE_COMPLETION: 0 after reads and writes, 1 after a delete.
	assert!(!close_deleted(Some(0)));
	assert!(close_deleted(Some(1)));
	assert!(!close_deleted(None));
}

#[test]
fn a_directory_not_yet_populated_is_partial_and_a_populated_one_is_not() {
	// Both measured on a directory placeholder.
	assert!(is_partial(0x0040_1410));
	assert!(!is_partial(0x410));
	// A dehydrated file is offline.
	assert!(is_partial(0x1420));
	assert!(is_partial(
		FILE_ATTRIBUTE_RECALL_ON_OPEN | FILE_ATTRIBUTE_DIRECTORY
	));
}

#[test]
fn pin_attr_from_attributes() {
	// Measured: a dehydrated pinned file reads 0x481620, a hydrated unpinned one 0x100420.
	assert_eq!(pin_attr(0x0048_1620), PinAttr::Pinned);
	assert_eq!(pin_attr(0x0010_0420), PinAttr::Unpinned);
	assert_eq!(pin_attr(0x0040_1620), PinAttr::Unspecified);
	assert_eq!(
		pin_attr(FILE_ATTRIBUTE_PINNED | FILE_ATTRIBUTE_UNPINNED),
		PinAttr::Pinned
	);
}

#[test]
fn a_dehydration_request_reads_its_background_flag_and_reason() {
	assert_eq!(
		dehydrate_request(0, 1),
		(false, DehydrateReason::UserManual)
	);
	assert_eq!(
		dehydrate_request(1, 2),
		(true, DehydrateReason::SystemLowSpace)
	);
	assert_eq!(
		dehydrate_request(1, 3),
		(true, DehydrateReason::SystemInactivity)
	);
	assert_eq!(
		dehydrate_request(0, 4),
		(false, DehydrateReason::SystemOsUpgrade)
	);
	assert_eq!(dehydrate_request(2, 0), (false, DehydrateReason::None));
	assert_eq!(dehydrate_request(0, 9), (false, DehydrateReason::Other(9)));
}

#[test]
fn the_dehydrate_veto_status_is_the_measured_access_denied() {
	assert_eq!(STATUS_CLOUD_FILE_ACCESS_DENIED.cast_unsigned(), 0xC000_CF18);
}

/// The bytes `CfGetSyncRootInfoByPath` would write for a root with `file_id`, `name` and
/// `identity`.
fn sync_root_bytes(file_id: i64, name: &str, identity: &[u8]) -> Vec<u8> {
	let mut bytes = vec![0_u8; SYNC_ROOT_IDENTITY_OFFSET];
	bytes[..8].copy_from_slice(&file_id.to_ne_bytes());
	for (i, unit) in name.encode_utf16().enumerate() {
		let at = SYNC_ROOT_PROVIDER_NAME_OFFSET + 2 * i;
		bytes[at..at + 2].copy_from_slice(&unit.to_ne_bytes());
	}
	let len = u32::try_from(identity.len()).unwrap();
	bytes[SYNC_ROOT_IDENTITY_LEN_OFFSET..SYNC_ROOT_IDENTITY_LEN_OFFSET + 4]
		.copy_from_slice(&len.to_ne_bytes());
	bytes.extend_from_slice(identity);
	bytes
}

#[test]
fn a_sync_root_info_reads_its_file_id_provider_name_and_identity() {
	let bytes = sync_root_bytes(0x1234_5678_9ABC, "Filen", &[1, 2, 3]);
	assert_eq!(
		parse_sync_root_info(&bytes),
		Some(SyncRootStandard {
			file_id: 0x1234_5678_9ABC,
			provider_name: wide("Filen"),
			identity: vec![1, 2, 3],
		})
	);
}

#[test]
fn a_sync_root_info_shorter_than_the_identity_it_announces_is_refused() {
	let mut bytes = sync_root_bytes(7, "Filen", &[1, 2, 3]);
	bytes.pop();
	assert_eq!(parse_sync_root_info(&bytes), None);
	assert_eq!(
		parse_sync_root_info(&bytes[..SYNC_ROOT_IDENTITY_LEN_OFFSET]),
		None
	);
}

#[test]
fn a_provider_name_filling_its_field_without_a_nul_is_refused() {
	let name: String = "x".repeat(SYNC_ROOT_PROVIDER_NAME_UNITS);
	assert_eq!(parse_sync_root_info(&sync_root_bytes(7, &name, &[1])), None);
}

/// The GUID the ownership tests take as this provider's.
const OUR_ID: u128 = 0xB895_1154_1E5E_45B3_A367_0589_E4DE_5FDC;

#[test]
fn a_sync_root_identity_is_the_format_the_guid_and_the_account_root() {
	let identity = SyncRootIdentity::new(
		0x0102_0304_0506_0708_090A_0B0C_0D0E_0F10,
		Uuid::from_u128(0xABCD),
	);
	let mut expected = vec![1_u8];
	expected.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
	expected.extend_from_slice(Uuid::from_u128(0xABCD).as_bytes());
	assert_eq!(identity.as_bytes(), expected.as_slice());
	assert_eq!(
		SyncRootIdentity::account(
			identity.as_bytes(),
			0x0102_0304_0506_0708_090A_0B0C_0D0E_0F10
		),
		Some(Uuid::from_u128(0xABCD))
	);
}

#[test]
fn a_sync_root_identity_of_another_guid_format_or_length_names_no_account() {
	let identity = SyncRootIdentity::new(OUR_ID, Uuid::from_u128(0xABCD));
	assert_eq!(
		SyncRootIdentity::account(identity.as_bytes(), OUR_ID + 1),
		None
	);
	let mut format = identity.as_bytes().to_vec();
	format[0] = 2;
	assert_eq!(SyncRootIdentity::account(&format, OUR_ID), None);
	assert_eq!(
		SyncRootIdentity::account(&identity.as_bytes()[..32], OUR_ID),
		None
	);
	let mut longer = identity.as_bytes().to_vec();
	longer.push(0);
	assert_eq!(SyncRootIdentity::account(&longer, OUR_ID), None);
	assert_eq!(SyncRootIdentity::account(&[], OUR_ID), None);
}

#[test]
fn a_sync_root_of_ours_at_the_path_names_its_account() {
	let account = Uuid::from_u128(0xABCD);
	let identity = SyncRootIdentity::new(OUR_ID, account);
	let info = parse_sync_root_info(&sync_root_bytes(42, "Filen", identity.as_bytes())).unwrap();
	// The 128-bit id's high half is the volume's extension, zero on NTFS.
	assert_eq!(
		sync_root_owner(&info, &wide("Filen"), OUR_ID, 42),
		SyncRootOwner::Ours {
			account_root: account
		}
	);
}

#[test]
fn a_root_named_like_ours_without_our_guid_is_another_providers() {
	let account = Uuid::from_u128(0xABCD);
	let other_guid = SyncRootIdentity::new(OUR_ID ^ 1, account);
	// What the provider registered before its identity carried the GUID: a root's placeholder
	// identity.
	let unguided = PlaceholderIdentity::Root { root: account }.encode();
	for identity in [other_guid.as_bytes(), unguided.as_bytes(), &[0xFF, 0xFE]] {
		let info = parse_sync_root_info(&sync_root_bytes(42, "Filen", identity)).unwrap();
		assert_eq!(
			sync_root_owner(&info, &wide("Filen"), OUR_ID, 42),
			SyncRootOwner::Other,
			"{identity:?}"
		);
	}
}

#[test]
fn another_providers_root_or_a_path_inside_a_root_is_not_ours() {
	let identity = SyncRootIdentity::new(OUR_ID, Uuid::from_u128(1));
	let info = parse_sync_root_info(&sync_root_bytes(42, "OneDrive", identity.as_bytes())).unwrap();
	assert_eq!(
		sync_root_owner(&info, &wide("Filen"), OUR_ID, 42),
		SyncRootOwner::Other
	);
	let ours = parse_sync_root_info(&sync_root_bytes(42, "Filen", identity.as_bytes())).unwrap();
	assert_eq!(
		sync_root_owner(&ours, &wide("Filen"), OUR_ID, 43),
		SyncRootOwner::Other
	);
}

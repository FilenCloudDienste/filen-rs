use super::*;

#[test]
fn a_negative_size_is_refused() {
	assert_eq!(FileSize::from_i64(-1), Err(NegativeSize { size: -1 }));
	assert_eq!(
		FileSize::from_i64(i64::MIN),
		Err(NegativeSize { size: i64::MIN })
	);
}

#[test]
fn sizes_zero_and_i64_max_are_kept() {
	let zero = FileSize::from_i64(0).unwrap();
	assert_eq!((zero.get(), zero.as_i64()), (0, 0));
	let max = FileSize::from_i64(i64::MAX).unwrap();
	assert_eq!(
		(max.get(), max.as_i64()),
		(9_223_372_036_854_775_807, 9_223_372_036_854_775_807)
	);
}

#[test]
fn the_unix_epoch_is_116444736000000000_ticks() {
	assert_eq!(
		FileTime::from_unix_millis(0).map(FileTime::ticks),
		Ok(116_444_736_000_000_000)
	);
}

#[test]
fn the_filetime_epoch_is_tick_zero() {
	assert_eq!(
		FileTime::from_unix_millis(-11_644_473_600_000).map(FileTime::ticks),
		Ok(0)
	);
}

#[test]
fn a_time_before_1601_is_refused() {
	assert_eq!(
		FileTime::from_unix_millis(-11_644_473_600_001),
		Err(UnrepresentableTime {
			millis: -11_644_473_600_001
		})
	);
	assert_eq!(
		FileTime::from_unix_millis(i64::MIN),
		Err(UnrepresentableTime { millis: i64::MIN })
	);
}

#[test]
fn the_last_representable_millisecond_converts_and_the_next_is_refused() {
	assert_eq!(
		FileTime::from_unix_millis(910_692_730_085_477).map(FileTime::ticks),
		Ok(9_223_372_036_854_770_000)
	);
	assert_eq!(
		FileTime::from_unix_millis(910_692_730_085_478),
		Err(UnrepresentableTime {
			millis: 910_692_730_085_478
		})
	);
	assert_eq!(
		FileTime::from_unix_millis(i64::MAX),
		Err(UnrepresentableTime { millis: i64::MAX })
	);
}

#[test]
fn a_spec_reports_the_identity_and_kind_it_was_built_with() {
	let name = PlaceholderName::parse("a.txt").unwrap();
	let time = FileTime::from_unix_millis(0).unwrap();
	let size = FileSize::from_i64(10_000).unwrap();
	let file = PlaceholderSpec::file(
		name.clone(),
		Uuid::from_u128(1),
		Uuid::from_u128(2),
		size,
		time,
		time,
	);
	assert_eq!(
		file.identity(),
		PlaceholderIdentity::File {
			stable: Uuid::from_u128(1),
			version: Uuid::from_u128(2),
		}
	);
	assert_eq!(file.kind(), SpecKind::File { size });
	let dir = PlaceholderSpec::dir(name, Uuid::from_u128(3), time, time);
	assert_eq!(
		dir.identity(),
		PlaceholderIdentity::Dir {
			uuid: Uuid::from_u128(3)
		}
	);
	assert_eq!(dir.kind(), SpecKind::Dir);
}

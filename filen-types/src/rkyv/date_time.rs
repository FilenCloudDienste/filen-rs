use core::fmt;

use chrono::{DateTime, Utc};
use rkyv::{
	Archive, Archived, Deserialize, Resolver, Serialize,
	rancor::{Fallible, ResultExt},
	with::{ArchiveWith, DeserializeWith, SerializeWith},
};

use crate::error::TransparentError;

pub struct DateTimeUtcDef;

impl ArchiveWith<DateTime<Utc>> for DateTimeUtcDef {
	type Archived = Archived<(i64, u32)>;
	type Resolver = Resolver<(i64, u32)>;

	fn resolve_with(
		field: &DateTime<Utc>,
		resolver: Self::Resolver,
		out: rkyv::Place<Self::Archived>,
	) {
		let parts = (field.timestamp(), field.timestamp_subsec_nanos());
		parts.resolve(resolver, out);
	}
}

impl<S: Fallible + ?Sized> SerializeWith<DateTime<Utc>, S> for DateTimeUtcDef {
	fn serialize_with(
		field: &DateTime<Utc>,
		serializer: &mut S,
	) -> Result<Self::Resolver, S::Error> {
		let parts = (field.timestamp(), field.timestamp_subsec_nanos());
		parts.serialize(serializer)
	}
}

#[derive(Debug)]
struct DateTimeOutOfRangeError {
	seconds: i64,
	sub_sec_nanos: u32,
}

impl fmt::Display for DateTimeOutOfRangeError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(
			f,
			"DateTime out of range: seconds={}, sub_sec_nanos={}",
			self.seconds, self.sub_sec_nanos
		)
	}
}

impl core::error::Error for DateTimeOutOfRangeError {}

impl<D: Fallible + ?Sized> DeserializeWith<Archived<(i64, u32)>, DateTime<Utc>, D>
	for DateTimeUtcDef
where
	D::Error: rkyv::rancor::Source,
{
	fn deserialize_with(
		field: &Archived<(i64, u32)>,
		deserializer: &mut D,
	) -> Result<DateTime<Utc>, D::Error> {
		let (seconds, sub_sec_nanos) = field.deserialize(deserializer)?;

		DateTime::<Utc>::from_timestamp(seconds, sub_sec_nanos)
			.ok_or_else(|| {
				TransparentError::new(DateTimeOutOfRangeError {
					seconds,
					sub_sec_nanos,
				})
			})
			.into_error()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	// `AsTimeStamp` is a "with" wrapper, so it can only be applied to a field.
	// This mirrors how it is meant to be used at the call sites.
	#[derive(Debug, PartialEq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
	struct Wrapper {
		#[rkyv(with = DateTimeUtcDef)]
		dt: DateTime<Utc>,
	}

	fn at(seconds: i64, sub_sec_nanos: u32) -> DateTime<Utc> {
		DateTime::<Utc>::from_timestamp(seconds, sub_sec_nanos)
			.expect("test timestamp must be in range")
	}

	fn round_trip(dt: DateTime<Utc>) -> DateTime<Utc> {
		let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&Wrapper { dt }).unwrap();
		rkyv::from_bytes::<Wrapper, rkyv::rancor::Error>(&bytes)
			.unwrap()
			.dt
	}

	// ---------- round-trip ----------------------------------------------------

	#[test]
	fn round_trip_epoch() {
		let dt = at(0, 0);
		assert_eq!(round_trip(dt), dt);
	}

	#[test]
	fn round_trip_preserves_sub_second_nanos() {
		let dt = at(1_700_000_000, 123_456_789);
		let decoded = round_trip(dt);
		assert_eq!(decoded, dt);
		assert_eq!(decoded.timestamp(), 1_700_000_000);
		assert_eq!(decoded.timestamp_subsec_nanos(), 123_456_789);
	}

	#[test]
	fn round_trip_max_sub_second_nanos() {
		let dt = at(42, 999_999_999);
		assert_eq!(round_trip(dt), dt);
	}

	#[test]
	fn round_trip_negative_timestamp() {
		// 1969-12-31T23:59:59.999999999Z — one nanosecond before the epoch.
		let dt = at(-1, 999_999_999);
		assert_eq!(round_trip(dt), dt);
	}

	#[test]
	fn round_trip_far_future_and_past() {
		// Year ~5138 and year ~1901, both comfortably within `DateTime` range.
		assert_eq!(round_trip(at(100_000_000_000, 1)), at(100_000_000_000, 1));
		assert_eq!(round_trip(at(-2_177_452_800, 500)), at(-2_177_452_800, 500));
	}

	#[test]
	fn round_trip_leap_second() {
		// chrono encodes a leap second as a sub-second nano value >= 1e9, which is
		// only valid when `secs % 60 == 59`. The raw `(timestamp, subsec_nanos)` pair
		// stored by `serialize_with` carries that out-of-band nanosecond verbatim, so
		// the leap second must survive even though the nanos exceed 999_999_999.
		let dt = at(59, 1_500_000_000);
		assert!(
			dt.timestamp_subsec_nanos() >= 1_000_000_000,
			"expected a leap-second value, got {} ns",
			dt.timestamp_subsec_nanos(),
		);
		let decoded = round_trip(dt);
		assert_eq!(decoded, dt);
		assert_eq!(decoded.timestamp_subsec_nanos(), 1_500_000_000);
	}

	// ---------- serialization output ------------------------------------------

	#[test]
	fn serializes_as_raw_timestamp_pair() {
		// `serialize_with` must emit exactly the `(timestamp, subsec_nanos)` pair.
		// `Wrapper` has a single field whose archived form is `Archived<(i64, u32)>`,
		// so its bytes must match those of the bare tuple. This checks the serialize
		// side independently of the deserialize side.
		let dt = at(1_700_000_000, 123_456_789);
		let wrapper_bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&Wrapper { dt }).unwrap();
		let pair_bytes =
			rkyv::to_bytes::<rkyv::rancor::Error>(&(dt.timestamp(), dt.timestamp_subsec_nanos()))
				.unwrap();
		assert_eq!(wrapper_bytes.as_slice(), pair_bytes.as_slice());
	}

	// ---------- deserialization error path ------------------------------------

	#[test]
	fn deserialize_rejects_out_of_range_timestamp() {
		// A valid `DateTime` can never serialize an out-of-range timestamp, so we
		// archive the raw `(seconds, nanos)` pair directly with a seconds value far
		// outside the representable range and run it through `deserialize_with`.
		let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&(i64::MAX, 0u32)).unwrap();
		let archived = rkyv::access::<Archived<(i64, u32)>, rkyv::rancor::Error>(&bytes).unwrap();
		let mut pool = rkyv::de::Pool::default();
		let result: Result<DateTime<Utc>, rkyv::rancor::BoxedError> =
			DateTimeUtcDef::deserialize_with(
				archived,
				rkyv::rancor::Strategy::<_, rkyv::rancor::BoxedError>::wrap(&mut pool),
			);
		let err = result.unwrap_err();
		assert!(
			err.to_string().contains("DateTime out of range"),
			"unexpected error: {err}",
		);
	}
}

use std::borrow::Cow;

use chrono::{DateTime, Utc};
use filen_types::{
	api::v3::dir::color::DirColor, fs::ParentUuid, rkyv::date_time::DateTimeUtcDef,
	traits::CowHelpers,
};
use rkyv::with::Map;
use uuid::Uuid;

use crate::{
	fs::{cache::CacheableConversionError, dir::meta::DirectoryMeta},
	io::RemoteDirectory,
};

#[derive(
	Clone, Debug, PartialEq, Eq, CowHelpers, rkyv::Serialize, rkyv::Deserialize, rkyv::Archive,
)]
pub struct CacheableDir<'a> {
	pub uuid: Uuid,
	pub parent: Uuid,
	pub color: DirColor<'a>,
	pub favorited: bool,
	#[rkyv(with = DateTimeUtcDef)]
	pub timestamp: DateTime<Utc>,

	pub name: Cow<'a, str>,
	#[rkyv(with = Map<DateTimeUtcDef>)]
	pub created: Option<DateTime<Utc>>,
}

impl TryFrom<RemoteDirectory> for CacheableDir<'static> {
	type Error = (RemoteDirectory, CacheableConversionError);

	fn try_from(mut value: RemoteDirectory) -> Result<Self, Self::Error> {
		let parent = match value.parent {
			ParentUuid::Uuid(uuid) => (&uuid).into(),
			other => {
				return Err((value, CacheableConversionError::ParentNotUuid(other)));
			}
		};

		let decrypted_meta = match value.meta {
			DirectoryMeta::Decoded(meta) => meta,
			other => {
				let debug = format!("{:?}", other);
				value.meta = other;
				return Err((value, CacheableConversionError::MetadataNotDecrypted(debug)));
			}
		};

		Ok(Self {
			uuid: (&value.uuid).into(),
			parent,
			color: value.color,
			favorited: value.favorited,
			timestamp: value.timestamp,

			name: decrypted_meta.name,
			created: decrypted_meta.created,
		})
	}
}

impl<'a> TryFrom<&'a RemoteDirectory> for CacheableDir<'a> {
	type Error = CacheableConversionError;

	fn try_from(value: &'a RemoteDirectory) -> Result<Self, Self::Error> {
		let decrypted_meta = match &value.meta {
			DirectoryMeta::Decoded(meta) => meta,
			other => {
				return Err(CacheableConversionError::MetadataNotDecrypted(format!(
					"{:?}",
					other
				)));
			}
		};

		Ok(Self {
			uuid: (&value.uuid).into(),
			parent: match value.parent {
				ParentUuid::Uuid(uuid) => (&uuid).into(),
				other => {
					return Err(CacheableConversionError::ParentNotUuid(other));
				}
			},
			color: value.color.as_borrowed_cow(),
			favorited: value.favorited,
			timestamp: value.timestamp,

			name: Cow::Borrowed(&decrypted_meta.name),
			created: decrypted_meta.created,
		})
	}
}

impl CacheableDir<'_> {
	/// Two `CacheableDir`s hash equal iff they represent the same logical directory *content*,
	/// regardless of which path built them. The cache resync diff compares this fingerprint instead
	/// of the derived [`PartialEq`], so a field that differs *by construction path* does not produce
	/// a spurious `Changed` event and resync churn.
	///
	/// Excluded fields:
	/// - `color`: the `FolderMove` / `FolderSubCreated` / `FolderRestore` socket events carry no
	///   color, so their builders hardcode [`DirColor::Default`] — which diverges from the real
	///   color a recursive listing reports. Color is tracked authoritatively via the dedicated
	///   `FolderColorChanged` event, so it does not participate in change-detection.
	/// - `timestamp`: socket events carry the *event* time while a listing carries the listing time,
	///   so it diverges across paths and is not a content signal (the pre-rework `DBDir::eq`
	///   likewise excluded it).
	pub fn content_fingerprint(&self) -> [u8; 32] {
		use crate::fs::cache::fingerprint::write_opt_dt_ms;

		let mut hasher = blake3::Hasher::new();
		hasher.update(self.uuid.as_bytes());
		hasher.update(self.parent.as_bytes());
		hasher.update(self.name.as_bytes());
		write_opt_dt_ms(&mut hasher, self.created);
		hasher.update(&[u8::from(self.favorited)]);
		*hasher.finalize().as_bytes()
	}
}

#[cfg(test)]
mod fingerprint_tests {
	use super::*;

	fn dt(ms: i64) -> DateTime<Utc> {
		DateTime::from_timestamp_millis(ms).expect("valid timestamp")
	}

	fn base() -> CacheableDir<'static> {
		CacheableDir {
			uuid: Uuid::from_u128(0x1111_1111_1111_1111_1111_1111_1111_1111),
			parent: Uuid::from_u128(0x2222_2222_2222_2222_2222_2222_2222_2222),
			color: DirColor::Default,
			favorited: false,
			timestamp: dt(1_700_000_000_000),
			name: Cow::Borrowed("folder"),
			created: Some(dt(1_699_000_000_000)),
		}
	}

	/// The headline guarantee, and the genuinely real divergence: a dir cached from a `FolderMove` /
	/// `FolderSubCreated` / `FolderRestore` socket event has `color = DirColor::Default` (those wire
	/// events carry no color), while the same dir from a recursive listing carries its real color.
	/// Without excluding `color`, every resync would emit a spurious `Changed` for such a dir.
	#[test]
	fn fingerprint_excludes_color_and_timestamp() {
		let from_socket_event = base(); // color hardcoded to Default by the socket builders
		let mut from_listing = from_socket_event.clone();
		from_listing.color = DirColor::Blue; // the real color, as a listing reports it
		from_listing.timestamp = dt(1_800_000_000_000);

		// Derived PartialEq sees a difference (this is what would cause spurious `Changed`s):
		assert_ne!(from_socket_event, from_listing);
		// The content fingerprint treats them as the same logical content:
		assert_eq!(
			from_socket_event.content_fingerprint(),
			from_listing.content_fingerprint()
		);
	}

	/// Every field that IS part of content identity must change the fingerprint.
	#[test]
	fn fingerprint_changes_with_each_content_field() {
		let base = base();
		let baseline = base.content_fingerprint();
		let fp = |mutate: fn(&mut CacheableDir<'static>)| {
			let mut c = base.clone();
			mutate(&mut c);
			c.content_fingerprint()
		};

		assert_ne!(baseline, fp(|c| c.uuid = Uuid::from_u128(9)));
		assert_ne!(baseline, fp(|c| c.parent = Uuid::from_u128(9)));
		assert_ne!(baseline, fp(|c| c.name = Cow::Borrowed("renamed")));
		assert_ne!(baseline, fp(|c| c.favorited = true));
		assert_ne!(baseline, fp(|c| c.created = None));
		assert_ne!(baseline, fp(|c| c.created = Some(dt(1_698_000_000_000))));
	}

	#[test]
	fn fingerprint_is_deterministic() {
		let a = base();
		assert_eq!(a.content_fingerprint(), a.clone().content_fingerprint());
	}
}

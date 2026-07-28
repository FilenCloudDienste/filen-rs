//! The server-minted whole-life file id.
//!
//! This module is deliberately private and [`StableUuid`] is deliberately not
//! constructable: no `new`, no `From<Uuid>`, no `FromStr`, private field. A
//! value can only come into existence at the sanctioned deserialization
//! boundaries — serde for wire payloads, `rusqlite` for reading back rows that
//! persisted a wire value, and the FFI lift for values a foreign caller
//! previously received from us. If code appears to need to mint one from a
//! plain [`Uuid`], the surrounding types are modeling the domain wrong; adjust
//! the types instead of smuggling a uuid into the stable slot.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::fs::Uuid;

/// A file's server-minted whole-life id: unlike [`Uuid`], which is re-minted
/// on every content edit and version restore, this identifies the file for its
/// entire lifetime. Only files have one — dirs and roots are identified by
/// their `uuid`, which the server never re-mints.
///
/// The `rkyv` impls are here for the same reason as the `rusqlite` ones below:
/// reading an archived cache payload back is deserialization of a value that
/// came off the wire, not construction of a new one. The archived form
/// delegates to [`Uuid`]'s, so it stays byte-identical to a plain uuid.
#[derive(
	Debug,
	Clone,
	Copy,
	PartialEq,
	Eq,
	Hash,
	PartialOrd,
	Ord,
	Serialize,
	Deserialize,
	rkyv::Archive,
	rkyv::Serialize,
	rkyv::Deserialize,
)]
#[serde(transparent)]
#[rkyv(derive(Debug), compare(PartialEq))]
pub struct StableUuid(Uuid);

impl From<StableUuid> for Uuid {
	fn from(value: StableUuid) -> Self {
		value.0
	}
}

impl PartialEq<Uuid> for StableUuid {
	fn eq(&self, other: &Uuid) -> bool {
		&self.0 == other
	}
}

impl PartialEq<StableUuid> for Uuid {
	fn eq(&self, other: &StableUuid) -> bool {
		self == &other.0
	}
}

impl fmt::Display for StableUuid {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		self.0.fmt(f)
	}
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
#[wasm_bindgen::prelude::wasm_bindgen(typescript_custom_section)]
const TS_STABLE_UUID: &'static str = r#"export type StableUuid = UuidStr;"#;

// The FFI boundary only round-trips values foreign code received from us, so
// lifting counts as deserialization, not construction.
#[cfg(feature = "uniffi")]
uniffi::custom_type!(StableUuid, String, {
	lower: |id: &StableUuid| id.0.to_string(),
	try_lift: |s: String| {
		std::str::FromStr::from_str(&s)
			.map(StableUuid)
			.map_err(|_| uniffi::deps::anyhow::anyhow!("invalid stable UUID string: {}", s))
	},
});

// Reading a row back is deserialization of a persisted wire value; both impls
// delegate to `Uuid`'s so the stored form stays byte-identical to a plain uuid
// column.
#[cfg(feature = "rusqlite")]
impl rusqlite::types::FromSql for StableUuid {
	fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
		<Uuid as rusqlite::types::FromSql>::column_result(value).map(StableUuid)
	}
}

#[cfg(feature = "rusqlite")]
impl rusqlite::types::ToSql for StableUuid {
	fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
		self.0.to_sql()
	}
}

// Test seam: fixtures must be able to mint ids without a server round trip.
// Gated behind the `test-seams` feature, which only dev-dependencies enable —
// production builds cannot construct one.
#[cfg(feature = "test-seams")]
impl StableUuid {
	/// Mints a stable id out of thin air, bypassing the deserialization-only
	/// guarantee. Test fixtures only.
	pub fn new_for_test(uuid: Uuid) -> Self {
		StableUuid(uuid)
	}
}

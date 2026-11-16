use super::*;
use crate::test_support::{ffi_dir, ffi_file};

const STABLE: Uuid = Uuid::from_u128(0x0011_2233_4455_6677_8899_aabb_ccdd_eeff);
const VERSION: Uuid = Uuid::from_u128(1);

const STABLE_BYTES: [u8; 16] = [
	0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
];
const VERSION_BYTES: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];

fn blob(bytes: &[u8]) -> IdentityBlob {
	IdentityBlob::try_from(bytes).unwrap()
}

#[test]
fn each_kind_round_trips_through_its_bytes() {
	for identity in [
		PlaceholderIdentity::Root { root: STABLE },
		PlaceholderIdentity::Dir { uuid: VERSION },
		PlaceholderIdentity::File {
			stable: STABLE,
			version: VERSION,
		},
		PlaceholderIdentity::Detached { nonce: STABLE },
	] {
		assert_eq!(
			PlaceholderIdentity::decode(&identity.encode()),
			Ok(identity)
		);
	}
}

#[test]
fn a_file_identity_encodes_to_its_literal_34_bytes() {
	let encoded = PlaceholderIdentity::File {
		stable: STABLE,
		version: VERSION,
	}
	.encode();
	let mut expected = vec![1, 2];
	expected.extend_from_slice(&STABLE_BYTES);
	expected.extend_from_slice(&VERSION_BYTES);
	assert_eq!(expected.len(), 34);
	assert_eq!(encoded.as_bytes(), expected.as_slice());
}

#[test]
fn a_root_and_a_dir_identity_encode_to_their_literal_18_bytes() {
	let mut root = vec![1, 0];
	root.extend_from_slice(&STABLE_BYTES);
	assert_eq!(
		PlaceholderIdentity::Root { root: STABLE }
			.encode()
			.as_bytes(),
		root.as_slice()
	);
	let mut dir = vec![1, 1];
	dir.extend_from_slice(&STABLE_BYTES);
	assert_eq!(dir.len(), 18);
	assert_eq!(
		PlaceholderIdentity::Dir { uuid: STABLE }
			.encode()
			.as_bytes(),
		dir.as_slice()
	);
}

#[test]
fn an_empty_blob_is_refused() {
	assert_eq!(IdentityBlob::try_from(&[][..]), Err(IdentityError::Empty));
}

#[test]
fn a_blob_of_4096_bytes_is_accepted_and_4097_refused() {
	assert_eq!(blob(&[7; 4096]).as_bytes().len(), 4096);
	assert_eq!(
		IdentityBlob::try_from(&[7; 4097][..]),
		Err(IdentityError::TooLong { len: 4097 })
	);
}

#[test]
fn an_unknown_format_byte_is_refused() {
	let mut bytes = PlaceholderIdentity::Dir { uuid: STABLE }
		.encode()
		.as_bytes()
		.to_vec();
	bytes[0] = 2;
	assert_eq!(
		PlaceholderIdentity::decode(&blob(&bytes)),
		Err(IdentityError::UnknownFormat { format: 2 })
	);
}

#[test]
fn an_identity_without_a_kind_is_refused() {
	assert_eq!(
		PlaceholderIdentity::decode(&blob(&[1])),
		Err(IdentityError::NoKind)
	);
}

#[test]
fn an_unknown_kind_is_refused() {
	let mut bytes = vec![1, 4];
	bytes.extend_from_slice(&STABLE_BYTES);
	assert_eq!(
		PlaceholderIdentity::decode(&blob(&bytes)),
		Err(IdentityError::UnknownKind { kind: 4 })
	);
}

#[test]
fn a_detached_identity_encodes_to_its_literal_18_bytes() {
	let mut expected = vec![1, 3];
	expected.extend_from_slice(&STABLE_BYTES);
	assert_eq!(
		PlaceholderIdentity::Detached { nonce: STABLE }
			.encode()
			.as_bytes(),
		expected.as_slice()
	);
	assert_eq!(
		PlaceholderIdentity::Detached { nonce: STABLE }.cache_id(),
		None
	);
}

#[test]
fn a_short_identity_of_each_kind_is_refused() {
	for (kind, expected) in [(0, 18), (1, 18), (2, 34)] {
		let mut bytes = vec![1, kind];
		bytes.extend_from_slice(&STABLE_BYTES);
		bytes.extend_from_slice(&VERSION_BYTES);
		bytes.truncate(expected - 1);
		assert_eq!(
			PlaceholderIdentity::decode(&blob(&bytes)),
			Err(IdentityError::Length {
				kind,
				expected,
				actual: expected - 1,
			}),
			"kind {kind}"
		);
	}
	assert_eq!(
		PlaceholderIdentity::decode(&blob(&[1, 2])),
		Err(IdentityError::Length {
			kind: 2,
			expected: 34,
			actual: 2,
		})
	);
}

#[test]
fn a_trailing_byte_is_refused() {
	for (identity, kind, expected) in [
		(PlaceholderIdentity::Root { root: STABLE }, 0, 18),
		(PlaceholderIdentity::Dir { uuid: STABLE }, 1, 18),
		(
			PlaceholderIdentity::File {
				stable: STABLE,
				version: VERSION,
			},
			2,
			34,
		),
	] {
		let mut bytes = identity.encode().as_bytes().to_vec();
		bytes.push(0);
		assert_eq!(
			PlaceholderIdentity::decode(&blob(&bytes)),
			Err(IdentityError::Length {
				kind,
				expected,
				actual: expected + 1,
			}),
			"kind {kind}"
		);
	}
}

#[test]
fn blob_debug_prints_hex() {
	assert_eq!(
		format!("{:?}", blob(&[0x01, 0xab, 0x00, 0xff])),
		"IdentityBlob(01ab00ff)"
	);
}

#[test]
fn cache_ids_are_the_namespaces_the_cache_resolves() {
	assert_eq!(
		[
			PlaceholderIdentity::Root { root: STABLE },
			PlaceholderIdentity::Dir { uuid: STABLE },
			PlaceholderIdentity::File {
				stable: STABLE,
				version: VERSION,
			},
		]
		.map(|identity| identity.cache_id().unwrap().0),
		[
			"00112233-4455-6677-8899-aabbccddeeff",
			"stable/00112233-4455-6677-8899-aabbccddeeff",
			"stable/00112233-4455-6677-8899-aabbccddeeff",
		]
	);
}

#[test]
fn a_listed_child_maps_to_its_identity() {
	let file = FfiNonRootObject::File(ffi_file(STABLE, VERSION, "a.txt", 1));
	let dir = FfiNonRootObject::Dir(ffi_dir(STABLE, "sub"));
	assert_eq!(
		PlaceholderIdentity::from_child(&file),
		Ok(PlaceholderIdentity::File {
			stable: STABLE,
			version: VERSION,
		})
	);
	assert_eq!(
		PlaceholderIdentity::from_child(&dir),
		Ok(PlaceholderIdentity::Dir { uuid: STABLE })
	);
}

#[test]
fn a_child_with_an_unparseable_stable_uuid_is_malformed() {
	let mut file = ffi_file(STABLE, VERSION, "a.txt", 1);
	file.stable_uuid = "0011".to_owned();
	assert_eq!(
		PlaceholderIdentity::from_child(&FfiNonRootObject::File(file)),
		Err(MalformedId)
	);
	let mut dir = ffi_dir(STABLE, "sub");
	dir.uuid = String::new();
	assert_eq!(
		PlaceholderIdentity::from_child(&FfiNonRootObject::Dir(dir)),
		Err(MalformedId)
	);
}

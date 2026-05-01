use std::borrow::Cow;

use filen_types::{
	api::v3::dir::link::info::LinkPasswordSalt, crypto::LinkHashedPassword,
	serde::str::SizedHexString,
};

use super::error::ConversionError;

pub(crate) fn derive_password_for_link(
	password: Option<&str>,
	salt: &LinkPasswordSalt<'_>,
) -> Result<LinkHashedPassword<'static>, ConversionError> {
	Ok(match (password, salt) {
		(Some(password), LinkPasswordSalt::V3(salt)) => LinkHashedPassword(Cow::Owned(
			SizedHexString::from(super::v3::derive_password(password.as_bytes(), salt)?)
				.to_string(),
		)),
		(Some(password), LinkPasswordSalt::V2(salt)) => LinkHashedPassword(Cow::Owned(
			SizedHexString::from(super::v2::derive_password(
				password.as_bytes(),
				salt.as_bytes(),
			)?)
			.to_string(),
		)),
		(Some(password), LinkPasswordSalt::None) => LinkHashedPassword(Cow::Owned(
			SizedHexString::from(super::v2::hash(password.as_bytes())).to_string(),
		)),
		(None, _) => LinkHashedPassword(Cow::Borrowed("empty")),
	})
}

pub(crate) fn empty_hash() -> LinkHashedPassword<'static> {
	LinkHashedPassword(Cow::Borrowed("empty"))
}

pub(crate) fn new_random_salt() -> LinkPasswordSalt<'static> {
	crate::crypto::v3::make_link_salt()
}

// empty {
//   salt: '33c6d93abd6972e908f7cd4737a5afb53ea33717940316f3a6aa36e37c6ca794d4bd1c567635b8bfe7998156e54506e7051506358374a98f283da90610e08259327b4168f50636405abf6089cac12475ff165848297b6edeb1cfe74ebd2afd390ebe41ae4c5a6e5ba4eeeb0702b5b2bdef0ad127f1db6a42d0626deb53cc65d958886aee21ca7714495f96f5ee90a1776d3df6c54ad2b4adfbdea0fcc3b1a01310aefdb5f03b9ed495ad021626dec15eb14190ffbb6f6c1ea0e66f99368d239303af6e0bd7fb12cd6a62ba90232035ab6b02dde3430d6e1a3aa794805e1eee1d8d1a8d2671a9caa22afd7d7c71209e5da0137aab25b335e265d287cfb0af9665',
//   hash: 'empty'
// }
// password {
//   salt: '74209f28e2de8b2b9e2a5316188216cb6fc6167befc8eac3a6e24033ea5056e68561806b7fbcee0e1103cdd9d4b1e031884d9188c34dfa133516b2f0f80014a0f8c0446f2578d7047a98fa1e9774af41ddc53a93751c4008cb52c6ebbb4ae43a930fecb2a5bd7a6ad06b96a093e4785524b9a247f82b227befa5e2775a7169292928d87fcc0e4495dfcc82e37fd179dd1a975dfa0e8a6cc04a40c856ca3ccde7c8acba4beaa539e45179f317965f8168a0954beeee1549d90b251d4bcbcbb4a7f446f7672ed3f7b7eb900f9e1e226e29e349d9fd19ea8e884c5526a4339b7d1419465b09cc3240b4f2f3e615d4ec8021e0ed96e5dc42f3edaa90ccc500794d96',
//   hash: '6373e3bd2a0640d067a83d2953edf5dea76e3f32cfe24c4484727225fb683b836a70ffa11fa344e75b70724b996377bce515d0182bf0cf1e1a1530687a8e14bc'
// }
// password2 {
//   salt: '56c99259fe281b46eca165dddf5183e8c2c90e26c85bf97ce11c69362e7ae92f3b476d74dd1251c2194c08f931d08092006e410452f165e5435580906c46a9327c43c4ae3851e5c8a41086e47bcf885c397889e96a2422912332477cb4070b1190a4dc7a7c640dac43e5b36d5e4e8da04d90eb997ed56353cd8283ac0902b79c2c1ca8369dae0d976c7036c8ef316351760d29a364e9bf57189674e96afc5861736e76ff63f80b6280acdde2793abc55147ad2112092554619e2a5642bbd23a4832d2a7f790fbdd31b7a4be070f214b19e9c4edc2ee1c66510c8b7772b61a2d4b316231e9d0fa249ab9b7005eb3efd31f355b4b3a2575e69ca3f968ee928c047',
//   hash: 'c6153835a8e3ea0bf172bcb02093b3e9193718cb1d58aebf367e258486bd591496658bee5e2cb5b66e5970e846ce641fd0c9fe79772612e5cdf147f2d308cd8c'
// }
//

#[cfg(test)]
mod tests {
	use filen_types::serde::str::SizedStringBase64Chars;

	use super::*;
	#[test]
	fn check_kown_hashes_v2() {
		assert_eq!(
			LinkHashedPassword(Cow::Borrowed("empty")),
			derive_password_for_link(
				None,
				&LinkPasswordSalt::V2(
					SizedStringBase64Chars::try_from(
						"zm9EsUNlNyfzN3EFbLnHrN9rWR5QuZU8".to_string()
					)
					.unwrap()
				)
			)
			.unwrap()
		);
		assert_eq!(
			LinkHashedPassword(Cow::Borrowed(
				"ce273dada8b49ee004a002a9c46c44e789c644e8ad32446f031e73abadc83e257b571103d314088a87c21dbdc094c96e94f01bbb453110c6cec22b5644797376"
			)),
			derive_password_for_link(
				Some("password"),
				&LinkPasswordSalt::V2(
					SizedStringBase64Chars::try_from(
						"x__X9T9953af53zes9u-4pIhAqhxOjBx".to_string()
					)
					.unwrap()
				)
			)
			.unwrap()
		);
		assert_eq!(
			LinkHashedPassword(Cow::Borrowed(
				"b2a5886da5702887f110914f70aca3d7f0352354efece1b12ab143afa4d88e69b9ad63f230b2c7774e8ffbfeeedd1509c5711cdca3685907e735bedea51e567e"
			)),
			derive_password_for_link(
				Some("password2"),
				&LinkPasswordSalt::V2(
					SizedStringBase64Chars::try_from(
						"rRYk5S_x7je62wFO1etMbEMlctAy-CMr".to_string()
					)
					.unwrap()
				)
			)
			.unwrap()
		);
	}

	#[test]
	fn check_known_hashes_v3() {
		assert_eq!(
			LinkHashedPassword(Cow::Borrowed("empty")),
			derive_password_for_link(
				None,
				&LinkPasswordSalt::V3(Box::new(SizedHexString::new_from_hex_str(
					"33c6d93abd6972e908f7cd4737a5afb53ea33717940316f3a6aa36e37c6ca794d4bd1c567635b8bfe7998156e54506e7051506358374a98f283da90610e08259327b4168f50636405abf6089cac12475ff165848297b6edeb1cfe74ebd2afd390ebe41ae4c5a6e5ba4eeeb0702b5b2bdef0ad127f1db6a42d0626deb53cc65d958886aee21ca7714495f96f5ee90a1776d3df6c54ad2b4adfbdea0fcc3b1a01310aefdb5f03b9ed495ad021626dec15eb14190ffbb6f6c1ea0e66f99368d239303af6e0bd7fb12cd6a62ba90232035ab6b02dde3430d6e1a3aa794805e1eee1d8d1a8d2671a9caa22afd7d7c71209e5da0137aab25b335e265d287cfb0af9665"
				).unwrap())))
				.unwrap()
		);
		assert_eq!(
			LinkHashedPassword(Cow::Borrowed(
				"906c444ee01ab94e55e550f4dff2ac72648483aa9f783a95ce8b5e2308afae713ebca8d836a8186025768a5474e16678a6c2a5eee306c7450e6815f6d0caf9d7"
			)),
			derive_password_for_link(
				Some("password"),
				&LinkPasswordSalt::V3(Box::new(SizedHexString::new_from_hex_str(
				"67b9cda4866f766fbe643739198e4125481c6cd1d3d7f50070dcab4c857f5f6efcb61dcbe28475ce5c5bdb5cf976b6d6392f9128809671a997b2159ed45628d7705a302387ae13c26b8762490d79f57cbca70f2ac06d6bafab63e240b45c9db9afb105ea08692914941a5e0ddb3575d4fa40083bba1797c8c77b0e72e1dffd20cc929bd355b50ce93898ca486bd556cd60ce01fa5eedc1e4a34295c1cace184617f6ce0a8a4ccc5059cb93b4afe22615934c9f03a238530299e495c18c903d60482f45291db94053cc655514e5aa87003ad912461a88efca639c86b4abcd24efdb6939c42612997a003257c9eb6a5e7e440afbdce279f5beb9f0f731d2d6f3f3"
				).unwrap())))
				.unwrap()
		);
		assert_eq!(
			LinkHashedPassword(Cow::Borrowed(
				"c6153835a8e3ea0bf172bcb02093b3e9193718cb1d58aebf367e258486bd591496658bee5e2cb5b66e5970e846ce641fd0c9fe79772612e5cdf147f2d308cd8c"
			)),
			derive_password_for_link(
				Some("password2"),
				&LinkPasswordSalt::V3(Box::new(SizedHexString::new_from_hex_str(
				"56c99259fe281b46eca165dddf5183e8c2c90e26c85bf97ce11c69362e7ae92f3b476d74dd1251c2194c08f931d08092006e410452f165e5435580906c46a9327c43c4ae3851e5c8a41086e47bcf885c397889e96a2422912332477cb4070b1190a4dc7a7c640dac43e5b36d5e4e8da04d90eb997ed56353cd8283ac0902b79c2c1ca8369dae0d976c7036c8ef316351760d29a364e9bf57189674e96afc5861736e76ff63f80b6280acdde2793abc55147ad2112092554619e2a5642bbd23a4832d2a7f790fbdd31b7a4be070f214b19e9c4edc2ee1c66510c8b7772b61a2d4b316231e9d0fa249ab9b7005eb3efd31f355b4b3a2575e69ca3f968ee928c047"
				).unwrap())))
				.unwrap()
		);
	}
}

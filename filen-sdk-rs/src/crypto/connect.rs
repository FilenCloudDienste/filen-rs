use super::error::ConversionError;

pub(crate) fn derive_password_for_link(
	password: Option<&str>,
	salt: Option<&[u8]>,
) -> Result<Vec<u8>, ConversionError> {
	Ok(match (password, salt) {
		(Some(password), Some(salt)) if salt.len() == 256 => {
			super::v3::derive_password(password.as_bytes(), salt)?.to_vec()
		}
		(Some(password), Some(salt)) if salt.len() == 16 => {
			super::v2::derive_password(password.as_bytes(), salt)?.to_vec()
		}
		(Some(password), _) => super::v2::hash(password.as_bytes()).to_vec(),
		(None, _) => super::v2::hash("empty".as_bytes()).to_vec(),
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn check_known_hashes() {
		assert_eq!(
			&faster_hex::hex_string(&derive_password_for_link(None, Some(&[12, 0])).unwrap()),
			"8f83dfba6522ce8c34c5afefa64878e3a4ac554d"
		);
	}
}

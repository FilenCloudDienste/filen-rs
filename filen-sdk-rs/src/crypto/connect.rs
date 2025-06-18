use super::error::ConversionError;

pub(crate) fn derive_password_for_link(
	password: Option<&str>,
	salt: &[u8],
) -> Result<Vec<u8>, ConversionError> {
	match password {
		Some(password) => match salt.len() {
			256 => Ok(super::v3::derive_password(password.as_bytes(), salt)?.to_vec()),
			16 => Ok(super::v2::derive_password(password.as_bytes(), salt)?.to_vec()),
			_ => {
				let password = match password.len() {
					0 => "empty",
					_ => password,
				};
				Ok(super::v2::hash(password.as_bytes()).to_vec())
			}
		},
		None => Ok(super::v2::hash("empty".as_bytes()).to_vec()),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn check_known_hashes() {
		assert_eq!(
			&faster_hex::hex_string(&derive_password_for_link(None, &[12, 0]).unwrap()),
			"8f83dfba6522ce8c34c5afefa64878e3a4ac554d"
		);
	}
}

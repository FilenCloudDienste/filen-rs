//! Zip encryption: WinZip AES (read and written) and the legacy "traditional PKWARE"
//! encryption (ZipCrypto, read only: it is broken, and the SDK never writes it).

use std::io::{self, Read, Take, Write};

use aes_gcm::aes::{Aes128, Aes192, Aes256};
use ctr::{
	Ctr128LE,
	cipher::{KeyIvInit, StreamCipher},
};
use filen_macros::js_type;
use hmac::{Hmac, Mac};
use sha1::Sha1;

use crate::{
	Error, ErrorKind,
	fs::archive::{error::read_failure, password::ArchivePassword},
};

/// PBKDF2 rounds WinZip AES derives its keys with, fixed by the format.
const AES_KDF_ROUNDS: u32 = 1000;

/// Bytes of the HMAC-SHA1 authentication code after an AES entry's data.
pub(crate) const AES_AUTH_CODE_LEN: usize = 10;
pub(crate) const AES_AUTH_CODE_LEN_U64: u64 = AES_AUTH_CODE_LEN as u64;

/// Bytes of the password verifier after the salt.
pub(crate) const AES_VERIFIER_LEN: u64 = 2;

/// Bytes of the encryption header before a ZipCrypto entry's data.
pub(crate) const ZIP_CRYPTO_HEADER_LEN: usize = 12;
pub(crate) const ZIP_CRYPTO_HEADER_LEN_U64: u64 = ZIP_CRYPTO_HEADER_LEN as u64;

/// The key size of a WinZip AES entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[js_type(import, export, no_default)]
pub enum AesStrength {
	Aes128,
	Aes192,
	Aes256,
}

impl AesStrength {
	/// The strength byte of the AES extra field.
	pub(crate) fn from_byte(byte: u8) -> Option<Self> {
		match byte {
			1 => Some(Self::Aes128),
			2 => Some(Self::Aes192),
			3 => Some(Self::Aes256),
			_ => None,
		}
	}

	pub(crate) fn byte(self) -> u8 {
		match self {
			Self::Aes128 => 1,
			Self::Aes192 => 2,
			Self::Aes256 => 3,
		}
	}

	fn key_len(self) -> usize {
		match self {
			Self::Aes128 => 16,
			Self::Aes192 => 24,
			Self::Aes256 => 32,
		}
	}

	/// Bytes of the salt before an entry's data: half the key size.
	pub(crate) fn salt_len(self) -> usize {
		self.key_len() / 2
	}
}

enum Cipher {
	Aes128(Ctr128LE<Aes128>),
	Aes192(Ctr128LE<Aes192>),
	Aes256(Ctr128LE<Aes256>),
}

impl Cipher {
	fn apply(&mut self, data: &mut [u8]) {
		match self {
			Self::Aes128(cipher) => cipher.apply_keystream(data),
			Self::Aes192(cipher) => cipher.apply_keystream(data),
			Self::Aes256(cipher) => cipher.apply_keystream(data),
		}
	}
}

/// The keys of one AES entry and its password verifier.
struct AesKeys {
	cipher: Cipher,
	mac: Hmac<Sha1>,
	verifier: [u8; 2],
}

fn derive(password: &ArchivePassword, salt: &[u8], strength: AesStrength) -> AesKeys {
	let key_len = strength.key_len();
	let mut derived = vec![0u8; 2 * key_len + 2];
	pbkdf2::pbkdf2_hmac::<Sha1>(password.as_bytes(), salt, AES_KDF_ROUNDS, &mut derived);
	let (key, rest) = derived.split_at(key_len);
	let (auth, verifier) = rest.split_at(key_len);
	// WinZip counts blocks from 1, little-endian over the whole block
	let mut iv = [0u8; 16];
	iv[0] = 1;
	let cipher = match strength {
		AesStrength::Aes128 => Cipher::Aes128(Ctr128LE::new(key.into(), &iv.into())),
		AesStrength::Aes192 => Cipher::Aes192(Ctr128LE::new(key.into(), &iv.into())),
		AesStrength::Aes256 => Cipher::Aes256(Ctr128LE::new(key.into(), &iv.into())),
	};
	AesKeys {
		cipher,
		mac: Hmac::<Sha1>::new_from_slice(auth).expect("HMAC takes a key of any length"),
		verifier: [verifier[0], verifier[1]],
	}
}

/// Why decrypting an entry failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CryptoError {
	/// The password does not open the entry.
	#[error("wrong password")]
	WrongPassword,
	/// The data does not match its authentication code: damaged, or tampered with.
	#[error("the encrypted data does not match its authentication code")]
	AuthenticationFailed,
	/// The entry is too short to hold the AES header and authentication code.
	#[error("an AES entry too short for its header")]
	TooShort,
	/// Reading the encryption header failed.
	#[error(transparent)]
	Read(#[from] io::Error),
}

impl From<CryptoError> for io::Error {
	fn from(error: CryptoError) -> Self {
		match error {
			CryptoError::Read(error) => error,
			error => io::Error::new(io::ErrorKind::InvalidData, error),
		}
	}
}

impl From<CryptoError> for Error {
	fn from(error: CryptoError) -> Self {
		let kind = match error {
			CryptoError::Read(error) => return read_failure(error),
			CryptoError::WrongPassword => ErrorKind::ArchiveWrongPassword,
			CryptoError::AuthenticationFailed | CryptoError::TooShort => ErrorKind::ArchiveCorrupt,
		};
		Error::custom_with_source(kind, error, None::<&str>)
	}
}

/// The decrypted data of a WinZip AES entry. Reading it to the end checks its authentication
/// code; an entry is only authenticated once this returns `Ok(0)`.
pub(crate) struct AesReader<R> {
	inner: Take<R>,
	cipher: Cipher,
	mac: Hmac<Sha1>,
	authenticated: bool,
}

impl<R: Read> AesReader<R> {
	/// Opens the entry whose encrypted form (salt, verifier, data, auth code) is the next
	/// `stored_len` bytes of `inner`.
	pub(crate) fn new(
		mut inner: R,
		password: &ArchivePassword,
		strength: AesStrength,
		stored_len: u64,
	) -> Result<Self, CryptoError> {
		let overhead = strength.salt_len() as u64 + AES_VERIFIER_LEN + AES_AUTH_CODE_LEN_U64;
		let data_len = stored_len
			.checked_sub(overhead)
			.ok_or(CryptoError::TooShort)?;
		let mut salt = [0u8; 16];
		let salt = &mut salt[..strength.salt_len()];
		inner.read_exact(salt)?;
		let mut verifier = [0u8; 2];
		inner.read_exact(&mut verifier)?;
		let keys = derive(password, salt, strength);
		if keys.verifier != verifier {
			return Err(CryptoError::WrongPassword);
		}
		Ok(Self {
			inner: inner.take(data_len),
			cipher: keys.cipher,
			mac: keys.mac,
			authenticated: false,
		})
	}
}

impl<R: Read> Read for AesReader<R> {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		if buf.is_empty() {
			return Ok(0);
		}
		let n = self.inner.read(buf)?;
		if n > 0 {
			self.mac.update(&buf[..n]);
			self.cipher.apply(&mut buf[..n]);
			return Ok(n);
		}
		if self.inner.limit() > 0 {
			return Err(io::ErrorKind::UnexpectedEof.into());
		}
		if !self.authenticated {
			let mut code = [0u8; AES_AUTH_CODE_LEN];
			self.inner.get_mut().read_exact(&mut code)?;
			let mac = std::mem::replace(
				&mut self.mac,
				Hmac::<Sha1>::new_from_slice(&[]).expect("any key length"),
			);
			// the code is the MAC's first bytes, compared in constant time
			mac.verify_truncated_left(&code)
				.map_err(|_| CryptoError::AuthenticationFailed)?;
			self.authenticated = true;
		}
		Ok(0)
	}
}

/// Encrypts an entry's data as WinZip AES: the salt and verifier first, the authentication code
/// on [`AesWriter::finish`].
pub(crate) struct AesWriter<W> {
	inner: W,
	cipher: Cipher,
	mac: Hmac<Sha1>,
	buf: Vec<u8>,
}

impl<W: Write> AesWriter<W> {
	pub(crate) fn new(
		mut inner: W,
		password: &ArchivePassword,
		strength: AesStrength,
		salt: &[u8],
	) -> io::Result<Self> {
		debug_assert_eq!(salt.len(), strength.salt_len());
		let keys = derive(password, salt, strength);
		inner.write_all(salt)?;
		inner.write_all(&keys.verifier)?;
		Ok(Self {
			inner,
			cipher: keys.cipher,
			mac: keys.mac,
			buf: Vec::new(),
		})
	}

	/// Writes the authentication code; the writer is done.
	pub(crate) fn finish(mut self) -> io::Result<W> {
		let code = self.mac.finalize().into_bytes();
		self.inner.write_all(&code[..AES_AUTH_CODE_LEN])?;
		Ok(self.inner)
	}
}

impl<W: Write> Write for AesWriter<W> {
	fn write(&mut self, data: &[u8]) -> io::Result<usize> {
		self.buf.clear();
		self.buf.extend_from_slice(data);
		self.cipher.apply(&mut self.buf);
		self.mac.update(&self.buf);
		self.inner.write_all(&self.buf)?;
		Ok(data.len())
	}

	fn flush(&mut self) -> io::Result<()> {
		self.inner.flush()
	}
}

/// The three keys of ZipCrypto.
struct CryptoKeys([u32; 3]);

impl CryptoKeys {
	fn new(password: &[u8]) -> Self {
		let mut keys = Self([0x1234_5678, 0x2345_6789, 0x3456_7890]);
		for &byte in password {
			keys.update(byte);
		}
		keys
	}

	fn update(&mut self, byte: u8) {
		let [k0, k1, k2] = &mut self.0;
		*k0 = crc32_byte(*k0, byte);
		*k1 = k1
			.wrapping_add(*k0 & 0xFF)
			.wrapping_mul(134_775_813)
			.wrapping_add(1);
		*k2 = crc32_byte(*k2, (*k1 >> 24) as u8);
	}

	fn decrypt(&mut self, byte: u8) -> u8 {
		let plain = byte ^ self.stream_byte();
		self.update(plain);
		plain
	}

	/// The byte the next data byte is XORed with, from the low 16 bits of the third key.
	fn stream_byte(&self) -> u8 {
		let [low, high, ..] = (self.0[2] | 2).to_le_bytes();
		let temp = u16::from_le_bytes([low, high]);
		(temp.wrapping_mul(temp ^ 1) >> 8) as u8
	}
}

/// One step of the reflected CRC-32 (the zip polynomial), without the usual inversions, as
/// ZipCrypto's key schedule uses it.
fn crc32_byte(crc: u32, byte: u8) -> u32 {
	let mut value = (crc ^ u32::from(byte)) & 0xFF;
	for _ in 0..8 {
		value = if value & 1 == 1 {
			(value >> 1) ^ 0xEDB8_8320
		} else {
			value >> 1
		};
	}
	value ^ (crc >> 8)
}

/// The decrypted data of a ZipCrypto entry.
pub(crate) struct ZipCryptoReader<R> {
	inner: R,
	keys: CryptoKeys,
}

impl<R: Read> ZipCryptoReader<R> {
	/// Opens an entry whose 12-byte encryption header comes next in `inner`; `check` is the
	/// byte its last header byte decrypts to with the right password. A wrong password passes
	/// this check once in 256; the entry's CRC-32 catches it then.
	pub(crate) fn new(
		mut inner: R,
		password: &ArchivePassword,
		check: u8,
	) -> Result<Self, CryptoError> {
		let mut keys = CryptoKeys::new(password.as_bytes());
		let mut header = [0u8; ZIP_CRYPTO_HEADER_LEN];
		inner.read_exact(&mut header)?;
		let mut last = 0;
		for byte in header {
			last = keys.decrypt(byte);
		}
		if last != check {
			return Err(CryptoError::WrongPassword);
		}
		Ok(Self { inner, keys })
	}
}

impl<R: Read> Read for ZipCryptoReader<R> {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		let n = self.inner.read(buf)?;
		for byte in &mut buf[..n] {
			*byte = self.keys.decrypt(*byte);
		}
		Ok(n)
	}
}

#[cfg(test)]
pub(crate) mod test_support {
	use super::{CryptoKeys, ZIP_CRYPTO_HEADER_LEN};

	/// Encrypts with ZipCrypto, for the tests' fixtures only: the SDK never writes it.
	pub(crate) fn zip_crypto_encrypt(password: &[u8], check: u8, data: &[u8]) -> Vec<u8> {
		let mut keys = CryptoKeys::new(password);
		let mut header = [0x5Au8; ZIP_CRYPTO_HEADER_LEN];
		header[11] = check;
		header
			.iter()
			.chain(data)
			.map(|&plain| {
				let cipher = plain ^ keys.stream_byte();
				keys.update(plain);
				cipher
			})
			.collect()
	}
}

#[cfg(test)]
mod tests {
	use super::{test_support::zip_crypto_encrypt, *};
	use crate::fs::archive::test_support::archive_password;

	#[test]
	fn an_aes_entry_shorter_than_its_header_is_refused_before_reading() {
		let stored = [0u8; 4];
		assert!(matches!(
			AesReader::new(&stored[..], &archive_password("pw"), AesStrength::Aes256, 4),
			Err(CryptoError::TooShort)
		));
	}

	#[test]
	fn aes_round_trips_and_authenticates() {
		for strength in [
			AesStrength::Aes128,
			AesStrength::Aes192,
			AesStrength::Aes256,
		] {
			let data: Vec<u8> = (0..10_000u32).map(|i| i.to_le_bytes()[0]).collect();
			let salt = vec![7u8; strength.salt_len()];
			let mut writer =
				AesWriter::new(Vec::new(), &archive_password("pw"), strength, &salt).unwrap();
			writer.write_all(&data).unwrap();
			let stored = writer.finish().unwrap();
			let open = |password: &str, stored: &[u8]| {
				let mut reader = AesReader::new(
					stored,
					&archive_password(password),
					strength,
					stored.len() as u64,
				)?;
				let mut out = Vec::new();
				reader.read_to_end(&mut out)?;
				Ok::<_, io::Error>(out)
			};
			assert_eq!(open("pw", &stored).unwrap(), data, "{strength:?}");
			let error = open("wrong", &stored).unwrap_err();
			assert!(matches!(
				error.get_ref().and_then(|e| e.downcast_ref()),
				Some(CryptoError::WrongPassword)
			));
			let mut tampered = stored.clone();
			tampered[strength.salt_len() + 2 + 100] ^= 1;
			let error = open("pw", &tampered).unwrap_err();
			assert!(matches!(
				error.get_ref().and_then(|e| e.downcast_ref()),
				Some(CryptoError::AuthenticationFailed)
			));
			// so does a flipped byte of the code itself, at either end of it
			for from_end in [1, AES_AUTH_CODE_LEN] {
				let mut tampered = stored.clone();
				tampered[stored.len() - from_end] ^= 0x80;
				let error = open("pw", &tampered).unwrap_err();
				assert!(matches!(
					error.get_ref().and_then(|e| e.downcast_ref()),
					Some(CryptoError::AuthenticationFailed)
				));
			}
		}
	}

	#[test]
	fn zip_crypto_decrypts_what_it_encrypts() {
		let data = b"legacy encrypted entry";
		let stored = zip_crypto_encrypt(b"pw", 0xAB, data);
		let mut out = Vec::new();
		ZipCryptoReader::new(&stored[..], &archive_password("pw"), 0xAB)
			.unwrap()
			.read_to_end(&mut out)
			.unwrap();
		assert_eq!(out, data);
		assert!(ZipCryptoReader::new(&stored[..], &archive_password("nope"), 0xAB).is_err());
	}

	#[test]
	fn the_crc_step_matches_the_zip_crc() {
		// a full CRC-32 from the per-byte step, with the usual inversions, is the standard one
		let crc = !b"123456789"
			.iter()
			.fold(!0u32, |crc, &b| crc32_byte(crc, b));
		assert_eq!(crc, 0xCBF4_3926);
	}
}

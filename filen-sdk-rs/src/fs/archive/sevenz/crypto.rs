//! 7z's AES-256 coder: CBC over the coder's input, with a key derived from the UTF-16LE
//! password by `2^cycles` rounds of SHA-256 over salt, password and a round counter.

use std::io::{self, Read, Write};

use aes::{
	Aes256,
	cipher::{BlockDecrypt, BlockEncrypt, KeyInit, generic_array::GenericArray},
};
use sha2_v11::{Digest, Sha256};
use zeroize::Zeroizing;

use super::SevenZError;

pub(crate) const BLOCK: usize = 16;
/// AES-CBC data of other than a whole number of blocks.
pub(crate) const AES_PARTIAL_BLOCK: &str = "7z AES data is not a whole number of blocks";

/// The coder id of 7z's AES-256 + SHA-256.
pub(crate) const AES_ID: u64 = 0x06F1_0701;

/// The key derivation rounds (as a power of two) the SDK writes, as 7-Zip does.
pub(crate) const WRITE_CYCLES_POWER: u8 = 19;

/// The most key derivation rounds (as a power of two) the SDK reads: each extra power doubles
/// the time a derivation takes, and an archive picks its own. 7-Zip writes 19.
pub(crate) const MAX_CYCLES_POWER: u8 = 22;

/// The marker 7z uses for "no derivation: the key is salt and password as they are".
const RAW_KEY_POWER: u8 = 0x3F;

pub(crate) type Key = Zeroizing<[u8; 32]>;

/// An AES coder's properties.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct AesProps {
	pub(crate) cycles_power: u8,
	pub(crate) salt: Vec<u8>,
	pub(crate) iv: [u8; BLOCK],
}

impl AesProps {
	/// A first byte with the cycles power and whether salt and IV follow, then a byte with
	/// their extra lengths, then salt and IV.
	pub(crate) fn parse(props: &[u8]) -> Result<Self, SevenZError> {
		let corrupt = || SevenZError::Corrupt("invalid 7z AES properties");
		let (&first, rest) = props.split_first().ok_or_else(corrupt)?;
		let mut parsed = Self {
			cycles_power: first & 0x3F,
			salt: Vec::new(),
			iv: [0; BLOCK],
		};
		if first & 0xC0 == 0 {
			return Ok(parsed);
		}
		let (&second, rest) = rest.split_first().ok_or_else(corrupt)?;
		let salt_len = usize::from(first >> 7) + usize::from(second >> 4);
		let iv_len = usize::from((first >> 6) & 1) + usize::from(second & 0x0F);
		if rest.len() != salt_len + iv_len || iv_len > BLOCK {
			return Err(corrupt());
		}
		parsed.salt = rest[..salt_len].to_vec();
		parsed.iv[..iv_len].copy_from_slice(&rest[salt_len..]);
		Ok(parsed)
	}

	/// The properties with a 16-byte salt and IV.
	pub(crate) fn encode(&self) -> Vec<u8> {
		debug_assert_eq!(self.salt.len(), BLOCK);
		let mut props = vec![self.cycles_power | 0xC0, 0xFF];
		props.extend_from_slice(&self.salt);
		props.extend_from_slice(&self.iv);
		props
	}
}

/// Derives the key for `password` (UTF-16LE) under `props`. `on_round` is called every 2^16
/// rounds, so a long derivation (2^22 rounds of a long password take a minute on wasm) can show
/// it is progressing and be stopped: its error ends the derivation.
pub(crate) fn derive_key(
	password: &[u8],
	props: &AesProps,
	on_round: &mut dyn FnMut() -> io::Result<()>,
) -> Result<Key, SevenZError> {
	let mut key = Zeroizing::new([0u8; 32]);
	if props.cycles_power == RAW_KEY_POWER {
		let raw = props.salt.iter().chain(password).copied();
		for (byte, from) in key.iter_mut().zip(raw) {
			*byte = from;
		}
		return Ok(key);
	}
	if props.cycles_power > MAX_CYCLES_POWER {
		return Err(SevenZError::Unsupported(
			"a 7z key derivation over the rounds the SDK spends",
		));
	}
	let mut round = Zeroizing::new(Vec::with_capacity(props.salt.len() + password.len() + 8));
	round.extend_from_slice(&props.salt);
	round.extend_from_slice(password);
	let counter_at = round.len();
	round.extend_from_slice(&[0; 8]);
	let mut sha = Sha256::new();
	for counter in 0..1u64 << props.cycles_power {
		round[counter_at..].copy_from_slice(&counter.to_le_bytes());
		sha.update(&round);
		if counter & 0xFFFF == 0xFFFF {
			on_round()?;
		}
	}
	key.copy_from_slice(&sha.finalize());
	Ok(key)
}

/// Decrypts a CBC stream: the plaintext is the ciphertext's length, padding included (the
/// coder's unpack size says where the data ends).
pub(crate) struct AesCbcReader<R> {
	inner: R,
	cipher: Aes256,
	previous: [u8; BLOCK],
	/// Decrypted bytes not handed out yet.
	plain: [u8; BLOCK],
	plain_at: usize,
	plain_len: usize,
}

impl<R: Read> AesCbcReader<R> {
	pub(crate) fn new(inner: R, key: &Key, iv: [u8; BLOCK]) -> Self {
		Self {
			inner,
			cipher: Aes256::new(GenericArray::from_slice(&key[..])),
			previous: iv,
			plain: [0; BLOCK],
			plain_at: 0,
			plain_len: 0,
		}
	}
}

impl<R: Read> Read for AesCbcReader<R> {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		if buf.is_empty() {
			return Ok(0);
		}
		if self.plain_at == self.plain_len {
			let mut block = [0u8; BLOCK];
			let mut filled = 0;
			while filled < BLOCK {
				match self.inner.read(&mut block[filled..])? {
					0 if filled == 0 => return Ok(0),
					0 => {
						return Err(io::Error::new(
							io::ErrorKind::InvalidData,
							SevenZError::Corrupt(AES_PARTIAL_BLOCK),
						));
					}
					n => filled += n,
				}
			}
			let ciphertext = block;
			self.cipher
				.decrypt_block(GenericArray::from_mut_slice(&mut block));
			for (plain, previous) in block.iter_mut().zip(self.previous) {
				*plain ^= previous;
			}
			self.previous = ciphertext;
			self.plain = block;
			self.plain_at = 0;
			self.plain_len = BLOCK;
		}
		let n = buf.len().min(self.plain_len - self.plain_at);
		buf[..n].copy_from_slice(&self.plain[self.plain_at..self.plain_at + n]);
		self.plain_at += n;
		Ok(n)
	}
}

/// Encrypts into a CBC stream; [`AesCbcWriter::finish`] pads the last block with zeros.
pub(crate) struct AesCbcWriter<W> {
	inner: W,
	cipher: Aes256,
	previous: [u8; BLOCK],
	pending: [u8; BLOCK],
	pending_len: usize,
	/// Plaintext bytes taken, which is the coder's unpack size.
	taken: u64,
}

impl<W: Write> AesCbcWriter<W> {
	pub(crate) fn new(inner: W, key: &Key, iv: [u8; BLOCK]) -> Self {
		Self {
			inner,
			cipher: Aes256::new(GenericArray::from_slice(&key[..])),
			previous: iv,
			pending: [0; BLOCK],
			pending_len: 0,
			taken: 0,
		}
	}

	fn encrypt_pending(&mut self) -> io::Result<()> {
		let mut block = self.pending;
		for (plain, previous) in block.iter_mut().zip(self.previous) {
			*plain ^= previous;
		}
		self.cipher
			.encrypt_block(GenericArray::from_mut_slice(&mut block));
		self.inner.write_all(&block)?;
		self.previous = block;
		self.pending_len = 0;
		Ok(())
	}

	/// Pads and writes the last block; the writer, and the plaintext length.
	pub(crate) fn finish(mut self) -> io::Result<(W, u64)> {
		if self.pending_len > 0 {
			self.pending[self.pending_len..].fill(0);
			self.encrypt_pending()?;
		}
		Ok((self.inner, self.taken))
	}
}

impl<W: Write> Write for AesCbcWriter<W> {
	fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
		let n = buf.len().min(BLOCK - self.pending_len);
		self.pending[self.pending_len..self.pending_len + n].copy_from_slice(&buf[..n]);
		self.pending_len += n;
		if self.pending_len == BLOCK {
			self.encrypt_pending()?;
		}
		self.taken += n as u64;
		Ok(n)
	}

	fn flush(&mut self) -> io::Result<()> {
		self.inner.flush()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn props(cycles_power: u8) -> AesProps {
		AesProps {
			cycles_power,
			salt: (0..16).collect(),
			iv: [7; BLOCK],
		}
	}

	#[test]
	fn props_round_trip_and_reject_bad_lengths() {
		let props = props(19);
		let encoded = props.encode();
		assert_eq!(encoded.len(), 34);
		assert!(AesProps::parse(&encoded).unwrap() == props);
		assert!(AesProps::parse(&encoded[..33]).is_err());
		assert!(AesProps::parse(&[]).is_err());
		// no salt and no IV: an all-zero IV
		let bare = AesProps::parse(&[19]).unwrap();
		assert!(bare.salt.is_empty() && bare.iv == [0; BLOCK]);
		// a 17-byte IV cannot be
		assert!(AesProps::parse(&[0x40 | 19, 0x10]).is_err());
	}

	#[test]
	fn derivation_matches_keys_computed_elsewhere() {
		// computed with Python's hashlib, as 7-Zip's 7zAes.cpp derives a key: SHA-256 over salt,
		// UTF-16LE password and a little-endian 64-bit round counter, 2^cycles times. That the
		// SDK reads 7-Zip's own encrypted archives is checked on the fixtures
		// (`sevenz_fixtures_extract_to_their_manifest` in extract/codec/tests.rs).
		let utf16 =
			|text: &str| -> Vec<u8> { text.encode_utf16().flat_map(u16::to_le_bytes).collect() };
		for (password, cycles_power, expected) in [
			(
				"fixture password",
				WRITE_CYCLES_POWER,
				"08c019f74d46b880101787dce0a31ef59ce32750c0b109066445d546be5d7d09",
			),
			(
				"p",
				4,
				"04c79caf1d88a343ac1070d5d9f512a6f072dd20ded484af25ff8a04be860ed7",
			),
		] {
			let key = derive_key(&utf16(password), &props(cycles_power), &mut || Ok(())).unwrap();
			let hex: String = key.iter().map(|byte| format!("{byte:02x}")).collect();
			assert_eq!(hex, expected, "{password} at 2^{cycles_power}");
		}
		assert!(matches!(
			derive_key(b"p\0", &props(MAX_CYCLES_POWER + 1), &mut || Ok(())),
			Err(SevenZError::Unsupported(_))
		));
	}

	#[test]
	fn a_long_derivation_reports_its_rounds_and_stops_when_told() {
		let mut rounds = 0;
		derive_key(b"pw", &props(18), &mut || {
			rounds += 1;
			Ok(())
		})
		.unwrap();
		// every 2^16 rounds
		assert_eq!(rounds, 4);
		let mut rounds = 0;
		let stopped = derive_key(b"pw", &props(MAX_CYCLES_POWER), &mut || {
			rounds += 1;
			Err(io::Error::other("the job ended"))
		});
		assert!(matches!(stopped, Err(SevenZError::Read(_))));
		assert_eq!(rounds, 1, "the derivation stops at once");
	}

	#[test]
	fn key_material_is_wiped_when_dropped() {
		// the AES key schedule the coders hold, and the hash state a derivation leaves, which
		// holds the password's last bytes
		fn wiped<T: zeroize::ZeroizeOnDrop>() {}
		wiped::<Aes256>();
		wiped::<Sha256>();
	}

	#[test]
	fn cbc_round_trips_with_zero_padding() {
		let key = Zeroizing::new([3u8; 32]);
		for len in [0usize, 1, 15, 16, 17, 1000] {
			let data: Vec<u8> = (0..len).map(|i| i as u8).collect();
			let mut writer = AesCbcWriter::new(Vec::new(), &key, [9; BLOCK]);
			writer.write_all(&data).unwrap();
			let (encrypted, taken) = writer.finish().unwrap();
			assert_eq!(taken, len as u64);
			assert_eq!(encrypted.len(), len.div_ceil(BLOCK) * BLOCK);
			let mut decrypted = Vec::new();
			AesCbcReader::new(&encrypted[..], &key, [9; BLOCK])
				.read_to_end(&mut decrypted)
				.unwrap();
			assert_eq!(&decrypted[..len], &data[..]);
			assert!(decrypted[len..].iter().all(|&b| b == 0));
		}
		assert!(
			AesCbcReader::new(&[0u8; 17][..], &key, [0; BLOCK])
				.read_to_end(&mut Vec::new())
				.is_err()
		);
	}
}

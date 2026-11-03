//! The self-signed certificate `serve --self-signed` serves HTTPS with: generated once into the
//! config directory and reused, so that a client that pinned its fingerprint keeps trusting it,
//! and generated again when it is about to expire or does not name an address a server listens
//! on.

use std::{collections::BTreeSet, fs, io::Write as _, net::IpAddr, path::Path};

use anyhow::{Context as _, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD as base64};
use chrono::{DateTime, Datelike as _, NaiveDate, TimeDelta, Utc};
use filen_sdk_rs::rclone::ServeTls;
use rcgen::{CertificateParams, DnType, KeyPair};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// The directory in the config directory that holds the certificate.
const DIR: &str = "serve-tls";
const CERT_FILE: &str = "cert.pem";
const KEY_FILE: &str = "key.pem";
/// What the certificate names and when it expires, which reading it back would need an X.509
/// parser for, and the fingerprint of the certificate it describes.
const INFO_FILE: &str = "cert.json";

/// How long a new certificate is valid for, in days.
const VALID_DAYS: u64 = 365;
/// How long before it expires a certificate is replaced.
const RENEW_BEFORE: TimeDelta = TimeDelta::days(1);

/// The names every certificate covers: this computer by name and by loopback address.
const LOCAL_NAMES: [&str; 3] = ["localhost", "127.0.0.1", "::1"];

/// The certificate a server serves HTTPS with.
#[derive(Debug)]
pub(crate) struct SelfSignedCert {
	pub(crate) tls: ServeTls,
	/// The SHA-256 fingerprint of the certificate, uppercase hex bytes joined by `:`.
	pub(crate) fingerprint: String,
	pub(crate) origin: CertOrigin,
}

/// Where the certificate came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CertOrigin {
	/// Made now: there was none, it was about to expire, or it did not name a listen address.
	Generated,
	/// Made by an earlier `serve`.
	Reused,
}

/// What [`INFO_FILE`] holds.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CertInfo {
	/// The certificate's subject alternative names.
	names: BTreeSet<String>,
	not_after: DateTime<Utc>,
	/// The fingerprint of the certificate written with this file, which tells a certificate left
	/// from an interrupted write apart.
	fingerprint: String,
}

/// The certificate in `config_dir` for servers listening on `hosts` (IP addresses or
/// `localhost`), generated when there is none that is valid past `now` plus a day and names every
/// host. A host that listens on every interface (`0.0.0.0`, `::`) adds no name: a client reaches
/// it by an address the certificate cannot know, so it pins the fingerprint instead.
pub(crate) fn ensure(
	config_dir: &Path,
	hosts: &[&str],
	now: DateTime<Utc>,
) -> Result<SelfSignedCert> {
	let dir = config_dir.join(DIR);
	let tls = tls_paths(&dir)?;
	let required = names_for(hosts);
	let existing = read_existing(&dir);
	if let Some((info, fingerprint)) = &existing
		&& info.not_after - RENEW_BEFORE > now
		&& info.names.is_superset(&required)
	{
		return Ok(SelfSignedCert {
			tls,
			fingerprint: fingerprint.clone(),
			origin: CertOrigin::Reused,
		});
	}
	// Names an earlier server needed stay, so that switching between hosts does not change the
	// certificate every time.
	let mut names = required;
	if let Some((info, _)) = existing {
		names.extend(info.names);
	}
	let fingerprint = generate(&dir, names, now)?;
	Ok(SelfSignedCert {
		tls,
		fingerprint,
		origin: CertOrigin::Generated,
	})
}

/// The paths of the certificate and key in `dir`, as the SDK takes them.
fn tls_paths(dir: &Path) -> Result<ServeTls> {
	let path = |name: &str| {
		let path = dir.join(name);
		path.to_str()
			.map(str::to_owned)
			.with_context(|| format!("The path {} is not valid UTF-8", path.display()))
	};
	Ok(ServeTls {
		cert_path: path(CERT_FILE)?,
		key_path: path(KEY_FILE)?,
	})
}

/// The names a certificate for servers on `hosts` must cover.
fn names_for(hosts: &[&str]) -> BTreeSet<String> {
	let mut names = LOCAL_NAMES
		.map(str::to_owned)
		.into_iter()
		.collect::<BTreeSet<_>>();
	for host in hosts {
		match host.parse::<IpAddr>() {
			Ok(ip) if ip.is_unspecified() => {}
			// in its canonical form, as the certificate holds it
			Ok(ip) => {
				names.insert(ip.to_string());
			}
			Err(_) => {
				names.insert((*host).to_owned());
			}
		}
	}
	names
}

/// The certificate in `dir` and the fingerprint of its PEM file, when it is complete: its info
/// file names it, and the key is there. Anything less is no certificate, to be generated again.
fn read_existing(dir: &Path) -> Option<(CertInfo, String)> {
	let read = || -> Result<(CertInfo, String)> {
		let info: CertInfo = serde_json::from_slice(&fs::read(dir.join(INFO_FILE))?)?;
		let fingerprint = pem_fingerprint(&fs::read_to_string(dir.join(CERT_FILE))?)
			.context("not a PEM certificate")?;
		anyhow::ensure!(info.fingerprint == fingerprint, "the certificate changed");
		anyhow::ensure!(dir.join(KEY_FILE).is_file(), "the key is missing");
		Ok((info, fingerprint))
	};
	match read() {
		Ok(existing) => Some(existing),
		Err(e) => {
			log::info!(
				"Generating a new self-signed certificate in {}: {e:#}",
				dir.display()
			);
			None
		}
	}
}

/// Writes a new key and a certificate naming `names`, valid for [`VALID_DAYS`] from `now`, into
/// `dir`, returning its fingerprint. Each file is replaced whole, the info file last: until it
/// is, the certificate it describes is the old one, and the next `serve` generates another.
fn generate(dir: &Path, names: BTreeSet<String>, now: DateTime<Utc>) -> Result<String> {
	let key = KeyPair::generate().context("Failed to generate a key")?;
	let mut params = CertificateParams::new(names.iter().cloned().collect::<Vec<_>>())
		.context("Failed to describe the certificate")?;
	params
		.distinguished_name
		.push(DnType::CommonName, "Filen CLI serve");
	// from the day before, for a client whose clock is behind
	let not_before = now.date_naive() - TimeDelta::days(1);
	let not_after = now.date_naive() + chrono::Days::new(VALID_DAYS);
	let (year, month, day) = ymd(not_before);
	params.not_before = rcgen::date_time_ymd(year, month, day);
	let (year, month, day) = ymd(not_after);
	params.not_after = rcgen::date_time_ymd(year, month, day);
	let cert = params
		.self_signed(&key)
		.context("Failed to make the certificate")?;
	let fingerprint = fingerprint(cert.der());
	let info = CertInfo {
		names,
		not_after: not_after
			.and_hms_opt(0, 0, 0)
			.expect("midnight is a time (should be impossible)")
			.and_utc(),
		fingerprint: fingerprint.clone(),
	};
	fs::create_dir_all(dir).with_context(|| format!("Failed to create {}", dir.display()))?;
	write_private(dir, CERT_FILE, cert.pem().as_bytes())?;
	write_private(dir, KEY_FILE, key.serialize_pem().as_bytes())?;
	write_private(dir, INFO_FILE, &serde_json::to_vec_pretty(&info)?)?;
	Ok(fingerprint)
}

/// The year, month and day of `date`, as rcgen takes a date.
fn ymd(date: NaiveDate) -> (i32, u8, u8) {
	let small =
		|value: u32| u8::try_from(value).expect("a month or day fits a u8 (should be impossible)");
	(date.year(), small(date.month()), small(date.day()))
}

/// Replaces `dir/name` with `contents`, through a temporary file that only this user can read on
/// unix (0600), which keeps the private key private.
fn write_private(dir: &Path, name: &str, contents: &[u8]) -> Result<()> {
	let path = dir.join(name);
	let mut file = tempfile::NamedTempFile::new_in(dir)
		.with_context(|| format!("Failed to write {}", path.display()))?;
	file.write_all(contents)
		.with_context(|| format!("Failed to write {}", path.display()))?;
	file.persist(&path)
		.with_context(|| format!("Failed to write {}", path.display()))?;
	Ok(())
}

/// The fingerprint of the DER certificate `der`.
fn fingerprint(der: &[u8]) -> String {
	Sha256::digest(der)
		.iter()
		.map(|byte| format!("{byte:02X}"))
		.collect::<Vec<_>>()
		.join(":")
}

/// The fingerprint of the first certificate in the PEM text `pem`.
fn pem_fingerprint(pem: &str) -> Option<String> {
	let (_, rest) = pem.split_once("-----BEGIN CERTIFICATE-----")?;
	let (body, _) = rest.split_once("-----END CERTIFICATE-----")?;
	let body = body.split_whitespace().collect::<String>();
	base64.decode(body).ok().map(|der| fingerprint(&der))
}

#[cfg(test)]
mod tests {
	use chrono::TimeZone as _;
	use tempfile::TempDir;

	use super::*;

	fn now() -> DateTime<Utc> {
		Utc.with_ymd_and_hms(2026, 11, 3, 12, 0, 0).unwrap()
	}

	fn info(config_dir: &TempDir) -> CertInfo {
		serde_json::from_slice(&fs::read(config_dir.path().join(DIR).join(INFO_FILE)).unwrap())
			.unwrap()
	}

	#[test]
	fn a_new_certificate_names_this_computer_and_the_hosts_listened_on() {
		let config_dir = tempfile::tempdir().unwrap();

		let cert = ensure(
			config_dir.path(),
			&["192.168.1.5", "0.0.0.0", "::0001"],
			now(),
		)
		.unwrap();

		assert_eq!(cert.origin, CertOrigin::Generated);
		assert_eq!(
			info(&config_dir).names.into_iter().collect::<Vec<_>>(),
			["127.0.0.1", "192.168.1.5", "::1", "localhost"]
		);
		assert_eq!(
			info(&config_dir).not_after,
			Utc.with_ymd_and_hms(2027, 11, 3, 0, 0, 0).unwrap()
		);
		let dir = config_dir.path().join(DIR);
		assert_eq!(cert.tls.cert_path, dir.join(CERT_FILE).to_str().unwrap());
		assert_eq!(cert.tls.key_path, dir.join(KEY_FILE).to_str().unwrap());
		assert_eq!(
			pem_fingerprint(&fs::read_to_string(dir.join(CERT_FILE)).unwrap()).unwrap(),
			cert.fingerprint
		);
		assert!(
			fs::read_to_string(dir.join(KEY_FILE))
				.unwrap()
				.starts_with("-----BEGIN PRIVATE KEY-----")
		);
	}

	#[cfg(unix)]
	#[test]
	fn only_the_user_can_read_the_key() {
		use std::os::unix::fs::PermissionsExt as _;

		let config_dir = tempfile::tempdir().unwrap();
		let cert = ensure(config_dir.path(), &["127.0.0.1"], now()).unwrap();

		let mode = fs::metadata(cert.tls.key_path)
			.unwrap()
			.permissions()
			.mode();
		assert_eq!(mode & 0o777, 0o600);
	}

	#[test]
	fn a_certificate_is_reused_until_a_day_before_it_expires() {
		let config_dir = tempfile::tempdir().unwrap();
		let first = ensure(config_dir.path(), &["127.0.0.1"], now()).unwrap();

		let reused = ensure(
			config_dir.path(),
			&["127.0.0.1"],
			now() + TimeDelta::days(363),
		)
		.unwrap();
		assert_eq!(reused.origin, CertOrigin::Reused);
		assert_eq!(reused.fingerprint, first.fingerprint);

		let renewed = ensure(
			config_dir.path(),
			&["127.0.0.1"],
			now() + TimeDelta::days(364),
		)
		.unwrap();
		assert_eq!(renewed.origin, CertOrigin::Generated);
		assert_ne!(renewed.fingerprint, first.fingerprint);
	}

	#[test]
	fn a_host_the_certificate_does_not_name_replaces_it_keeping_the_old_names() {
		let config_dir = tempfile::tempdir().unwrap();
		let first = ensure(config_dir.path(), &["192.168.1.5"], now()).unwrap();

		let second = ensure(config_dir.path(), &["10.0.0.2"], now()).unwrap();
		assert_eq!(second.origin, CertOrigin::Generated);
		assert_ne!(second.fingerprint, first.fingerprint);
		assert_eq!(
			info(&config_dir).names.into_iter().collect::<Vec<_>>(),
			["10.0.0.2", "127.0.0.1", "192.168.1.5", "::1", "localhost"]
		);

		let third = ensure(config_dir.path(), &["192.168.1.5"], now()).unwrap();
		assert_eq!(third.origin, CertOrigin::Reused);
		assert_eq!(third.fingerprint, second.fingerprint);
	}

	#[test]
	fn a_listen_address_on_every_interface_needs_no_new_certificate() {
		let config_dir = tempfile::tempdir().unwrap();
		ensure(config_dir.path(), &["127.0.0.1"], now()).unwrap();

		let cert = ensure(config_dir.path(), &["0.0.0.0", "::", "localhost"], now()).unwrap();

		assert_eq!(cert.origin, CertOrigin::Reused);
	}

	#[test]
	fn a_certificate_its_info_does_not_describe_is_replaced() {
		let config_dir = tempfile::tempdir().unwrap();
		let first = ensure(config_dir.path(), &["127.0.0.1"], now()).unwrap();
		fs::write(&first.tls.cert_path, "not a certificate").unwrap();

		let second = ensure(config_dir.path(), &["127.0.0.1"], now()).unwrap();

		assert_eq!(second.origin, CertOrigin::Generated);
		assert_eq!(
			pem_fingerprint(&fs::read_to_string(&second.tls.cert_path).unwrap()).unwrap(),
			second.fingerprint
		);
	}

	#[test]
	fn a_missing_key_replaces_the_certificate() {
		let config_dir = tempfile::tempdir().unwrap();
		let first = ensure(config_dir.path(), &["127.0.0.1"], now()).unwrap();
		fs::remove_file(&first.tls.key_path).unwrap();

		let second = ensure(config_dir.path(), &["127.0.0.1"], now()).unwrap();

		assert_eq!(second.origin, CertOrigin::Generated);
		assert!(Path::new(&second.tls.key_path).is_file());
	}

	#[test]
	fn a_fingerprint_is_the_sha256_digest_in_uppercase_hex_pairs() {
		assert_eq!(
			fingerprint(b"binary"),
			"9A:3A:45:D0:15:31:A2:0E:89:AC:6A:E1:0B:0B:0B:EB:04:92:AC:D7:21:6A:36:8A:A0:62:D1:A5:FE:CA:F9:CD"
		);
	}

	#[test]
	fn a_pem_fingerprint_is_that_of_the_decoded_certificate() {
		// "binary", base64 encoded over two lines
		let pem = "-----BEGIN CERTIFICATE-----\nYmlu\nYXJ5\n-----END CERTIFICATE-----\n";
		assert_eq!(pem_fingerprint(pem), Some(fingerprint(b"binary")));
		assert_eq!(pem_fingerprint("YmluYXJ5"), None);
	}
}

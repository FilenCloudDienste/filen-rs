//! A Credential Manager round trip of the sealing key, under a throwaway account: none stored, one
//! generated and stored by the session store, the same one loaded by a second process, a malformed
//! secret replaced, and the credential deleted.

use std::{env, fs, path::Path, process::Command, time::Duration};

use filen_cloud_files::{
	CredentialManagerKeyStore, DataDir, KeyAccount, KeyStore, KeyStoreError, SealingKey,
	SessionStore,
};

use crate::report::{Outcome, Report, wait_for};

/// A fingerprint of `key` for comparing it across processes: its BLAKE3, never the key itself.
fn fingerprint(key: Option<&SealingKey>) -> String {
	key.map_or_else(
		|| "none".to_owned(),
		|key| blake3::hash(key.as_bytes()).to_hex().to_string(),
	)
}

/// What a second process loads for `account`: a fingerprint, `none`, or the error.
fn load_elsewhere(
	account: &KeyAccount,
	out: &Path,
	name: &str,
) -> Result<String, Box<dyn std::error::Error>> {
	let file = out.join(name);
	let status = Command::new(env::current_exe()?)
		.args([
			"keys-load".as_ref(),
			account.as_str().as_ref(),
			file.as_os_str(),
		])
		.status()?;
	if !wait_for(&file, Duration::from_secs(5)) {
		return Ok(format!("no answer ({status})"));
	}
	Ok(fs::read_to_string(&file)?)
}

/// The second process: loads the key of `account` and writes its fingerprint to `file`.
pub(crate) fn load_in_child(account: &str, file: &Path) -> Outcome {
	let store = CredentialManagerKeyStore::new(&KeyAccount::new(account)?)?;
	let answer = match store.load() {
		Ok(key) => fingerprint(key.as_ref()),
		Err(error) => format!("error: {error}"),
	};
	fs::write(file, answer)?;
	Ok(true)
}

pub(crate) fn run(out: &Path) -> Outcome {
	fs::create_dir_all(out)?;
	let mut report = Report::create(&out.join("keys.txt"))?;
	let account = KeyAccount::new(&format!("harness-{}", std::process::id()))?;
	let store = CredentialManagerKeyStore::new(&account)?;
	report.info("account", account.as_str());

	let first = store.load();
	report.check(
		"1-none-stored",
		matches!(first, Ok(None)),
		format!("{:?}", first.as_ref().map(Option::is_some)),
	);

	let data = DataDir::new(out.join("keys-data"))?;
	let opened = SessionStore::open(data.clone(), &store);
	report.check(
		"2-open",
		opened.is_ok(),
		format!("{:?}", opened.as_ref().err()),
	);

	let loaded = store.load();
	let again = store.load();
	let stored = match (&loaded, &again) {
		(Ok(Some(a)), Ok(Some(b))) => Some((fingerprint(Some(a)), fingerprint(Some(b)))),
		_ => None,
	};
	report.check(
		"3-stored-and-stable",
		stored.as_ref().is_some_and(|(a, b)| a == b),
		format!("{loaded:?} / {again:?}"),
	);
	let elsewhere = load_elsewhere(&account, out, "keys-load-1.txt")?;
	report.check(
		"3-second-process",
		stored.as_ref().is_some_and(|(a, _)| *a == elsewhere),
		format!(
			"second process loaded {}",
			if elsewhere.len() == 64 {
				"the same fingerprint"
			} else {
				&elsewhere
			}
		),
	);

	keyring::Entry::new(
		CredentialManagerKeyStore::SERVICE,
		&CredentialManagerKeyStore::user(&account),
	)?
	.set_secret(&[1; 5])?;
	let malformed = store.load();
	report.check(
		"4-malformed",
		matches!(&malformed, Err(KeyStoreError::WrongLength(error)) if error.len == 5),
		format!("{malformed:?}"),
	);

	let reopened = SessionStore::open(data, &store);
	let replaced = store.load();
	report.check(
		"5-replaced",
		reopened.is_ok() && matches!(replaced, Ok(Some(_))),
		format!("{:?} / {replaced:?}", reopened.as_ref().err()),
	);

	let deleted = store.delete();
	let gone = store.load();
	report.check(
		"6-deleted",
		deleted.is_ok() && matches!(gone, Ok(None)),
		format!("{deleted:?} / {gone:?}"),
	);
	let elsewhere = load_elsewhere(&account, out, "keys-load-2.txt")?;
	report.check("6-second-process", elsewhere == "none", &elsewhere);
	let deleted_again = store.delete();
	report.check(
		"6-delete-missing",
		deleted_again.is_ok(),
		format!("{deleted_again:?}"),
	);
	fs::write(out.join("keys.done"), "done")?;
	Ok(report.passed())
}

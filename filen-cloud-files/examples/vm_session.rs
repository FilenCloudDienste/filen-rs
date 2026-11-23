//! `vm_session`: hands the Windows VM checks a signed-in session of the test account without anyone
//! reading it. Logs in as the live tests do (`test_utils::RESOURCES`, credentials from TEST_EMAIL /
//! TEST_PASSWORD or a dotenv file) and writes only non-secret status on stdout:
//!
//! - `seed <session-out> <file>...`: uploads the files into a fresh test directory of the account
//!   and writes the `AuthFile` JSON the helper's `setAuth` takes to `session-out`, a new file. On
//!   Unix only its owner can read it; on Windows, where the VM checks run, it takes the ACL its
//!   directory passes on, so `session-out` belongs in a directory only the test's user can read.
//!   Prints the user id, the directory's name and uuid and each file's uuid. The directory is kept;
//!   `delete` removes it.
//! - `delete <dir-uuid>`: deletes that directory permanently.
//! - `upload <dir-uuid> <file>`: uploads the file into that directory under its own name, a new
//!   version of a file already there by that name; `rename <file-uuid> <name>`, `move <file-uuid>
//!   <dir-uuid>`, `trash <file-uuid>` and `mkdir <dir-uuid> <name>`: the remote changes another
//!   client makes, each printing the item's uuid.
//! - `scan <session> <file>...`: counts, in each file, the occurrences of the session's secrets and
//!   of the account's credentials, so a log can be checked before anyone reads it.

use std::{
	env,
	fs::{self, OpenOptions},
	io::Write,
	mem::ManuallyDrop,
	path::Path,
	process::ExitCode,
};

use filen_sdk_rs::{
	auth::Client,
	fs::{HasName, HasUUID, file::meta::FileMetaChanges},
};
use filen_types::fs::Uuid;
use serde_json::{Value, json};
use test_utils::{RESOURCES, rt};

/// The `AuthFile` JSON the desktop app pushes for `client`'s session, as `cloud_files_live` builds
/// it.
fn session_json(client: &Client) -> Vec<u8> {
	let session = client.to_stringified();
	serde_json::to_vec(&json!({
		"providerEnabled": true,
		"sdkConfig": {
			"email": session.email,
			"password": "",
			"twoFactorCode": "",
			"masterKeys": session.auth_info.split('|').collect::<Vec<_>>(),
			"apiKey": session.api_key,
			"publicKey": "",
			"privateKey": session.private_key,
			"authVersion": session.auth_version,
			"baseFolderUUID": session.root_uuid,
			"userId": session.user_id,
			"metadataCache": false,
			"tmpPath": "",
			"connectToSocket": false,
		}
	}))
	.expect("a JSON value serialises")
}

/// Writes `bytes` to `path`, a file that must not exist yet: readable by its owner only on Unix;
/// on Windows with the ACL its directory passes on, which this does not narrow.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
	let mut options = OpenOptions::new();
	options.write(true).create_new(true);
	#[cfg(unix)]
	std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
	options.open(path)?.write_all(bytes)
}

fn seed(session_out: &Path, files: &[String]) -> Result<(), String> {
	rt().block_on(async {
		let (resources, lock) = RESOURCES.get_resources_with_lock().await;
		// The directory outlives this run: its drop would delete it.
		let resources = ManuallyDrop::new(resources);
		if !resources.wait_visible().await {
			return Err("the server never showed the new test directory".to_owned());
		}
		println!(
			"dir {} {}",
			resources.dir.name().unwrap_or_default(),
			resources.dir.uuid()
		);
		for file in files {
			let path = Path::new(file);
			let name = path
				.file_name()
				.and_then(|name| name.to_str())
				.ok_or_else(|| format!("{file} has no file name"))?;
			let data = fs::read(path).map_err(|e| format!("reading {file}: {e}"))?;
			let builder = resources
				.client
				.make_file_builder(name, resources.dir.uuid())
				.map_err(|e| format!("{name}: {e}"))?;
			let uploaded = resources
				.client
				.upload_file(builder, &data)
				.await
				.map_err(|e| format!("uploading {name}: {e}"))?;
			println!("file {name} {} {}", uploaded.uuid(), data.len());
		}
		drop(lock);
		println!("user {}", resources.client.to_stringified().user_id);
		write_private(session_out, &session_json(&resources.client))
			.map_err(|e| format!("writing the session: {e}"))?;
		println!("session written");
		Ok(())
	})
}

fn delete(dir: &str) -> Result<(), String> {
	let uuid: Uuid = dir.parse().map_err(|e| format!("{dir}: {e}"))?;
	rt().block_on(async {
		let client = RESOURCES.client().await;
		let lock = client
			.lock_drive()
			.await
			.map_err(|e| format!("locking the drive: {e}"))?;
		let dir = client
			.get_dir(uuid)
			.await
			.map_err(|e| format!("getting the directory: {e}"))?;
		client
			.delete_dir_permanently(dir)
			.await
			.map_err(|e| format!("deleting the directory: {e}"))?;
		drop(lock);
		println!("deleted {uuid}");
		Ok(())
	})
}

/// The remote change `mode` with its `args`, as another client of the account makes it.
fn change(mode: &str, args: &[String]) -> Result<(), String> {
	let uuid = |arg: &str| arg.parse::<Uuid>().map_err(|e| format!("{arg}: {e}"));
	rt().block_on(async {
		let client = RESOURCES.client().await;
		match (mode, args) {
			("upload", [dir, file]) => {
				let path = Path::new(file);
				let name = path
					.file_name()
					.and_then(|name| name.to_str())
					.ok_or_else(|| format!("{file} has no file name"))?;
				let data = fs::read(path).map_err(|e| format!("reading {file}: {e}"))?;
				let builder = client
					.make_file_builder(name, uuid(dir)?)
					.map_err(|e| format!("{name}: {e}"))?;
				let uploaded = client
					.upload_file(builder, &data)
					.await
					.map_err(|e| format!("uploading {name}: {e}"))?;
				println!("uploaded {name} {} {}", uploaded.uuid(), data.len());
			}
			("rename", [file, name]) => {
				let mut file = client
					.get_file(uuid(file)?)
					.await
					.map_err(|e| format!("getting the file: {e}"))?;
				let changes = FileMetaChanges::default()
					.name(name)
					.map_err(|e| format!("{name}: {e}"))?;
				client
					.update_file_metadata(&mut file, changes)
					.await
					.map_err(|e| format!("renaming: {e}"))?;
				println!("renamed {}", file.uuid());
			}
			("move", [file, dir]) => {
				let mut file = client
					.get_file(uuid(file)?)
					.await
					.map_err(|e| format!("getting the file: {e}"))?;
				let dir = client
					.get_dir(uuid(dir)?)
					.await
					.map_err(|e| format!("getting the directory: {e}"))?;
				client
					.move_file(&mut file, &(&dir).into())
					.await
					.map_err(|e| format!("moving: {e}"))?;
				println!("moved {}", file.uuid());
			}
			("trash", [file]) => {
				let mut file = client
					.get_file(uuid(file)?)
					.await
					.map_err(|e| format!("getting the file: {e}"))?;
				client
					.trash_file(&mut file)
					.await
					.map_err(|e| format!("trashing: {e}"))?;
				println!("trashed {}", file.uuid());
			}
			("mkdir", [parent, name]) => {
				let parent = client
					.get_dir(uuid(parent)?)
					.await
					.map_err(|e| format!("getting the directory: {e}"))?;
				let dir = client
					.create_dir(&(&parent).into(), name)
					.await
					.map_err(|e| format!("creating {name}: {e}"))?;
				println!("dir {name} {}", dir.uuid());
			}
			_ => return Err(format!("{mode}: wrong arguments")),
		}
		Ok(())
	})
}

/// The non-empty secret strings of the session at `path` and the account's credentials.
fn secrets(path: &Path) -> Result<Vec<String>, String> {
	// serde_json's message can quote the payload, so only its category is shown.
	let session: Value =
		serde_json::from_slice(&fs::read(path).map_err(|e| format!("reading the session: {e}"))?)
			.map_err(|e| format!("the session is not JSON ({:?})", e.classify()))?;
	let config = &session["sdkConfig"];
	let mut found: Vec<String> = ["email", "password", "apiKey", "privateKey"]
		.iter()
		.filter_map(|key| config[key].as_str().map(str::to_owned))
		.collect();
	if let Some(keys) = config["masterKeys"].as_array() {
		found.extend(
			keys.iter()
				.filter_map(|key| key.as_str().map(str::to_owned)),
		);
	}
	let (email, password, _) = RESOURCES.get_credentials();
	found.push(email);
	found.push(password);
	found.retain(|secret| !secret.is_empty());
	Ok(found)
}

fn scan(session: &Path, files: &[String]) -> Result<(), String> {
	let secrets = secrets(session)?;
	for file in files {
		let data = fs::read(file).map_err(|e| format!("reading {file}: {e}"))?;
		let text = String::from_utf8_lossy(&data);
		let hits: usize = secrets
			.iter()
			.map(|secret| text.matches(secret.as_str()).count())
			.sum();
		println!("{file}: {} bytes, {hits} secret occurrences", data.len());
	}
	Ok(())
}

fn main() -> ExitCode {
	let args: Vec<String> = env::args().skip(1).collect();
	let result = match args.split_first() {
		Some((mode, rest)) if mode == "seed" && !rest.is_empty() => {
			seed(Path::new(&rest[0]), &rest[1..])
		}
		Some((mode, [dir])) if mode == "delete" => delete(dir),
		Some((mode, rest))
			if ["upload", "rename", "move", "trash", "mkdir"].contains(&mode.as_str()) =>
		{
			change(mode, rest)
		}
		Some((mode, rest)) if mode == "scan" && !rest.is_empty() => {
			scan(Path::new(&rest[0]), &rest[1..])
		}
		_ => Err(
			"usage: vm_session seed <session-out> <file>... | delete <dir-uuid> | scan <session> \
			 <file>... | upload <dir-uuid> <file> | rename <file-uuid> <name> | move <file-uuid> \
			 <dir-uuid> | trash <file-uuid> | mkdir <dir-uuid> <name>"
				.to_owned(),
		),
	};
	match result {
		Ok(()) => ExitCode::SUCCESS,
		Err(e) => {
			eprintln!("{e}");
			ExitCode::FAILURE
		}
	}
}

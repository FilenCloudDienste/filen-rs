//! Lend a live test that cannot create remote items a fresh directory of the test account: the
//! napi addon's live test (`filen-sdk-rs/web/napi/live.test.mjs`), whose addon has no API to make
//! one. The directory comes from [`RESOURCES`] like every live test's. Once the server shows it,
//! its uuid is printed on stdout; the directory is deleted permanently when stdin closes, so it goes
//! when the caller ends it and when the caller dies.
//!
//! Credentials come from TEST_EMAIL / TEST_PASSWORD or a dotenv file, as for the Rust tests. Set
//! FILEN_TEST_LOG_STDERR, or the SDK's logs share stdout with the uuid.

use std::{io, process::ExitCode};

use filen_sdk_rs::fs::HasUUID;
use test_utils::{RESOURCES, rt};

fn main() -> ExitCode {
	// Every return drops `resources` outside the runtime, where its drop deletes the directory
	// before the process exits; inside it, the drop would only spawn the deletion.
	let resources = rt().block_on(RESOURCES.get_resources());
	if !rt().block_on(resources.wait_visible()) {
		eprintln!("the server never showed the new test directory");
		return ExitCode::FAILURE;
	}
	println!("{}", resources.dir.uuid());
	if let Err(e) = io::copy(&mut io::stdin().lock(), &mut io::sink()) {
		eprintln!("reading stdin failed, deleting the directory now: {e}");
		return ExitCode::FAILURE;
	}
	ExitCode::SUCCESS
}

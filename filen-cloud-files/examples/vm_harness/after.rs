//! After the provider unregistered: hydrated files stay as ordinary files with their bytes, and
//! dehydrated placeholders are gone.

use std::{fs, os::windows::fs::MetadataExt, path::Path, time::Duration};

use filen_cloud_files::test_support::pattern;

use crate::{
	check::{answers, transfers_of_size},
	report::{Outcome, Report, events, wait_for},
	tree,
};

/// `FILE_ATTRIBUTE_REPARSE_POINT`: a placeholder is one.
const REPARSE_POINT: u32 = 0x400;

pub(crate) fn run(root: &Path, out: &Path) -> Outcome {
	let mut report = Report::create(&out.join("after.txt"))?;
	if !wait_for(&out.join("provider.unregistered"), Duration::from_secs(60)) {
		report.fail(
			"unregistered",
			"the provider did not unregister within 60 s",
		);
		fs::write(out.join("after.done"), "done")?;
		return Ok(false);
	}
	let marker = fs::read_to_string(out.join("provider.unregistered"))?;
	report.check("unregistered", marker == "ok", &marker);
	abandoned(&mut report, out);
	let hydrated = fs::read_to_string(out.join("hydrated.txt")).unwrap_or_default();
	for name in hydrated.lines().filter(|line| !line.is_empty()) {
		let path = root.join(name);
		let step = format!("kept-{name}");
		let Some(size) = tree::size_of(name) else {
			report.fail(&step, "not a file of the tree");
			continue;
		};
		match fs::metadata(&path) {
			Ok(metadata) if metadata.file_attributes() & REPARSE_POINT != 0 => report.fail(
				&step,
				format!(
					"still a reparse point, attributes {:#x}",
					metadata.file_attributes()
				),
			),
			Ok(_) => match fs::read(&path) {
				Ok(bytes) => report.check(
					&step,
					bytes == pattern(size),
					format!("{} bytes", bytes.len()),
				),
				Err(error) => report.fail(&step, error),
			},
			Err(error) => report.fail(&step, error),
		}
	}
	// Files a whole read left partly hydrated (check step 10): unregistering deletes them as it
	// deletes any placeholder that is not fully hydrated.
	let partial = fs::read_to_string(out.join("partial.txt")).unwrap_or_default();
	for name in partial.lines().filter(|line| !line.is_empty()) {
		report.info(
			&format!("partial-{name}"),
			format!("exists: {}", root.join(name).exists()),
		);
	}
	let never = root.join("never.bin");
	report.check(
		"gone-never.bin",
		!never.exists(),
		format!("exists: {}", never.exists()),
	);
	for name in [
		"damaged.bin",
		"damaged-big.bin",
		"cancel.bin",
		"openclose.bin",
		"truncated.bin",
	] {
		let detail = match fs::metadata(root.join(name)) {
			Ok(metadata) => format!(
				"exists, {} bytes, attributes {:#x}",
				metadata.len(),
				metadata.file_attributes()
			),
			Err(error) => format!("{error}"),
		};
		report.info(&format!("left-{name}"), detail);
	}
	let removed = fs::remove_dir_all(root);
	report.check("remove-root", removed.is_ok(), format!("{removed:?}"));
	fs::write(out.join("after.done"), "done")?;
	Ok(report.passed())
}

/// The read check step 13 left waiting: the shutdown must have failed it, not left it to the OS's
/// 60 s timeout.
fn abandoned(report: &mut Report, out: &Path) {
	let file = out.join("abandon.txt");
	let finished = wait_for(&file, Duration::from_secs(30));
	let line = fs::read_to_string(&file).unwrap_or_default();
	let events = events(out);
	let transfers = transfers_of_size(&events, tree::size_of("abandon.bin").expect("a tree file"));
	let answered = answers(&events, &transfers);
	report.check(
		"abandoned-read",
		finished && line.starts_with("err ") && answered.iter().any(|answer| answer == "abandoned"),
		format!("reader {line:?}; answers {answered:?}"),
	);
}

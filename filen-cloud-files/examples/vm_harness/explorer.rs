//! What File Explorer is told about a served root, checked from outside the helper as a user's
//! process sees it: the registry keys through `reg.exe`, the shell's commands and properties
//! through PowerShell's `Shell.Application`, run as the logged-on user. Used by `helper-drive`'s
//! H18 steps, and by `explorer <op> <root> <out>` after an `--unregister-all`.

use std::{
	fs,
	os::windows::fs::MetadataExt,
	path::Path,
	process::Command,
	thread,
	time::{Duration, Instant},
};

use crate::report::{Outcome, Report};

/// The machine-wide sync root list.
const SYNC_ROOT_MANAGER: &str =
	r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\SyncRootManager";
/// The user's own COM classes.
const USER_CLSIDS: &str = r"HKCU\Software\Classes\CLSID";
/// The navigation pane's entries.
const DESKTOP_NAMESPACE: &str =
	r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\Desktop\NameSpace";
/// The desktop icons hidden.
const HIDE_DESKTOP_ICONS: &str =
	r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\HideDesktopIcons\NewStartPanel";
/// The command that pins a file, as Explorer's menu names it, its accelerator removed.
const PIN_VERB: &str = "Always keep on this device";
/// The command that frees a file's space.
const FREE_VERB: &str = "Free up space";
/// `FILE_ATTRIBUTE_PINNED`.
const PINNED: u32 = 0x8_0000;
/// `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`: a placeholder with data not on disk has it.
const RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;

/// What a command printed, stdout then stderr; the error when it did not start.
fn output(command: &mut Command) -> String {
	command.output().map_or_else(
		|error| format!("not started: {error}"),
		|output| {
			format!(
				"{}{}",
				String::from_utf8_lossy(&output.stdout),
				String::from_utf8_lossy(&output.stderr)
			)
		},
	)
}

/// `reg query <args>`'s output.
fn reg_query(args: &[&str]) -> String {
	output(Command::new("reg").arg("query").args(args))
}

/// Whether a `reg query` found nothing.
fn not_found(text: &str) -> bool {
	text.contains("unable to find") || text.contains("End of search: 0 match")
}

/// What a PowerShell script printed.
fn powershell(script: &str) -> String {
	output(Command::new("powershell.exe").args([
		"-NoProfile",
		"-NonInteractive",
		"-ExecutionPolicy",
		"Bypass",
		"-Command",
		script,
	]))
}

/// The SID of the user this process runs as, from `whoami`.
fn own_sid() -> Option<String> {
	let text = output(Command::new("whoami").args(["/user", "/fo", "csv", "/nh"]));
	text.trim()
		.rsplit(',')
		.next()
		.map(|sid| sid.trim_matches('"').to_owned())
		.filter(|sid| sid.starts_with("S-1-"))
}

/// Whether `text` holds a `reg query` line naming `name` of `kind` whose data is `data`.
fn has_value(text: &str, name: &str, kind: &str, data: &str) -> bool {
	text.lines().any(|line| {
		let fields: Vec<&str> = line
			.split("    ")
			.map(str::trim)
			.filter(|f| !f.is_empty())
			.collect();
		fields.first() == Some(&name)
			&& fields.get(1) == Some(&kind)
			&& fields
				.get(2)
				.is_some_and(|value| value.eq_ignore_ascii_case(data))
	})
}

/// The data of the value `name` a `reg query` printed.
fn value_of<'a>(text: &'a str, name: &str) -> Option<&'a str> {
	text.lines().find_map(|line| {
		let fields: Vec<&str> = line
			.split("    ")
			.map(str::trim)
			.filter(|f| !f.is_empty())
			.collect();
		(fields.first() == Some(&name))
			.then(|| fields.get(2).copied())
			.flatten()
	})
}

/// Checks every key the helper writes for `user`'s root at `root`, with its exact type, as
/// `display_name` names it, dumping them to `<out>\explorer-keys.txt`. The navigation-pane class
/// it found, for [`check_unbranded`].
pub(crate) fn check_branded(
	report: &mut Report,
	step: &str,
	root: &Path,
	user: &str,
	display_name: &str,
	out: &Path,
) -> std::io::Result<Option<String>> {
	let root = root.to_string_lossy();
	let Some(sid) = own_sid() else {
		report.fail(step, "the user's SID could not be read");
		return Ok(None);
	};
	let entry = format!(r"{SYNC_ROOT_MANAGER}\Filen!{sid}!{user}");
	let machine = reg_query(&[&entry, "/s"]);
	let clsid = value_of(&machine, "NamespaceCLSID").map(str::to_owned);
	let class = clsid
		.as_deref()
		.map(|clsid| reg_query(&[&format!(r"{USER_CLSIDS}\{clsid}"), "/s"]))
		.unwrap_or_default();
	let namespace = clsid
		.as_deref()
		.map(|clsid| reg_query(&[&format!(r"{DESKTOP_NAMESPACE}\{clsid}")]))
		.unwrap_or_default();
	let hidden = clsid
		.as_deref()
		.map(|clsid| reg_query(&[HIDE_DESKTOP_ICONS, "/v", clsid]))
		.unwrap_or_default();
	fs::write(
		out.join(format!("explorer-keys-{step}.txt")),
		format!("{machine}\n{class}\n{namespace}\n{hidden}"),
	)?;
	report.check(
		&format!("{step}-sync-root-list"),
		has_value(
			&machine,
			"DisplayNameResource",
			"REG_EXPAND_SZ",
			display_name,
		) && value_of(&machine, "IconResource").is_some()
			&& has_value(&machine, "Flags", "REG_DWORD", "0x22")
			&& clsid.as_deref().is_some_and(|clsid| clsid.starts_with('{'))
			&& has_value(&machine, &sid, "REG_SZ", &root),
		format!("{entry}: {}", machine.lines().count()),
	);
	report.check(
		&format!("{step}-navigation-class"),
		has_value(&class, "(Default)", "REG_SZ", display_name)
			&& has_value(&class, "System.IsPinnedToNameSpaceTree", "REG_DWORD", "0x1")
			&& has_value(&class, "FilenCloudFiles", "REG_DWORD", "0x1")
			&& has_value(&class, "TargetFolderPath", "REG_EXPAND_SZ", &root)
			&& has_value(&class, "Attributes", "REG_DWORD", "0x11")
			&& has_value(&class, "FolderValueFlags", "REG_DWORD", "0x228")
			&& has_value(&class, "Attributes", "REG_DWORD", "0xf080004d")
			&& value_of(&class, "CLSID")
				.is_some_and(|c| c.eq_ignore_ascii_case("{0E5AAE11-A475-4c5b-AB00-C66DE400274E}")),
		format!("{clsid:?}: {} lines", class.lines().count()),
	);
	report.check(
		&format!("{step}-navigation-entry"),
		has_value(&namespace, "(Default)", "REG_SZ", display_name)
			&& clsid
				.as_deref()
				.is_some_and(|clsid| has_value(&hidden, clsid, "REG_DWORD", "0x1")),
		format!("{} {}", namespace.trim(), hidden.trim()),
	);
	Ok(clsid)
}

/// Checks nothing the helper writes names `root` any more: no sync root list entry and no
/// navigation-pane class holds its path, and the class `clsid`, when known, is gone with its entry
/// and hidden icon. Dumps what it found to `<out>\explorer-gone-<step>.txt`.
pub(crate) fn check_unbranded(
	report: &mut Report,
	step: &str,
	root: &Path,
	clsid: Option<&str>,
	out: &Path,
) -> std::io::Result<()> {
	let root = root.to_string_lossy();
	let machine = reg_query(&[SYNC_ROOT_MANAGER, "/s", "/f", &root, "/d", "/e"]);
	let classes = reg_query(&[USER_CLSIDS, "/s", "/f", &root, "/d", "/e"]);
	let (class, namespace, hidden) = clsid.map_or_else(Default::default, |clsid| {
		(
			reg_query(&[&format!(r"{USER_CLSIDS}\{clsid}")]),
			reg_query(&[&format!(r"{DESKTOP_NAMESPACE}\{clsid}")]),
			reg_query(&[HIDE_DESKTOP_ICONS, "/v", clsid]),
		)
	});
	fs::write(
		out.join(format!("explorer-gone-{step}.txt")),
		format!("{machine}\n{classes}\n{class}\n{namespace}\n{hidden}"),
	)?;
	let class_gone =
		clsid.is_none() || (not_found(&class) && not_found(&namespace) && not_found(&hidden));
	report.check(
		step,
		not_found(&machine) && not_found(&classes) && class_gone,
		format!(
			"list {} class {} entry {}",
			not_found(&machine),
			not_found(&classes),
			class_gone
		),
	);
	Ok(())
}

/// The commands Explorer's menu offers on `path`, their accelerators removed, and its
/// `System.StorageProviderId`.
fn verbs_and_provider(path: &Path) -> (Vec<String>, String) {
	let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
		return (Vec::new(), String::new());
	};
	let script = format!(
		"$f = (New-Object -ComObject Shell.Application).NameSpace('{}').ParseName('{}'); \
		 ($f.Verbs() | ForEach-Object {{ $_.Name -replace '&','' }}) -join '|'; \
		 $f.ExtendedProperty('System.StorageProviderId')",
		dir.to_string_lossy().replace('\'', "''"),
		name.to_string_lossy().replace('\'', "''"),
	);
	let text = powershell(&script);
	let mut lines = text.lines();
	let verbs = lines
		.next()
		.unwrap_or_default()
		.split('|')
		.map(str::trim)
		.filter(|verb| !verb.is_empty())
		.map(str::to_owned)
		.collect();
	(verbs, lines.next().unwrap_or_default().trim().to_owned())
}

/// Runs the Explorer command `verb` (accelerators ignored) on `path`, as a click on its menu does.
fn invoke(path: &Path, verb: &str) -> String {
	let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
		return "no parent".to_owned();
	};
	powershell(&format!(
		"$f = (New-Object -ComObject Shell.Application).NameSpace('{}').ParseName('{}'); \
		 $v = $f.Verbs() | Where-Object {{ ($_.Name -replace '&','') -eq '{verb}' }} | Select-Object -First 1; \
		 if ($v) {{ $v.DoIt(); 'invoked' }} else {{ 'no such verb' }}",
		dir.to_string_lossy().replace('\'', "''"),
		name.to_string_lossy().replace('\'', "''"),
	))
}

/// `path`'s attributes, read without opening it, so no hydration is raised.
fn attributes(path: &Path) -> Option<u32> {
	fs::symlink_metadata(path)
		.ok()
		.map(|meta| meta.file_attributes())
}

/// Waits up to `limit` for `path`'s attributes to satisfy `wanted`.
fn wait_attributes(path: &Path, limit: Duration, wanted: impl Fn(u32) -> bool) -> Option<u32> {
	let deadline = Instant::now() + limit;
	loop {
		let now = attributes(path);
		if now.is_some_and(&wanted) || Instant::now() >= deadline {
			return now;
		}
		thread::sleep(Duration::from_millis(250));
	}
}

/// The shell's view of the dehydrated file `file` in a branded root: both commands offered, the
/// provider named, and each command doing what it says within 10 s.
pub(crate) fn check_commands(report: &mut Report, step: &str, file: &Path) {
	let (verbs, provider) = verbs_and_provider(file);
	report.check(
		&format!("{step}-verbs"),
		verbs.iter().any(|verb| verb == PIN_VERB) && verbs.iter().any(|verb| verb == FREE_VERB),
		verbs.join("|"),
	);
	report.check(
		&format!("{step}-provider"),
		provider.starts_with("Filen"),
		format!("System.StorageProviderId = {provider:?}"),
	);
	let before = attributes(file);
	let invoked = invoke(file, PIN_VERB);
	let pinned = wait_attributes(file, Duration::from_secs(10), |attrs| {
		attrs & PINNED != 0 && attrs & RECALL_ON_DATA_ACCESS == 0
	});
	report.check(
		&format!("{step}-pin"),
		pinned.is_some_and(|attrs| attrs & PINNED != 0 && attrs & RECALL_ON_DATA_ACCESS == 0),
		format!("{} {before:x?} -> {pinned:x?}", invoked.trim()),
	);
	let invoked = invoke(file, FREE_VERB);
	let freed = wait_attributes(file, Duration::from_secs(10), |attrs| {
		attrs & RECALL_ON_DATA_ACCESS != 0 && attrs & PINNED == 0
	});
	report.check(
		&format!("{step}-free"),
		freed.is_some_and(|attrs| attrs & RECALL_ON_DATA_ACCESS != 0 && attrs & PINNED == 0),
		format!("{} -> {freed:x?}", invoked.trim()),
	);
}

/// Plants what a run killed between the OS's unregister and its clean-up leaves: a sync root list
/// entry and a marked navigation-pane class, both naming `gone`, a root the OS does not hold, and
/// checks they are there (`H18-planted`), so the sweep's check is not met by nothing planted. The
/// class's name.
pub(crate) fn plant_orphan(report: &mut Report, gone: &Path) -> Option<String> {
	let Some(sid) = own_sid() else {
		report.fail("H18-planted", "the user's SID was not read");
		return None;
	};
	let gone = gone.to_string_lossy();
	let clsid = "{0F11E000-0000-8000-8000-00000000F11E}";
	let entry = format!(r"{SYNC_ROOT_MANAGER}\Filen!{sid}!999");
	let class = format!(r"{USER_CLSIDS}\{clsid}");
	let add = |args: &[&str]| output(Command::new("reg").arg("add").args(args).arg("/f"));
	add(&[&entry, "/v", "Flags", "/t", "REG_DWORD", "/d", "34"]);
	add(&[
		&format!(r"{entry}\UserSyncRoots"),
		"/v",
		&sid,
		"/t",
		"REG_SZ",
		"/d",
		&gone,
	]);
	add(&[
		&class,
		"/v",
		"FilenCloudFiles",
		"/t",
		"REG_DWORD",
		"/d",
		"1",
	]);
	add(&[
		&format!(r"{class}\Instance\InitPropertyBag"),
		"/v",
		"TargetFolderPath",
		"/t",
		"REG_EXPAND_SZ",
		"/d",
		&gone,
	]);
	add(&[
		&format!(r"{DESKTOP_NAMESPACE}\{clsid}"),
		"/ve",
		"/d",
		"orphan",
	]);
	let planted = [
		reg_query(&[&format!(r"{entry}\UserSyncRoots"), "/v", &sid]),
		reg_query(&[&class, "/v", "FilenCloudFiles"]),
		reg_query(&[
			&format!(r"{class}\Instance\InitPropertyBag"),
			"/v",
			"TargetFolderPath",
		]),
		reg_query(&[&format!(r"{DESKTOP_NAMESPACE}\{clsid}"), "/ve"]),
	];
	report.check(
		"H18-planted",
		planted
			.iter()
			.all(|text| !not_found(text) && text.contains("REG_")),
		planted.join("\n"),
	);
	Some(clsid.to_owned())
}

/// `explorer gone <root> <out>`: [`check_unbranded`] of `root`, into `<out>\explorer.txt`.
pub(crate) fn run(op: &str, root: &Path, out: &Path) -> Outcome {
	fs::create_dir_all(out)?;
	let mut report = Report::create(&out.join("explorer.txt"))?;
	match op {
		"gone" => check_unbranded(&mut report, "E-gone", root, None, out)?,
		// What an elevated unregister leaves: the machine-wide entry gone, the user's class kept.
		"machine-gone" => {
			let root_text = root.to_string_lossy();
			let machine = reg_query(&[SYNC_ROOT_MANAGER, "/s", "/f", &root_text, "/d", "/e"]);
			let classes = reg_query(&[USER_CLSIDS, "/s", "/f", &root_text, "/d", "/e"]);
			fs::write(
				out.join("explorer-machine-gone.txt"),
				format!("{machine}\n{classes}"),
			)?;
			report.check(
				"E-machine-gone",
				not_found(&machine) && !not_found(&classes),
				format!(
					"list gone {} class left {}",
					not_found(&machine),
					!not_found(&classes)
				),
			);
		}
		_ => report.fail("E-op", format!("unknown op {op}")),
	}
	Ok(report.passed())
}

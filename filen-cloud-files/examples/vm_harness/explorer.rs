//! What File Explorer is told about a served root, checked from outside the helper as a user's
//! process sees it: the registry keys through `reg.exe`, the shell's commands and properties
//! through PowerShell's `Shell.Application`, run as the logged-on user, and the context menu
//! commands' command lines, run as File Explorer runs them against `verb_stub`. Used by
//! `helper-drive`'s H18 and H19 steps, and by `explorer <op> <root> <out>` after an
//! `--unregister-all`.

use std::{
	fs,
	os::windows::{fs::MetadataExt, process::CommandExt},
	path::{Path, PathBuf},
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
		"gone" => {
			check_unbranded(&mut report, "E-gone", root, None, out)?;
			// The user's last root went: the context menu commands with it.
			check_command_keys_gone(&mut report, "E-commands-gone");
		}
		// What an elevated unregister leaves: the machine-wide entry gone, the user's class kept.
		"machine-gone" => {
			let root_text = root.to_string_lossy();
			let machine = reg_query(&[SYNC_ROOT_MANAGER, "/s", "/f", &root_text, "/d", "/e"]);
			let classes = reg_query(&[USER_CLSIDS, "/s", "/f", &root_text, "/d", "/e"]);
			fs::write(
				out.join("explorer-machine-gone.txt"),
				format!("{machine}\n{classes}"),
			)?;
			// The user's context menu commands, which an elevated run leaves as they were: none
			// when the root it unregistered was registered without them, as `leave` registers it.
			let commands = reg_query(&[FILE_VERBS]);
			report.info(
				"E-commands",
				format!("submenu on files present: {}", !not_found(&commands)),
			);
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

/// The context menu commands' submenu on files, and on folders.
const FILE_VERBS: &str = r"HKCU\Software\Classes\*\shell\FilenCloudFiles";
const DIRECTORY_VERBS: &str = r"HKCU\Software\Classes\Directory\shell\FilenCloudFiles";

/// Each command the submenus hold: its key, its action, and whether folders offer it.
const COMMANDS: [(&str, &str, bool); 3] = [
	("01share", "share", true),
	("02web", "web", true),
	("03versions", "versions", false),
];

/// Whether `text` holds a `reg query` line naming the empty `REG_SZ` value `name`.
fn has_empty(text: &str, name: &str) -> bool {
	text.lines().any(|line| {
		let fields: Vec<&str> = line
			.split("    ")
			.map(str::trim)
			.filter(|f| !f.is_empty())
			.collect();
		fields == [name, "REG_SZ"]
	})
}

/// The command line File Explorer is to run for `action`, as the helper writes it.
fn command_line(app: &Path, action: &str) -> String {
	format!(
		"\"{}\" --filen-explorer-command={action} \"%1\"",
		app.display()
	)
}

/// Checks both submenus the helper writes for the commands starting `app`, every value with its
/// exact type: scoped to this provider's items, marked, labelled `menu` with `icon`, single
/// selection, each command's label (`share`, and the helper's English defaults) and command line,
/// and version history on files only. Dumps them to `<out>\explorer-commands-<step>.txt`.
pub(crate) fn check_command_keys(
	report: &mut Report,
	step: &str,
	app: &Path,
	labels: [&str; 4],
	icon: &str,
	out: &Path,
) -> std::io::Result<()> {
	let [menu, share, web, versions] = labels;
	let mut dump = String::new();
	for (tree, on_files) in [(FILE_VERBS, true), (DIRECTORY_VERBS, false)] {
		let head = reg_query(&[tree]);
		dump.push_str(&head);
		let kind = if on_files { "files" } else { "folders" };
		report.check(
			&format!("{step}-{kind}-submenu"),
			has_value(
				&head,
				"AppliesTo",
				"REG_SZ",
				r#"System.StorageProviderId:="Filen""#,
			) && has_value(&head, "FilenCloudFiles", "REG_DWORD", "0x1")
				&& has_value(&head, "MUIVerb", "REG_SZ", menu)
				&& has_value(&head, "Icon", "REG_SZ", icon)
				&& has_value(&head, "MultiSelectModel", "REG_SZ", "Single")
				&& has_empty(&head, "SubCommands"),
			format!("{tree}: {} lines", head.lines().count()),
		);
		for ((key, action, on_folders), label) in COMMANDS.into_iter().zip([share, web, versions]) {
			let verb = format!(r"{tree}\shell\{key}");
			let values = reg_query(&[&verb]);
			let command = reg_query(&[&format!(r"{verb}\command")]);
			dump.push_str(&values);
			dump.push_str(&command);
			let name = format!("{step}-{kind}-{action}");
			if on_files || on_folders {
				report.check(
					&name,
					has_value(&values, "MUIVerb", "REG_SZ", label)
						&& has_value(&values, "MultiSelectModel", "REG_SZ", "Single")
						&& has_value(&command, "(Default)", "REG_SZ", &command_line(app, action)),
					format!("{} {}", values.trim(), command.trim()),
				);
			} else {
				report.check(
					&name,
					not_found(&values),
					format!("absent: {}", values.trim()),
				);
			}
		}
	}
	fs::write(out.join(format!("explorer-commands-{step}.txt")), dump)?;
	Ok(())
}

/// Checks neither submenu is there any more.
pub(crate) fn check_command_keys_gone(report: &mut Report, step: &str) {
	let files = reg_query(&[FILE_VERBS]);
	let folders = reg_query(&[DIRECTORY_VERBS]);
	report.check(
		step,
		not_found(&files) && not_found(&folders),
		format!("files {} folders {}", files.trim(), folders.trim()),
	);
}

/// Plants both submenus, marked, as a run killed between the OS's unregister and its clean-up
/// leaves them once its last root is gone, and checks they are there (`<step>-planted`).
pub(crate) fn plant_command_keys(report: &mut Report, step: &str) {
	let add = |args: &[&str]| output(Command::new("reg").arg("add").args(args).arg("/f"));
	for tree in [FILE_VERBS, DIRECTORY_VERBS] {
		add(&[tree, "/v", "FilenCloudFiles", "/t", "REG_DWORD", "/d", "1"]);
		add(&[tree, "/v", "MUIVerb", "/t", "REG_SZ", "/d", "orphan"]);
	}
	let planted = [
		reg_query(&[FILE_VERBS, "/v", "FilenCloudFiles"]),
		reg_query(&[DIRECTORY_VERBS, "/v", "FilenCloudFiles"]),
	];
	report.check(
		&format!("{step}-planted"),
		planted.iter().all(|text| text.contains("REG_DWORD")),
		planted.join("\n"),
	);
}

/// A plain command the scope check plants beside the helper's submenus, with their scope.
const SCOPE_PROBES: [&str; 2] = [
	r"HKCU\Software\Classes\*\shell\FilenScopeProbe",
	r"HKCU\Software\Classes\Directory\shell\FilenScopeProbe",
];
/// The probe's label.
const SCOPE_PROBE_LABEL: &str = "Filen scope probe";

/// Checks the submenu the helper wrote is offered on each of `inside` and on none of `outside`,
/// and that opening a menu raised no download: the attributes of each item inside stay as they
/// were. The shell's `Verbs()` lists no cascading submenu (one with `SubCommands`), though File
/// Explorer's menu shows it, so the scope is checked through a plain command planted for the
/// check with the `AppliesTo` the helper wrote, and removed after; whether `Verbs()` lists the
/// submenu `menu` itself is reported.
pub(crate) fn check_menu_scope(
	report: &mut Report,
	step: &str,
	menu: &str,
	inside: &[PathBuf],
	outside: &[PathBuf],
) {
	let head = reg_query(&[FILE_VERBS]);
	let Some(scope) = value_of(&head, "AppliesTo") else {
		report.fail(
			&format!("{step}-scope"),
			format!("no AppliesTo: {}", head.trim()),
		);
		return;
	};
	let add = |args: &[&str]| output(Command::new("reg").arg("add").args(args).arg("/f"));
	for probe in SCOPE_PROBES {
		add(&[
			probe,
			"/v",
			"MUIVerb",
			"/t",
			"REG_SZ",
			"/d",
			SCOPE_PROBE_LABEL,
		]);
		add(&[probe, "/v", "AppliesTo", "/t", "REG_SZ", "/d", scope]);
		add(&[
			&format!(r"{probe}\command"),
			"/ve",
			"/t",
			"REG_SZ",
			"/d",
			"notepad.exe",
		]);
	}
	let planted = reg_query(&[SCOPE_PROBES[0], "/v", "AppliesTo"]);
	report.check(
		&format!("{step}-probe"),
		value_of(&planted, "AppliesTo") == Some(scope),
		planted.trim(),
	);
	let shown = |path: &Path| {
		let (verbs, provider) = verbs_and_provider(path);
		let probe = verbs.iter().any(|verb| verb == SCOPE_PROBE_LABEL);
		let submenu = verbs.iter().any(|verb| verb == menu);
		(probe, submenu, verbs, provider)
	};
	for (n, path) in inside.iter().enumerate() {
		let before = attributes(path);
		let (offered, submenu, verbs, provider) = shown(path);
		let after = attributes(path);
		report.check(
			&format!("{step}-inside-{n}"),
			offered && before == after,
			format!(
				"{}: {provider:?} {before:x?} -> {after:x?}; submenu listed {submenu}; {}",
				path.display(),
				verbs.join("|")
			),
		);
	}
	for (n, path) in outside.iter().enumerate() {
		let (offered, submenu, verbs, provider) = shown(path);
		report.check(
			&format!("{step}-outside-{n}"),
			!offered && !submenu,
			format!("{}: {provider:?}; {}", path.display(), verbs.join("|")),
		);
	}
	for probe in SCOPE_PROBES {
		output(Command::new("reg").args(["delete", probe, "/f"]));
	}
	let left = SCOPE_PROBES.map(|probe| reg_query(&[probe]));
	report.check(
		&format!("{step}-probe-removed"),
		left.iter().all(|text| not_found(text)),
		left.join(" ").trim(),
	);
}

/// Where the stub writes what each of its processes was given.
fn stub_records() -> Option<PathBuf> {
	std::env::var_os("LOCALAPPDATA").map(|base| PathBuf::from(base).join("filen-verb-stub"))
}

/// The arguments the stub's process `pid` recorded, each decoded from its UTF-16 units.
fn stub_argv(pid: u32) -> Option<Vec<String>> {
	let text = fs::read_to_string(stub_records()?.join(format!("{pid}.txt"))).ok()?;
	text.split('\n')
		.map(|line| {
			let units = line
				.split(' ')
				.filter(|unit| !unit.is_empty())
				.map(|unit| u16::from_str_radix(unit, 16).ok())
				.collect::<Option<Vec<u16>>>()?;
			String::from_utf16(&units).ok()
		})
		.collect()
}

/// Runs the command line the submenu on files holds for `action` on `item` as File Explorer
/// does, `%1` replaced by the item's full path and the rest passed to the program unparsed: what
/// the program was given.
fn run_command(action: &str, item: &Path) -> Result<Vec<String>, String> {
	let command = reg_query(&[&format!(
		r"{FILE_VERBS}\shell\{}\command",
		COMMANDS
			.iter()
			.find(|(_, name, _)| *name == action)
			.map_or("", |(key, _, _)| *key)
	)]);
	let line = value_of(&command, "(Default)").ok_or_else(|| format!("no command: {command}"))?;
	let (app, args) = line
		.strip_prefix('"')
		.and_then(|rest| rest.split_once('"'))
		.ok_or_else(|| format!("not a quoted program: {line}"))?;
	let args = args.trim_start().replace("%1", &item.to_string_lossy());
	let mut child = Command::new(app)
		.raw_arg(&args)
		.spawn()
		.map_err(|error| format!("not started: {error}"))?;
	let pid = child.id();
	child.wait().map_err(|error| error.to_string())?;
	stub_argv(pid).ok_or_else(|| format!("process {pid} recorded nothing"))
}

/// Runs each command on each of `items` and checks the program got exactly its action and the
/// item's full path, nothing else, whatever the name holds; and that no download was raised: each
/// item's attributes stay as they were.
pub(crate) fn check_command_lines(report: &mut Report, step: &str, items: &[PathBuf]) {
	for (n, item) in items.iter().enumerate() {
		for (_, action, _) in COMMANDS {
			let before = attributes(item);
			let given = run_command(action, item);
			let after = attributes(item);
			let expected = [
				format!("--filen-explorer-command={action}"),
				item.to_string_lossy().into_owned(),
			];
			report.check(
				&format!("{step}-{n}-{action}"),
				given
					.as_ref()
					.is_ok_and(|argv| argv.len() == 3 && argv.get(1..) == Some(&expected[..]))
					&& before == after,
				format!("{given:?} {before:x?} -> {after:x?}"),
			);
		}
	}
}

/// Tries the share command on `item` through the shell's verbs, as `check_commands` runs the
/// root's: whether a submenu's command is reached this way is not known, so it is reported, not
/// checked.
pub(crate) fn report_cascade_invoke(report: &mut Report, step: &str, item: &Path, share: &str) {
	let records = |dir: Option<PathBuf>| {
		dir.and_then(|dir| fs::read_dir(dir).ok())
			.map_or(0, |entries| entries.filter_map(Result::ok).count())
	};
	let before = records(stub_records());
	let invoked = invoke(item, &share.replace('&', ""));
	thread::sleep(Duration::from_secs(3));
	let after = records(stub_records());
	report.info(
		step,
		format!("{}: stub records {before} -> {after}", invoked.trim()),
	);
}

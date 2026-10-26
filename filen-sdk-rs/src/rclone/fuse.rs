//! Which mount backend the system has.
//!
//! - macOS: macFUSE when it is installed (rclone's `cmount`), otherwise rclone's `nfsmount`, which
//!   serves the mount over NFS on loopback and mounts it with the system's own NFS client: no
//!   kernel extension, no installer and no admin prompt. The system lists such a mount as
//!   `localhost:/Filen`.
//! - Windows: WinFsp (`cmount`), which the app installs.
//! - Linux: FUSE 3 (`fusermount3` and `/dev/fuse`), which the distribution provides.
//!
//! `nfsmount` was tried on macOS 26.5 with rclone 1.74.3 as a normal user, against a local
//! directory remote: `mount/mount` mounted `localhost:/Filen` (`nfs, nodev, nosuid, mounted by`
//! the user) within a second, with no prompt; files written through it were read back and reached
//! the remote; `mount/unmount` and `mount/unmountall` unmounted it at once. rclone needs `/sbin`
//! on its `PATH` for it (it runs `mount`). After rclone is killed the mount is stale and `stat` on
//! it blocks indefinitely (for the first few seconds the NFS client still answers from its
//! attribute cache), and `umount -f` as the same user removes it in under a second.

#[cfg(any(target_os = "macos", target_os = "linux", test))]
use std::path::Path;

#[cfg(windows)]
use crate::rclone::system::system_command;

/// A mount backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FuseProvider {
	/// macFUSE, on macOS.
	MacFuse,
	/// rclone's NFS mount, on macOS without macFUSE.
	NfsMount,
	/// WinFsp, on Windows.
	WinFsp,
	/// FUSE 3, on Linux.
	Fuse3,
}

impl FuseProvider {
	/// rc `mount/mount`'s `mountType` for the backend.
	pub(crate) fn mount_type(self) -> &'static str {
		match self {
			Self::MacFuse | Self::WinFsp => "cmount",
			Self::NfsMount => "nfsmount",
			Self::Fuse3 => "mount",
		}
	}
}

/// Whether mounts can be made, and with what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FuseStatus {
	/// Mounts use `provider`, at `version` where the system says.
	Ready {
		provider: FuseProvider,
		version: Option<String>,
	},
	/// There is no mount backend. `installable` is the one the app can install, `None` where the
	/// user installs it (Linux).
	Missing { installable: Option<FuseProvider> },
}

/// The mount backend this system has.
pub async fn fuse_status() -> FuseStatus {
	#[cfg(target_os = "macos")]
	{
		macos_status(Path::new("/"))
	}
	#[cfg(target_os = "linux")]
	{
		linux_status(Path::new("/"), std::env::var_os("PATH").as_deref())
	}
	#[cfg(windows)]
	{
		windows_status().await
	}
	#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
	{
		FuseStatus::Missing { installable: None }
	}
}

/// macOS under `root` (`/` outside tests): macFUSE when both its file system bundle and its
/// library are there, otherwise `nfsmount`, which needs nothing installed.
#[cfg(any(target_os = "macos", test))]
fn macos_status(root: &Path) -> FuseStatus {
	let bundle = root.join("Library/Filesystems/macfuse.fs");
	let library = [
		"usr/local/lib/libfuse.2.dylib",
		"usr/local/lib/libosxfuse.2.dylib",
	]
	.iter()
	.any(|library| root.join(library).is_file());
	if bundle.is_dir() && library {
		let version = std::fs::read_to_string(bundle.join("Contents/Info.plist"))
			.ok()
			.and_then(|plist| plist_string(&plist, "CFBundleShortVersionString"));
		return FuseStatus::Ready {
			provider: FuseProvider::MacFuse,
			version,
		};
	}
	FuseStatus::Ready {
		provider: FuseProvider::NfsMount,
		version: None,
	}
}

/// The string value of `key` in an XML property list.
#[cfg(any(target_os = "macos", test))]
fn plist_string(plist: &str, key: &str) -> Option<String> {
	let after_key = plist.split_once(&format!("<key>{key}</key>"))?.1;
	let value = after_key.trim_start().strip_prefix("<string>")?;
	let (value, _) = value.split_once("</string>")?;
	Some(value.trim().to_owned()).filter(|value| !value.is_empty())
}

/// Linux under `root` (`/` outside tests): FUSE 3 when `fusermount3` is on `path` or in `/usr/bin`
/// or `/bin`, and `/dev/fuse` exists.
#[cfg(any(target_os = "linux", test))]
fn linux_status(root: &Path, path: Option<&std::ffi::OsStr>) -> FuseStatus {
	let mut dirs = path
		.map(|path| std::env::split_paths(path).collect::<Vec<_>>())
		.unwrap_or_default();
	dirs.extend(["/usr/bin", "/bin"].map(std::path::PathBuf::from));
	let fusermount = dirs.iter().any(|dir| {
		let dir = dir.strip_prefix("/").unwrap_or(dir.as_path());
		root.join(dir).join("fusermount3").is_file()
	});
	if fusermount && root.join("dev/fuse").exists() {
		FuseStatus::Ready {
			provider: FuseProvider::Fuse3,
			version: None,
		}
	} else {
		FuseStatus::Missing { installable: None }
	}
}

/// WinFsp when its registry key names an install directory holding the DLL for this
/// architecture.
#[cfg(windows)]
async fn windows_status() -> FuseStatus {
	let installed = match winfsp_install_dir().await {
		Some(dir) => {
			let dll = match std::env::consts::ARCH {
				"aarch64" => "winfsp-a64.dll",
				_ => "winfsp-x64.dll",
			};
			dir.join("bin").join(dll).is_file()
		}
		None => false,
	};
	if installed {
		FuseStatus::Ready {
			provider: FuseProvider::WinFsp,
			version: None,
		}
	} else {
		FuseStatus::Missing {
			installable: Some(FuseProvider::WinFsp),
		}
	}
}

/// WinFsp's install directory, from its 32-bit registry key (where its installer writes it).
#[cfg(windows)]
async fn winfsp_install_dir() -> Option<std::path::PathBuf> {
	let output = system_command("reg")
		.args([
			r"query",
			r"HKLM\SOFTWARE\WinFsp",
			"/v",
			"InstallDir",
			"/reg:32",
		])
		.output()
		.await
		.ok()?;
	if !output.status.success() {
		return None;
	}
	reg_sz_value(&String::from_utf8_lossy(&output.stdout), "InstallDir").map(Into::into)
}

/// The value of the `REG_SZ` `name` in `reg query` output, such as
/// `    InstallDir    REG_SZ    C:\Program Files (x86)\WinFsp\`.
#[cfg(any(windows, test))]
fn reg_sz_value(output: &str, name: &str) -> Option<String> {
	output.lines().find_map(|line| {
		let rest = line.trim_start().strip_prefix(name)?;
		let value = rest.trim_start().strip_prefix("REG_SZ")?;
		Some(value.trim().to_owned()).filter(|value| !value.is_empty())
	})
}

#[cfg(test)]
mod tests {
	use std::{ffi::OsStr, path::Path};

	use super::{FuseProvider, FuseStatus, linux_status, macos_status, reg_sz_value};

	fn touch(root: &Path, path: &str) {
		let path = root.join(path);
		std::fs::create_dir_all(path.parent().unwrap()).unwrap();
		std::fs::write(path, b"").unwrap();
	}

	const MACFUSE_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
	<key>CFBundleIdentifier</key>
	<string>io.macfuse.filesystems.macfuse</string>
	<key>CFBundleShortVersionString</key>
	<string>5.0.7</string>
</dict>
</plist>
"#;

	#[test]
	fn fuse_status_reads_known_paths() {
		let root = tempfile::tempdir().unwrap();
		touch(root.path(), "usr/local/lib/libfuse.2.dylib");
		touch(
			root.path(),
			"Library/Filesystems/macfuse.fs/Contents/Info.plist",
		);
		std::fs::write(
			root.path()
				.join("Library/Filesystems/macfuse.fs/Contents/Info.plist"),
			MACFUSE_PLIST,
		)
		.unwrap();

		assert_eq!(
			macos_status(root.path()),
			FuseStatus::Ready {
				provider: FuseProvider::MacFuse,
				version: Some("5.0.7".to_owned())
			}
		);
	}

	#[test]
	fn macos_without_macfuse_mounts_over_nfs() {
		let root = tempfile::tempdir().unwrap();
		// FUSE-T's library alone is not macFUSE.
		touch(root.path(), "usr/local/lib/libfuse-t.dylib");

		assert_eq!(
			macos_status(root.path()),
			FuseStatus::Ready {
				provider: FuseProvider::NfsMount,
				version: None
			}
		);
	}

	#[test]
	fn macfuse_without_its_library_is_not_used() {
		let root = tempfile::tempdir().unwrap();
		std::fs::create_dir_all(root.path().join("Library/Filesystems/macfuse.fs")).unwrap();

		assert_eq!(
			macos_status(root.path()),
			FuseStatus::Ready {
				provider: FuseProvider::NfsMount,
				version: None
			}
		);
	}

	#[test]
	fn linux_with_fusermount3_and_dev_fuse_is_ready() {
		let root = tempfile::tempdir().unwrap();
		touch(root.path(), "usr/bin/fusermount3");
		touch(root.path(), "dev/fuse");

		assert_eq!(
			linux_status(root.path(), None),
			FuseStatus::Ready {
				provider: FuseProvider::Fuse3,
				version: None
			}
		);
	}

	#[test]
	fn linux_finds_fusermount3_on_path() {
		let root = tempfile::tempdir().unwrap();
		touch(root.path(), "opt/fuse/bin/fusermount3");
		touch(root.path(), "dev/fuse");

		assert_eq!(
			linux_status(root.path(), Some(OsStr::new("/opt/fuse/bin"))),
			FuseStatus::Ready {
				provider: FuseProvider::Fuse3,
				version: None
			}
		);
	}

	#[test]
	fn linux_without_dev_fuse_is_missing() {
		let root = tempfile::tempdir().unwrap();
		touch(root.path(), "usr/bin/fusermount3");

		assert_eq!(
			linux_status(root.path(), None),
			FuseStatus::Missing { installable: None }
		);
	}

	#[test]
	fn reads_winfsp_install_dir_from_reg_query() {
		let output = "\r\nHKEY_LOCAL_MACHINE\\SOFTWARE\\WOW6432Node\\WinFsp\r\n    InstallDir    REG_SZ    C:\\Program Files (x86)\\WinFsp\\\r\n\r\n";

		assert_eq!(
			reg_sz_value(output, "InstallDir"),
			Some(r"C:\Program Files (x86)\WinFsp\".to_owned())
		);
		assert_eq!(reg_sz_value(output, "Version"), None);
	}

	#[test]
	fn mount_types_per_provider() {
		assert_eq!(FuseProvider::MacFuse.mount_type(), "cmount");
		assert_eq!(FuseProvider::NfsMount.mount_type(), "nfsmount");
		assert_eq!(FuseProvider::WinFsp.mount_type(), "cmount");
		assert_eq!(FuseProvider::Fuse3.mount_type(), "mount");
	}
}

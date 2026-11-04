/// The target OSes `platform_net` is set for.
const PLATFORM_NET_TARGET_OSES: [&str; 5] = ["windows", "macos", "linux", "ios", "android"];

fn main() {
	// Without a rerun-if line, cargo reruns the script on any change in the package.
	println!("cargo::rerun-if-changed=build.rs");
	// `platform_net`: the targets whose TLS checks certificates with the OS's verifier
	// (`src/platform_net`). Cargo's `[target]` tables cannot read a build script's cfg, so the
	// list there is spelled out again; keep the two alike.
	println!("cargo::rustc-check-cfg=cfg(platform_net)");
	let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
	if PLATFORM_NET_TARGET_OSES.contains(&os.as_str()) {
		println!("cargo::rustc-cfg=platform_net");
	}
	// The addon's link args: `-undefined dynamic_lookup` on macOS (N-API resolves from the host
	// at load) and `-z nodelete` on linux-gnu (a TLS destructor after unload segfaults).
	#[cfg(feature = "napi")]
	napi_build::setup();
}

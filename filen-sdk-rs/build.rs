fn main() {
	// Without a rerun-if line, cargo reruns the script on any change in the package.
	println!("cargo::rerun-if-changed=build.rs");
	// The addon's link args: `-undefined dynamic_lookup` on macOS (N-API resolves from the host
	// at load) and `-z nodelete` on linux-gnu (a TLS destructor after unload segfaults).
	#[cfg(feature = "napi")]
	napi_build::setup();
}

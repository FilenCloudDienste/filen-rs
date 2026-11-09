fn main() {
	println!("cargo:rerun-if-changed=build.rs");
	println!(
		"cargo:rustc-env=BUILD_TARGET={}",
		std::env::var("TARGET").unwrap()
	);
}

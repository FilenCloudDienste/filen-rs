use std::{
	env,
	path::{Path, PathBuf},
};

use cmake::Config;

fn main() {
	println!("cargo:rerun-if-changed=wrapper.h");
	if env::var("CARGO_CFG_TARGET_OS").unwrap() == "android" {
		// if we don't bundle libc++ this causes problems on android
		println!("cargo:rustc-link-lib=static:-bundle=c++");
	} else if env::var("CARGO_CFG_TARGET_OS").unwrap() != "windows" {
		println!("cargo:rustc-link-lib=c++");
	}

	let libde265_path = build_libde265();
	let libheif_path = build_libheif(&libde265_path);

	let include_path = libheif_path.join("include");

	let bindings = bindgen::Builder::default()
		.header("wrapper.h")
		.clang_arg(format!("-I{}", include_path.display()))
		.parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
		.generate()
		.expect("Unable to generate bindings");

	let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
	bindings
		.write_to_file(out_path.join("bindings.rs"))
		.expect("Couldn't write bindings!");
}

fn config_cmake_for_android(config: &mut Config) {
	if env::var("CARGO_CFG_TARGET_OS").unwrap() != "android" {
		return;
	}

	let Ok(sysroot_path) = env::var("CARGO_NDK_SYSROOT_PATH") else {
		println!(
			"cargo:warning=CARGO_NDK_SYSROOT_PATH is not set, skipping Android NDK configuration"
		);
		return;
	};

	// Android 16KiB page size force
	config.define("ANDROID_SUPPORT_FLEXIBLE_PAGE_SIZES", "ON");

	// /toolchains/llvm/prebuilt/darwin-x86_64/sysroot/
	let ndk_root = PathBuf::from(&sysroot_path)
		.parent() // remove /sysroot
		.and_then(|p| p.parent()) // remove /darwin-x86_64
		.and_then(|p| p.parent()) // remove /prebuilt
		.and_then(|p| p.parent()) // remove /llvm
		.and_then(|p| p.parent()) // remove /toolchains
		.map(|p| p.to_path_buf());

	if let Some(ndk_root) = ndk_root {
		let toolchain_file = ndk_root.join("build/cmake/android.toolchain.cmake");
		if toolchain_file.exists() {
			config.define("CMAKE_TOOLCHAIN_FILE", toolchain_file);
			config.define("ANDROID_NDK", ndk_root);
		}
	} else {
		println!(
			"cargo:warning=Could not determine NDK root path, skipping Android NDK configuration"
		);
	}

	if let Ok(android_target) = env::var("CARGO_NDK_ANDROID_TARGET") {
		config.define("ANDROID_ABI", android_target);
	} else {
		println!("cargo:warning=CARGO_NDK_ANDROID_TARGET is not set, using default Android ABI");
	}
}

fn config_cmake_for_macos(config: &mut Config) {
	if env::var("CARGO_CFG_TARGET_OS").unwrap() != "macos" {
		return;
	}

	// todo add handling for x86_64
	let deployment_target =
		env::var("MACOSX_DEPLOYMENT_TARGET").unwrap_or_else(|_| "11.0".to_string()); // Default to 11.0 (which is standard for arm) if not set
	config.define("CMAKE_OSX_DEPLOYMENT_TARGET", deployment_target);
}

fn config_cmake_for_ios(config: &mut Config) {
	if env::var("CARGO_CFG_TARGET_OS")
		.ok()
		.is_none_or(|os| os != "ios")
	{
		return;
	}

	let deployment_target = env::var("DEPLOYMENT_TARGET").unwrap_or_else(|_| "12.0".to_string());
	config.define("CMAKE_OSX_DEPLOYMENT_TARGET", &deployment_target);

	if env::var("TARGET").unwrap().contains("ios-sim") {
		config.define("CMAKE_OSX_SYSROOT", "iphonesimulator");
	} else {
		config.define("CMAKE_OSX_SYSROOT", "iphoneos");
	}
}

fn config_cmake_for_libcxx(config: &mut Config) {
	// Force CMake to use libc++ instead of libstdc++
	config.define("CMAKE_CXX_FLAGS", "-stdlib=libc++");
	config.define("CMAKE_EXE_LINKER_FLAGS", "-stdlib=libc++");
	config.define("CMAKE_SHARED_LINKER_FLAGS", "-stdlib=libc++");

	// Ensure we're using clang++ for consistency
	config.define("CMAKE_CXX_COMPILER", "clang++");
	config.define("CMAKE_C_COMPILER", "clang");

	// This was causing issues on the windows runner and we don't care about documentation
	config.define("CMAKE_DISABLE_FIND_PACKAGE_Doxygen", "TRUE");
}

fn build_libde265() -> PathBuf {
	let mut config = Config::new("deps/libde265");
	config_cmake_for_android(&mut config);
	config_cmake_for_macos(&mut config);
	config_cmake_for_libcxx(&mut config);
	config_cmake_for_ios(&mut config);

	// ideally I'd also want to disable DEC265 here, but there's no way to do that with cmake
	config.define("ENABLE_SDL", "OFF");
	config.define("ENABLE_ENCODER", "OFF");

	config.define("BUILD_SHARED_LIBS", "OFF");

	let dst = config.build();
	println!("cargo:rerun-if-changed=deps/libde265");
	println!("cargo:rustc-link-search=native={}/lib", dst.display());

	if env::var("CARGO_CFG_TARGET_OS").unwrap() == "windows" {
		println!("cargo:rustc-link-lib=static=libde265");
	} else {
		println!("cargo:rustc-link-lib=static=de265");
	}

	dst
}

fn build_libheif(libde265_path: &Path) -> PathBuf {
	let mut config = Config::new("deps/libheif");
	config_cmake_for_android(&mut config);
	config_cmake_for_macos(&mut config);
	config_cmake_for_libcxx(&mut config);
	config_cmake_for_ios(&mut config);

	config.define("LIBDE265_INCLUDE_DIR", libde265_path.join("include"));

	if env::var("CARGO_CFG_TARGET_OS").unwrap() == "windows" {
		config.define("LIBDE265_LIBRARY", libde265_path.join("lib/libde265.lib"));

		config.define("CMAKE_C_FLAGS", "/DLIBDE265_STATIC_BUILD");
		config.define("CMAKE_CXX_FLAGS", "/DLIBDE265_STATIC_BUILD");
	} else {
		config.define("LIBDE265_LIBRARY", libde265_path.join("lib/libde265.a"));
	}

	config.define("WITH_LIBDE265", "ON");

	config.define("WITH_X265", "OFF");
	config.define("WITH_AOM_ENCODER", "OFF");
	config.define("WITH_AOM_DECODER", "OFF");
	config.define("WITH_RAV1E", "OFF");
	config.define("WITH_DAV1D", "OFF");
	config.define("WITH_SvtEnc", "OFF");
	config.define("WITH_JPEG_DECODER", "OFF");
	config.define("WITH_JPEG_ENCODER", "OFF");
	config.define("WITH_OpenJPEG_DECODER", "OFF");
	config.define("WITH_OpenJPEG_ENCODER", "OFF");
	config.define("WITH_LIBSHARPYUV", "OFF");
	config.define("WITH_OpenH264_DECODER", "OFF");

	config.define("WITH_EXAMPLES", "OFF");
	config.define("BUILD_TESTING", "OFF");

	config.define("BUILD_SHARED_LIBS", "OFF");

	let dst = config.build();

	println!("cargo:rerun-if-changed=deps/libheif");
	println!("cargo:rustc-link-search=native={}/lib", dst.display());
	println!("cargo:rustc-link-lib=static=heif");

	dst
}

use std::{
	env,
	path::{Path, PathBuf},
	process::Command,
};

use cmake::Config;

fn main() {
	println!("cargo:rerun-if-changed=wrapper.h");
	if is_wasm() {
		// No host C++ runtime exists for wasm32-unknown-unknown; wasi-sdk's
		// static archives stand in for it.
		link_wasi_runtime();
	} else if env::var("CARGO_CFG_TARGET_OS").unwrap() == "android" {
		// if we don't bundle libc++ this causes problems on android
		println!("cargo:rustc-link-lib=static:-bundle=c++");
	} else if env::var("CARGO_CFG_TARGET_OS").unwrap() != "windows" {
		println!("cargo:rustc-link-lib=c++");
	}

	let libde265_path = build_libde265();
	let dav1d_path = build_dav1d();
	let libheif_path = build_libheif(&libde265_path, &dav1d_path);

	let include_path = libheif_path.join("include");

	let mut builder = bindgen::Builder::default()
		.header("wrapper.h")
		.clang_arg(format!("-I{}", include_path.display()));
	if is_wasm() {
		// bindgen must see 32-bit wasm layouts and wasi-libc's headers, not
		// the host's. clang's wasm targets default symbols to hidden
		// visibility, and bindgen silently drops non-default-visibility
		// functions — hence -fvisibility=default.
		builder = builder
			.clang_arg("--target=wasm32-wasi")
			.clang_arg("-fvisibility=default")
			.clang_arg(format!(
				"--sysroot={}",
				wasi_sdk_path().join("share/wasi-sysroot").display()
			));
	}
	let bindings = builder
		.parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
		.generate()
		.expect("Unable to generate bindings");

	let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
	bindings
		.write_to_file(out_path.join("bindings.rs"))
		.expect("Couldn't write bindings!");
}

/// The browser build (wasm32-unknown-unknown). The wasm32-wasi* targets are
/// not supported by this workspace; the wasi-sdk sysroot is only borrowed as
/// the C/C++ toolchain, the artifact still links into the wasm-bindgen module.
fn is_wasm() -> bool {
	env::var("CARGO_CFG_TARGET_ARCH").unwrap() == "wasm32"
}

/// The threaded (shared-memory) wasm profile carries `+atomics` in its target
/// features and its link demands that every object matches; the single-thread
/// service-worker profile must not. This is what picks between the two wasi
/// sysroots.
fn wasm_uses_atomics() -> bool {
	// The two wasm profiles differ only in RUSTFLAGS; make sure flipping
	// between them re-runs this script.
	println!("cargo:rerun-if-env-changed=CARGO_CFG_TARGET_FEATURE");
	env::var("CARGO_CFG_TARGET_FEATURE")
		.unwrap_or_default()
		.split(',')
		.any(|feature| feature == "atomics")
}

fn wasi_triple() -> &'static str {
	if wasm_uses_atomics() {
		"wasm32-wasi-threads"
	} else {
		"wasm32-wasi"
	}
}

fn wasi_sdk_path() -> PathBuf {
	println!("cargo:rerun-if-env-changed=WASI_SDK_PATH");
	let path = PathBuf::from(env::var("WASI_SDK_PATH").expect(
		"WASI_SDK_PATH must point at an extracted wasi-sdk release \
		 (https://github.com/WebAssembly/wasi-sdk, needs the \
		 wasm32-wasi-threads sysroot) to build heif-decoder for wasm32",
	));
	assert!(
		path.join("share/wasi-sysroot").is_dir(),
		"WASI_SDK_PATH ({}) has no share/wasi-sysroot",
		path.display()
	);
	path
}

fn config_cmake_for_wasi(config: &mut Config) {
	if !is_wasm() {
		return;
	}

	let sdk = wasi_sdk_path();
	let toolchain = if wasm_uses_atomics() {
		"share/cmake/wasi-sdk-pthread.cmake"
	} else {
		"share/cmake/wasi-sdk.cmake"
	};
	config.define("CMAKE_TOOLCHAIN_FILE", sdk.join(toolchain));
	config.define("WASI_SDK_PREFIX", &sdk);
	// CMake's compiler probes build and link tiny executables, but a bare wasm
	// target has no process model to run them in — probe with a static
	// library instead.
	config.define("CMAKE_TRY_COMPILE_TARGET_TYPE", "STATIC_LIBRARY");
	// - `_WASI_EMULATED_SIGNAL`: libde265 includes <signal.h>, which
	//   wasi-libc only provides in emulated form.
	// - `-mno-nontrapping-fptoint`: both wasm profiles disable this feature on
	//   the Rust side and the linker unions feature sets, so the C++ objects
	//   must not use it either.
	// Defining CMAKE_{C,CXX}_FLAGS ourselves also stops cmake-rs from
	// injecting cc-rs's host-configured flags (CC_wasm32_unknown_unknown &
	// friends, aimed at sqlite's cc build), whose `--target` would fight the
	// wasi toolchain file.
	// wasi-compat.h papers over the small holes in wasi-libc that libheif's
	// never-reached-in-decode corners trip on (see the header).
	println!("cargo:rerun-if-changed=wasi-compat.h");
	let compat_header = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap())
		.join("wasi-compat.h")
		.display()
		.to_string();
	// Exceptions stay ON (wasi-sdk's default): -fno-exceptions cannot compile
	// libheif's file.cc, which includes the throw-using heif_cxx.h. libheif's
	// own code never throws deliberately — the C API returns errors — but the
	// C++ runtime does (a `.resize()` on an absurd declared size raises
	// length_error/bad_alloc), and nothing catches it: the
	// __cxa_throw/__cxa_allocate_exception references this leaves in the
	// objects are satisfied by trapping stubs in src/lib.rs, which is the same
	// abort-on-throw semantics -fno-exceptions would have produced.
	//
	// The header path is quoted: a checkout under a path with a space in it
	// would otherwise split into two flags and fail the build.
	let mut flags = format!(
		"-D_WASI_EMULATED_SIGNAL -mno-nontrapping-fptoint -mbulk-memory -include \"{compat_header}\""
	);
	if wasm_uses_atomics() {
		flags.push_str(" -pthread");
	}
	config.define("CMAKE_C_FLAGS", &flags);
	config.define("CMAKE_CXX_FLAGS", &flags);
}

/// wasi-sdk's static runtime, standing in for the host `-lc++` link: libc++
/// and libc++abi (the exceptions-off `noeh` variants), wasi-libc, the signal
/// emulation libde265 is compiled against, and clang's builtins for the wasi
/// objects. rust-lld reads wasi objects fine — the two targets share the wasm
/// object format and only differ in what the host must provide at runtime.
fn link_wasi_runtime() {
	let sdk = wasi_sdk_path();
	let libdir = sdk.join("share/wasi-sysroot/lib").join(wasi_triple());
	println!(
		"cargo:rustc-link-search=native={}",
		libdir.join("noeh").display()
	);
	println!("cargo:rustc-link-search=native={}", libdir.display());
	println!("cargo:rustc-link-lib=static=c++");
	println!("cargo:rustc-link-lib=static=c++abi");
	println!("cargo:rustc-link-lib=static=wasi-emulated-signal");
	println!("cargo:rustc-link-lib=static=c");

	// lib/clang/<version>/lib/wasm32-unknown-wasi[-threads]/libclang_rt.builtins.a
	//
	// An SDK that carries more than one version dir must resolve the same way
	// every time — `read_dir` order is the filesystem's, not sorted — so the
	// highest major version wins.
	let clang_dir = std::fs::read_dir(sdk.join("lib/clang"))
		.expect("wasi-sdk has no lib/clang")
		.filter_map(Result::ok)
		.map(|entry| entry.path())
		.filter(|path| path.is_dir())
		.max_by_key(|path| {
			path.file_name()
				.and_then(|name| name.to_str())
				.and_then(|name| name.split('.').next())
				.and_then(|major| major.parse::<u32>().ok())
				.unwrap_or(0)
		})
		.expect("wasi-sdk lib/clang has no version dir");
	let rt_triple = if wasm_uses_atomics() {
		"wasm32-unknown-wasi-threads"
	} else {
		"wasm32-unknown-wasi"
	};
	println!(
		"cargo:rustc-link-search=native={}",
		clang_dir.join("lib").join(rt_triple).display()
	);
	println!("cargo:rustc-link-lib=static=clang_rt.builtins");
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

	if let Ok(android_target) = env::var("ANDROID_ABI") {
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
	if is_wasm() {
		// The wasi toolchain file picks the compiler, and libc++ is the only
		// C++ runtime wasi-sdk has.
		return;
	}

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
	// Each cmake project needs its own out dir. cmake-rs builds in
	// `<out_dir>/build` and deletes that whole tree when its CMakeCache.txt
	// belongs to another project, so with both sharing `OUT_DIR`, libde265
	// and libheif wiped each other on every run. The rebuild rewrote
	// heif_version.h, which bindgen has cargo watch, so every cargo command
	// reran this script and rebuilt both libraries from scratch.
	config.out_dir(PathBuf::from(env::var("OUT_DIR").unwrap()).join("libde265"));
	config_cmake_for_android(&mut config);
	config_cmake_for_macos(&mut config);
	config_cmake_for_libcxx(&mut config);
	config_cmake_for_ios(&mut config);
	config_cmake_for_wasi(&mut config);

	config.define("ENABLE_SDL", "OFF");
	config.define("ENABLE_ENCODER", "OFF");
	// The dec265 command-line tool — dead weight on every target, and on wasm
	// its link even fails (CMake's Threads probe answers -lpthreads there).
	config.define("ENABLE_DECODER", "OFF");

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

/// AV1, for AVIF. libheif's container layer is codec-pluggable and identical
/// either way, so this buys AVIF through the exact grid/`thmb`/alpha machinery
/// HEIC already goes through.
///
/// dav1d rather than libaom, on every target. libaom's DECODER recovers from a
/// malformed bitstream with `setjmp`/`longjmp` (`av1/decoder/decodeframe.c`,
/// `av1/decoder/obu.c`, `av1/av1_dx_iface.c`), which a decode-only build cannot
/// configure away; on wasm `setjmp` is only expressible through the
/// exception-handling proposal, and the `__c_longjmp` tag that leaves in the
/// module is refused by wasm-bindgen 0.2.104's parser. dav1d contains no
/// `setjmp` or `longjmp` at all, so the browser build gets AVIF too — and two
/// AV1 decoders in one tree would be pure maintenance cost.
///
/// The price is dav1d's build system: meson + ninja, plus nasm for the x86
/// SIMD. Those are demanded HERE, and note who meets them: this crate is a
/// workspace member with no `default-members`, so any cargo command run at the
/// workspace root builds it whatever features were asked for. Skipping the
/// tooling means `--workspace --exclude heif-decoder`, or a package-scoped
/// `-p` command — not leaving the feature off.
fn build_dav1d() -> PathBuf {
	let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
	let build_dir = out_dir.join("dav1d-build");
	let prefix = out_dir.join("dav1d");

	// dav1d's x86 SIMD is nasm's; every other architecture assembles through
	// the C compiler's integrated assembler. wasm has neither a SIMD path in
	// dav1d nor an assembler to reach one with.
	let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
	let asm = !is_wasm();
	require_build_tool("meson", "--version");
	require_build_tool("ninja", "--version");
	if asm && (target_arch == "x86" || target_arch == "x86_64") {
		require_build_tool("nasm", "-v");
	}

	// meson refuses to re-`setup` a configured directory, and its `--wipe`
	// replays the PREVIOUS options rather than the ones passed here. Starting
	// clean is the only variant that answers correctly when the options
	// change; build.rs only re-runs when the submodule or the target does.
	let _ = std::fs::remove_dir_all(&build_dir);

	let mut setup = Command::new("meson");
	setup
		.arg("setup")
		.arg(&build_dir)
		.arg("deps/dav1d")
		.arg(format!("--prefix={}", prefix.display()))
		// meson answers `lib64` / `lib/<multiarch>` on some Linux
		// distributions; the DAV1D_LIBRARY seed in `build_libheif` and the
		// link search below both assume `lib`.
		.arg("--libdir=lib")
		.arg("--buildtype=release")
		.arg("--default-library=static")
		.arg(format!("-Denable_asm={asm}"))
		// The CLI tool, its tests, the examples and the docs are dead weight
		// on every target, and `enable_tools` is what would demand a working
		// `fseeko` and an xxhash muxer.
		.arg("-Denable_tools=false")
		.arg("-Denable_tests=false")
		.arg("-Denable_examples=false")
		.arg("-Denable_docs=false")
		// dav1d's diagnostics go to stderr through its default callback.
		// There is nowhere to print them in a thumbnailer, and on wasm the
		// write is a stubbed WASI call that fails anyway — so drop the
		// strings instead of shipping them.
		.arg("-Dlogging=false");
	// meson probes the compiler from PATH, and a plain Windows shell — a CI
	// runner's, or a developer's — has no MSVC environment loaded: it would
	// find whichever gcc the image ships and hand the MSVC link an archive it
	// cannot use. `--vsenv` makes meson locate and activate Visual Studio
	// itself. It is accepted (and inert) elsewhere, but only Windows needs it.
	if cfg!(windows) {
		setup.arg("--vsenv");
	}
	if let Some(cross_file) = write_meson_cross_file(&out_dir) {
		setup.arg(format!("--cross-file={}", cross_file.display()));
	}
	run(&mut setup, "meson setup (dav1d)");

	// `meson install` runs ninja itself, so this is the whole build.
	run(
		Command::new("meson")
			.arg("install")
			.arg("-C")
			.arg(&build_dir),
		"meson install (dav1d)",
	);

	println!("cargo:rerun-if-changed=deps/dav1d");
	println!("cargo:rustc-link-search=native={}/lib", prefix.display());
	println!("cargo:rustc-link-lib=static=dav1d");

	prefix
}

/// meson has no equivalent of cmake-rs's "derive the toolchain from cargo's
/// environment", so every cross target needs a cross file of its own. Written
/// into `OUT_DIR` rather than checked in, because every path in one is
/// machine-local: the wasi-sdk, the NDK, the selected Xcode SDK.
///
/// `None` means "build for the host" — the one case meson works out itself.
fn write_meson_cross_file(out_dir: &Path) -> Option<PathBuf> {
	let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap();
	let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();

	let (bin_dir, compiler_args, system, c_args) = if is_wasm() {
		let sdk = wasi_sdk_path();
		(
			sdk.join("bin"),
			vec![format!("--target={}", wasi_triple())],
			// Not one of meson's known systems, which is right: dav1d only
			// tests `system()` for the platforms with special cases (windows,
			// android, linux, darwin/ios/tvos) and wasi wants none of them.
			// The wasm path it does have is keyed on `cpu_family`.
			"wasi".to_string(),
			// Mirrors `config_cmake_for_wasi`: both wasm profiles disable
			// nontrapping-fptoint on the Rust side and the linker unions
			// feature sets, so the C objects must not use it either.
			// `-pthread` is what carries `+atomics` into the objects, and the
			// threaded profile's link demands every object match.
			if wasm_uses_atomics() {
				vec!["-mno-nontrapping-fptoint", "-mbulk-memory", "-pthread"]
			} else {
				vec!["-mno-nontrapping-fptoint", "-mbulk-memory"]
			},
		)
	} else if target_os == "android" {
		let sysroot = PathBuf::from(env::var("CARGO_NDK_SYSROOT_PATH").expect(
			"CARGO_NDK_SYSROOT_PATH must be set to cross-compile heif-decoder for Android \
			 (cargo-ndk sets it); dav1d's meson cross file is generated from it",
		));
		// <ndk>/toolchains/llvm/prebuilt/<host>/sysroot -> .../bin
		let bin_dir = sysroot
			.parent()
			.expect("CARGO_NDK_SYSROOT_PATH has no parent")
			.join("bin");
		// The API level belongs ON the triple: without it clang cannot find
		// `crtbegin_dynamic.o` (they sit in a per-API directory), and meson's
		// very first link probe fails. `ANDROID_PLATFORM` is the one cargo-ndk
		// fills with the level it chose — `CARGO_NDK_ANDROID_PLATFORM` carries
		// the ABI instead, whatever its name suggests.
		let platform = env::var("ANDROID_PLATFORM").unwrap_or_else(|_| "21".to_string());
		let triple = match target_arch.as_str() {
			"aarch64" => "aarch64-linux-android",
			"arm" => "armv7a-linux-androideabi",
			"x86_64" => "x86_64-linux-android",
			"x86" => "i686-linux-android",
			other => panic!("unsupported Android architecture {other} for dav1d"),
		};
		(
			bin_dir,
			vec![
				format!("--target={triple}{platform}"),
				format!("--sysroot={}", sysroot.display()),
			],
			"android".to_string(),
			vec![],
		)
	} else if target_os == "ios" {
		let deployment_target =
			env::var("DEPLOYMENT_TARGET").unwrap_or_else(|_| "12.0".to_string());
		let (sdk, os_part) = if env::var("TARGET").unwrap().contains("ios-sim") {
			("iphonesimulator", "ios-simulator")
		} else {
			("iphoneos", "ios")
		};
		let sdk_path = String::from_utf8(
			Command::new("xcrun")
				.args(["--sdk", sdk, "--show-sdk-path"])
				.output()
				.expect("xcrun must be on PATH to cross-compile heif-decoder for iOS")
				.stdout,
		)
		.expect("xcrun printed non-UTF-8");
		(
			// Apple clang is found through xcrun's own PATH entry, not a
			// toolchain directory of ours.
			PathBuf::new(),
			vec![
				format!("--target={target_arch}-apple-{os_part}{deployment_target}"),
				format!("-isysroot{}", sdk_path.trim()),
			],
			"ios".to_string(),
			vec![],
		)
	} else {
		return None;
	};

	let cpu_family = match target_arch.as_str() {
		"wasm32" => "wasm32",
		"aarch64" => "aarch64",
		"x86_64" => "x86_64",
		"arm" => "arm",
		"x86" => "x86",
		other => panic!("no meson cpu_family mapping for {other}"),
	};
	let tool = |name: &str| {
		if bin_dir.as_os_str().is_empty() {
			name.to_string()
		} else {
			bin_dir.join(name).display().to_string()
		}
	};
	let quoted = |values: &[String]| {
		values
			.iter()
			.map(|value| format!("'{value}'"))
			.collect::<Vec<_>>()
			.join(", ")
	};

	let mut compiler = vec![tool("clang")];
	compiler.extend(compiler_args);
	// Apple's toolchain ships `ar`/`strip`; meson cannot identify an `llvm-ar`
	// as an archiver there. The wasi-sdk and the NDK both ship the llvm names.
	let (archiver, stripper) = if target_os == "ios" {
		("ar", "strip")
	} else {
		("llvm-ar", "llvm-strip")
	};
	// `c_std`: dav1d's project asks for c99, and under `__STRICT_ANSI__` both
	// wasi-libc and Bionic hide every POSIX declaration — `clock_gettime`
	// included, whose absence dav1d's configure treats as fatal. `gnu99` is
	// the same language with the platform's default feature-test macros left
	// alone.
	let contents = format!(
		"[binaries]\n\
		 c = [{}]\n\
		 ar = '{}'\n\
		 strip = '{}'\n\
		 \n\
		 [host_machine]\n\
		 system = '{system}'\n\
		 cpu_family = '{cpu_family}'\n\
		 cpu = '{cpu_family}'\n\
		 endian = 'little'\n\
		 \n\
		 [built-in options]\n\
		 c_args = [{}]\n\
		 c_std = 'gnu99'\n",
		quoted(&compiler),
		tool(archiver),
		tool(stripper),
		quoted(&c_args.into_iter().map(str::to_string).collect::<Vec<_>>()),
	);
	let path = out_dir.join("dav1d-cross.ini");
	std::fs::write(&path, contents).expect("could not write the dav1d meson cross file");
	Some(path)
}

/// dav1d has no build system but meson's, and nothing else in this workspace
/// needs meson, ninja or nasm. Say so here, where the answer is actionable,
/// rather than letting the failure surface as a "command not found" from
/// inside a build script three layers down.
fn require_build_tool(tool: &str, version_flag: &str) {
	let found = Command::new(tool)
		.arg(version_flag)
		.stdout(std::process::Stdio::null())
		.stderr(std::process::Stdio::null())
		.status()
		.is_ok_and(|status| status.success());
	assert!(
		found,
		"`{tool}` is required to build heif-decoder's AV1 decoder: dav1d builds with \
		 meson + ninja, and nasm assembles its x86 SIMD. Install it (macOS: \
		 `brew install {tool}`; Debian/Ubuntu: `apt-get install {tool}`; Windows: \
		 `choco install {tool}`), or keep this crate out of the build entirely with \
		 `--workspace --exclude heif-decoder` — it is a workspace member, so a \
		 workspace-root cargo command builds it whatever features you asked for."
	);
}

fn run(command: &mut Command, what: &str) {
	let status = command
		.current_dir(env::var("CARGO_MANIFEST_DIR").unwrap())
		.status()
		.unwrap_or_else(|err| panic!("could not run {what}: {err}"));
	assert!(status.success(), "{what} failed: {status}");
}

fn build_libheif(libde265_path: &Path, dav1d_path: &Path) -> PathBuf {
	let mut config = Config::new("deps/libheif");
	// Its own out dir: see `build_libde265`.
	config.out_dir(PathBuf::from(env::var("OUT_DIR").unwrap()).join("libheif"));
	config_cmake_for_android(&mut config);
	config_cmake_for_macos(&mut config);
	config_cmake_for_libcxx(&mut config);
	config_cmake_for_ios(&mut config);
	config_cmake_for_wasi(&mut config);

	if is_wasm() {
		// `ENABLE_MULTITHREADING_SUPPORT` is what compiles libheif's mutexes
		// in — including the ones guarding its process-wide state (the colour
		// conversion pipeline's operation pool, `heif_init`'s plugin
		// registry). The threaded profile RUNS DECODES ON A RAYON POOL, so two
		// Web Workers sharing one linear memory can be inside a cold module at
		// once: the locks have to be there. The single-threaded service-worker
		// profile has no second thread and no atomics to build them from, so
		// it keeps them out.
		config.define(
			"ENABLE_MULTITHREADING_SUPPORT",
			if wasm_uses_atomics() { "ON" } else { "OFF" },
		);
		// Never on either profile: this is the switch that spawns
		// std::thread WORKERS, which no browser module can do on its own.
		// libde265's pthread use survives (it has no such switch) and is kept
		// dormant at runtime instead — see DecodeOptions in src/lib.rs.
		config.define("ENABLE_PARALLEL_TILE_DECODING", "OFF");
	}

	// EVERY target, not just wasm. libheif defaults `WITH_DAV1D_PLUGIN` to ON
	// — dav1d is the one codec whose `plugin_option` asks for it — so with
	// plugin loading enabled the AV1 decoder was built as a SEPARATE `.so`
	// next to the static library instead of into it, and libheif was left
	// looking for it at runtime under the absolute `OUT_DIR` of whichever
	// machine ran the build. On a device that directory does not exist and
	// iOS cannot dlopen an unsigned library anyway: a missing plugin
	// directory fails silently, so every AVIF thumbnail came back
	// "unsupported" with nothing in the log while HEIC kept working. Turning
	// plugin loading off compiles the decoder in (`ar t libheif.a` gains
	// `decoder_dav1d.cc.o`), drops the 10 MB orphan and the baked-in path,
	// and takes the dlopen/opendir/getenv surface with it.
	config.define("ENABLE_PLUGIN_LOADING", "OFF");

	config.define("LIBDE265_INCLUDE_DIR", libde265_path.join("include"));

	if env::var("CARGO_CFG_TARGET_OS").unwrap() == "windows" {
		config.define("LIBDE265_LIBRARY", libde265_path.join("lib/libde265.lib"));

		config.define("CMAKE_C_FLAGS", "/DLIBDE265_STATIC_BUILD");
		config.define("CMAKE_CXX_FLAGS", "/DLIBDE265_STATIC_BUILD");
	} else {
		config.define("LIBDE265_LIBRARY", libde265_path.join("lib/libde265.a"));
	}

	config.define("WITH_LIBDE265", "ON");

	// FindDAV1D.cmake is a pkg-config hint plus `find_path`/`find_library`,
	// with no `find_package(... CONFIG)` branch ahead of it — so seeding the
	// two cache variables settles which dav1d libheif compiles against, and a
	// system install cannot hijack it (which is exactly what FindAOM.cmake's
	// CONFIG branch could do back when this vendored libaom).
	config.define("DAV1D_INCLUDE_DIR", dav1d_path.join("include"));
	config.define(
		"DAV1D_LIBRARY",
		dav1d_path.join(if env::var("CARGO_CFG_TARGET_OS").unwrap() == "windows" {
			"lib/dav1d.lib"
		} else {
			"lib/libdav1d.a"
		}),
	);
	config.define("WITH_DAV1D", "ON");

	config.define("WITH_X265", "OFF");
	config.define("WITH_AOM_DECODER", "OFF");
	config.define("WITH_AOM_ENCODER", "OFF");
	config.define("WITH_RAV1E", "OFF");
	config.define("WITH_SvtEnc", "OFF");
	config.define("WITH_JPEG_DECODER", "OFF");
	config.define("WITH_JPEG_ENCODER", "OFF");
	config.define("WITH_OpenJPEG_DECODER", "OFF");
	config.define("WITH_OpenJPEG_ENCODER", "OFF");
	config.define("WITH_LIBSHARPYUV", "OFF");
	config.define("WITH_OpenH264_DECODER", "OFF");
	config.define("WITH_OpenH264_ENCODER", "OFF");
	config.define("WITH_OPENJPH_DECODER", "OFF");
	config.define("WITH_OPENJPH_ENCODER", "OFF");
	// Every optional codec is listed, including ones we have never wanted:
	// libheif's CMake AUTO-DETECTS them, so a codec merely installed on the
	// build machine gets compiled into libheif.a while nothing links its
	// library — the x264 encoder did exactly that on the 1.20 -> 1.23 bump and
	// broke the link with undefined _x264_* symbols. Only WITH_LIBDE265 is ON.
	config.define("WITH_X264", "OFF");
	config.define("WITH_KVAZAAR", "OFF");
	config.define("WITH_UVG266", "OFF");
	config.define("WITH_VVDEC", "OFF");
	config.define("WITH_VVENC", "OFF");
	config.define("WITH_FFMPEG_DECODER", "OFF");
	config.define("WITH_WEBCODECS", "OFF");
	config.define("WITH_GDK_PIXBUF", "OFF");

	// OFF is libheif's default; pinned because turning it ON reopens a decode
	// trap. `Box_snuc::parse` (deps/libheif/libheif/codecs/uncompressed/
	// unc_boxes.cc) bounds `image_width * image_height` against whatever limits
	// are in force AT PARSE TIME — libheif's globals, since
	// `HeifSession::set_decode_limits` only tightens the context afterwards. So
	// w*h = 2^28 clears `max_image_size_pixels` (32768^2), clears
	// `max_memory_block_size` (4 GB), skips total-memory accounting altogether
	// (`MemoryHandle::alloc` returns early for the global context), then resizes
	// two `std::vector<float>` of 2^28 entries — 1 GiB apiece, against the wasm
	// module's 1 GiB linker cap (`.cargo/config.toml`, `--max-memory`). The
	// `bad_alloc` has nothing to catch it, so it reaches the trapping
	// `__cxa_throw` stub in src/lib.rs and kills the module — from a ~40-byte
	// box in an uploaded file. That path is dead only because `snuc` is
	// registered under `#if WITH_UNCOMPRESSED_CODEC` (box.cc) and unc_boxes.cc
	// compiles only under it. Whoever wants uncompressed HEIF must fix the
	// parse-time limits first.
	config.define("WITH_UNCOMPRESSED_CODEC", "OFF");

	config.define("WITH_EXAMPLES", "OFF");
	config.define("BUILD_TESTING", "OFF");

	config.define("BUILD_SHARED_LIBS", "OFF");

	let dst = config.build();

	println!("cargo:rerun-if-changed=deps/libheif");
	println!("cargo:rustc-link-search=native={}/lib", dst.display());
	println!("cargo:rustc-link-lib=static=heif");

	dst
}

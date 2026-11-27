#!/bin/bash
set -e
# The cache's wasm SQLite (sqlite-wasm-rs) compiles sqlite3.c for wasm32, which Apple clang
# cannot target — point cc at an LLVM clang (brew install llvm) unless the caller already did.
# The C objects must also carry the atomics/bulk-memory target features, or the shared-memory
# (threaded) link rejects them.
export CC_wasm32_unknown_unknown="${CC_wasm32_unknown_unknown:-/opt/homebrew/opt/llvm/bin/clang}"
export AR_wasm32_unknown_unknown="${AR_wasm32_unknown_unknown:-/opt/homebrew/opt/llvm/bin/llvm-ar}"
export CFLAGS_wasm32_unknown_unknown="${CFLAGS_wasm32_unknown_unknown:--matomics -mbulk-memory}"
# heif-decoder cross-compiles its vendored libheif/libde265 with wasi-sdk
# (https://github.com/WebAssembly/wasi-sdk): point WASI_SDK_PATH at an
# extracted release (>= 25, so it carries the wasm32-wasi-threads sysroot).
if [ ! -d "${WASI_SDK_PATH:-}/share/wasi-sysroot" ]; then
	echo "WASI_SDK_PATH must point at an extracted wasi-sdk release (needed for -F heif-decoder)" >&2
	exit 1
fi
# std is rebuilt with its size-optimized algorithms (`optimize_for_size`: smaller sort, integer
# formatting and the like, same results), as the whole build is opt-level "s". Naming a feature
# drops std's defaults, panic-unwind and backtrace, which neither build links: wasm32 panics abort.
BUILD_STD="-Z build-std=panic_abort,std -Z build-std-features=optimize_for_size"
wasm-pack build --target web -s filen --out-name sdk-rs --out-dir web/tmp --profile web-release --no-pack . -- -F wasm-full,cache,heif-decoder $BUILD_STD
rm web/tmp/.gitignore
rsync -av web/tmp/ web
rm -rf web/tmp
# don't need support for atomics in service worker
export RUSTFLAGS="-C target-feature=-nontrapping-fptoint,+bulk-memory --cfg getrandom_backend=\"wasm_js\""
wasm-pack build --target web -s filen --out-name sdk-rs --out-dir web/tmp --profile web-release --no-pack . -- --no-default-features -F service-worker,heif-decoder $BUILD_STD
rm web/tmp/.gitignore
rsync -av web/tmp/ web/service-worker
rm -rf web/tmp

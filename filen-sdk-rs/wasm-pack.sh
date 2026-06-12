#!/bin/bash
set -e
# The cache's wasm SQLite (sqlite-wasm-rs) compiles sqlite3.c for wasm32, which Apple clang
# cannot target — point cc at an LLVM clang (brew install llvm) unless the caller already did.
# The C objects must also carry the atomics/bulk-memory target features, or the shared-memory
# (threaded) link rejects them.
export CC_wasm32_unknown_unknown="${CC_wasm32_unknown_unknown:-/opt/homebrew/opt/llvm/bin/clang}"
export AR_wasm32_unknown_unknown="${AR_wasm32_unknown_unknown:-/opt/homebrew/opt/llvm/bin/llvm-ar}"
export CFLAGS_wasm32_unknown_unknown="${CFLAGS_wasm32_unknown_unknown:--matomics -mbulk-memory}"
wasm-pack build --target web -s filen --out-name sdk-rs --out-dir web/tmp --profile web-release --no-pack . -- -F wasm-full,cache -Z build-std=panic_abort,std
rm web/tmp/.gitignore
rsync -av web/tmp/ web
rm -rf web/tmp
# don't need support for atomics in service worker
export RUSTFLAGS="-C target-feature=-nontrapping-fptoint,+bulk-memory --cfg getrandom_backend=\"wasm_js\""
wasm-pack build --target web -s filen --out-name sdk-rs --out-dir web/tmp --profile web-release --no-pack . -- --no-default-features -F service-worker -Z build-std=panic_abort,std
rm web/tmp/.gitignore
rsync -av web/tmp/ web/service-worker
rm -rf web/tmp
